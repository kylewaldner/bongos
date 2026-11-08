//! `kotlin: license status|activate|trial` and `kotlin: review intellij eula` (decisions D9, D10
//! and D15). The command lines, the status interpretation and the recheck decision are plain
//! functions; the workspace glue below them only opens pickers, buffers, toasts and the terminal tab.
//!
//! Secrets never go on a command line. An activation code or a license server URL is typed by the
//! user at the CLI's own prompt in the terminal tab (`license activate` with no argument). A key file
//! the user picked is handed over as a path to a private copy: a file with mode 0600 in a directory
//! with mode 0700 that is deleted when the terminal task exits. The task runs without shell history.
//! bongos stores no credentials.

use std::{
    borrow::Cow,
    fs,
    future::Future,
    path::{Path, PathBuf},
    process::ExitStatus,
    rc::Rc,
    sync::{Arc, atomic::AtomicBool},
};

use anyhow::{Context as _, Result, bail};
use futures::{FutureExt as _, StreamExt as _};
use gpui::{
    App, AsyncWindowContext, Context, DismissEvent, Entity, EventEmitter, Focusable,
    PathPromptOptions, Render, SharedString, Task, WeakEntity, Window,
};
use lsp::LanguageServerBinary;
use picker::{Picker, PickerDelegate};
use settings::{KotlinServerKind, Settings as _, SettingsLocation, WorktreeId};
use task::{SpawnInTerminal, TaskId};
use ui::{HighlightedLabel, ListItem, ListItemSpacing, prelude::*};
use util::rel_path::RelPath;
use workspace::{ModalView, Toast, Workspace, notifications::NotificationId};

use crate::{
    KotlinDirs, KotlinProjectManager, KotlinSettings, KotlinSetup,
    adapter::INTELLIJ_DOWNLOAD_HINT,
    commands::target,
    install::{ProgressFn, spawn_ensure_installed},
    installed::{check_installed, not_installed, server_name, version_dir},
    intellij::{EULA_FILE, LICENSE_TIMEOUT, LicenseOutcome, accept_eula, eula_accepted, eula_hash},
    manager::FallbackReason,
    manifest::{Manifest, ManifestEntry, current_platform},
};

/// The title of the buffer `kotlin: license status` opens.
pub const LICENSE_STATUS_TITLE: &str = "Kotlin License Status";

/// The title of the buffer `kotlin: review intellij eula` opens.
pub const EULA_TITLE: &str = "IntelliJ EULA";

/// What `kotlin: license status` says when the IntelliJ server is neither requested nor running.
pub const NOT_ACTIVE_MESSAGE: &str = "IntelliJ server is not active";

/// Opens read-only text in a buffer with a fixed title. The app passes its report-buffer opener, so
/// this crate needs no editor.
pub type ShowText = fn(&mut Workspace, String, &'static str, &mut Window, &mut Context<Workspace>);

// ---------------------------------------------------------------------------------------------
// Command lines
// ---------------------------------------------------------------------------------------------

fn to_args<const N: usize>(args: [&str; N]) -> Vec<String> {
    args.into_iter().map(String::from).collect()
}

/// The arguments of `<launcher> license status`.
pub(crate) fn status_args() -> Vec<String> {
    to_args(["license", "status"])
}

/// The arguments of `<launcher> license trial`.
pub(crate) fn trial_args() -> Vec<String> {
    to_args(["license", "trial"])
}

/// The rows of the activation picker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ActivateKind {
    Account,
    Code,
    KeyFile,
    ServerUrl,
}

impl ActivateKind {
    pub(crate) const ALL: [Self; 4] = [Self::Account, Self::Code, Self::KeyFile, Self::ServerUrl];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Account => "JetBrains account",
            Self::Code => "Activation code or key",
            Self::KeyFile => "Key file...",
            Self::ServerUrl => "License server URL",
        }
    }
}

/// One way to activate, once everything but the secret is known.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ActivateForm {
    /// `license login --code`: prints a URL and a code to confirm in the browser.
    Account,
    /// `license activate`: the CLI asks for the code or key.
    Code,
    /// `license activate <key file>`: the file the user picked.
    KeyFile(PathBuf),
    /// `license activate`: the CLI asks for the URL.
    ServerUrl,
}

/// The arguments of the form. `key_copy` is the private copy of a key file, used in place of the
/// picked file when there is one. A code or URL is never an argument: the CLI prompts for it.
pub(crate) fn activate_args(form: &ActivateForm, key_copy: Option<&Path>) -> Vec<String> {
    match form {
        ActivateForm::Account => to_args(["license", "login", "--code"]),
        ActivateForm::Code | ActivateForm::ServerUrl => to_args(["license", "activate"]),
        ActivateForm::KeyFile(picked) => {
            let mut args = to_args(["license", "activate"]);
            args.push(key_copy.unwrap_or(picked).to_string_lossy().into_owned());
            args
        }
    }
}

/// What a terminal task runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum LicenseJob {
    Activate(ActivateForm),
    Trial,
}

impl LicenseJob {
    fn label(&self) -> &'static str {
        match self {
            Self::Activate(_) => "kotlin: license activate",
            Self::Trial => "kotlin: license trial",
        }
    }

    fn args(&self, key_copy: Option<&Path>) -> Vec<String> {
        match self {
            Self::Activate(form) => activate_args(form, key_copy),
            Self::Trial => trial_args(),
        }
    }

    /// The key file to copy privately, if the job has one.
    fn key_file(&self) -> Option<&Path> {
        match self {
            Self::Activate(ActivateForm::KeyFile(path)) => Some(path),
            _ => None,
        }
    }
}

/// Single quotes around anything the shell would treat specially. Terminal tasks run through the
/// user's shell, which does not quote for us.
pub(crate) fn sh_quote(text: &str) -> String {
    let plain = !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c));
    if plain {
        text.to_string()
    } else {
        format!("'{}'", text.replace('\'', "'\\''"))
    }
}

/// Variables that keep the task's commands out of the shell's history file.
const NO_HISTORY_ENV: [(&str, &str); 4] = [
    ("HISTFILE", "/dev/null"),
    ("HISTSIZE", "0"),
    ("SAVEHIST", "0"),
    ("fish_history", ""),
];

