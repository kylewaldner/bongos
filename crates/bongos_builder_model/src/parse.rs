//! The parser. It never fails: every problem is a [`Problem`] with its line, and parsing goes on.
//! A line that cannot become a node (an unknown keyword, a bad label, an indentation fault, a
//! second root) is reported and the lines under it, which are the ones indented deeper, are
//! skipped; parsing resumes at the next line of the same or a lower indentation. A line that
//! becomes a node with a bad attribute keeps the node.

use crate::grammar::{
    By, Edge, FORMAT_VERSION, Header, KIND_NAMES, LABEL_HELP, Located, Node, Parsed, Problem,
    ROLE_NAMES, TaskAttributes, Todo, TodoMark, UNBLOCK_NAMES, is_label,
};
use bongos_work_model::rules::EdgeKind;

const HEADER_WORDS: [&str; 5] = ["format", "proposal", "for", "by", "why"];
const PROPOSAL_RANK: usize = 1;

pub fn parse(text: &str) -> Parsed {
    let mut parser = Parser::default();
    let mut line_count = 0;
    for (index, raw) in text.lines().enumerate() {
        line_count = index + 1;
        parser.line(line_count, raw);
    }
    parser.finish(line_count.max(1))
}

#[derive(Default)]
struct Parser {
    header: Header,
    problems: Vec<Problem>,
    /// The open tasks, outermost first; the task at index `n` is at indentation level `n`.
    stack: Vec<Node>,
    seen: [bool; 5],
    last_rank: usize,
    body_started: bool,
    /// Lines indented deeper than this belong to a line that was reported and dropped.
    skip_deeper_than: Option<usize>,
}

impl Parser {
    fn error(&mut self, line: usize, message: impl Into<String>) {
        self.problems.push(Problem::error(line, message));
    }

    fn line(&mut self, number: usize, raw: &str) {
        let text = raw.trim_end();
        let content = text.trim_start_matches([' ', '\t']);
        if content.is_empty() || content.starts_with('#') {
            return;
        }
        let indentation = text.get(..text.len() - content.len()).unwrap_or_default();
        let tabs = indentation.matches('\t').count();
        let spaces = indentation.len() - tabs;
        let estimated_level = (spaces + 2 * tabs) / 2;
        if self
            .skip_deeper_than
            .is_some_and(|level| estimated_level > level)
        {
            return;
        }
        self.skip_deeper_than = None;
        if tabs > 0 {
            self.error(
                number,
                "tab in indentation: indent with two spaces per level",
            );
            self.skip_deeper_than = Some(estimated_level);
            return;
        }
        if spaces % 2 != 0 {
            self.error(
                number,
                format!("indentation of {spaces} spaces is not a multiple of two"),
            );
            self.skip_deeper_than = Some(estimated_level);
            return;
        }
        let level = spaces / 2;
        let (word, rest) = split_word(content);
        if level == 0 {
            self.top_level_line(number, word, rest);
            return;
        }
        if self.stack.is_empty() {
            self.error(number, "indented line before the first task");
            self.skip_deeper_than = Some(level);
            return;
        }
        if level > self.stack.len() {
            self.error(
                number,
                format!(
                    "indentation too deep: expected at most {} spaces, found {spaces}",
                    2 * self.stack.len()
                ),
            );
            self.skip_deeper_than = Some(level);
            return;
        }
        self.close_to(level);
        match word {
            "task" => match self.task_line(number, rest) {
                Some(node) => self.stack.push(node),
                None => self.skip_deeper_than = Some(level),
            },
            "result" | "todo" | "after" | "stacked" => self.item_line(number, word, rest),
            _ => {
                self.error(
                    number,
                    format!(
                        "unknown line '{word}': expected task, result, todo, after or stacked on"
                    ),
                );
                self.skip_deeper_than = Some(level);
            }
        }
    }

    fn top_level_line(&mut self, number: usize, word: &str, rest: &str) {
        if HEADER_WORDS.contains(&word) {
            self.header_line(number, word, rest);
        } else if word == "task" {
            self.root_line(number, rest);
        } else {
            self.error(
                number,
                format!(
                    "unknown line '{word}': a top-level line is a header line \
                     (proposal, for, by, why) or the root task"
                ),
            );
            self.skip_deeper_than = Some(0);
        }
    }

