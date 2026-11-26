//! The wait-for graph that refuses a request which would deadlock.
//!
//! Nodes are holder paths and waiting requests. A waiting request has an edge, per pool it waits
//! in, to every path whose hold it waits for (by the grant rule's own fit test, see
//! [`crate::grant`]) and to every older waiting request of that pool, since nobody is granted
//! ahead of an older waiter. A path has an edge to every waiting request of its own path or of a
//! path it encloses, because a holder does not release while it, or a node inside it, waits.
//! Requests are nodes of their own (named by host and id) so that two requests of one path are
//! never confused; a cycle is reported as the paths it passes through.
//!
//! Every table given counts (the project's and the machine's), with every host's entries, dead or
//! alive, as the grant rule counts them. Nothing here reads the system.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use crate::grant::{
    PoolState, PoolTable, Request, RequestId, Satisfied, blocking_holds, own_holds,
};
use crate::holder::{Holder, HolderLabel, HolderPath, HostRef};
use crate::needs::Need;

/// Why a request is refused before it is queued.
#[derive(Debug, Clone, PartialEq)]
pub enum Refused {
    /// An enclosing holder holds this pool in a way the request cannot fit beside, and it is not
    /// released while the request waits.
    OwnHold { pool: String },
    /// Queuing the request would close this cycle, which starts at the requester.
    Deadlock { cycle: Vec<CycleLink> },
}

/// One path of a deadlock cycle. `waits_for` is the pool where this path waits for the next link;
/// it is `None` for an enclosing path that waits only through a node inside it.
#[derive(Debug, Clone, PartialEq)]
pub struct CycleLink {
    pub project: String,
    pub path: HolderPath,
    pub label: HolderLabel,
    pub waits_for: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Node {
    Path(String, HolderPath),
    Request(HostRef, RequestId),
}

/// A waiting request, gathered from every table, with the pools it waits in.
struct Waiting<'a> {
    holder: &'a Holder,
    /// (pool, need, ticket) in each pool it waits in; the new request has no ticket.
    needs: Vec<(&'a PoolState, &'a str, &'a Need, Option<u64>)>,
}

#[derive(Default)]
struct Graph {
    /// Each edge keeps the first pool (by name) it was found in, for the report.
    edges: BTreeMap<Node, BTreeMap<Node, Option<String>>>,
    labels: BTreeMap<Node, (String, HolderPath, HolderLabel)>,
}

impl Graph {
    fn name(&mut self, node: &Node, holder: &Holder) {
        self.labels.entry(node.clone()).or_insert_with(|| {
            (
                holder.project.clone(),
                holder.path.clone(),
                holder.label.clone(),
            )
        });
    }

    fn edge(&mut self, from: Node, to: Node, pool: Option<&str>) {
        let label = self
            .edges
            .entry(from)
            .or_default()
            .entry(to)
            .or_insert(None);
        if let Some(pool) = pool {
            if label.as_deref().map_or(true, |old| pool < old) {
                *label = Some(pool.to_string());
            }
        }
    }
}

fn path_node(holder: &Holder) -> Node {
    Node::Path(holder.project.clone(), holder.path.clone())
}

fn request_node(holder: &Holder, request: RequestId) -> Node {
    Node::Request(holder.host, request)
}

fn find_pool<'a>(tables: &[&'a PoolTable], name: &str) -> Option<&'a PoolState> {
    tables.iter().find_map(|table| table.pools.get(name))
}