/// The one-shot terminal task for `<launcher> <args>`: a new tab, no shell history, the launcher's
/// own environment.
pub(crate) fn terminal_task(
    binary: &LanguageServerBinary,
    args: &[String],
    label: &str,
    cwd: &Path,
) -> SpawnInTerminal {
    let launcher = binary.path.to_string_lossy().into_owned();
    let mut env: collections::HashMap<String, String> =
        binary.env.clone().unwrap_or_default().into_iter().collect();
    for (key, value) in NO_HISTORY_ENV {
        env.insert(key.into(), value.into());
    }
    let command_label = std::iter::once(binary.path.file_name().map_or_else(
        || launcher.clone(),
        |name| name.to_string_lossy().into_owned(),
    ))
    .chain(args.iter().cloned())
    .collect::<Vec<_>>()
    .join(" ");
    SpawnInTerminal {
        id: TaskId(label.to_string()),
        full_label: label.to_string(),
        label: label.to_string(),
        command: Some(sh_quote(&launcher)),
        args: args.iter().map(|arg| sh_quote(arg)).collect(),
        command_label,
        cwd: Some(cwd.to_path_buf()),
        env,
        use_new_terminal: true,
        allow_concurrent_runs: true,
        show_summary: true,
        show_command: true,
        show_rerun: false,
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------------------------
// Private copy of a secret file
// ---------------------------------------------------------------------------------------------

/// A file with mode 0600 in a directory with mode 0700. Both are deleted when this is dropped.
pub(crate) struct SecretFile {
    _dir: tempfile::TempDir,
    path: PathBuf,
}

impl SecretFile {
    /// Creates `<parent>/bongos-license-<random>/<name>` holding `contents`.
    pub(crate) fn create(parent: &Path, name: &str, contents: &[u8]) -> Result<Self> {
        let dir = tempfile::Builder::new()
            .prefix("bongos-license-")
            .tempdir_in(parent)
            .context("creating a private directory")?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700))
                .context("restricting the private directory")?;
        }
        let path = dir.path().join(name);
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        let mut file = options
            .open(&path)
            .context("creating the private key file")?;
        std::io::Write::write_all(&mut file, contents).context("writing the private key file")?;
        Ok(Self { _dir: dir, path })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    #[cfg(test)]
    pub(crate) fn dir(&self) -> &Path {
        self._dir.path()
    }
}

// ---------------------------------------------------------------------------------------------
// Status
// ---------------------------------------------------------------------------------------------

/// `license status` only means something for the IntelliJ server: it needs to be the server asked
/// for or the one running.
pub(crate) fn status_gate(
    requested: KotlinServerKind,
    effective: KotlinServerKind,
) -> Result<(), &'static str> {
    if requested == KotlinServerKind::Intellij || effective == KotlinServerKind::Intellij {
        Ok(())
    } else {
        Err(NOT_ACTIVE_MESSAGE)
    }
}

/// Whether the status after a license command means the worktree should run IntelliJ now.
pub(crate) fn restarts_after(outcome: LicenseOutcome) -> bool {
    outcome == LicenseOutcome::Licensed
}

/// The fallback a pre-start `license status` answer causes (decision D10); `None` when licensed.
pub(crate) fn fallback_for(outcome: LicenseOutcome) -> Option<FallbackReason> {
    match outcome {
        LicenseOutcome::Licensed => None,
        LicenseOutcome::Unlicensed => Some(FallbackReason::Unlicensed),
        LicenseOutcome::Offline | LicenseOutcome::TimedOut => {
            Some(FallbackReason::LicenseCheckFailed)
        }
    }
}

// ---------------------------------------------------------------------------------------------
// License signals during a session (decision D10)
// ---------------------------------------------------------------------------------------------

/// Text that means the running IntelliJ server lost its license, matched against
/// `window/showMessage`, `window/logMessage` and stderr lines. Pinned from the unlicensed output
/// recorded in step 1 (`No valid license found` / `License: not licensed, feature requests are
/// parked (NoLicenseDiscovered)`); the licensed session's messages are manual check M3, and a
/// correction is an edit of this list.
pub const LICENSE_LOST_SIGNALS: &[&str] = &[
    "No valid license found",
    "NoLicenseDiscovered",
    "License: not licensed",
    "feature requests are parked",
];

/// Exit codes that mean the IntelliJ server stopped for its license. The JetBrains VS Code
/// extension treats 7 and 11 as license failures; 7 is the expiry row of the fallback table, so
/// only 11 is a lost license here.
pub const LICENSE_LOST_EXIT_CODES: &[i32] = &[11];

/// Whether a message or a stderr line from the IntelliJ server says its license is gone.
pub fn is_license_lost_signal(text: &str) -> bool {
    LICENSE_LOST_SIGNALS
        .iter()
        .any(|signal| text.contains(signal))
}

/// Whether the IntelliJ server's exit code says its license is gone.
pub fn is_license_lost_exit(code: Option<i32>) -> bool {
    code.is_some_and(|code| LICENSE_LOST_EXIT_CODES.contains(&code))
}

/// The result of `license status`, with the text for the buffer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StatusReport {
    pub outcome: LicenseOutcome,
    pub text: String,
}

fn outcome_line(outcome: LicenseOutcome) -> &'static str {
    match outcome {
        LicenseOutcome::Licensed => "Result: licensed",
        LicenseOutcome::Unlicensed => "Result: not licensed",
        LicenseOutcome::Offline => "Result: license check failed (offline?)",
        LicenseOutcome::TimedOut => "Result: license check timed out (offline?)",
    }
}

/// The buffer text: the CLI's own output, then the verdict bongos draws from the exit code.
pub(crate) fn status_text(
    outcome: LicenseOutcome,
    code: Option<i32>,
    stdout: &str,
    stderr: &str,
) -> String {
    let mut text = String::new();
    for output in [stdout.trim_end(), stderr.trim_end()] {
        if !output.is_empty() {
            text.push_str(output);
            text.push('\n');
        }
    }
    if text.is_empty() {
        text.push_str("(license status printed nothing)\n");
    }
    text.push('\n');
    text.push_str(outcome_line(outcome));
    text.push('\n');
    match code {
        Some(code) => text.push_str(&format!("Exit code: {code}\n")),
        None => text.push_str("Exit code: none (the process was killed)\n"),
    }
    text
}

