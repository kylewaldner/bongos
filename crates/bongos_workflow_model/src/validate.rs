//! Validation of a parsed definition: everything that can be known before a run starts.
//!
//! [`validate`] is pure. The caller gathers what lives outside the definition into a
//! [`ValidationContext`]: the setting ceilings, the MCP servers installed for each agent CLI, the
//! shipped program nodes and the files in the definition directory. Every [`Diagnostic`] names the
//! line it was found on and, when it belongs to one, the node or end. A definition with any
//! [`Severity::Error`] never starts.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::fmt;
use std::path::{Component, Path};

use crate::definition::{
    AgentKind, Definition, EndKind, FieldType, Node, NodeKind, Outcome, Route, Target,
};

/// The program nodes bongos ships, named by `use` (D10).
pub const SHIPPED_PROGRAMS: &[&str] = &["merge", "wait"];

/// The permission modes an agent node may declare: the modes Claude Code accepts (checked on
/// 2.1.322, where `manual` replaced `default`).
pub const PERMISSION_MODES: &[&str] = &[
    "acceptEdits",
    "auto",
    "bypassPermissions",
    "manual",
    "dontAsk",
    "plan",
];

const BYPASS_PERMISSIONS: &str = "bypassPermissions";
const ON_BOUND_FAIL: &str = "fail";
const PROGRAM_LANGUAGE: &str = "python";
/// Above this many combinations of routed choice values, route coverage is checked value by value.
const MAX_COVERAGE_COMBINATIONS: usize = 1024;
/// At most this many "no route" errors are listed for one node.
const MAX_NO_ROUTE_ERRORS: usize = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Severity {
    Error,
    Warning,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Diagnostic {
    /// The node or end the problem belongs to; `None` for the definition as a whole.
    pub node: Option<String>,
    pub line: usize,
    pub severity: Severity,
    pub message: String,
}

impl Diagnostic {
    pub fn is_error(&self) -> bool {
        self.severity == Severity::Error
    }
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let severity = match self.severity {
            Severity::Error => "error",
            Severity::Warning => "warning",
        };
        match &self.node {
            Some(node) => write!(
                formatter,
                "line {}: {severity} in '{node}': {}",
                self.line, self.message
            ),
            None => write!(
                formatter,
                "line {}: {severity}: {}",
                self.line, self.message
            ),
        }
    }
}

/// Whether any diagnostic is an error.
pub fn has_errors(diagnostics: &[Diagnostic]) -> bool {
    diagnostics.iter().any(Diagnostic::is_error)
}

/// `workflows.ceilings` (D19): what a definition may declare at most.
#[derive(Debug, Clone, PartialEq)]
pub struct Ceilings {
    pub max_turns: u32,
    pub max_cost_usd: f64,
    pub max_minutes: u32,
    pub max_passes: u32,
    pub retries: u32,
    pub program_max_minutes: u32,
    pub allow_bypass_permissions: bool,
}

impl Default for Ceilings {
    fn default() -> Self {
        Self {
            max_turns: 200,
            max_cost_usd: 25.0,
            max_minutes: 240,
            max_passes: 20,
            retries: 5,
            program_max_minutes: 120,
            allow_bypass_permissions: false,
        }
    }
}

/// What validation needs from outside the definition.
#[derive(Debug, Clone, PartialEq)]
pub struct ValidationContext {
    pub ceilings: Ceilings,
    /// `workflows.fan_out_width`: the widest per-item fan-out a route may declare.
    pub fan_out_width: u32,
    /// `workflows.programs.allow_network`.
    pub allow_network: bool,
    /// The MCP servers installed for each agent CLI, after `workflows.mcp_exclude` (D8).
    pub mcp_servers: BTreeMap<AgentKind, BTreeSet<String>>,
    /// The shipped program nodes a `use` may name.
    pub programs: BTreeSet<String>,
    /// Whether the definition directory exists.
    pub dir_exists: bool,
    /// The files in the definition directory, relative to it, with `/` separators.
    pub files: BTreeSet<String>,
}

impl Default for ValidationContext {
    fn default() -> Self {
        Self {
            ceilings: Ceilings::default(),
            fan_out_width: 8,
            allow_network: true,
            mcp_servers: BTreeMap::new(),
            programs: SHIPPED_PROGRAMS
                .iter()
                .map(|name| name.to_string())
                .collect(),
            dir_exists: true,
            files: BTreeSet::new(),
        }
    }
}

/// Checks a definition against the format's rules and the context. Diagnostics are ordered by line.
pub fn validate(definition: &Definition, context: &ValidationContext) -> Vec<Diagnostic> {
    let mut validator = Validator {
        definition,
        context,
        files: context
            .files
            .iter()
            .filter_map(|file| normalize(file))
            .collect(),
        out: Vec::new(),
    };
    validator.run();
    let mut diagnostics = validator.out;
    let mut seen = HashSet::new();
    diagnostics.retain(|diagnostic| {
        seen.insert((
            diagnostic.node.clone(),
            diagnostic.line,
            diagnostic.severity,
            diagnostic.message.clone(),
        ))
    });
    diagnostics.sort_by_key(|diagnostic| diagnostic.line);
    diagnostics
}

