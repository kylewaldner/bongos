//! The layout of a run's graph for the run view: pure, from a definition and the replayed state.
//!
//! Nodes are layered by longest path from `start` over forward edges; a bounded route back to a
//! node on the path to it is a back edge, left out of the layering and drawn as an arc labeled
//! with its pass count. A node that ran once per item of a fan-out gets one lane per item. A node
//! no path has reached yet is [`Status::Pending`] (drawn dimmed); once the run is over those
//! nodes are [`Status::Skipped`]. [`GraphLayout::text`] is the debug dump of the view.

use std::collections::{BTreeMap, BTreeSet};

use crate::definition::{Definition, EndKind, NodeKind, Target};
use crate::record::{EndState, NodeKey, RunState};

/// What a laid-out box shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// Not reached yet.
    Pending,
    /// Not reached and never will be: the run is over.
    Skipped,
    Running,
    /// Stopped at an open gate.
    Waiting,
    Succeeded,
    Failed,
    /// An end the run reached.
    Reached,
}

impl Status {
    pub fn label(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Skipped => "skipped",
            Self::Running => "running",
            Self::Waiting => "waiting",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Reached => "reached",
        }
    }

    /// Whether the box is drawn dimmed.
    pub fn dimmed(self) -> bool {
        matches!(self, Self::Pending | Self::Skipped)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoxKind {
    Agent,
    Program,
    EndSuccess,
    EndFailure,
}

impl BoxKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Program => "program",
            Self::EndSuccess => "end (success)",
            Self::EndFailure => "end (failure)",
        }
    }
}

/// One box of the graph.
#[derive(Debug, Clone, PartialEq)]
pub struct LayoutNode {
    /// The node or end name.
    pub name: String,
    pub kind: BoxKind,
    pub column: usize,
    pub lane: usize,
    /// The fan-out branch this box is the lane of; empty outside any fan-out.
    pub branch: Vec<(String, u32)>,
    pub status: Status,
    /// The highest pass reached; 0 when never started.
    pub passes: u32,
}

impl LayoutNode {
    /// The key of the box in the run, when it is a node: its branch and latest pass.
    pub fn key(&self) -> Option<NodeKey> {
        matches!(self.kind, BoxKind::Agent | BoxKind::Program).then(|| NodeKey {
            node: self.name.clone(),
            branch: self.branch.clone(),
            pass: self.passes.max(1),
        })
    }
}

/// A forward edge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayoutEdge {
    pub from: String,
    pub to: String,
    pub label: String,
}

/// A back edge, drawn as an arc.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LayoutArc {
    pub from: String,
    pub to: String,
    /// "pass 2 of 3".
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct GraphLayout {
    pub columns: usize,
    pub nodes: Vec<LayoutNode>,
    pub edges: Vec<LayoutEdge>,
    pub arcs: Vec<LayoutArc>,
}

struct Edge {
    to: String,
    label: String,
    max_passes: Option<u32>,
}