/// Runs `<launcher> license status` and keeps its output. `timeout` completes when time is up; the
/// process is killed then and when the check is dropped. Only a command that cannot start is an
/// error.
pub(crate) async fn license_report(
    binary: &LanguageServerBinary,
    timeout: impl Future<Output = ()>,
) -> Result<StatusReport> {
    let mut command = util::command::new_command(&binary.path);
    command
        .args(status_args())
        .stdin(util::command::Stdio::null())
        .stdout(util::command::Stdio::piped())
        .stderr(util::command::Stdio::piped())
        .kill_on_drop(true);
    if let Some(env) = &binary.env {
        command.envs(env);
    }
    let output = command.output().fuse();
    let timeout = timeout.fuse();
    futures::pin_mut!(output, timeout);
    futures::select_biased! {
        output = output => {
            let output = output
                .with_context(|| format!("running {} license status", binary.path.display()))?;
            let code = output.status.code();
            let outcome = LicenseOutcome::from_exit_code(code);
            Ok(StatusReport {
                outcome,
                text: status_text(
                    outcome,
                    code,
                    &String::from_utf8_lossy(&output.stdout),
                    &String::from_utf8_lossy(&output.stderr),
                ),
            })
        }
        () = timeout => Ok(StatusReport {
            outcome: LicenseOutcome::TimedOut,
            text: status_text(
                LicenseOutcome::TimedOut,
                None,
                "license status did not answer in time",
                "",
            ),
        }),
    }
}

// ---------------------------------------------------------------------------------------------
// The IntelliJ bundle and its EULA
// ---------------------------------------------------------------------------------------------

/// An installed IntelliJ bundle.
pub(crate) struct Bundle {
    pub binary: LanguageServerBinary,
    pub dir: PathBuf,
}

/// What the commands do about the IntelliJ bundle.
pub(crate) enum BundlePlan {
    /// Installed: run it.
    Ready(Bundle),
    /// Missing, and `kotlin.intellij.download` allows installing it (D16).
    Install,
    /// Missing, and the setting does not allow installing it.
    NeedsSetting,
}

pub(crate) fn installed_bundle(dirs: &KotlinDirs, entry: &ManifestEntry) -> Option<Bundle> {
    let binary = check_installed(&dirs.install_root, KotlinServerKind::Intellij, entry)?;
    Some(Bundle {
        binary,
        dir: version_dir(
            &dirs.install_root,
            KotlinServerKind::Intellij,
            &entry.version,
        ),
    })
}

pub(crate) fn plan_bundle(
    dirs: &KotlinDirs,
    entry: &ManifestEntry,
    download_allowed: bool,
) -> BundlePlan {
    match installed_bundle(dirs, entry) {
        Some(bundle) => BundlePlan::Ready(bundle),
        None if download_allowed => BundlePlan::Install,
        None => BundlePlan::NeedsSetting,
    }
}

/// The agreement of an installed bundle, as shown to the user.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct EulaReview {
    /// The first 16 hex characters of the SHA-256 of `EULA.txt`, the value `--eula=` takes.
    pub hash: String,
    pub text: String,
    /// Whether the user accepted an agreement with this hash before.
    pub accepted: bool,
}

/// Reads `EULA.txt` once, so the hash is that of exactly the text shown.
pub(crate) fn read_eula(bundle_dir: &Path, state_dir: &Path) -> Result<EulaReview> {
    let path = bundle_dir.join(EULA_FILE);
    let bytes = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    let hash = eula_hash(&bytes);
    Ok(EulaReview {
        accepted: eula_accepted(state_dir, &hash),
        text: String::from_utf8_lossy(&bytes).into_owned(),
        hash,
    })
}

/// Records the user's acceptance of the agreement they were shown. Called from the toast's
/// "Accept" button only.
pub(crate) fn accept_review(review: &EulaReview, state_dir: &Path) -> Result<()> {
    accept_eula(state_dir, &review.hash)
}

/// The toast text while the bundle downloads.
pub(crate) fn progress_message(done: u64, total: Option<u64>) -> String {
    let megabytes = |bytes: u64| bytes / (1024 * 1024);
    match total {
        Some(total) if total > 0 => format!(
            "Downloading the IntelliJ server: {}% ({}/{} MB)",
            ((done.saturating_mul(100) + total / 2) / total).min(100),
            megabytes(done),
            megabytes(total)
        ),
        _ => format!("Downloading the IntelliJ server: {} MB", megabytes(done)),
    }
}

// ---------------------------------------------------------------------------------------------
// Workspace glue
// ---------------------------------------------------------------------------------------------

struct LicenseNotice;

fn install_notice_id() -> NotificationId {
    NotificationId::named("kotlin-intellij-install".into())
}

fn notice(
    workspace: &mut Workspace,
    message: impl Into<Cow<'static, str>>,
    cx: &mut Context<Workspace>,
) {
    workspace.show_toast(
        Toast::new(NotificationId::unique::<LicenseNotice>(), message),
        cx,
    );
}

fn notice_weak(workspace: &WeakEntity<Workspace>, message: String, cx: &mut AsyncWindowContext) {
    workspace
        .update(cx, |workspace, cx| notice(workspace, message, cx))
        .ok();
}

/// What a command knows about the worktree it acts on.
#[derive(Clone)]
struct Target {
    manager: Entity<KotlinProjectManager>,
    worktree_id: WorktreeId,
    root: PathBuf,
    requested: KotlinServerKind,
    effective: KotlinServerKind,
    dirs: KotlinDirs,
    manifest: Arc<Manifest>,
    download_allowed: bool,
}

impl Target {
    fn of(workspace: &Workspace, cx: &App) -> Result<Self, &'static str> {
        let (manager, worktree_id) = target(workspace, cx).ok_or("No Kotlin project is open")?;
        let (root, requested, effective) = manager
            .read(cx)
            .worktree(worktree_id)
            .map(|state| (state.root.clone(), state.requested.server, state.effective))
            .ok_or("No Kotlin project is open")?;
        let setup = cx
            .try_global::<KotlinSetup>()
            .ok_or("Kotlin support is not set up")?;
        let download_allowed = KotlinSettings::get(
            Some(SettingsLocation {
                worktree_id,
                path: RelPath::empty(),
            }),
            cx,
        )
        .intellij_download;
        Ok(Self {
            manager,
            worktree_id,
            root,
            requested,
            effective,
            dirs: setup.dirs.clone(),
            manifest: setup.manifest.clone(),
            download_allowed,
        })
    }

    fn entry(&self) -> Result<ManifestEntry> {
        let platform = current_platform().ok_or_else(|| {
            anyhow::anyhow!(
                "no {} build for this platform",
                server_name(KotlinServerKind::Intellij)
            )
        })?;
        self.manifest.entry(KotlinServerKind::Intellij, platform)
    }
}