pub(crate) fn outcome_name(outcome: Outcome) -> &'static str {
    match outcome {
        Outcome::Pr => "pr",
        Outcome::Document => "document",
        Outcome::Tasks => "tasks",
    }
}

/// Whether a node name is safe in a git branch name and a directory name: lowercase ASCII
/// letters, digits, `-` and `_`, starting with a letter or digit.
fn is_safe_node_name(name: &str) -> bool {
    name.chars()
        .next()
        .is_some_and(|first| first.is_ascii_lowercase() || first.is_ascii_digit())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

fn agent_name(agent: AgentKind) -> &'static str {
    match agent {
        AgentKind::Claude => "claude",
        AgentKind::Codex => "codex",
    }
}

/// A relative path inside the definition directory with `.` parts removed, or `None` when the path
/// is absolute, empty or climbs out with `..`.
fn normalize(path: &str) -> Option<String> {
    let mut parts = Vec::new();
    for component in Path::new(path).components() {
        match component {
            Component::Normal(part) => parts.push(part.to_str()?.to_string()),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EdgeKind {
    /// `to = "<node>"`.
    To,
    /// One branch of `to = [...]` or `fan_out`.
    FanOut,
    /// `on_bound = "<node>"`.
    OnBound,
}

#[derive(Debug, Clone, Copy)]
struct Edge {
    to: usize,
    kind: EdgeKind,
    bounded: bool,
    line: usize,
}

struct Graph<'a> {
    index: HashMap<&'a str, usize>,
    edges: Vec<Vec<Edge>>,
    /// The ends each node routes to (including `on_bound`).
    ends: Vec<BTreeSet<&'a str>>,
}

struct Validator<'a> {
    definition: &'a Definition,
    context: &'a ValidationContext,
    files: BTreeSet<String>,
    out: Vec<Diagnostic>,
}

impl<'a> Validator<'a> {
    fn error(&mut self, node: Option<&str>, line: usize, message: impl Into<String>) {
        self.out.push(Diagnostic {
            node: node.map(str::to_string),
            line,
            severity: Severity::Error,
            message: message.into(),
        });
    }

    fn warning(&mut self, node: Option<&str>, line: usize, message: impl Into<String>) {
        self.out.push(Diagnostic {
            node: node.map(str::to_string),
            line,
            severity: Severity::Warning,
            message: message.into(),
        });
    }

    fn is_node(&self, name: &str) -> bool {
        self.definition.node(name).is_some()
    }

    fn is_end(&self, name: &str) -> bool {
        self.definition.end(name).is_some()
    }

    fn run(&mut self) {
        let definition = self.definition;
        for warning in definition.warnings() {
            self.warning(warning.node.as_deref(), warning.line, warning.message);
        }

        if !self.context.dir_exists {
            self.error(None, 1, "the definition directory is missing");
        }

        for end in &definition.ends {
            if self.is_node(&end.name) {
                self.error(
                    Some(&end.name),
                    end.line,
                    format!("'{}' is both a node and an end", end.name),
                );
            }
        }

        let start = &definition.start;
        if self.is_end(&start.value) && !self.is_node(&start.value) {
            self.error(
                None,
                start.line,
                format!("start must be a node, not end '{}'", start.value),
            );
        } else if !self.is_node(&start.value) {
            self.error(
                None,
                start.line,
                format!("start names unknown node '{}'", start.value),
            );
        }

        if !definition
            .ends
            .iter()
            .any(|end| end.kind == EndKind::Success)
        {
            self.error(
                None,
                start.line,
                "no success end; add an end with kind = \"success\"",
            );
        }

        for node in &definition.nodes {
            self.check_node(node);
        }

        let graph = self.graph();
        self.check_graph(&graph);
    }

    fn check_node(&mut self, node: &'a Node) {
        let name = node.name.as_str();
        if !is_safe_node_name(name) {
            self.error(
                Some(name),
                node.line,
                format!(
                    "node name '{name}' may use only lowercase letters, digits, '-' and '_', \
                     starting with a letter or digit (it names branches and directories)"
                ),
            );
        }
        match node.kind.value {
            NodeKind::Agent => self.check_agent(node),
            NodeKind::Program => self.check_program(node),
        }

        for field in &node.output {
            if let FieldType::Choice(values) = &field.field_type {
                let mut seen = HashSet::new();
                for value in values {
                    if !seen.insert(value) {
                        self.error(
                            Some(name),
                            field.line,
                            format!("choice field '{}' repeats the value '{value}'", field.name),
                        );
                    }
                }
            }
        }

        if let Some(gate) = &node.gate
            && gate.question.trim().is_empty()
        {
            self.error(Some(name), gate.line, "the gate has an empty question");
        }

        let retries = node
            .settings
            .retries
            .map(|value| (value, node.settings.lines.retries, false))
            .or_else(|| {
                let defaults = &self.definition.defaults;
                defaults
                    .retries
                    .map(|value| (value, defaults.lines.retries, true))
            });
        if let Some((retries, line, inherited)) = retries {
            let ceiling = self.context.ceilings.retries;
            if retries > ceiling {
                self.error(
                    Some(name),
                    line.unwrap_or(node.line),
                    format!(
                        "retries {retries}{} is above the ceiling {ceiling} \
                         (workflows.ceilings.retries)",
                        from_defaults(inherited)
                    ),
                );
            }
        }

        for route in &node.routes {
            self.check_route(node, route);
        }
        self.check_coverage(node);
    }

    fn check_file(&mut self, node: &str, what: &str, path: &str, line: usize) {
        match normalize(path) {
            None => self.error(
                Some(node),
                line,
                format!(
                    "{what} file '{path}' must be a relative path inside the definition directory"
                ),
            ),
            Some(path) => {
                if self.context.dir_exists && !self.files.contains(&path) {
                    self.error(Some(node), line, format!("{what} file '{path}' is missing"));
                }
            }
        }
    }

    fn check_agent(&mut self, node: &'a Node) {
        let name = node.name.as_str();
        match &node.prompt {
            Some(prompt) => self.check_file(name, "prompt", &prompt.value, prompt.line),
            None => self.error(Some(name), node.line, "an agent node needs a `prompt`"),
        }
        if let Some(script) = &node.script {
            self.error(
                Some(name),
                script.line,
                "`script` is for program nodes; an agent node takes a `prompt`",
            );
        }
        if let Some(use_node) = &node.use_node {
            self.error(
                Some(name),
                use_node.line,
                "`use` is for program nodes; an agent node takes a `prompt`",
            );
        }
        if let Some(permissions) = &node.permissions {
            self.error(
                Some(name),
                permissions.line,
                "`permissions` is for program nodes; an agent node is bounded by \
                 `allowed_tools` and `permission_mode`",
            );
        }

        let definition: &'a Definition = self.definition;
        let context: &'a ValidationContext = self.context;
        let defaults = &definition.defaults;
        let own = &node.settings;
        let ceilings = &context.ceilings;

        if let Some((mode, line, inherited)) = pick(
            own.permission_mode.as_ref(),
            own.lines.permission_mode,
            defaults.permission_mode.as_ref(),
            defaults.lines.permission_mode,
        ) {
            let line = line.unwrap_or(node.line);
            if !PERMISSION_MODES.contains(&mode.as_str()) {
                self.error(
                    Some(name),
                    line,
                    format!(
                        "unknown permission mode '{mode}'{}; expected one of {}",
                        from_defaults(inherited),
                        PERMISSION_MODES.join(", ")
                    ),
                );
            } else if mode == BYPASS_PERMISSIONS && !ceilings.allow_bypass_permissions {
                self.error(
                    Some(name),
                    line,
                    format!(
                        "permission mode '{mode}'{} is not allowed: \
                         workflows.ceilings.allow_bypass_permissions is false",
                        from_defaults(inherited)
                    ),
                );
            }
        }

        let turns = pick(
            own.max_turns.as_ref(),
            own.lines.max_turns,
            defaults.max_turns.as_ref(),
            defaults.lines.max_turns,
        )
        .map(|(value, line, inherited)| (f64::from(*value), line, inherited));
        let cost = pick(
            own.max_cost_usd.as_ref(),
            own.lines.max_cost_usd,
            defaults.max_cost_usd.as_ref(),
            defaults.lines.max_cost_usd,
        )
        .map(|(value, line, inherited)| (*value, line, inherited));
        let minutes = pick(
            own.max_minutes.as_ref(),
            own.lines.max_minutes,
            defaults.max_minutes.as_ref(),
            defaults.lines.max_minutes,
        )
        .map(|(value, line, inherited)| (f64::from(*value), line, inherited));
        let caps = [
            ("max_turns", turns, f64::from(ceilings.max_turns)),
            ("max_cost_usd", cost, ceilings.max_cost_usd),
            ("max_minutes", minutes, f64::from(ceilings.max_minutes)),
        ];
        for (key, value, ceiling) in caps {
            if let Some((value, line, inherited)) = value {
                let cap = Cap {
                    key,
                    value,
                    inherited,
                    ceiling,
                    ceiling_key: key,
                };
                self.check_cap(name, line.unwrap_or(node.line), cap);
            }
        }

        let agent = own.agent.or(defaults.agent).unwrap_or(AgentKind::Claude);
        let (tools, lines) = match &own.allowed_tools {
            Some(tools) => (tools, &own.lines.allowed_tools),
            None => match &defaults.allowed_tools {
                Some(tools) => (tools, &defaults.lines.allowed_tools),
                None => return,
            },
        };
        let inherited = own.allowed_tools.is_none();
        for (index, tool) in tools.iter().enumerate() {
            let line = lines.get(index).copied().unwrap_or(node.line);
            self.check_tool(name, agent, tool, line, inherited);
        }
    }

    fn check_cap(&mut self, node: &str, line: usize, cap: Cap<'_>) {
        let Cap {
            key,
            value,
            inherited,
            ceiling,
            ceiling_key,
        } = cap;
        if !value.is_finite() || value <= 0.0 {
            self.error(
                Some(node),
                line,
                format!("{key}{} must be above zero", from_defaults(inherited)),
            );
        } else if value > ceiling {
            self.error(
                Some(node),
                line,
                format!(
                    "{key} {value}{} is above the ceiling {ceiling} \
                     (workflows.ceilings.{ceiling_key})",
                    from_defaults(inherited)
                ),
            );
        }
    }

    fn check_tool(
        &mut self,
        node: &str,
        agent: AgentKind,
        tool: &str,
        line: usize,
        inherited: bool,
    ) {
        let source = from_defaults(inherited);
        let Some(rest) = tool.strip_prefix("mcp__") else {
            if agent == AgentKind::Codex && tool.starts_with("Bash(") {
                self.warning(
                    Some(node),
                    line,
                    format!(
                        "'{tool}'{source}: codex does not limit commands by pattern, so this \
                         entry has no effect on a codex node"
                    ),
                );
            }
            return;
        };
        let (server, tool_name) = rest.split_once("__").unwrap_or((rest, ""));
        if server.is_empty() {
            self.error(
                Some(node),
                line,
                format!(
                    "allowed tool '{tool}'{source} names no MCP server; \
                     use mcp__<server>__<tool> or mcp__<server>__*"
                ),
            );
            return;
        }
        if tool_name.is_empty() {
            self.error(
                Some(node),
                line,
                format!(
                    "allowed tool '{tool}'{source} names a server, not a tool; \
                     use mcp__{server}__<tool> or mcp__{server}__*"
                ),
            );
            return;
        }
        let installed = self
            .context
            .mcp_servers
            .get(&agent)
            .is_some_and(|servers| servers.contains(server));
        if !installed {
            let cli = agent_name(agent);
            self.error(
                Some(node),
                line,
                format!(
                    "MCP server '{server}'{source} is not installed for {cli}; install it with \
                     `{cli} mcp add`, or remove it from allowed_tools"
                ),
            );
        }
        if agent == AgentKind::Codex && tool_name != "*" {
            self.error(
                Some(node),
                line,
                format!("'{tool}'{source}: codex limits MCP by server; use mcp__{server}__*"),
            );
        }
    }

    fn check_program(&mut self, node: &'a Node) {
        let name = node.name.as_str();
        if let Some(prompt) = &node.prompt {
            self.error(
                Some(name),
                prompt.line,
                "`prompt` is for agent nodes; a program node takes `script` or `use`",
            );
        }
        match (&node.script, &node.use_node) {
            (Some(_), Some(use_node)) => self.error(
                Some(name),
                use_node.line,
                "a program node takes `script` or `use`, not both",
            ),
            (None, None) => self.error(
                Some(name),
                node.line,
                "a program node needs `script` or `use`",
            ),
            _ => {}
        }
        if let Some(script) = &node.script {
            self.check_file(name, "script", &script.value, script.line);
        }
        if let Some(use_node) = &node.use_node
            && !self.context.programs.contains(&use_node.value)
        {
            let known = self
                .context
                .programs
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(", ");
            self.error(
                Some(name),
                use_node.line,
                format!(
                    "'{}' is not a shipped program node (shipped: {known})",
                    use_node.value
                ),
            );
        }
        if let Some(language) = &node.language
            && language != PROGRAM_LANGUAGE
        {
            let line = node.script.as_ref().map_or(node.line, |script| script.line);
            self.error(
                Some(name),
                line,
                format!("unsupported language '{language}'; program nodes are {PROGRAM_LANGUAGE}"),
            );
        }
        if let Some(permissions) = &node.permissions {
            if !permissions.network.is_empty() && !self.context.allow_network {
                self.error(
                    Some(name),
                    permissions.line,
                    format!(
                        "network permission ({}) is not allowed: \
                         workflows.programs.allow_network is false",
                        permissions.network.join(", ")
                    ),
                );
            }
            if !permissions.mcp.is_empty() {
                self.error(
                    Some(name),
                    permissions.line,
                    "MCP from program nodes needs the bongos MCP server (feature 21)",
                );
            }
        }
        if let Some(minutes) = node.settings.max_minutes {
            let line = node.settings.lines.max_minutes.unwrap_or(node.line);
            let cap = Cap {
                key: "max_minutes",
                value: f64::from(minutes),
                inherited: false,
                ceiling: f64::from(self.context.ceilings.program_max_minutes),
                ceiling_key: "program_max_minutes",
            };
            self.check_cap(name, line, cap);
        }
    }

    fn check_target(&mut self, node: &str, line: usize, what: &str, target: &str) {
        if !self.is_node(target) && !self.is_end(target) {
            self.error(
                Some(node),
                line,
                format!("{what} names unknown node or end '{target}'"),
            );
        }
    }

    fn check_branch_target(&mut self, node: &str, line: usize, what: &str, target: &str) {
        if self.is_node(target) {
            return;
        }
        if self.is_end(target) {
            self.error(
                Some(node),
                line,
                format!("{what} '{target}' is an end; a fan-out branch must start at a node"),
            );
        } else {
            self.error(
                Some(node),
                line,
                format!("{what} names unknown node '{target}'"),
            );
        }
    }

    fn check_route(&mut self, node: &'a Node, route: &'a Route) {
        let name = node.name.as_str();
        let line = route.line;

        match (&route.to, &route.fan_out) {
            (None, None) => self.error(Some(name), line, "a route needs `to` or `fan_out`"),
            (Some(_), Some(_)) => self.error(
                Some(name),
                line,
                "a route takes `to` or `fan_out`, not both",
            ),
            _ => {}
        }
        if route.default && !route.when.is_empty() {
            self.error(Some(name), line, "a default route takes no `when`");
        }

        match &route.to {
            Some(Target::One(target)) => self.check_target(name, line, "route", target),
            Some(Target::Many(targets)) => {
                if targets.is_empty() {
                    self.error(Some(name), line, "a fan-out `to` list is empty");
                }
                for target in targets {
                    self.check_branch_target(name, line, "fan-out branch", target);
                }
            }
            None => {}
        }

        if let Some(fan_out) = &route.fan_out {
            self.check_branch_target(name, line, "fan_out.to", &fan_out.to);
            match node.output.iter().find(|field| field.name == fan_out.over) {
                None => self.error(
                    Some(name),
                    line,
                    format!("fan_out.over names undeclared field '{}'", fan_out.over),
                ),
                Some(field) if !matches!(field.field_type, FieldType::List(_)) => self.error(
                    Some(name),
                    line,
                    format!("fan_out.over field '{}' is not a list", fan_out.over),
                ),
                Some(_) => {}
            }
            if let Some(width) = fan_out.width {
                let limit = self.context.fan_out_width;
                if width == 0 {
                    self.error(Some(name), line, "fan_out.width must be at least 1");
                } else if width > limit {
                    self.error(
                        Some(name),
                        line,
                        format!(
                            "fan_out.width {width} is above the limit {limit} \
                             (workflows.fan_out_width)"
                        ),
                    );
                }
            }
        }

        for condition in &route.when {
            match node
                .output
                .iter()
                .find(|field| field.name == condition.field)
            {
                None => self.error(
                    Some(name),
                    line,
                    format!("route on undeclared field '{}'", condition.field),
                ),
                Some(field) => match &field.field_type {
                    FieldType::Choice(values) => {
                        if !values.contains(&condition.value) {
                            self.error(
                                Some(name),
                                line,
                                format!(
                                    "'{}' is not a value of choice field '{}' ({})",
                                    condition.value,
                                    condition.field,
                                    values.join(", ")
                                ),
                            );
                        }
                    }
                    FieldType::List(_) => self.error(
                        Some(name),
                        line,
                        format!("route on list field '{}'", condition.field),
                    ),
                    FieldType::Text | FieldType::Path | FieldType::Number => {}
                },
            }
        }

        match route.max_passes {
            Some(0) => self.error(Some(name), line, "max_passes must be at least 1"),
            Some(passes) => {
                let ceiling = self.context.ceilings.max_passes;
                if passes > ceiling {
                    self.error(
                        Some(name),
                        line,
                        format!(
                            "max_passes {passes} is above the ceiling {ceiling} \
                             (workflows.ceilings.max_passes)"
                        ),
                    );
                }
            }
            None => {}
        }
        if let Some(on_bound) = &route.on_bound {
            if route.max_passes.is_none() {
                self.error(
                    Some(name),
                    line,
                    "`on_bound` needs `max_passes` on the route",
                );
            }
            if on_bound != ON_BOUND_FAIL {
                self.check_target(name, line, "on_bound", on_bound);
            }
        }
    }

    /// A closed choice that routes needs a route for every value, or a default.
    fn check_coverage(&mut self, node: &'a Node) {
        let name = node.name.as_str();
        if node.routes.is_empty()
            || node
                .routes
                .iter()
                .any(|route| route.default || route.when.is_empty())
        {
            return;
        }

        let mut fields: Vec<(&str, &[String], usize)> = Vec::new();
        for field in &node.output {
            if let FieldType::Choice(values) = &field.field_type
                && node.routes.iter().any(|route| {
                    route
                        .when
                        .iter()
                        .any(|condition| condition.field == field.name)
                })
            {
                fields.push((field.name.as_str(), values.as_slice(), field.line));
            }
        }
        if fields.is_empty() {
            return;
        }

        // A route is sure to match only when every condition names a routed choice field.
        let routes =
            node.routes
                .iter()
                .filter(|route| {
                    route.when.iter().all(|condition| {
                        fields.iter().any(|(field, _, _)| *field == condition.field)
                    })
                })
                .collect::<Vec<_>>();

        let combinations = fields.iter().try_fold(1usize, |total, (_, values, _)| {
            total.checked_mul(values.len())
        });
        let line = fields[0].2;
        let mut uncovered = Vec::new();
        match combinations {
            Some(count) if count <= MAX_COVERAGE_COMBINATIONS => {
                let mut choice = vec![0; fields.len()];
                'combinations: loop {
                    let covered = routes.iter().any(|route| {
                        route.when.iter().all(|condition| {
                            fields
                                .iter()
                                .zip(&choice)
                                .any(|((field, values, _), index)| {
                                    *field == condition.field && values[*index] == condition.value
                                })
                        })
                    });
                    if !covered {
                        uncovered.push(if fields.len() == 1 {
                            format!(
                                "no route for '{}' (field '{}') and no default route",
                                fields[0].1[choice[0]], fields[0].0
                            )
                        } else {
                            let values = fields
                                .iter()
                                .zip(&choice)
                                .map(|((field, values, _), index)| {
                                    format!("{field} = '{}'", values[*index])
                                })
                                .collect::<Vec<_>>()
                                .join(", ");
                            format!("no route for {values} and no default route")
                        });
                    }
                    let mut position = 0;
                    loop {
                        if position == choice.len() {
                            break 'combinations;
                        }
                        choice[position] += 1;
                        if choice[position] < fields[position].1.len() {
                            break;
                        }
                        choice[position] = 0;
                        position += 1;
                    }
                }
            }
            _ => {
                for (field, values, _) in &fields {
                    for value in values.iter() {
                        let mentioned = routes.iter().any(|route| {
                            route.when.iter().any(|condition| {
                                condition.field == *field && &condition.value == value
                            })
                        });
                        if !mentioned {
                            uncovered.push(format!(
                                "no route for '{value}' (field '{field}') and no default route"
                            ));
                        }
                    }
                }
            }
        }

        let total = uncovered.len();
        for message in uncovered.into_iter().take(MAX_NO_ROUTE_ERRORS) {
            self.error(Some(name), line, message);
        }
        if total > MAX_NO_ROUTE_ERRORS {
            self.error(
                Some(name),
                line,
                format!(
                    "{} more combinations of values have no route",
                    total - MAX_NO_ROUTE_ERRORS
                ),
            );
        }
    }

    fn graph(&self) -> Graph<'a> {
        let definition = self.definition;
        let index = definition
            .nodes
            .iter()
            .enumerate()
            .map(|(position, node)| (node.name.as_str(), position))
            .collect::<HashMap<_, _>>();
        let mut edges = vec![Vec::new(); definition.nodes.len()];
        let mut ends = vec![BTreeSet::new(); definition.nodes.len()];

        for (from, node) in definition.nodes.iter().enumerate() {
            for route in &node.routes {
                let bounded = route.max_passes.is_some();
                let mut add = |target: &'a str, kind: EdgeKind, bounded: bool| {
                    if let Some(&to) = index.get(target) {
                        edges[from].push(Edge {
                            to,
                            kind,
                            bounded,
                            line: route.line,
                        });
                    } else if definition.end(target).is_some() {
                        ends[from].insert(target);
                    }
                };
                match &route.to {
                    Some(Target::One(target)) => add(target, EdgeKind::To, bounded),
                    Some(Target::Many(targets)) => {
                        for target in targets {
                            add(target, EdgeKind::FanOut, bounded);
                        }
                    }
                    None => {}
                }
                if let Some(fan_out) = &route.fan_out {
                    add(&fan_out.to, EdgeKind::FanOut, bounded);
                }
                if bounded
                    && let Some(on_bound) = &route.on_bound
                    && on_bound != ON_BOUND_FAIL
                {
                    add(on_bound, EdgeKind::OnBound, false);
                }
            }
        }

        Graph { index, edges, ends }
    }

    fn check_graph(&mut self, graph: &Graph<'a>) {
        let definition = self.definition;
        let nodes = &definition.nodes;
        let count = nodes.len();

        // Reachability from start.
        let start = graph.index.get(definition.start.value.as_str()).copied();
        let mut reached = vec![false; count];
        if let Some(start) = start {
            let mut queue = VecDeque::from([start]);
            reached[start] = true;
            while let Some(node) = queue.pop_front() {
                for edge in &graph.edges[node] {
                    if !reached[edge.to] {
                        reached[edge.to] = true;
                        queue.push_back(edge.to);
                    }
                }
            }
            for (position, node) in nodes.iter().enumerate() {
                if !reached[position] {
                    self.error(
                        Some(&node.name),
                        node.line,
                        format!(
                            "node '{}' is unreachable from start '{}'",
                            node.name, definition.start.value
                        ),
                    );
                }
            }
        }

        // Nodes that can reach an end.
        let reaches_end = reverse_reach(graph, |node| !graph.ends[node].is_empty());
        for (position, node) in nodes.iter().enumerate() {
            if !reaches_end[position] {
                self.error(
                    Some(&node.name),
                    node.line,
                    format!("node '{}' cannot reach any end", node.name),
                );
            }
        }

        // Success ends and their promised outcomes.
        if start.is_some() {
            let mut any_success_reached = false;
            for end in definition
                .ends
                .iter()
                .filter(|end| end.kind == EndKind::Success)
            {
                let before_end =
                    reverse_reach(graph, |node| graph.ends[node].contains(end.name.as_str()));
                let on_path = (0..count)
                    .filter(|&node| reached[node] && before_end[node])
                    .collect::<Vec<_>>();
                if on_path.is_empty() {
                    continue;
                }
                any_success_reached = true;
                for &outcome in &end.outcomes {
                    let declared = on_path.iter().any(|&node| {
                        nodes[node]
                            .output
                            .iter()
                            .any(|field| field.outcome == Some(outcome))
                    });
                    if !declared {
                        let outcome = outcome_name(outcome);
                        self.error(
                            Some(&end.name),
                            end.line,
                            format!(
                                "end '{}' promises outcome '{outcome}' but no node on a path to \
                                 it declares a field with outcome = \"{outcome}\"",
                                end.name
                            ),
                        );
                    }
                }
            }
            let has_success = definition
                .ends
                .iter()
                .any(|end| end.kind == EndKind::Success);
            if has_success && !any_success_reached {
                self.error(
                    None,
                    definition.start.line,
                    "no success end is reachable from start",
                );
            }
        }

        // Cycles: with bounded edges removed, the rest must be acyclic.
        let unbounded = graph
            .edges
            .iter()
            .map(|edges| {
                edges
                    .iter()
                    .filter(|edge| !edge.bounded)
                    .map(|edge| edge.to)
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        for component in strongly_connected(&unbounded) {
            let is_cycle = component.len() > 1 || unbounded[component[0]].contains(&component[0]);
            if !is_cycle {
                continue;
            }
            let mut members = component.clone();
            members.sort_unstable();
            let first = members[0];
            let line = graph.edges[first]
                .iter()
                .find(|edge| !edge.bounded && members.contains(&edge.to))
                .map_or(nodes[first].line, |edge| edge.line);
            let names = members
                .iter()
                .map(|&node| nodes[node].name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            self.error(
                Some(&nodes[first].name),
                line,
                format!(
                    "cycle with no bounded edge through {names}; give one of its routes max_passes"
                ),
            );
        }

        self.check_fan_outs(graph);
    }

    fn check_fan_outs(&mut self, graph: &Graph<'a>) {
        let nodes = &self.definition.nodes;
        let is_join = |node: usize| nodes[node].join.is_some();
        let mut fed_joins = BTreeSet::new();

        for (source, source_node) in nodes.iter().enumerate() {
            let branches = graph.edges[source]
                .iter()
                .filter(|edge| edge.kind == EdgeKind::FanOut)
                .map(|edge| edge.to)
                .collect::<BTreeSet<_>>();
            for branch in branches {
                if is_join(branch) {
                    fed_joins.insert(branch);
                    continue;
                }

                // The branch: every node reached before a join, following unbounded edges.
                let mut region = BTreeSet::from([branch]);
                let mut stack = vec![branch];
                let mut nested = BTreeSet::new();
                while let Some(node) = stack.pop() {
                    for edge in &graph.edges[node] {
                        if edge.kind == EdgeKind::FanOut {
                            nested.insert((node, edge.line));
                            continue;
                        }
                        if edge.bounded {
                            continue;
                        }
                        if is_join(edge.to) {
                            fed_joins.insert(edge.to);
                        } else if region.insert(edge.to) {
                            stack.push(edge.to);
                        }
                    }
                }

                for &(node, line) in &nested {
                    self.error(
                        Some(&nodes[node].name),
                        line,
                        format!(
                            "a fan-out inside a branch of fan-out '{}' is not supported in v1",
                            source_node.name
                        ),
                    );
                }

                for &node in &region {
                    for edge in &graph.edges[node] {
                        if edge.bounded && !region.contains(&edge.to) && !is_join(edge.to) {
                            self.error(
                                Some(&nodes[node].name),
                                edge.line,
                                format!(
                                    "a bounded edge from a branch of fan-out '{}' to '{}' \
                                     outside it is not supported in v1",
                                    source_node.name, nodes[edge.to].name
                                ),
                            );
                        }
                    }
                }

                // Every node of the branch must lead to a join or an end inside the branch.
                let mut done = region
                    .iter()
                    .filter(|&&node| {
                        !graph.ends[node].is_empty()
                            || nested.iter().any(|(nested, _)| *nested == node)
                            || graph.edges[node]
                                .iter()
                                .any(|edge| !edge.bounded && is_join(edge.to))
                    })
                    .copied()
                    .collect::<BTreeSet<_>>();
                loop {
                    let more = region
                        .iter()
                        .filter(|node| !done.contains(node))
                        .filter(|&&node| {
                            graph.edges[node]
                                .iter()
                                .any(|edge| region.contains(&edge.to) && done.contains(&edge.to))
                        })
                        .copied()
                        .collect::<Vec<_>>();
                    if more.is_empty() {
                        break;
                    }
                    done.extend(more);
                }
                for &node in &region {
                    if !done.contains(&node) {
                        self.error(
                            Some(&nodes[node].name),
                            nodes[node].line,
                            format!(
                                "in a branch of fan-out '{}', node '{}' reaches no join or end",
                                source_node.name, nodes[node].name
                            ),
                        );
                    }
                }
            }
        }

        for (position, node) in nodes.iter().enumerate() {
            if let Some(join) = &node.join
                && !fed_joins.contains(&position)
            {
                self.error(
                    Some(&node.name),
                    join.line,
                    format!("the join on '{}' is fed by no fan-out", node.name),
                );
            }
        }
    }
}

/// A declared cap and the ceiling it must stay within.
struct Cap<'k> {
    key: &'k str,
    value: f64,
    inherited: bool,
    ceiling: f64,
    ceiling_key: &'k str,
}

fn from_defaults(inherited: bool) -> &'static str {
    if inherited { " (from [defaults])" } else { "" }
}

