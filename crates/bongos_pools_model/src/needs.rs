//! The grammar of a `needs` entry, the same wherever a pool is named: a pool name, or a table
//! `{ pool, mode, count }`.

use crate::decl::{PoolDecl, PoolKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NeedMode {
    Shared,
    Exclusive,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Need {
    pub pool: String,
    /// Written `mode`; `None` is the pool's default (`shared` on `rw`).
    pub mode: Option<NeedMode>,
    /// Written `count`; `None` is one.
    pub count: Option<u32>,
}

impl Need {
    pub fn count(&self) -> u32 {
        self.count.unwrap_or(1)
    }

    /// Checks this need against the pool it names: `mode` only on `rw`, `count` only on `slots`
    /// and at most the pool's size, and neither on a `guard` pool. A mutex refuses `count > 1`.
    pub fn check_against(&self, pool: &PoolDecl) -> Result<(), String> {
        if self.mode.is_some() && pool.kind != PoolKind::Rw {
            return Err(format!(
                "`mode` is only valid on an rw pool, and pool `{}` is {}",
                pool.name,
                pool.kind.word()
            ));
        }
        let Some(count) = self.count else {
            return Ok(());
        };
        match pool.kind {
            PoolKind::Slots => match pool.size {
                Some(size) if count > size => Err(format!(
                    "count {count} is above the size {size} of pool `{}`",
                    pool.name
                )),
                _ => Ok(()),
            },
            PoolKind::Mutex if count > 1 => Err(format!(
                "count {count} is above 1, the size of mutex pool `{}`",
                pool.name
            )),
            PoolKind::Mutex => Ok(()),
            PoolKind::Rw | PoolKind::Guard => Err(format!(
                "`count` is only valid on a slots pool, and pool `{}` is {}",
                pool.name,
                pool.kind.word()
            )),
        }
    }
}

/// Parses one `needs` entry.
pub fn parse_need(value: &toml::Value) -> Result<Need, String> {
    let table = match value {
        toml::Value::String(pool) => {
            return Ok(Need {
                pool: pool.clone(),
                mode: None,
                count: None,
            });
        }
        toml::Value::Table(table) => table,
        _ => {
            return Err(
                "a needs entry is a pool name or a { pool, mode, count } table".to_string(),
            );
        }
    };
    for key in table.keys() {
        if !["pool", "mode", "count"].contains(&key.as_str()) {
            return Err(format!("unknown key \"{key}\" in a needs entry"));
        }
    }
    let pool = table
        .get("pool")
        .and_then(toml::Value::as_str)
        .ok_or("a needs entry needs a `pool` name")?
        .to_string();
    let mode = match table.get("mode") {
        None => None,
        Some(toml::Value::String(mode)) => match mode.as_str() {
            "shared" => Some(NeedMode::Shared),
            "exclusive" => Some(NeedMode::Exclusive),
            other => {
                return Err(format!(
                    "mode must be \"shared\" or \"exclusive\", not \"{other}\""
                ));
            }
        },
        Some(_) => return Err("mode must be \"shared\" or \"exclusive\"".to_string()),
    };
    let count = match table.get("count") {
        None => None,
        Some(toml::Value::Integer(count)) if *count >= 1 => {
            Some(u32::try_from(*count).map_err(|_| "count is too large".to_string())?)
        }
        Some(_) => return Err("count must be a whole number of at least 1".to_string()),
    };
    Ok(Need { pool, mode, count })
}