/// The pinned IntelliJ bundle, installed first (with progress in a toast) when it is missing and
/// `kotlin.intellij.download` allows it.
async fn ensure_bundle(
    workspace: &WeakEntity<Workspace>,
    target: &Target,
    cx: &mut AsyncWindowContext,
) -> Result<Bundle> {
    let entry = target.entry()?;
    match plan_bundle(&target.dirs, &entry, target.download_allowed) {
        BundlePlan::Ready(bundle) => Ok(bundle),
        BundlePlan::NeedsSetting => bail!(INTELLIJ_DOWNLOAD_HINT),
        BundlePlan::Install => {
            let (sender, mut updates) = futures::channel::mpsc::unbounded();
            let progress: Arc<ProgressFn> = Arc::new(move |done, total| {
                sender.unbounded_send((done, total)).ok();
            });
            let dir = target
                .dirs
                .install_root
                .join(server_name(KotlinServerKind::Intellij));
            let mut task = cx
                .update(|_, cx| {
                    spawn_ensure_installed(
                        KotlinServerKind::Intellij,
                        entry.clone(),
                        dir,
                        cx.http_client(),
                        progress,
                        Arc::new(AtomicBool::new(false)),
                        cx,
                    )
                })?
                .fuse();
            let mut shown = String::new();
            let result = loop {
                futures::select_biased! {
                    update = updates.select_next_some() => {
                        let message = progress_message(update.0, update.1);
                        if message != shown {
                            workspace
                                .update(cx, |workspace, cx| {
                                    workspace.show_toast(
                                        Toast::new(install_notice_id(), message.clone()),
                                        cx,
                                    )
                                })
                                .ok();
                            shown = message;
                        }
                    }
                    result = task => break result,
                }
            };
            workspace
                .update(cx, |workspace, cx| {
                    workspace.dismiss_toast(&install_notice_id(), cx)
                })
                .ok();
            result.context("installing the IntelliJ server")?;
            installed_bundle(&target.dirs, &entry)
                .ok_or_else(|| not_installed(KotlinServerKind::Intellij))
        }
    }
}

/// Runs `license status` in the background with the launcher's environment.
async fn status_of(
    binary: LanguageServerBinary,
    cx: &mut AsyncWindowContext,
) -> Result<StatusReport> {
    let timer = cx.update(|_, cx| cx.background_executor().timer(LICENSE_TIMEOUT))?;
    cx.background_spawn(async move { license_report(&binary, timer).await })
        .await
}

/// `kotlin: license status`.
pub fn license_status(
    workspace: &mut Workspace,
    show: ShowText,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let target = match Target::of(workspace, cx) {
        Ok(target) => target,
        Err(message) => return notice(workspace, message, cx),
    };
    if let Err(message) = status_gate(target.requested, target.effective) {
        return notice(workspace, message, cx);
    }
    cx.spawn_in(window, async move |workspace, cx| {
        let report = async {
            let bundle = target
                .entry()
                .ok()
                .and_then(|entry| installed_bundle(&target.dirs, &entry))
                .ok_or_else(|| not_installed(KotlinServerKind::Intellij))?;
            status_of(bundle.binary, cx).await
        }
        .await;
        workspace
            .update_in(cx, |workspace, window, cx| match report {
                Ok(report) => show(workspace, report.text, LICENSE_STATUS_TITLE, window, cx),
                Err(error) => notice(
                    workspace,
                    format!("Cannot run license status: {error:#}"),
                    cx,
                ),
            })
            .ok();
    })
    .detach();
}

/// `kotlin: license trial`.
pub fn license_trial(workspace: &mut Workspace, window: &mut Window, cx: &mut Context<Workspace>) {
    start_job(workspace, LicenseJob::Trial, window, cx);
}

/// `kotlin: license activate`: a picker of the four forms.
pub fn license_activate(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    if let Err(message) = Target::of(workspace, cx) {
        return notice(workspace, message, cx);
    }
    let workspace_handle = cx.entity().downgrade();
    workspace.toggle_modal(window, cx, |window, cx| {
        let on_choose: Rc<dyn Fn(ActivateKind, &mut Window, &mut App)> =
            Rc::new(move |kind, window, cx| {
                let workspace = workspace_handle.clone();
                match kind {
                    ActivateKind::KeyFile => {
                        let picked = cx.prompt_for_paths(PathPromptOptions {
                            files: true,
                            directories: false,
                            multiple: false,
                            prompt: Some("Choose the license key file".into()),
                        });
                        window
                            .spawn(cx, async move |cx| {
                                let Some(path) = picked
                                    .await
                                    .ok()
                                    .and_then(Result::ok)
                                    .flatten()
                                    .and_then(|paths| paths.into_iter().next())
                                else {
                                    return;
                                };
                                workspace
                                    .update_in(cx, |workspace, window, cx| {
                                        let form = ActivateForm::KeyFile(path);
                                        start_job(workspace, LicenseJob::Activate(form), window, cx)
                                    })
                                    .ok();
                            })
                            .detach();
                    }
                    other => {
                        let form = match other {
                            ActivateKind::Account => ActivateForm::Account,
                            ActivateKind::ServerUrl => ActivateForm::ServerUrl,
                            _ => ActivateForm::Code,
                        };
                        window
                            .spawn(cx, async move |cx| {
                                workspace
                                    .update_in(cx, |workspace, window, cx| {
                                        start_job(workspace, LicenseJob::Activate(form), window, cx)
                                    })
                                    .ok();
                            })
                            .detach();
                    }
                }
            });
        let delegate = ActivateDelegate::new(cx.entity().downgrade(), on_choose);
        ActivatePicker::new(delegate, window, cx)
    });
}