    fn root_line(&mut self, number: usize, rest: &str) {
        if !self.body_started && !self.seen[PROPOSAL_RANK] {
            self.error(number, "missing 'proposal N' line before the first task");
        }
        self.body_started = true;
        if !self.stack.is_empty() {
            let label = rest.split(':').next().unwrap_or_default();
            self.error(
                number,
                format!("second top-level task '{label}': a proposal has exactly one root task"),
            );
            self.skip_deeper_than = Some(0);
            return;
        }
        match self.task_line(number, rest) {
            Some(node) => self.stack.push(node),
            None => self.skip_deeper_than = Some(0),
        }
    }

    fn header_line(&mut self, number: usize, word: &str, rest: &str) {
        if self.body_started {
            self.error(
                number,
                format!("header line '{word}' must come before the first task"),
            );
            return;
        }
        let rank = HEADER_WORDS
            .iter()
            .position(|candidate| *candidate == word)
            .unwrap_or_default();
        if self.seen.get(rank).copied().unwrap_or_default() {
            self.error(number, format!("repeated header line '{word}'"));
            return;
        }
        if let Some(seen) = self.seen.get_mut(rank) {
            *seen = true;
        }
        if rank < self.last_rank {
            self.error(
                number,
                format!(
                    "header line '{word}' is out of order: the order is format, proposal, \
                     for, by, why"
                ),
            );
        }
        self.last_rank = self.last_rank.max(rank);
        match word {
            "format" => self.format_line(number, rest),
            "proposal" => self.proposal_line(number, rest),
            "for" => {
                if rest.is_empty() || rest.contains(char::is_whitespace) {
                    self.error(number, "'for' needs one task id, as in 'for <id>'");
                } else {
                    self.header.for_task = Some(Located {
                        value: rest.to_string(),
                        line: number,
                    });
                }
            }
            "by" => match parse_by(rest) {
                Some(by) => {
                    self.header.by = Some(Located {
                        value: by,
                        line: number,
                    })
                }
                None => self.error(
                    number,
                    format!(
                        "bad 'by' line '{rest}': write 'by kyle', 'by chat <id>' or \
                         'by workflow <name>'"
                    ),
                ),
            },
            _ => {
                self.header.why = Some(Located {
                    value: rest.to_string(),
                    line: number,
                })
            }
        }
    }

    fn format_line(&mut self, number: usize, rest: &str) {
        let Some(version) = parse_whole_number(rest) else {
            self.error(
                number,
                format!("bad format version '{rest}': write 'format {FORMAT_VERSION}'"),
            );
            return;
        };
        self.header.format = Some(Located {
            value: version,
            line: number,
        });
        if version != FORMAT_VERSION {
            self.error(
                number,
                format!(
                    "unsupported format version {version}: this bongos reads \
                     format {FORMAT_VERSION}"
                ),
            );
        }
    }

    fn proposal_line(&mut self, number: usize, rest: &str) {
        if rest.is_empty() {
            self.error(number, "'proposal' needs a number, as in 'proposal 3'");
            return;
        }
        match parse_whole_number(rest).filter(|value| *value >= 1) {
            Some(value) => {
                self.header.number = Some(Located {
                    value,
                    line: number,
                })
            }
            None => self.error(
                number,
                format!(
                    "bad proposal number '{rest}': write a whole number from 1, as in 'proposal 3'"
                ),
            ),
        }
    }

    /// Pops open tasks into their parents until `length` remain.
    fn close_to(&mut self, length: usize) {
        while self.stack.len() > length {
            let Some(closed) = self.stack.pop() else {
                return;
            };
            match self.stack.last_mut() {
                Some(parent) => parent.children.push(closed),
                None => {
                    self.stack.push(closed);
                    return;
                }
            }
        }
    }

    fn task_line(&mut self, number: usize, rest: &str) -> Option<Node> {
        let Some((label, body)) = rest.split_once(':') else {
            self.error(
                number,
                "a task line is 'task <label>: <goal>', with a colon after the label",
            );
            return None;
        };
        if !is_label(label) {
            self.error(number, format!("bad label '{label}': {LABEL_HELP}"));
            return None;
        }
        if !body.is_empty() && !body.starts_with(' ') {
            self.error(
                number,
                format!("task '{label}': write a space after the colon"),
            );
            return None;
        }
        let mut segments = body.split(" | ");
        let goal = segments.next().unwrap_or_default().trim();
        if goal.is_empty() {
            self.error(number, format!("task '{label}' has no goal"));
            return None;
        }
        let mut node = Node::new(number, label, goal);
        for segment in segments {
            self.task_attribute(number, &mut node.attributes, segment.trim());
        }
        Some(node)
    }

