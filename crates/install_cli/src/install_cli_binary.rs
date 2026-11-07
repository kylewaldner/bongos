use super::register_zed_scheme;
use anyhow::Result;
use gpui::{AppContext as _, AsyncApp, Context, PromptLevel, Window};
use release_channel::ReleaseChannel;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use util::ResultExt;
use workspace::notifications::simple_message_notification::MessageNotification;
use workspace::notifications::{DetachAndPromptErr, NotificationId};
use workspace::{Toast, Workspace};

/// Attempts to install the CLI symlink. Returns the installed path on success,
/// or `None` if the user dismissed the macOS administrator authentication
/// prompt. Returns an error if the install could not be completed, most
/// commonly because the user is not an admin.
async fn install_script(cx: &AsyncApp) -> Result<Option<PathBuf>> {
    let cli_path = cx.update(|cx| cx.path_for_auxiliary_executable("cli"))?;
    let link_path = Path::new("/usr/local/bin/bongos");
    let bin_dir_path = link_path.parent().unwrap();

    // Don't re-create symlink if it points to the same CLI binary.
    if smol::fs::read_link(link_path).await.ok().as_ref() == Some(&cli_path) {
        return Ok(Some(link_path.into()));
    }

    // If the symlink is not there or is outdated, first try replacing it
    // without escalating.
    smol::fs::remove_file(link_path).await.log_err();
    if smol::fs::unix::symlink(&cli_path, link_path)
        .await
        .log_err()
        .is_some()
    {
        return Ok(Some(link_path.into()));
    }

    // The symlink could not be created without escalating, so use osascript
    // with admin privileges to create it.
    let output = smol::process::Command::new("/usr/bin/osascript")
        .args([
            "-e",
            &format!(
                "do shell script \" \
                    mkdir -p \'{}\' && \
                    ln -sf \'{}\' \'{}\' \
                \" with administrator privileges",
                bin_dir_path.to_string_lossy(),
                cli_path.to_string_lossy(),
                link_path.to_string_lossy(),
            ),
        ])
        .output()
        .await?;

    if output.status.success() {
        return Ok(Some(link_path.into()));
    }

    // osascript reports "User canceled." (error -128) when the administrator
    // prompt is dismissed. Treat that as a cancellation rather than a failure
    // so we don't show an error the user already chose to avoid.
    let stderr = String::from_utf8_lossy(&output.stderr);
    if stderr.contains("User canceled") || stderr.contains("-128") {
        return Ok(None);
    }

    // The privileged write failed, most commonly because the user is not an
    // admin.
    anyhow::bail!("error running osascript: {}", stderr.trim());
}

/// Where the bundled CLI sits relative to the directory of the running editor binary, in the order they are tried.
///
/// `../bin/bongos` is the installed layout (CLI in `bin`, editor in `libexec`), and `./cli` is a development build, where
/// both binaries share the target directory.
const LINUX_CLI_LOCATIONS: [&str; 3] = ["../bin/bongos", "../bin/cli", "./cli"];

/// Finds the bundled CLI next to the running editor binary `editor_exe`.
///
/// gpui does not implement `path_for_auxiliary_executable` on Linux, so the path is derived from the editor binary's own
/// location. Returns `None` when no CLI exists at a known location, or when the only match is the editor itself.
fn find_linux_cli(editor_exe: &Path) -> Option<PathBuf> {
    let directory = editor_exe.parent()?;
    let editor_exe = editor_exe.canonicalize().ok()?;
    LINUX_CLI_LOCATIONS.iter().find_map(|location| {
        directory
            .join(location)
            .canonicalize()
            .ok()
            .filter(|path| path.is_file() && *path != editor_exe)
    })
}

/// Builds the Linux prompt text that tells the user where the bundled CLI is and how to put it on their PATH.
fn linux_prompt_detail(cli_path: Option<&Path>) -> String {
    let Some(cli_path) = cli_path else {
        return "The bongos CLI ships with the bongos bundle, but its location could not be determined.\n\nAdd the bundle's bin directory to your PATH, or symlink the CLI as `bongos`.".to_string();
    };
    let directory = cli_path.parent().unwrap_or(cli_path);
    format!(
        "The bongos CLI is bundled at {path}.\n\nAdd {directory} to your PATH, or symlink it as `bongos`, for example:\n\nln -s {path} ~/.local/bin/bongos",
        path = cli_path.display(),
        directory = directory.display(),
    )
}