/// Installs the bundle if needed, runs the job in a terminal tab, then rechecks the license and
/// restarts with IntelliJ when it is licensed (D15).
fn start_job(
    workspace: &mut Workspace,
    job: LicenseJob,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let target = match Target::of(workspace, cx) {
        Ok(target) => target,
        Err(message) => return notice(workspace, message, cx),
    };
    cx.spawn_in(window, async move |workspace, cx| {
        if let Err(error) = run_job(&workspace, &target, &job, cx).await {
            notice_weak(&workspace, format!("{}: {error:#}", job.label()), cx);
        }
    })
    .detach();
}

async fn run_job(
    workspace: &WeakEntity<Workspace>,
    target: &Target,
    job: &LicenseJob,
    cx: &mut AsyncWindowContext,
) -> Result<()> {
    let bundle = ensure_bundle(workspace, target, cx).await?;

    // The private copy of a key file lives until the terminal task exits.
    let secret = match job.key_file() {
        Some(picked) => {
            let picked = picked.to_path_buf();
            let contents = cx
                .background_spawn(async move {
                    fs::read(&picked).with_context(|| format!("reading {}", picked.display()))
                })
                .await?;
            Some(SecretFile::create(
                &std::env::temp_dir(),
                "license.key",
                &contents,
            )?)
        }
        None => None,
    };
    let args = job.args(secret.as_ref().map(SecretFile::path));
    let task = terminal_task(&bundle.binary, &args, job.label(), &target.root);
    let exit: Option<Result<ExitStatus>> = workspace
        .update_in(cx, |workspace, window, cx| {
            workspace.spawn_in_terminal(task, window, cx)
        })?
        .await;
    drop(secret);
    match exit {
        None => bail!("no terminal is available"),
        Some(Err(error)) => return Err(error.context("running the terminal task")),
        Some(Ok(_)) => {}
    }

    // The task ended, whatever its exit code: ask the server whether it is licensed now.
    let outcome = match status_of(bundle.binary, cx).await {
        Ok(report) => report.outcome,
        Err(error) => {
            log::warn!("license status after {}: {error:#}", job.label());
            LicenseOutcome::Offline
        }
    };
    let worktree_id = target.worktree_id;
    let restarted = target.manager.update(cx, |manager, cx| {
        manager.license_checked(worktree_id, outcome, cx)
    });
    let message = if restarted {
        "IntelliJ is licensed: restarting the Kotlin server with IntelliJ".to_string()
    } else {
        match outcome.failure_reason() {
            Some(reason) => format!("{reason}: the Kotlin server was not changed"),
            None => "IntelliJ is licensed".to_string(),
        }
    };
    notice_weak(workspace, message, cx);
    Ok(())
}

/// `kotlin: review intellij eula`: installs the bundle if it is missing, shows `EULA.txt` in a
/// read-only buffer, and offers "Accept" in a toast.
pub fn review_intellij_eula(
    workspace: &mut Workspace,
    show: ShowText,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let target = match Target::of(workspace, cx) {
        Ok(target) => target,
        Err(message) => return notice(workspace, message, cx),
    };
    cx.spawn_in(window, async move |workspace, cx| {
        let review = async {
            let bundle = ensure_bundle(&workspace, &target, cx).await?;
            let state_dir = target.dirs.state_dir.clone();
            cx.background_spawn(async move { read_eula(&bundle.dir, &state_dir) })
                .await
        }
        .await;
        let review = match review {
            Ok(review) => review,
            Err(error) => {
                notice_weak(&workspace, format!("Cannot show the IntelliJ EULA: {error:#}"), cx);
                return;
            }
        };
        let state_dir = target.dirs.state_dir.clone();
        workspace
            .update_in(cx, move |workspace, window, cx| {
                show(workspace, review.text.clone(), EULA_TITLE, window, cx);
                if review.accepted {
                    let message = format!("IntelliJ EULA {} is already accepted", review.hash);
                    return notice(workspace, message, cx);
                }
                let weak = cx.entity().downgrade();
                let toast = Toast::new(
                    NotificationId::unique::<LicenseNotice>(),
                    "Read the IntelliJ license agreement. bongos starts the IntelliJ server only after you accept it.",
                )
                .on_click("Accept", move |window, cx| {
                    let (review, state_dir, workspace) =
                        (review.clone(), state_dir.clone(), weak.clone());
                    window
                        .spawn(cx, async move |cx| {
                            let hash = review.hash.clone();
                            let result = cx
                                .background_spawn(async move { accept_review(&review, &state_dir) })
                                .await;
                            let message = match result {
                                Ok(()) => format!(
                                    "IntelliJ EULA {hash} accepted. Run kotlin: restart server to start IntelliJ."
                                ),
                                Err(error) => format!("Cannot store the acceptance: {error:#}"),
                            };
                            notice_weak(&workspace, message, cx);
                        })
                        .detach();
                });
                workspace.show_toast(toast, cx);
            })
            .ok();
    })
    .detach();
}

// ---------------------------------------------------------------------------------------------
// The activation picker
// ---------------------------------------------------------------------------------------------

pub(crate) struct ActivatePicker {
    picker: Entity<Picker<ActivateDelegate>>,
}

impl ActivatePicker {
    fn new(delegate: ActivateDelegate, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let picker =
            cx.new(|cx| Picker::uniform_list(delegate, window, cx).initial_width(rems(22.)));
        Self { picker }
    }
}

impl ModalView for ActivatePicker {}

impl EventEmitter<DismissEvent> for ActivatePicker {}

impl Focusable for ActivatePicker {
    fn focus_handle(&self, cx: &App) -> gpui::FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl Render for ActivatePicker {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        v_flex().child(self.picker.clone())
    }
}

pub(crate) struct ActivateDelegate {
    choices: Vec<ActivateKind>,
    /// Indexes into `choices` that match the query.
    matches: Vec<usize>,
    selected_index: usize,
    picker: WeakEntity<ActivatePicker>,
    on_choose: Rc<dyn Fn(ActivateKind, &mut Window, &mut App)>,
}

impl ActivateDelegate {
    fn new(
        picker: WeakEntity<ActivatePicker>,
        on_choose: Rc<dyn Fn(ActivateKind, &mut Window, &mut App)>,
    ) -> Self {
        Self {
            choices: ActivateKind::ALL.to_vec(),
            matches: (0..ActivateKind::ALL.len()).collect(),
            selected_index: 0,
            picker,
            on_choose,
        }
    }
}

