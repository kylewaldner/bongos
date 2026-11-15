//! The `script` check kind: `run = { script = "checks/plan-format" }` runs master's copy of the
//! script (from the materialized tree of master, so a change cannot rewrite the check that
//! judges it) with the change's materialized tree, the change summary as JSON and the change
//! meta as JSON as its arguments. Exit 0 passes, exit 1 fails, any other exit or a script that
//! cannot be started (a missing interpreter) gives no verdict. A script still running after
//! [`SCRIPT_TIMEOUT`] is killed and gives no verdict.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use bongos_project_repo_model::checks::{CheckOutcome, CheckPhase};
use bongos_project_repo_model::layout::DEFINITIONS_DIR;
use futures::FutureExt as _;
use futures::future::{BoxFuture, Either};

use crate::landing_checks::{CheckInput, LandingCheck};

pub(crate) const SCRIPT_KIND: &str = "script";

/// How long a script check may run before it is killed.
pub(crate) const SCRIPT_TIMEOUT: Duration = Duration::from_secs(600);

/// How much of a script's output a failure or no verdict quotes.
const QUOTED_OUTPUT_BYTES: usize = 2000;

/// A check that runs a script of the project's `definitions/`.
pub struct ScriptCheck {
    /// Relative to `definitions/`.
    script: String,
    name: String,
}

/// Parses the `script` value of a `run` table.
pub(crate) fn parse_script(value: &toml::Value) -> Result<Arc<dyn LandingCheck>, String> {
    let script = value
        .as_str()
        .filter(|script| !script.trim().is_empty())
        .ok_or("script must be a path under definitions/")?;
    if script.starts_with('/') || script.split('/').any(|part| part == "..") {
        return Err(format!("script {script} must stay under definitions/"));
    }
    Ok(Arc::new(ScriptCheck {
        script: script.to_string(),
        name: format!("script {script}"),
    }))
}

fn quoted(output: &[u8]) -> String {
    let text = String::from_utf8_lossy(output);
    let text = text.trim();
    if text.len() <= QUOTED_OUTPUT_BYTES {
        return text.to_string();
    }
    let mut end = QUOTED_OUTPUT_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &text[..end])
}

impl LandingCheck for ScriptCheck {
    fn name(&self) -> &str {
        &self.name
    }

    fn phase(&self) -> CheckPhase {
        CheckPhase::BeforeQueue
    }

    fn run(&self, input: CheckInput) -> BoxFuture<'static, CheckOutcome> {
        let script: PathBuf = input.master_root.join(DEFINITIONS_DIR).join(&self.script);
        let root = input.root.to_path_buf();
        let summary = serde_json::to_string(input.summary);
        let meta = serde_json::to_string(input.meta);
        let shown = self.script.clone();
        let timer = input.timer.clone();
        async move {
            if !script.is_file() {
                return CheckOutcome::no_verdict(format!("master has no script {shown}"));
            }
            let (summary, meta) = match (summary, meta) {
                (Ok(summary), Ok(meta)) => (summary, meta),
                (Err(error), _) | (_, Err(error)) => {
                    return CheckOutcome::no_verdict(format!("serializing the change: {error}"));
                }
            };
            let mut command = util::command::new_command(&script);
            // Dropping the run on the timeout drops the child, which kills it.
            command
                .current_dir(&root)
                .arg(&root)
                .arg(summary)
                .arg(meta)
                .kill_on_drop(true);
            let output = match futures::future::select(
                command.output().boxed(),
                timer(SCRIPT_TIMEOUT),
            )
            .await
            {
                Either::Left((Ok(output), _)) => output,
                Either::Left((Err(error), _)) => {
                    return CheckOutcome::no_verdict(format!("{shown} could not run: {error}"));
                }
                Either::Right(_) => {
                    return CheckOutcome::no_verdict(format!(
                        "{shown} ran longer than {} seconds and was stopped",
                        SCRIPT_TIMEOUT.as_secs()
                    ));
                }
            };
            let mut said = output.stdout.clone();
            said.extend_from_slice(&output.stderr);
            let said = quoted(&said);
            match output.status.code() {
                Some(0) => CheckOutcome::Pass,
                Some(1) => CheckOutcome::fail(if said.is_empty() {
                    format!("{shown} failed")
                } else {
                    format!("{shown}: {said}")
                }),
                Some(code) => {
                    CheckOutcome::no_verdict(format!("{shown} exited with {code}: {said}"))
                }
                None => CheckOutcome::no_verdict(format!("{shown} was killed")),
            }
        }
        .boxed()
    }
}