    fn task_attribute(&mut self, number: usize, attributes: &mut TaskAttributes, segment: &str) {
        let (key, value) = split_word(segment);
        let repeated = match key {
            "id" => attributes.id.is_some(),
            "kind" => attributes.kind.is_some(),
            "state" => attributes.state.is_some(),
            "keeps" => attributes.keeps_work,
            "drop" => attributes.drop,
            _ => false,
        };
        if repeated {
            self.error(number, format!("task attribute '{key}' is repeated"));
            return;
        }
        match (key, value) {
            ("id", "") => self.error(number, "'id' needs a value, as in '| id <id>'"),
            ("id", value) if value.contains(char::is_whitespace) => {
                self.error(number, format!("bad id '{value}': an id has no spaces"))
            }
            ("id", value) => attributes.id = Some(value.to_string()),
            ("kind", value) => match lookup(&KIND_NAMES, value) {
                Some(kind) => attributes.kind = Some(kind),
                None => self.error(
                    number,
                    format!("unknown kind '{value}': expected {}", names(&KIND_NAMES)),
                ),
            },
            ("state", "") => self.error(number, "'state' needs a value, as in '| state done'"),
            ("state", value) if value.contains(char::is_whitespace) => {
                self.error(number, format!("bad state '{value}': a state is one word"))
            }
            ("state", value) => attributes.state = Some(value.to_string()),
            ("keeps", "work") => attributes.keeps_work = true,
            ("drop", "") => attributes.drop = true,
            _ => self.error(number, format!("unknown task attribute '{segment}'")),
        }
    }

    fn item_line(&mut self, number: usize, word: &str, rest: &str) {
        match word {
            "result" => {
                if rest.is_empty() {
                    self.error(number, "'result' needs text, as in 'result a PR adding X'");
                } else if let Some(parent) = self.stack.last_mut() {
                    parent.results.push(Located {
                        value: rest.to_string(),
                        line: number,
                    });
                }
            }
            "todo" => {
                if let Some(todo) = self.todo_line(number, rest)
                    && let Some(parent) = self.stack.last_mut()
                {
                    parent.todos.push(todo);
                }
            }
            _ => {
                if let Some(edge) = self.edge_line(number, word, rest)
                    && let Some(parent) = self.stack.last_mut()
                {
                    parent.edges.push(edge);
                }
            }
        }
    }

    fn todo_line(&mut self, number: usize, rest: &str) -> Option<Todo> {
        let mut segments = rest.split(" | ");
        let name = segments.next().unwrap_or_default().trim();
        if name.is_empty() {
            self.error(number, "'todo' needs a name, as in 'todo implement'");
            return None;
        }
        let mut todo = Todo {
            line: number,
            name: name.to_string(),
            role: None,
            workflow: None,
            mark: None,
            started: false,
        };
        let mut role_seen = false;
        let mut workflow_seen = false;
        for segment in segments {
            let segment = segment.trim();
            let (key, value) = split_word(segment);
            match (key, value) {
                ("role", _) if role_seen => self.error(number, "todo attribute 'role' is repeated"),
                ("role", value) => {
                    role_seen = true;
                    match lookup(&ROLE_NAMES, value) {
                        Some(role) => todo.role = Some(role),
                        None => self.error(
                            number,
                            format!("unknown role '{value}': expected {}", names(&ROLE_NAMES)),
                        ),
                    }
                }
                ("workflow", _) if workflow_seen => {
                    self.error(number, "todo attribute 'workflow' is repeated")
                }
                ("workflow", "") => {
                    workflow_seen = true;
                    self.error(number, "'workflow' needs a name, as in '| workflow review'")
                }
                ("workflow", value) => {
                    workflow_seen = true;
                    todo.workflow = Some(value.to_string());
                }
                ("started", "") if todo.started => {
                    self.error(number, "todo attribute 'started' is repeated")
                }
                ("started", "") => todo.started = true,
                ("done" | "skipped" | "cancelled", "") => {
                    let mark = match key {
                        "done" => TodoMark::Done,
                        "skipped" => TodoMark::Skipped,
                        _ => TodoMark::Cancelled,
                    };
                    if todo.mark.is_some() {
                        self.error(
                            number,
                            "a todo has at most one of done, skipped and cancelled",
                        );
                    } else {
                        todo.mark = Some(mark);
                    }
                }
                _ => self.error(number, format!("unknown todo attribute '{segment}'")),
            }
        }
        Some(todo)
    }