impl PickerDelegate for ActivateDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "kotlin license activate"
    }

    fn placeholder_text(&self, _: &mut Window, _: &mut App) -> Arc<str> {
        "Activate the IntelliJ server with...".into()
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(&mut self, ix: usize, _: &mut Window, _: &mut Context<Picker<Self>>) {
        self.selected_index = ix;
    }

    fn update_matches(
        &mut self,
        query: String,
        _: &mut Window,
        _: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let query = query.to_lowercase();
        self.matches = self
            .choices
            .iter()
            .enumerate()
            .filter(|(_, kind)| kind.label().to_lowercase().contains(&query))
            .map(|(index, _)| index)
            .collect();
        self.selected_index = self
            .selected_index
            .min(self.matches.len().saturating_sub(1));
        Task::ready(())
    }

    fn confirm(&mut self, _: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let kind = self
            .matches
            .get(self.selected_index)
            .and_then(|&index| self.choices.get(index))
            .copied();
        self.picker.update(cx, |_, cx| cx.emit(DismissEvent)).ok();
        if let Some(kind) = kind {
            (self.on_choose)(kind, window, cx);
        }
    }

    fn dismissed(&mut self, _: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.picker.update(cx, |_, cx| cx.emit(DismissEvent)).ok();
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _: &mut Window,
        _: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let kind = self.choices.get(*self.matches.get(ix)?)?;
        Some(
            ListItem::new(ix)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .child(HighlightedLabel::new(
                    SharedString::from(kind.label()),
                    Vec::new(),
                )),
        )
    }
}

#[cfg(test)]
mod tests {
    use std::{cell::RefCell, collections::BTreeMap};

    use gpui::{AppContext as _, BorrowAppContext as _, TestAppContext};
    use settings::SettingsStore;

    use super::*;
    use crate::{
        INSTALLED_MARKER, ServerStatus, installed::Marker, manager::KotlinHost,
        manifest::ArchiveKind, resolve::project_override,
    };

    const SECRET: &str = "SECRET-ACTIVATION-KEY-9f3a";

    fn binary(path: &Path) -> LanguageServerBinary {
        LanguageServerBinary {
            path: path.to_path_buf(),
            arguments: vec!["--stdio".into()],
            env: Some(
                BTreeMap::from([("A".to_string(), "b".to_string())])
                    .into_iter()
                    .collect(),
            ),
        }
    }

    // --- command lines ---

    #[test]
    fn test_status_is_refused_unless_intellij_is_requested_or_effective() {
        use KotlinServerKind::{Intellij, KotlinLsp};
        assert_eq!(
            status_gate(KotlinLsp, KotlinLsp),
            Err("IntelliJ server is not active")
        );
        assert_eq!(status_gate(Intellij, KotlinLsp), Ok(()));
        // IntelliJ fell back to kotlin-lsp, or runs without being requested any longer.
        assert_eq!(status_gate(KotlinLsp, Intellij), Ok(()));
        assert_eq!(status_gate(Intellij, Intellij), Ok(()));
        assert_eq!(NOT_ACTIVE_MESSAGE, "IntelliJ server is not active");
    }

    #[test]
    fn test_each_activate_form_builds_its_command_line() {
        let key = PathBuf::from("/home/me/idea.key");
        assert_eq!(
            activate_args(&ActivateForm::Account, None),
            ["license", "login", "--code"]
        );
        // A code or a URL is typed at the CLI's prompt, so no argument follows `activate`.
        assert_eq!(
            activate_args(&ActivateForm::Code, None),
            ["license", "activate"]
        );
        assert_eq!(
            activate_args(&ActivateForm::ServerUrl, None),
            ["license", "activate"]
        );
        assert_eq!(
            activate_args(&ActivateForm::KeyFile(key.clone()), None),
            ["license", "activate", "/home/me/idea.key"]
        );
        // The private copy replaces the picked file.
        assert_eq!(
            activate_args(
                &ActivateForm::KeyFile(key),
                Some(Path::new("/tmp/p/license.key"))
            ),
            ["license", "activate", "/tmp/p/license.key"]
        );
        assert_eq!(trial_args(), ["license", "trial"]);
        assert_eq!(status_args(), ["license", "status"]);
        assert_eq!(
            ActivateKind::ALL.map(ActivateKind::label),
            [
                "JetBrains account",
                "Activation code or key",
                "Key file...",
                "License server URL"
            ]
        );
    }

    #[test]
    fn test_no_secret_reaches_a_command_line() {
        let dir = tempfile::tempdir().unwrap();
        let picked = dir.path().join("idea.key");
        fs::write(&picked, SECRET).unwrap();
        let secret = SecretFile::create(dir.path(), "license.key", SECRET.as_bytes()).unwrap();

        let launcher = Path::new("/opt/it/bin/intellij-server");
        for job in [
            LicenseJob::Activate(ActivateForm::Account),
            LicenseJob::Activate(ActivateForm::Code),
            LicenseJob::Activate(ActivateForm::ServerUrl),
            LicenseJob::Activate(ActivateForm::KeyFile(picked.clone())),
            LicenseJob::Trial,
        ] {
            let args = job.args(job.key_file().map(|_| secret.path()));
            let task = terminal_task(&binary(launcher), &args, job.label(), dir.path());
            let mut everything = vec![task.command.clone().unwrap(), task.command_label.clone()];
            everything.extend(task.args.clone());
            everything.extend(args);
            for text in everything {
                assert!(
                    !text.contains(SECRET),
                    "{job:?} leaks the secret in {text:?}"
                );
            }
        }
        // The key file form hands over the path of the private copy, never the picked file's.
        let job = LicenseJob::Activate(ActivateForm::KeyFile(picked.clone()));
        let args = job.args(Some(secret.path()));
        assert_eq!(args[2], secret.path().to_string_lossy());
        assert_ne!(args[2], picked.to_string_lossy());
    }

