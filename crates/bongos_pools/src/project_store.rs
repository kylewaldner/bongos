//! The project store: a project's pools as rows of its state store's `kv` namespace `pools`,
//! `pool/<name>` per pool (kind, size, guard, holds and waiters) and `table` (the ticket counter
//! and the generation). Only the project's writer process writes them, through
//! `StateWriter::transact`; client windows read them with `StateStore::open_reader`
//! ([`ProjectPoolsReader`]). No `StateExporter` is registered for the namespace: holds are derived
//! from live processes and mean nothing in a snapshot or a backup. Waiting times are never
//! written; a view computes them from each waiter's `since`.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context as _, Result};
use bongos_pools_model::{Grant, PoolState, PoolTable, Pruned};
use bongos_project_repo::{Location, StateRead, StateReader, StateStore, StateTx, StateWriter};
use serde::{Deserialize, Serialize};

use crate::declare::{DeclareOutcome, KindClash, PoolConfig};
use crate::store::{PoolStore, StoreContext, Transacted, is_mixed};

/// The `kv` namespace of the project store.
pub const POOLS_NAMESPACE: &str = "pools";
/// The key prefix of a pool's row.
pub const POOL_KEY_PREFIX: &str = "pool/";
/// The key of the row holding the ticket counter and the generation.
pub const TABLE_KEY: &str = "table";

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
struct TableRow {
    next_ticket: u64,
    generation: u64,
}

/// The table as the namespace holds it, with its generation.
fn load(rows: Vec<(String, toml::Value)>) -> Result<(PoolTable, u64)> {
    let mut table = PoolTable::default();
    let mut generation = 0;
    for (key, value) in rows {
        if key == TABLE_KEY {
            let row: TableRow = value.try_into().context("reading the pools table row")?;
            table.next_ticket = row.next_ticket;
            generation = row.generation;
        } else if let Some(name) = key.strip_prefix(POOL_KEY_PREFIX) {
            let pool: PoolState = value
                .try_into()
                .with_context(|| format!("reading the row of pool {name}"))?;
            table.pools.insert(name.to_string(), pool);
        }
    }
    Ok((table, generation))
}

/// The project's pool table as `read` sees it, with its generation.
pub fn read_project_table(read: &StateRead) -> Result<(PoolTable, u64)> {
    load(read.kv(POOLS_NAMESPACE).scan("")?)
}

/// The project's pools for a client window, which never writes them.
pub struct ProjectPoolsReader {
    reader: StateReader,
}

impl ProjectPoolsReader {
    pub fn open(location: &Location) -> Result<ProjectPoolsReader> {
        Ok(ProjectPoolsReader {
            reader: StateStore::open_reader(location)?,
        })
    }

    /// The table with its generation.
    pub fn read(&self) -> Result<(PoolTable, u64)> {
        self.reader.read(read_project_table)
    }
}

/// The project store of the project's writer process.
pub struct ProjectStore {
    writer: Arc<StateWriter>,
    context: StoreContext,
    generation: AtomicU64,
}

impl ProjectStore {
    /// Opens the store on the project's writer and prunes it at once, so no hold of a process
    /// that is gone survives to the first grant. Returns what was pruned, for the log.
    pub fn open(writer: Arc<StateWriter>, context: StoreContext) -> Result<(ProjectStore, Pruned)> {
        let store = ProjectStore {
            writer,
            context,
            generation: AtomicU64::new(0),
        };
        let opened = store.try_transact_full(|_| Ok(()))?;
        Ok((store, opened.pruned))
    }

    pub fn context(&self) -> &StoreContext {
        &self.context
    }

    pub fn writer(&self) -> &Arc<StateWriter> {
        &self.writer
    }

    pub(crate) fn note_generation(&self, generation: u64) {
        self.generation.store(generation, Ordering::SeqCst);
    }

    /// Registers the project's own pools (scope `project`): a new pool is created, a declared one
    /// takes the new kind, size and guard and keeps its holds, waiters and guard reading. A pool
    /// in use (held or waited for) never changes kind; that pool is refused. Pools no longer
    /// declared are left for the caller to remove once empty.
    pub fn declare(&self, configs: &[PoolConfig]) -> Result<Transacted<DeclareOutcome>> {
        self.try_transact_full(|table| Ok(declare_in(table, configs)))
    }

    /// Runs `step` on the table loaded inside the open state transaction `tx`, writes back the
    /// rows that changed and the bumped generation. Returns the step's result and the new
    /// generation.
    pub(crate) fn transact_in<T>(
        tx: &mut StateTx,
        step: impl FnOnce(&mut PoolTable) -> Result<(T, Pruned, Vec<Grant>)>,
    ) -> Result<(Transacted<T>, PoolTable)> {
        let (before, generation) = load(tx.kv(POOLS_NAMESPACE).scan("")?)?;
        let mut table = before.clone();
        let (value, pruned, grants) = step(&mut table)?;
        let generation = generation + 1;
        let mut kv = tx.kv(POOLS_NAMESPACE);
        for (name, pool) in &table.pools {
            if before.pools.get(name) != Some(pool) {
                kv.put(
                    &format!("{POOL_KEY_PREFIX}{name}"),
                    toml::Value::try_from(pool)
                        .with_context(|| format!("writing the row of pool {name}"))?,
                )?;
            }
        }
        for name in before.pools.keys() {
            if !table.pools.contains_key(name) {
                kv.delete(&format!("{POOL_KEY_PREFIX}{name}"))?;
            }
        }
        kv.put(
            TABLE_KEY,
            toml::Value::try_from(TableRow {
                next_ticket: table.next_ticket,
                generation,
            })
            .context("writing the pools table row")?,
        )?;
        Ok((
            Transacted {
                value,
                pruned,
                grants,
                generation,
            },
            table,
        ))
    }
}

pub(crate) fn declare_in(table: &mut PoolTable, configs: &[PoolConfig]) -> DeclareOutcome {
    let mut outcome = DeclareOutcome::default();
    for config in configs {
        match table.pools.get_mut(&config.name) {
            None => {
                table.pools.insert(config.name.clone(), config.state());
            }
            Some(pool) => {
                let in_use = !pool.holds.is_empty() || !pool.waiters.is_empty();
                if pool.kind != config.kind && in_use {
                    outcome.refused.push(KindClash {
                        pool: config.name.clone(),
                        declared: config.kind,
                        stands: pool.kind,
                        set_by: None,
                    });
                    continue;
                }
                config.apply_to(pool);
            }
        }
    }
    outcome
}

impl PoolStore for ProjectStore {
    fn try_transact_full<T>(
        &self,
        f: impl FnOnce(&mut PoolTable) -> Result<T>,
    ) -> Result<Transacted<T>> {
        let context = &self.context;
        let (done, _) = self.writer.transact(|tx| {
            ProjectStore::transact_in(tx, |table| context.step(table, is_mixed, f))
        })?;
        self.note_generation(done.generation);
        Ok(done)
    }

    fn read(&self) -> Result<PoolTable> {
        let (table, generation) = self.writer.read(read_project_table)?;
        self.note_generation(generation);
        Ok(table)
    }

    fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }
}
