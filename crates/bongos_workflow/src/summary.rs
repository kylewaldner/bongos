//! The pre-run summary of `workflow: run` (feature 20, step 17): what a run of a definition would
//! do, read from the definition and the inputs resolved for the task, before anything starts.
//! Pure: no disk, no engine. The modal shows [`Summary::text`], and the tests read the same dump.
//!
//! It lists the graph outline, per node the kind, agent, model, permission mode, caps, program
//! permissions and sandbox state, and the inputs with their source (an override reads
//! `override`), marking the missing ones.

use std::fmt::Write as _;

use bongos_workflow_model::Layer;
use bongos_workflow_model::definition::{AgentKind, Definition, Node, NodeKind, Target};
use bongos_workflow_model::inputs::Input;

use crate::lifecycle::agent_of;
use crate::runner_claude::{AgentDefaults, NodeCaps};
use crate::runner_program::{ProgramCaps, ProgramSettings};

/// Whether a program node would run inside the OS sandbox, probed once for the machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxState {
    Sandboxed,
    /// The reason the sandbox cannot be created: programs run unconfined.
    NotSandboxed(String),
}

impl SandboxState {
    /// `sandboxed`, or `not sandboxed: <reason>` (the words of a try's summary).
    pub fn label(&self) -> String {
        match self {
            Self::Sandboxed => "sandboxed".to_string(),
            Self::NotSandboxed(reason) => format!("not sandboxed: {reason}"),
        }
    }
}

/// One node of the summary.
#[derive(Debug, Clone, PartialEq)]
pub struct NodeSummary {
    pub name: String,
    pub kind: NodeKind,
    /// The agent of an agent node.
    pub agent: Option<AgentKind>,
    pub model: Option<String>,
    pub permission_mode: Option<String>,
    /// `max_turns`, cost and minutes for an agent node; minutes for a program node.
    pub caps: String,
    /// The tools an agent node may use.
    pub allowed_tools: Vec<String>,
    /// The shipped program (`use`) or script a program node runs.
    pub program: Option<String>,
    pub commands: Vec<String>,
    pub network: Vec<String>,
    pub mcp: Vec<String>,
    /// The sandbox state of a program node that runs a script.
    pub sandbox: Option<SandboxState>,
    /// Where the node leads, in route order.
    pub leads_to: Vec<String>,
}

/// What a run of a definition would do.
#[derive(Debug, Clone, PartialEq)]
pub struct Summary {
    pub workflow: String,
    pub layer: Layer,
    pub description: Option<String>,
    pub start: String,
    pub nodes: Vec<NodeSummary>,
    pub ends: Vec<String>,
    pub inputs: Vec<Input>,
}

impl Summary {
    pub fn build(
        definition: &Definition,
        layer: Layer,
        inputs: Vec<Input>,
        sandbox: &SandboxState,
        agents: &AgentDefaults,
        programs: &ProgramSettings,
    ) -> Self {
        Self {
            workflow: definition.name.clone(),
            layer,
            description: definition.description.clone(),
            start: definition.start.value.clone(),
            nodes: definition
                .nodes
                .iter()
                .map(|node| node_summary(definition, node, sandbox, agents, programs))
                .collect(),
            ends: definition.ends.iter().map(|end| end.name.clone()).collect(),
            inputs,
        }
    }

    /// The inputs that are required and have no value.
    pub fn missing(&self) -> Vec<&Input> {
        self.inputs.iter().filter(|input| input.missing).collect()
    }

    /// Whether Enter may start the run: no input is missing.
    pub fn can_start(&self) -> bool {
        self.inputs.iter().all(|input| !input.missing)
    }

    /// The summary as text, the way the modal shows it.
    pub fn text(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(out, "Workflow {} ({})", self.workflow, self.layer.label());
        if let Some(description) = &self.description {
            let _ = writeln!(out, "{}", description.trim());
        }
        let _ = writeln!(out, "\nGraph (start: {})", self.start);
        for node in &self.nodes {
            let marker = if node.name == self.start { "*" } else { " " };
            let leads = if node.leads_to.is_empty() {
                String::new()
            } else {
                format!(" -> {}", node.leads_to.join(", "))
            };
            let _ = writeln!(out, " {marker} {}{leads}", node.name);
        }
        if !self.ends.is_empty() {
            let _ = writeln!(out, "   ends: {}", self.ends.join(", "));
        }
        let _ = writeln!(out, "\nNodes");
        for node in &self.nodes {
            write_node(&mut out, node);
        }
        let _ = writeln!(out, "\nInputs");
        if self.inputs.is_empty() {
            let _ = writeln!(out, "  none");
        }
        for input in &self.inputs {
            let value = match &input.value {
                Some(value) => one_line(&value.to_text()),
                None => "(no value)".to_string(),
            };
            let missing = if input.missing { " MISSING" } else { "" };
            let _ = writeln!(
                out,
                "  {}: {value} [from {}]{missing}",
                input.name, input.source
            );
        }
        let missing: Vec<&str> = self
            .missing()
            .into_iter()
            .map(|input| input.name.as_str())
            .collect();
        if !missing.is_empty() {
            let _ = writeln!(
                out,
                "\nMissing inputs: {}. Supply them to start.",
                missing.join(", ")
            );
        }
        out
    }
}