    #[test]
    fn test_the_terminal_task_is_one_shot_without_history() {
        let launcher = Path::new("/opt/my it/bin/intellij-server");
        let args = vec![
            "license".to_string(),
            "activate".into(),
            "/tmp/it's/key".into(),
        ];
        let task = terminal_task(
            &binary(launcher),
            &args,
            "kotlin: license activate",
            Path::new("/work"),
        );
        assert_eq!(
            task.command.as_deref(),
            Some("'/opt/my it/bin/intellij-server'")
        );
        assert_eq!(task.args, ["license", "activate", "'/tmp/it'\\''s/key'"]);
        assert_eq!(task.cwd.as_deref(), Some(Path::new("/work")));
        assert!(task.use_new_terminal && task.allow_concurrent_runs);
        assert_eq!(
            task.env.get("HISTFILE").map(String::as_str),
            Some("/dev/null")
        );
        assert_eq!(task.env.get("HISTSIZE").map(String::as_str), Some("0"));
        // The launcher's own environment is kept.
        assert_eq!(task.env.get("A").map(String::as_str), Some("b"));
        assert_eq!(sh_quote("plain-1.0/x"), "plain-1.0/x");
        assert_eq!(sh_quote(""), "''");
        assert_eq!(sh_quote("$(rm -rf ~)"), "'$(rm -rf ~)'");
    }

    // --- the private copy ---

    #[cfg(unix)]
    #[test]
    fn test_the_secret_file_is_private_and_removed() {
        use std::os::unix::fs::PermissionsExt as _;
        let parent = tempfile::tempdir().unwrap();
        let secret = SecretFile::create(parent.path(), "license.key", SECRET.as_bytes()).unwrap();
        let (dir, path) = (secret.dir().to_path_buf(), secret.path().to_path_buf());
        assert_eq!(dir.parent(), Some(parent.path()));
        assert_eq!(
            fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), SECRET);