/// Refuses the request when queuing it would close a cycle of the wait-for graph through it, as
/// [`Refused::Deadlock`] naming every path of the cycle from the requester on. The new request is
/// the one refused, so nothing that already waits is disturbed. A pool its enclosing holders
/// cover adds no edge. Each pool is looked up in the first table that has it; a pool no table has
/// adds no edge.
pub fn would_cycle(tables: &[&PoolTable], request: &Request) -> Result<(), Refused> {
    let satisfied = own_holds(|name| find_pool(tables, name), request).unwrap_or_default();
    let mut waiting: BTreeMap<Node, Waiting> = BTreeMap::new();
    for table in tables {
        for (name, pool) in &table.pools {
            for waiter in &pool.waiters {
                waiting
                    .entry(request_node(&waiter.holder, waiter.request))
                    .or_insert_with(|| Waiting {
                        holder: &waiter.holder,
                        needs: Vec::new(),
                    })
                    .needs
                    .push((pool, name.as_str(), &waiter.need, Some(waiter.ticket)));
            }
        }
    }
    let start = request_node(&request.holder, request.id);
    let new_needs: Vec<_> = request
        .needs
        .iter()
        .filter(|need| !satisfied.contains(&need.pool))
        .filter_map(|need| {
            find_pool(tables, &need.pool).map(|pool| (pool, need.pool.as_str(), need, None))
        })
        .collect();
    waiting
        .entry(start.clone())
        .or_insert_with(|| Waiting {
            holder: &request.holder,
            needs: Vec::new(),
        })
        .needs
        .extend(new_needs);

    let mut graph = Graph::default();
    graph.name(&start, &request.holder);
    let mut holders: BTreeMap<Node, &Holder> = BTreeMap::new();
    for (node, entry) in &waiting {
        graph.name(node, entry.holder);
        for (pool, name, need, ticket) in &entry.needs {
            for hold in blocking_holds(pool, need, entry.holder) {
                let target = path_node(&hold.holder);
                graph.name(&target, &hold.holder);
                holders.entry(target.clone()).or_insert(&hold.holder);
                graph.edge(node.clone(), target, Some(name));
            }
            for older in &pool.waiters {
                let is_older = ticket.map_or(true, |ticket| older.ticket < ticket);
                let target = request_node(&older.holder, older.request);
                if is_older && target != *node {
                    graph.edge(node.clone(), target, Some(name));
                }
            }
        }
    }
    for (path, holder) in &holders {
        for (node, entry) in &waiting {
            if holder.is_or_encloses(entry.holder) {
                graph.edge(path.clone(), node.clone(), None);
            }
        }
    }

    match shortest_cycle(&graph, &start) {
        Some(nodes) => Err(Refused::Deadlock {
            cycle: links(&graph, &nodes),
        }),
        None => Ok(()),
    }
}

/// Runs the whole pre-queue judgment across the tables: the enclosing holders' holds first
/// ([`crate::grant::check_own_hold`], each pool in the first table that has it), then
/// [`would_cycle`]. On success the broker queues [`Satisfied::remaining`].
pub fn judge(tables: &[&PoolTable], request: &Request) -> Result<Satisfied, Refused> {
    let satisfied = own_holds(|name| find_pool(tables, name), request)?;
    would_cycle(tables, request)?;
    Ok(satisfied)
}

/// The shortest cycle from `start` back to it, as its nodes in order from `start`, found
/// breadth first in node order so the answer is the same every time.
fn shortest_cycle(graph: &Graph, start: &Node) -> Option<Vec<Node>> {
    let mut parent: BTreeMap<&Node, &Node> = BTreeMap::new();
    let mut seen: BTreeSet<&Node> = BTreeSet::new();
    let mut queue: VecDeque<&Node> = VecDeque::new();
    seen.insert(start);
    queue.push_back(start);
    while let Some(node) = queue.pop_front() {
        let Some(targets) = graph.edges.get(node) else {
            continue;
        };
        for target in targets.keys() {
            if target == start {
                let mut nodes = vec![node.clone()];
                let mut at = node;
                while let Some(previous) = parent.get(at) {
                    nodes.push((*previous).clone());
                    at = previous;
                }
                nodes.reverse();
                return Some(nodes);
            }
            if seen.insert(target) {
                parent.insert(target, node);
                queue.push_back(target);
            }
        }
    }
    None
}

/// The paths of a cycle of nodes. Each request becomes its path, waiting in the pool of its edge
/// to the next node. A holder path followed by a request of that same path says only that the
/// path holds what the previous link waits for, so it is left out; a holder path followed by a
/// request of a path it encloses stays, with no pool.
fn links(graph: &Graph, nodes: &[Node]) -> Vec<CycleLink> {
    let mut links = Vec::new();
    for (index, node) in nodes.iter().enumerate() {
        let next = &nodes[(index + 1) % nodes.len()];
        if let Node::Path(project, path) = node {
            if matches!(next, Node::Request(..))
                && graph.labels[next].0 == *project
                && graph.labels[next].1 == *path
            {
                continue;
            }
        }
        let waits_for = graph
            .edges
            .get(node)
            .and_then(|targets| targets.get(next))
            .cloned()
            .flatten();
        let (project, path, label) = graph.labels[node].clone();
        links.push(CycleLink {
            project,
            path,
            label,
            waits_for,
        });
    }
    links
}