/// Installs the CLI as `bongos`. Called by the `bongos::InstallCli` action.
///
/// On macOS this symlinks the bundled CLI into `/usr/local/bin`, asking for administrator rights if needed. On Linux
/// nothing is installed and the prompt reports where the bundled CLI is.
pub fn install_cli_binary(window: &mut Window, cx: &mut Context<Workspace>) {
    cx.spawn_in(window, async move |workspace, cx| {
        if cfg!(any(target_os = "linux", target_os = "freebsd")) {
            let cli_path = std::env::current_exe()
                .ok()
                .and_then(|editor_exe| find_linux_cli(&editor_exe));
            let detail = linux_prompt_detail(cli_path.as_deref());
            let prompt = cx.prompt(
                PromptLevel::Warning,
                "The bongos CLI is bundled with the app",
                Some(&detail),
                &["OK"],
            );
            cx.background_spawn(prompt).detach();
            return Ok(());
        }
        let path = match install_script(cx.deref()).await {
            Ok(Some(path)) => path,
            // The user dismissed the administrator prompt; nothing to do.
            Ok(None) => return Ok(()),
            Err(error) => {
                log::error!("failed to install bongos CLI: {error:#}");
                workspace.update(cx, |workspace, cx| {
                    struct CliInstallFailed;

                    workspace.show_notification(
                        NotificationId::unique::<CliInstallFailed>(),
                        cx,
                        |cx| {
                            cx.new(|cx| {
                                MessageNotification::new(
                                    "You can add `bongos` to your PATH manually.",
                                    cx,
                                )
                                .with_title("Couldn't install the bongos CLI")
                            })
                        },
                    );
                })?;
                return Ok(());
            }
        };

        workspace.update_in(cx, |workspace, _, cx| {
            struct InstalledBongosCli;

            workspace.show_toast(
                Toast::new(
                    NotificationId::unique::<InstalledBongosCli>(),
                    format!(
                        "Installed `bongos` to {}. You can launch {} from your terminal.",
                        path.to_string_lossy(),
                        ReleaseChannel::global(cx).display_name()
                    ),
                ),
                cx,
            )
        })?;
        register_zed_scheme(cx).await.log_err();
        Ok(())
    })
    .detach_and_prompt_err("Cannot install the bongos CLI", window, cx, |_, _, _| None);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn linux_prompt_detail_reports_the_cli_path() {
        let detail = linux_prompt_detail(Some(Path::new("/opt/bongos/bin/bongos")));

        assert!(detail.contains("/opt/bongos/bin/bongos"));
        assert!(detail.contains("/opt/bongos/bin"));
        assert!(detail.contains("`bongos`"));
        assert!(!detail.to_lowercase().contains("zed"));
    }

    #[test]
    fn linux_prompt_detail_without_a_path_is_a_fixed_message() {
        let detail = linux_prompt_detail(None);

        assert!(detail.contains("PATH"));
        assert!(!detail.to_lowercase().contains("zed"));
    }

    fn touch(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"").unwrap();
    }

    #[test]
    fn finds_the_cli_in_an_installed_layout() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let editor = root.join("libexec/bongos-editor");
        touch(&editor);
        touch(&root.join("bin/bongos"));

        assert_eq!(find_linux_cli(&editor), Some(root.join("bin/bongos")));
    }

    #[test]
    fn finds_the_cli_next_to_a_development_build() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let editor = root.join("target/debug/bongos");
        touch(&editor);
        touch(&root.join("target/debug/cli"));

        assert_eq!(find_linux_cli(&editor), Some(root.join("target/debug/cli")));
    }

    #[test]
    fn does_not_mistake_the_editor_for_the_cli() {
        let root = tempfile::tempdir().unwrap();
        let editor = root.path().join("bin/bongos");
        touch(&editor);

        assert_eq!(find_linux_cli(&editor), None);
    }

    #[test]
    fn finds_nothing_when_no_cli_exists() {
        let root = tempfile::tempdir().unwrap();
        let editor = root.path().join("libexec/bongos-editor");
        touch(&editor);

        assert_eq!(find_linux_cli(&editor), None);
    }
}