        drop(secret);
        assert!(!path.exists(), "the file is deleted when the task exits");
        assert!(!dir.exists(), "so is its directory");
    }

    // --- status ---

    #[test]
    fn test_status_text_shows_the_output_and_the_verdict() {
        let text = status_text(
            LicenseOutcome::Unlicensed,
            Some(2),
            "No valid license found\n",
            "Account: not signed in\n",
        );
        assert_eq!(
            text,
            "No valid license found\nAccount: not signed in\n\nResult: not licensed\nExit code: 2\n"
        );
        let text = status_text(LicenseOutcome::Licensed, Some(0), "", "");
        assert!(text.starts_with("(license status printed nothing)"));
        assert!(text.contains("Result: licensed"));
        assert!(status_text(LicenseOutcome::TimedOut, None, "x", "").contains("timed out"));
    }

    #[cfg(unix)]
    #[test]
    fn test_license_report_runs_the_installed_launcher() {
        use futures::executor::block_on;
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("launcher");
        fs::write(
            &path,
            "#!/bin/sh\n[ \"$1 $2\" = \"license status\" ] || exit 99\necho \"License: $FAKE_LICENSE\"\nexit \"${FAKE_EXIT:-0}\"\n",
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        let with = |license: &str, exit: &str| LanguageServerBinary {
            path: path.clone(),
            arguments: Vec::new(),
            env: Some(
                [
                    ("FAKE_LICENSE".to_string(), license.to_string()),
                    ("FAKE_EXIT".to_string(), exit.to_string()),
                ]
                .into_iter()
                .collect(),
            ),
        };
        let report = block_on(license_report(
            &with("licensed", "0"),
            std::future::pending(),
        ))
        .unwrap();
        assert_eq!(report.outcome, LicenseOutcome::Licensed);
        assert!(report.text.contains("License: licensed"));
        let report = block_on(license_report(&with("none", "2"), std::future::pending())).unwrap();
        assert_eq!(report.outcome, LicenseOutcome::Unlicensed);
        assert!(report.text.contains("Exit code: 2"));
        let report = block_on(license_report(&with("?", "3"), std::future::pending())).unwrap();
        assert_eq!(report.outcome, LicenseOutcome::Offline);
        let missing = LanguageServerBinary {
            path: dir.path().join("missing"),
            arguments: Vec::new(),
            env: None,
        };
        assert!(block_on(license_report(&missing, std::future::pending())).is_err());
    }

    // --- bundle and EULA ---

    fn entry() -> ManifestEntry {
        ManifestEntry {
            version: "1.0".into(),
            url: "http://127.0.0.1/unused".into(),
            sha256: "ae0c4b01e17ab73a4a5227a78ea99e6dcbe3e499600315b22759a4e4cddd11df".into(),
            archive: ArchiveKind::TarGz,
            strip_components: 0,
            launcher: "bin/intellij-server".into(),
            runtime: None,
            args: vec!["--stdio".into()],
            env: BTreeMap::new(),
            built: None,
            expires: None,
        }
    }

    fn dirs(root: &Path) -> KotlinDirs {
        KotlinDirs {
            state_dir: root.join("state"),
            logs_dir: root.join("logs"),
            install_root: root.join("languages"),
        }
    }

    /// Lays out an installed IntelliJ bundle with `eula` as its EULA.txt.
    fn install_bundle(dirs: &KotlinDirs, entry: &ManifestEntry, eula: &str) -> PathBuf {
        let bundle = version_dir(
            &dirs.install_root,
            KotlinServerKind::Intellij,
            &entry.version,
        );
        let launcher = bundle.join(&entry.launcher);
        fs::create_dir_all(launcher.parent().unwrap()).unwrap();
        fs::write(&launcher, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            fs::set_permissions(&launcher, fs::Permissions::from_mode(0o755)).unwrap();
        }
        let marker = Marker {
            version: entry.version.clone(),
            sha256: entry.sha256.clone(),
        };
        fs::write(
            bundle.join(INSTALLED_MARKER),
            serde_json::to_string(&marker).unwrap(),
        )
        .unwrap();
        fs::write(bundle.join(EULA_FILE), eula).unwrap();
        bundle
    }

    #[test]
    fn test_a_missing_bundle_installs_only_when_the_setting_allows_it() {
        let root = tempfile::tempdir().unwrap();
        let (dirs, entry) = (dirs(root.path()), entry());
        assert!(matches!(
            plan_bundle(&dirs, &entry, false),
            BundlePlan::NeedsSetting
        ));
        assert!(matches!(
            plan_bundle(&dirs, &entry, true),
            BundlePlan::Install
        ));
        let bundle = install_bundle(&dirs, &entry, "agreement");
        // An installed bundle needs no setting and no network.
        for allowed in [false, true] {
            match plan_bundle(&dirs, &entry, allowed) {
                BundlePlan::Ready(ready) => {
                    assert_eq!(ready.dir, bundle);
                    assert_eq!(ready.binary.path, bundle.join("bin/intellij-server"));
                }
                _ => panic!("an installed bundle is ready"),
            }
        }
    }

    #[test]
    fn test_progress_message() {
        assert_eq!(
            progress_message(52 * 1024 * 1024, Some(137 * 1024 * 1024)),
            "Downloading the IntelliJ server: 38% (52/137 MB)"
        );
        assert_eq!(
            progress_message(7 * 1024 * 1024, None),
            "Downloading the IntelliJ server: 7 MB"
        );
    }

    #[test]
    fn test_the_eula_is_shown_by_hash_and_acceptance_is_stored_per_hash() {
        let root = tempfile::tempdir().unwrap();
        let (dirs, entry) = (dirs(root.path()), entry());
        let bundle = install_bundle(&dirs, &entry, "JETBRAINS USER AGREEMENT v1");

        let review = read_eula(&bundle, &dirs.state_dir).unwrap();
        assert_eq!(review.text, "JETBRAINS USER AGREEMENT v1");
        assert_eq!(review.hash, eula_hash(b"JETBRAINS USER AGREEMENT v1"));
        assert!(
            !review.accepted,
            "nothing is accepted before the user's act"
        );
        // Reading never accepts.
        assert!(!read_eula(&bundle, &dirs.state_dir).unwrap().accepted);

        accept_review(&review, &dirs.state_dir).unwrap();
        assert!(read_eula(&bundle, &dirs.state_dir).unwrap().accepted);
        let stored = fs::read_to_string(dirs.state_dir.join("kotlin-eula.json")).unwrap();
        assert!(stored.contains(&review.hash), "{stored}");

        // A new bundle with a changed agreement asks again.
        fs::write(bundle.join(EULA_FILE), "JETBRAINS USER AGREEMENT v2").unwrap();
        let changed = read_eula(&bundle, &dirs.state_dir).unwrap();
        assert_ne!(changed.hash, review.hash);
        assert!(!changed.accepted);
        // Without the file there is nothing to accept.
        fs::remove_file(bundle.join(EULA_FILE)).unwrap();
        assert!(read_eula(&bundle, &dirs.state_dir).is_err());
    }

    // --- the recheck ---

    #[test]
    fn test_only_a_licensed_status_restarts() {
        assert!(restarts_after(LicenseOutcome::Licensed));
        for outcome in [
            LicenseOutcome::Unlicensed,
            LicenseOutcome::Offline,
            LicenseOutcome::TimedOut,
        ] {
            assert!(!restarts_after(outcome), "{outcome:?}");
        }
    }

    #[derive(Clone, Default)]
    struct RecordingHost(Rc<RefCell<Vec<String>>>);

    impl KotlinHost for RecordingHost {
        fn restart(&self, _: WorktreeId, _: Option<lsp::LanguageServerId>, _: &mut App) {
            self.0.borrow_mut().push("restart".into());
        }
    }

    struct Fixture {
        manager: Entity<KotlinProjectManager>,
        host: RecordingHost,
        dirs: KotlinDirs,
        root: PathBuf,
        _temp: tempfile::TempDir,
    }

    fn fixture(cx: &mut TestAppContext, user_settings: &str) -> Fixture {
        cx.update(|cx| {
            let store = SettingsStore::new(cx, &settings::default_settings());
            cx.set_global(store);
            SettingsStore::observe_active_settings_profile_name(cx).detach();
            KotlinSettings::register(cx);
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store.set_user_settings(user_settings, cx).unwrap();
            });
        });
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("project");
        fs::create_dir_all(&root).unwrap();
        let dirs = dirs(temp.path());
        let host = RecordingHost::default();
        let manager =
            cx.new(|cx| KotlinProjectManager::new(Rc::new(host.clone()), dirs.clone(), cx));
        manager.update(cx, |manager, cx| {
            manager.add_worktree(WorktreeId::from_usize(1), root.clone(), cx);
            // A server has started, so a change of server restarts it.
            manager.begin_start(WorktreeId::from_usize(1), cx);
        });
        Fixture {
            manager,
            host,
            dirs,
            root,
            _temp: temp,
        }
    }

    fn recheck(fixture: &Fixture, outcome: LicenseOutcome, cx: &mut TestAppContext) -> bool {
        fixture.manager.update(cx, |manager, cx| {
            manager.license_checked(WorktreeId::from_usize(1), outcome, cx)
        })
    }

    #[gpui::test]
    fn test_a_licensed_recheck_restarts_with_intellij(cx: &mut TestAppContext) {
        let fixture = fixture(cx, "{}");
        assert!(recheck(&fixture, LicenseOutcome::Licensed, cx));
        assert_eq!(
            project_override(&fixture.dirs.state_dir, &fixture.root).unwrap(),
            Some(KotlinServerKind::Intellij)
        );
        assert_eq!(*fixture.host.0.borrow(), ["restart"]);
        let effective = fixture.manager.read_with(cx, |manager, _| {
            manager
                .worktree(WorktreeId::from_usize(1))
                .unwrap()
                .effective
        });
        assert_eq!(effective, KotlinServerKind::Intellij);
        assert_eq!(
            fixture.manager.read_with(cx, |manager, _| manager
                .status(WorktreeId::from_usize(1))
                .cloned()),
            Some(ServerStatus::Starting)
        );
    }

    #[gpui::test]
    fn test_an_intellij_setting_restarts_without_an_override(cx: &mut TestAppContext) {
        let fixture = fixture(cx, r#"{ "kotlin": { "server": "intellij" } }"#);
        assert!(recheck(&fixture, LicenseOutcome::Licensed, cx));
        assert_eq!(*fixture.host.0.borrow(), ["restart"]);
        assert_eq!(
            project_override(&fixture.dirs.state_dir, &fixture.root).unwrap(),
            None
        );
    }

    #[gpui::test]
    fn test_an_unlicensed_recheck_changes_nothing(cx: &mut TestAppContext) {
        for outcome in [
            LicenseOutcome::Unlicensed,
            LicenseOutcome::Offline,
            LicenseOutcome::TimedOut,
        ] {
            let fixture = fixture(cx, "{}");
            assert!(!recheck(&fixture, outcome, cx), "{outcome:?}");
            assert!(fixture.host.0.borrow().is_empty(), "{outcome:?}");
            assert_eq!(
                project_override(&fixture.dirs.state_dir, &fixture.root).unwrap(),
                None,
                "{outcome:?}"
            );
        }
    }
}