/// Lays out `definition` with the progress in `state`.
pub fn layout(definition: &Definition, state: &RunState) -> GraphLayout {
    let mut out: BTreeMap<String, Vec<Edge>> = BTreeMap::new();
    for node in &definition.nodes {
        let edges = out.entry(node.name.clone()).or_default();
        for route in &node.routes {
            let when: Vec<String> = route
                .when
                .iter()
                .map(|condition| format!("{}={}", condition.field, condition.value))
                .collect();
            let label = if route.default {
                "default".to_string()
            } else {
                when.join(", ")
            };
            let mut push = |to: &str, label: String| {
                edges.push(Edge {
                    to: to.to_string(),
                    label,
                    max_passes: route.max_passes,
                })
            };
            match &route.to {
                Some(Target::One(to)) => push(to, label.clone()),
                Some(Target::Many(targets)) => {
                    for to in targets {
                        push(to, label.clone());
                    }
                }
                None => {}
            }
            if let Some(fan_out) = &route.fan_out {
                push(&fan_out.to, format!("each {}", fan_out.over));
            }
            if let (Some(_), Some(on_bound)) = (route.max_passes, &route.on_bound)
                && on_bound != "fail"
            {
                edges.push(Edge {
                    to: on_bound.clone(),
                    label: "on bound".to_string(),
                    max_passes: None,
                });
            }
        }
    }

    // Depth-first from the start: an edge to a node on the stack is a back edge.
    let mut order = Vec::new();
    let mut visited = BTreeSet::new();
    let mut stack = Vec::new();
    let mut back: BTreeMap<(String, usize), Option<(String, u32)>> = BTreeMap::new();
    visit(
        &definition.start.value,
        &out,
        &mut visited,
        &mut stack,
        &mut order,
        &mut back,
    );
    order.reverse();
    let mut column: BTreeMap<String, usize> = BTreeMap::new();
    column.insert(definition.start.value.clone(), 0);
    for name in &order {
        let Some(&here) = column.get(name) else {
            continue;
        };
        for (index, edge) in out.get(name).into_iter().flatten().enumerate() {
            if back.contains_key(&(name.clone(), index)) {
                continue;
            }
            let entry = column.entry(edge.to.clone()).or_insert(0);
            *entry = (*entry).max(here + 1);
        }
    }
    // Nodes no path from the start reaches sit in the first column.
    for node in &definition.nodes {
        column.entry(node.name.clone()).or_insert(0);
    }
    for end in &definition.ends {
        column.entry(end.name.clone()).or_insert(0);
    }

    let over = state.is_over();
    let mut nodes = Vec::new();
    let names = definition
        .nodes
        .iter()
        .map(|node| (node.name.as_str(), Some(node.kind.value)));
    for (name, kind) in names {
        let passes = state.passes.get(name).copied().unwrap_or(0);
        let column = column[name];
        let unreached = if over {
            Status::Skipped
        } else {
            Status::Pending
        };
        match kind {
            Some(kind) => {
                let box_kind = match kind {
                    NodeKind::Agent => BoxKind::Agent,
                    NodeKind::Program => BoxKind::Program,
                };
                let mut branches: BTreeSet<Vec<(String, u32)>> = BTreeSet::new();
                let keys = state
                    .running
                    .keys()
                    .chain(state.open_gates.keys())
                    .chain(state.finished.iter().map(|finished| &finished.key));
                for key in keys.filter(|key| key.node == name) {
                    branches.insert(key.branch.clone());
                }
                if branches.is_empty() {
                    branches.insert(Vec::new());
                }
                for branch in branches {
                    let status = node_status(state, name, &branch, unreached);
                    let passes = if branch.is_empty() && status != Status::Pending {
                        passes
                    } else {
                        highest_pass(state, name, &branch).unwrap_or(0)
                    };
                    let lane = branch.last().map_or(0, |(_, index)| *index as usize);
                    nodes.push(LayoutNode {
                        name: name.to_string(),
                        kind: box_kind,
                        column,
                        lane,
                        branch,
                        status,
                        passes,
                    });
                }
            }
            None => {}
        }
    }
    for end in &definition.ends {
        let name = end.name.as_str();
        // The log records no route into an end: a succeeded run reached the success ends that a
        // succeeded node routes to.
        let routed_by_success = |node: &crate::definition::Node| {
            node.routes.iter().any(|route| match &route.to {
                Some(Target::One(to)) => to == name,
                Some(Target::Many(to)) => to.iter().any(|to| to == name),
                None => false,
            }) && nodes
                .iter()
                .any(|laid| laid.name == node.name && laid.status == Status::Succeeded)
        };
        let reached = state.routes.iter().any(|route| route.to == name)
            || (matches!(state.ended, Some((EndState::Succeeded, _)))
                && end.kind == EndKind::Success
                && definition.nodes.iter().any(routed_by_success));
        let unreached = if over {
            Status::Skipped
        } else {
            Status::Pending
        };
        nodes.push(LayoutNode {
            name: name.to_string(),
            kind: match end.kind {
                EndKind::Success => BoxKind::EndSuccess,
                EndKind::Failure => BoxKind::EndFailure,
            },
            column: column[name],
            lane: 0,
            branch: Vec::new(),
            status: if reached { Status::Reached } else { unreached },
            passes: 0,
        });
    }
    // Two boxes in one column never share a lane.
    let mut taken: BTreeSet<(usize, usize)> = BTreeSet::new();
    nodes.sort_by_key(|node| (node.column, node.lane));
    for node in &mut nodes {
        while !taken.insert((node.column, node.lane)) {
            node.lane += 1;
        }
    }
    nodes.sort_by(|a, b| (a.column, a.lane, &a.name).cmp(&(b.column, b.lane, &b.name)));

    let mut edges = Vec::new();
    let mut arcs = Vec::new();
    for node in &definition.nodes {
        for (index, edge) in out[&node.name].iter().enumerate() {
            if let Some(bound) = back.get(&(node.name.clone(), index)) {
                // The loop's bound sits on one route of the cycle: its target counts the passes.
                let (counted, max) = bound.clone().unwrap_or((edge.to.clone(), 1));
                let pass = state.passes.get(&counted).copied().unwrap_or(0).max(1);
                arcs.push(LayoutArc {
                    from: node.name.clone(),
                    to: edge.to.clone(),
                    label: format!("pass {pass} of {max}"),
                });
            } else {
                edges.push(LayoutEdge {
                    from: node.name.clone(),
                    to: edge.to.clone(),
                    label: edge.label.clone(),
                });
            }
        }
    }
    GraphLayout {
        columns: nodes.iter().map(|node| node.column + 1).max().unwrap_or(0),
        nodes,
        edges,
        arcs,
    }
}