    fn edge_line(&mut self, number: usize, word: &str, rest: &str) -> Option<Edge> {
        let (kind, target_text) = if word == "after" {
            (EdgeKind::After, rest)
        } else {
            let Some(target) = rest.strip_prefix("on ") else {
                let message = if rest == "on" {
                    "'stacked on' needs a task, as in 'stacked on schema'"
                } else {
                    "expected 'stacked on <task>'"
                };
                self.error(number, message);
                return None;
            };
            (EdgeKind::StackedOn, target.trim_start())
        };
        let mut segments = target_text.split(" | ");
        let target_text = segments.next().unwrap_or_default().trim();
        let phrase = if kind == EdgeKind::After {
            "after"
        } else {
            "stacked on"
        };
        if target_text.is_empty() {
            self.error(
                number,
                format!("'{phrase}' needs a task, as in '{phrase} schema'"),
            );
            return None;
        }
        let target: Vec<String> = target_text.split('/').map(str::to_string).collect();
        if let Some(bad) = target.iter().find(|label| !is_label(label)) {
            self.error(
                number,
                format!("bad label '{bad}' in '{phrase} {target_text}': {LABEL_HELP}"),
            );
            return None;
        }
        let mut unblock = None;
        for segment in segments {
            let segment = segment.trim();
            match segment.strip_prefix("unblock ") {
                Some(_) if unblock.is_some() => {
                    self.error(number, "edge attribute 'unblock' is repeated")
                }
                Some(value) => match lookup(&UNBLOCK_NAMES, value.trim()) {
                    Some(point) => unblock = Some(point),
                    None => self.error(
                        number,
                        format!(
                            "unknown unblock point '{}': expected {}",
                            value.trim(),
                            names(&UNBLOCK_NAMES)
                        ),
                    ),
                },
                None => self.error(number, format!("unknown edge attribute '{segment}'")),
            }
        }
        Some(Edge {
            line: number,
            kind,
            target,
            unblock,
        })
    }

    fn finish(mut self, last_line: usize) -> Parsed {
        if !self.body_started {
            if !self.seen[PROPOSAL_RANK] {
                self.error(last_line, "missing 'proposal N' line");
            }
            self.error(
                last_line,
                "no task: a proposal has one top-level 'task <label>: <goal>' line",
            );
        }
        self.close_to(1);
        let root = self.stack.pop().unwrap_or_else(|| Node::new(0, "", ""));
        self.problems.sort_by_key(|problem| problem.line);
        Parsed {
            header: self.header,
            root,
            problems: self.problems,
        }
    }
}

/// The first word of `text` and the rest, with the rest's leading spaces removed.
fn split_word(text: &str) -> (&str, &str) {
    match text.split_once(' ') {
        Some((word, rest)) => (word, rest.trim_start()),
        None => (text, ""),
    }
}

fn parse_whole_number(text: &str) -> Option<u32> {
    if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

fn parse_by(text: &str) -> Option<By> {
    if text == "kyle" {
        return Some(By::Kyle);
    }
    let (word, rest) = split_word(text);
    match word {
        "chat" if !rest.is_empty() && !rest.contains(char::is_whitespace) => {
            Some(By::Chat(rest.to_string()))
        }
        "workflow" if !rest.is_empty() => Some(By::Workflow(rest.to_string())),
        _ => None,
    }
}

fn lookup<T: Copy>(table: &[(&str, T)], name: &str) -> Option<T> {
    table
        .iter()
        .find(|(candidate, _)| *candidate == name)
        .map(|(_, value)| *value)
}

fn names<T>(table: &[(&str, T)]) -> String {
    let names: Vec<&str> = table.iter().map(|(name, _)| *name).collect();
    match names.as_slice() {
        [] => String::new(),
        [only] => (*only).to_string(),
        [rest @ .., last] => format!("{} or {last}", rest.join(", ")),
    }
}