/// The node's own value, else the default, with its line and whether it came from `[defaults]`.
fn pick<'v, T>(
    own: Option<&'v T>,
    own_line: Option<usize>,
    default: Option<&'v T>,
    default_line: Option<usize>,
) -> Option<(&'v T, Option<usize>, bool)> {
    match own {
        Some(value) => Some((value, own_line, false)),
        None => default.map(|value| (value, default_line, true)),
    }
}

/// The nodes from which some node matching `target` can be reached (the targets included).
fn reverse_reach(graph: &Graph<'_>, target: impl Fn(usize) -> bool) -> Vec<bool> {
    let count = graph.edges.len();
    let mut predecessors = vec![Vec::new(); count];
    for (from, edges) in graph.edges.iter().enumerate() {
        for edge in edges {
            predecessors[edge.to].push(from);
        }
    }
    let mut marked = (0..count).map(&target).collect::<Vec<_>>();
    let mut queue = (0..count)
        .filter(|&node| marked[node])
        .collect::<VecDeque<_>>();
    while let Some(node) = queue.pop_front() {
        for &from in &predecessors[node] {
            if !marked[from] {
                marked[from] = true;
                queue.push_back(from);
            }
        }
    }
    marked
}

/// Tarjan's strongly connected components.
pub(crate) fn strongly_connected(adjacency: &[Vec<usize>]) -> Vec<Vec<usize>> {
    struct State<'g> {
        adjacency: &'g [Vec<usize>],
        next: usize,
        index: Vec<Option<usize>>,
        low: Vec<usize>,
        on_stack: Vec<bool>,
        stack: Vec<usize>,
        components: Vec<Vec<usize>>,
    }

    fn visit(state: &mut State<'_>, node: usize) {
        state.index[node] = Some(state.next);
        state.low[node] = state.next;
        state.next += 1;
        state.stack.push(node);
        state.on_stack[node] = true;
        for &next in &state.adjacency[node] {
            match state.index[next] {
                None => {
                    visit(state, next);
                    state.low[node] = state.low[node].min(state.low[next]);
                }
                Some(index) if state.on_stack[next] => {
                    state.low[node] = state.low[node].min(index);
                }
                Some(_) => {}
            }
        }
        if Some(state.low[node]) == state.index[node] {
            let mut component = Vec::new();
            while let Some(member) = state.stack.pop() {
                state.on_stack[member] = false;
                component.push(member);
                if member == node {
                    break;
                }
            }
            state.components.push(component);
        }
    }

    let count = adjacency.len();
    let mut state = State {
        adjacency,
        next: 0,
        index: vec![None; count],
        low: vec![0; count],
        on_stack: vec![false; count],
        stack: Vec::new(),
        components: Vec::new(),
    };
    for node in 0..count {
        if state.index[node].is_none() {
            visit(&mut state, node);
        }
    }
    state.components
}