fn visit(
    name: &str,
    out: &BTreeMap<String, Vec<Edge>>,
    visited: &mut BTreeSet<String>,
    stack: &mut Vec<String>,
    order: &mut Vec<String>,
    back: &mut BTreeMap<(String, usize), Option<(String, u32)>>,
) {
    if !visited.insert(name.to_string()) {
        return;
    }
    stack.push(name.to_string());
    for (index, edge) in out.get(name).into_iter().flatten().enumerate() {
        if let Some(start) = stack.iter().position(|entry| entry == &edge.to) {
            // The cycle is the stack from the target down, closed by this edge.
            let mut cycle: Vec<(&str, &Edge)> = Vec::new();
            for pair in stack[start..].windows(2) {
                for step in out.get(&pair[0]).into_iter().flatten() {
                    if step.to == pair[1] {
                        cycle.push((&pair[0], step));
                    }
                }
            }
            cycle.push((name, edge));
            let bound = cycle
                .iter()
                .find_map(|(_, step)| step.max_passes.map(|max| (step.to.clone(), max)));
            back.insert((name.to_string(), index), bound);
        } else {
            visit(&edge.to, out, visited, stack, order, back);
        }
    }
    stack.pop();
    order.push(name.to_string());
}

fn highest_pass(state: &RunState, node: &str, branch: &[(String, u32)]) -> Option<u32> {
    state
        .running
        .keys()
        .chain(state.open_gates.keys())
        .chain(state.finished.iter().map(|finished| &finished.key))
        .filter(|key| key.node == node && key.branch == branch)
        .map(|key| key.pass)
        .max()
}

fn node_status(
    state: &RunState,
    node: &str,
    branch: &[(String, u32)],
    unreached: Status,
) -> Status {
    let Some(pass) = highest_pass(state, node, branch) else {
        return unreached;
    };
    let key = NodeKey {
        node: node.to_string(),
        branch: branch.to_vec(),
        pass,
    };
    if state.running.contains_key(&key) {
        Status::Running
    } else if state.open_gates.contains_key(&key) {
        Status::Waiting
    } else {
        match state.last_finish(&key) {
            Some(finished) if finished.succeeded() => Status::Succeeded,
            Some(_) => Status::Failed,
            None => unreached,
        }
    }
}

impl GraphLayout {
    /// The boxes of one column in lane order.
    pub fn column(&self, column: usize) -> impl Iterator<Item = &LayoutNode> {
        self.nodes.iter().filter(move |node| node.column == column)
    }

    /// The debug dump: columns, then edges and arcs.
    pub fn text(&self) -> String {
        let mut text = String::new();
        for column in 0..self.columns {
            text.push_str(&format!("column {column}\n"));
            for node in self.column(column) {
                let branch = if node.branch.is_empty() {
                    String::new()
                } else {
                    format!(" [{}]", crate::record::branch_label(&node.branch))
                };
                text.push_str(&format!(
                    "  lane {} {}{branch} ({}) {}",
                    node.lane,
                    node.name,
                    node.kind.label(),
                    node.status.label(),
                ));
                if node.passes > 1 {
                    text.push_str(&format!(" pass {}", node.passes));
                }
                text.push('\n');
            }
        }
        if !self.edges.is_empty() {
            text.push_str("edges\n");
            for edge in &self.edges {
                if edge.label.is_empty() {
                    text.push_str(&format!("  {} -> {}\n", edge.from, edge.to));
                } else {
                    text.push_str(&format!(
                        "  {} -> {} ({})\n",
                        edge.from, edge.to, edge.label
                    ));
                }
            }
        }
        if !self.arcs.is_empty() {
            text.push_str("arcs\n");
            for arc in &self.arcs {
                text.push_str(&format!("  {} -> {} \"{}\"\n", arc.from, arc.to, arc.label));
            }
        }
        text
    }
}