fn node_summary(
    definition: &Definition,
    node: &Node,
    sandbox: &SandboxState,
    agents: &AgentDefaults,
    programs: &ProgramSettings,
) -> NodeSummary {
    let kind = node.kind.value;
    let permissions = node.permissions.clone().unwrap_or_default();
    let mut leads_to = Vec::new();
    for route in &node.routes {
        match &route.to {
            Some(Target::One(to)) => leads_to.push(to.clone()),
            Some(Target::Many(many)) => leads_to.extend(many.iter().cloned()),
            None => {}
        }
        if let Some(fan_out) = &route.fan_out {
            leads_to.push(format!("{} (fan out over {})", fan_out.to, fan_out.over));
        }
    }
    let mut summary = NodeSummary {
        name: node.name.clone(),
        kind,
        agent: None,
        model: None,
        permission_mode: None,
        caps: String::new(),
        allowed_tools: Vec::new(),
        program: None,
        commands: permissions.commands,
        network: permissions.network,
        mcp: permissions.mcp,
        sandbox: None,
        leads_to,
    };
    match kind {
        NodeKind::Agent => {
            let caps = NodeCaps::resolve(definition, node, agents);
            summary.agent = Some(agent_of(definition, node));
            summary.caps = format!(
                "{} turns, ${:.2}, {} min",
                caps.max_turns, caps.max_cost_usd, caps.max_minutes
            );
            summary.model = Some(caps.model);
            summary.permission_mode = Some(caps.permission_mode);
            summary.allowed_tools = caps.allowed_tools;
        }
        NodeKind::Program => {
            let caps = ProgramCaps::resolve(node, programs);
            summary.caps = format!("{} min", caps.max_minutes);
            if let Some(used) = &node.use_node {
                summary.program = Some(format!("built in {}", used.value));
            } else if let Some(script) = &node.script {
                summary.program = Some(format!("script {}", script.value));
                summary.sandbox = Some(sandbox.clone());
            }
        }
    }
    summary
}

fn write_node(out: &mut String, node: &NodeSummary) {
    let kind = match node.kind {
        NodeKind::Agent => "agent",
        NodeKind::Program => "program",
    };
    let _ = writeln!(out, "  {} [{kind}]", node.name);
    if let Some(agent) = node.agent {
        let agent = match agent {
            AgentKind::Claude => "claude",
            AgentKind::Codex => "codex",
        };
        let _ = writeln!(out, "    agent: {agent}");
    }
    if let Some(model) = &node.model {
        let _ = writeln!(out, "    model: {model}");
    }
    if let Some(mode) = &node.permission_mode {
        let _ = writeln!(out, "    permission mode: {mode}");
    }
    if !node.allowed_tools.is_empty() {
        let _ = writeln!(out, "    allowed tools: {}", node.allowed_tools.join(", "));
    }
    let _ = writeln!(out, "    caps: {}", node.caps);
    if let Some(program) = &node.program {
        let _ = writeln!(out, "    runs: {program}");
    }
    if node.kind == NodeKind::Program {
        let list = |items: &[String]| {
            if items.is_empty() {
                "none".to_string()
            } else {
                items.join(", ")
            }
        };
        let _ = writeln!(out, "    permissions:");
        let _ = writeln!(out, "      commands: {}", list(&node.commands));
        let _ = writeln!(out, "      network: {}", list(&node.network));
        let _ = writeln!(out, "      mcp: {}", list(&node.mcp));
    }
    if let Some(sandbox) = &node.sandbox {
        let _ = writeln!(out, "    sandbox: {}", sandbox.label());
    }
}

fn one_line(text: &str) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() > 80 {
        let cut: String = line.chars().take(77).collect();
        format!("{cut}...")
    } else {
        line
    }
}
