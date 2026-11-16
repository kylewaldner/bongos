use std::ffi::OsString;
use std::path::{Path, PathBuf};

use bongos_extensions_model::{Layout, RevSpec, SourceUrl};
use tempfile::TempDir;
use util::command::{Stdio, new_command};

use super::*;

/// A scrubbed git environment: a temp home, no global or system config and a fixed identity.
struct Sandbox {
    temp: TempDir,
    env: Vec<(OsString, OsString)>,
}

impl Sandbox {
    fn new() -> Self {
        let temp = TempDir::new().unwrap();
        let home = temp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let env = [
            ("HOME", home.as_os_str().to_owned()),
            ("XDG_CONFIG_HOME", home.join(".config").into_os_string()),
            ("GIT_CONFIG_GLOBAL", OsString::from("/dev/null")),
            ("GIT_CONFIG_NOSYSTEM", OsString::from("1")),
            ("GIT_AUTHOR_NAME", OsString::from("Test")),
            ("GIT_AUTHOR_EMAIL", OsString::from("test@example.invalid")),
            ("GIT_COMMITTER_NAME", OsString::from("Test")),
            (
                "GIT_COMMITTER_EMAIL",
                OsString::from("test@example.invalid"),
            ),
            ("GIT_AUTHOR_DATE", OsString::from("1700000000 +0000")),
            ("GIT_COMMITTER_DATE", OsString::from("1700000000 +0000")),
        ]
        .into_iter()
        .map(|(k, v)| (OsString::from(k), v))
        .collect();
        Self { temp, env }
    }

    fn runner(&self) -> RealGit {
        RealGit::with_env(self.env.clone())
    }

    fn layout(&self) -> Layout {
        Layout::new(self.temp.path().join("extensions"))
    }

    async fn git(&self, dir: &Path, args: &[&str]) -> String {
        let mut command = new_command("git");
        for key in ["GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE"] {
            command.env_remove(key);
        }
        for (key, value) in &self.env {
            command.env(key, value);
        }
        let output = command
            .current_dir(dir)
            .args(args)
            .stdin(Stdio::null())
            .output()
            .await
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    async fn commit(&self, repo: &Path, file: &str) -> String {
        std::fs::write(repo.join(file), file).unwrap();
        self.git(repo, &["add", file]).await;
        self.git(repo, &["commit", "--quiet", "-m", file]).await;
        self.git(repo, &["rev-parse", "HEAD"]).await
    }

    /// A source repository on branch `main` with one commit; returns its path and URL.
    async fn source(&self) -> (PathBuf, SourceUrl) {
        let repo = self.temp.path().join("source");
        std::fs::create_dir_all(&repo).unwrap();
        self.git(&repo, &["init", "--quiet", "-b", "main"]).await;
        let url = SourceUrl::parse(&format!("file://{}", repo.display())).unwrap();
        (repo, url)
    }
}

fn name(n: &str) -> RevSpec {
    RevSpec::Name(n.to_string())
}

#[test]
fn real_resolves_a_lightweight_tag() {
    smol::block_on(async {
        let sandbox = Sandbox::new();
        let (repo, url) = sandbox.source().await;
        let tagged = sandbox.commit(&repo, "a").await;
        sandbox.git(&repo, &["tag", "v1"]).await;
        sandbox.commit(&repo, "b").await;
        let commit = resolve(&sandbox.runner(), &url, &name("v1")).await.unwrap();
        assert_eq!(commit, tagged);
    });
}

#[test]
fn real_resolves_an_annotated_tag_to_its_commit() {
    smol::block_on(async {
        let sandbox = Sandbox::new();
        let (repo, url) = sandbox.source().await;
        let tagged = sandbox.commit(&repo, "a").await;
        sandbox
            .git(&repo, &["tag", "-a", "v2", "-m", "release"])
            .await;
        let tag_object = sandbox.git(&repo, &["rev-parse", "v2"]).await;
        assert_ne!(tag_object, tagged);
        let commit = resolve(&sandbox.runner(), &url, &name("v2")).await.unwrap();
        assert_eq!(commit, tagged);
    });
}

#[test]
fn real_resolves_a_branch() {
    smol::block_on(async {
        let sandbox = Sandbox::new();
        let (repo, url) = sandbox.source().await;
        sandbox.commit(&repo, "a").await;
        sandbox
            .git(&repo, &["checkout", "--quiet", "-b", "dev"])
            .await;
        let dev = sandbox.commit(&repo, "b").await;
        sandbox.git(&repo, &["checkout", "--quiet", "main"]).await;
        let commit = resolve(&sandbox.runner(), &url, &name("dev"))
            .await
            .unwrap();
        assert_eq!(commit, dev);
    });
}

#[test]
fn real_resolves_the_default_branch() {
    smol::block_on(async {
        let sandbox = Sandbox::new();
        let (repo, url) = sandbox.source().await;
        sandbox.commit(&repo, "a").await;
        let main = sandbox.commit(&repo, "b").await;
        sandbox.git(&repo, &["branch", "other", "HEAD~1"]).await;
        let commit = resolve(&sandbox.runner(), &url, &RevSpec::Default)
            .await
            .unwrap();
        assert_eq!(commit, main);
    });
}

#[test]
fn real_missing_ref_fails_naming_it_and_the_url() {
    smol::block_on(async {
        let sandbox = Sandbox::new();
        let (repo, url) = sandbox.source().await;
        sandbox.commit(&repo, "a").await;
        let err = resolve(&sandbox.runner(), &url, &name("nope"))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("`nope`"), "{err}");
        assert!(err.contains(url.as_str()), "{err}");
    });
}

#[test]
fn real_stages_a_commit_not_on_any_ref() {
    smol::block_on(async {
        let sandbox = Sandbox::new();
        let (repo, url) = sandbox.source().await;
        sandbox.commit(&repo, "a").await;
        let hidden = sandbox.commit(&repo, "b").await;
        sandbox
            .git(&repo, &["reset", "--quiet", "--hard", "HEAD~1"])
            .await;
        sandbox
            .git(&repo, &["config", "uploadpack.allowAnySHA1InWant", "true"])
            .await;
        let refs = sandbox.runner().ls_remote(&url).await.unwrap();
        assert!(refs.iter().all(|r| r.oid != hidden));

        let runner = sandbox.runner();
        let layout = sandbox.layout();
        let staged = stage(&runner, &layout, &url, &hidden, None).await.unwrap();
        assert_eq!(staged.commit(), hidden);
        assert_eq!(runner.head(staged.dir()).await.unwrap(), hidden);
        assert!(staged.dir().join("b").is_file());
        assert_eq!(runner.remote_url(staged.dir()).await.unwrap(), url.as_str());

        let copy = sandbox.temp.path().join("copy");
        runner.clone_local(staged.dir(), &copy).await.unwrap();
        assert_eq!(runner.head(&copy).await.unwrap(), hidden);
    });
}

#[test]
fn real_dropped_stage_leaves_no_folder() {
    smol::block_on(async {
        let sandbox = Sandbox::new();
        let (repo, url) = sandbox.source().await;
        let commit = sandbox.commit(&repo, "a").await;
        let layout = sandbox.layout();
        let staged = stage(&sandbox.runner(), &layout, &url, &commit, None)
            .await
            .unwrap();
        let dir = staged.dir().to_path_buf();
        assert!(dir.join("a").is_file());
        drop(staged);
        assert!(!dir.exists());
        assert_eq!(std::fs::read_dir(layout.staging_dir()).unwrap().count(), 0);

        let kept = stage(&sandbox.runner(), &layout, &url, &commit, None)
            .await
            .unwrap()
            .keep();
        assert!(kept.join("a").is_file());
    });
}

#[test]
fn real_stage_refuses_a_symlinked_subdirectory_leaving_the_checkout() {
    smol::block_on(async {
        let sandbox = Sandbox::new();
        let (repo, url) = sandbox.source().await;
        std::fs::create_dir_all(repo.join("ext")).unwrap();
        std::fs::write(repo.join("ext/extension.toml"), "x").unwrap();
        std::os::unix::fs::symlink("../..", repo.join("escape")).unwrap();
        sandbox.git(&repo, &["add", "ext", "escape"]).await;
        sandbox.git(&repo, &["commit", "--quiet", "-m", "x"]).await;
        let commit = sandbox.git(&repo, &["rev-parse", "HEAD"]).await;
        let runner = sandbox.runner();
        let layout = sandbox.layout();

        let staged = stage(&runner, &layout, &url, &commit, Some("ext"))
            .await
            .unwrap();
        assert_eq!(staged.root(), staged.dir().join("ext"));
        assert!(staged.root().join("extension.toml").is_file());
        drop(staged);

        for path in ["escape", "escape/staging"] {
            let err = stage(&runner, &layout, &url, &commit, Some(path))
                .await
                .unwrap_err()
                .to_string();
            assert!(err.contains(url.as_str()), "{err}");
            assert!(err.contains(path), "{err}");
            assert_eq!(std::fs::read_dir(layout.staging_dir()).unwrap().count(), 0);
        }
    });
}

#[test]
fn real_stages_a_reachable_commit_the_server_will_not_serve_directly() {
    smol::block_on(async {
        let sandbox = Sandbox::new();
        let (repo, url) = sandbox.source().await;
        sandbox.commit(&repo, "a").await;
        sandbox
            .git(&repo, &["checkout", "--quiet", "-b", "dev"])
            .await;
        let wanted = sandbox.commit(&repo, "b").await;
        sandbox.commit(&repo, "c").await;
        sandbox.git(&repo, &["checkout", "--quiet", "main"]).await;
        for key in [
            "uploadpack.allowAnySHA1InWant",
            "uploadpack.allowReachableSHA1InWant",
            "uploadpack.allowTipSHA1InWant",
        ] {
            sandbox.git(&repo, &["config", key, "false"]).await;
        }

        // Protocol v0 enforces the want checks, so the shallow fetch is refused and the full
        // fetch fallback has to find the commit.
        let mut env = sandbox.env.clone();
        env.extend(
            [
                ("GIT_CONFIG_COUNT", "1"),
                ("GIT_CONFIG_KEY_0", "protocol.version"),
                ("GIT_CONFIG_VALUE_0", "0"),
            ]
            .map(|(k, v)| (OsString::from(k), OsString::from(v))),
        );
        let runner = RealGit::with_env(env);
        let layout = sandbox.layout();
        let staged = stage(&runner, &layout, &url, &wanted, None).await.unwrap();
        assert_eq!(runner.head(staged.dir()).await.unwrap(), wanted);
        assert!(staged.dir().join("b").is_file());
        assert!(!staged.dir().join("c").exists());
        // Only the fallback leaves a complete, not shallow, history.
        assert!(!staged.dir().join(".git/shallow").exists());
    });
}

const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
const TAG_OBJECT: &str = "1111111111111111111111111111111111111111";
const BRANCH: &str = "2222222222222222222222222222222222222222";

fn fake_url() -> SourceUrl {
    SourceUrl::parse("https://example.invalid/ext.git").unwrap()
}

#[test]
fn fake_records_the_prompt_free_env() {
    smol::block_on(async {
        let fake = FakeGit::new();
        let url = fake_url();
        fake.set_refs(&url, &[("HEAD", COMMIT)]);
        resolve(&fake, &url, &RevSpec::Default).await.unwrap();
        let envs = fake.recorded_envs();
        assert_eq!(envs.len(), 1);
        assert!(envs[0].set.contains(&("GIT_TERMINAL_PROMPT", "0")));
        assert!(envs[0].remove.contains(&"GIT_ASKPASS"));
        assert_eq!(envs[0], git_env());
        let user_chose_ssh =
            std::env::var_os("GIT_SSH_COMMAND").is_some() || std::env::var_os("GIT_SSH").is_some();
        assert_eq!(
            envs[0]
                .set
                .contains(&("GIT_SSH_COMMAND", BATCH_SSH_COMMAND)),
            !user_chose_ssh
        );
    });
}

#[test]
fn env_policy_runs_ssh_in_batch_mode_unless_the_user_chose_ssh() {
    let batch = ("GIT_SSH_COMMAND", "ssh -o BatchMode=yes");
    assert!(git_env_for(|_| false).set.contains(&batch));
    assert!(
        !git_env_for(|key| key == "GIT_SSH_COMMAND")
            .set
            .iter()
            .any(|(key, _)| *key == "GIT_SSH_COMMAND")
    );
    assert!(!git_env_for(|key| key == "GIT_SSH").set.contains(&batch));
}

#[test]
fn env_policy_scrubs_repository_selecting_variables() {
    let env = git_env_for(|_| false);
    for key in [
        "GIT_ASKPASS",
        "SSH_ASKPASS",
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_COMMON_DIR",
        "GIT_NAMESPACE",
        "GIT_CONFIG_PARAMETERS",
        "GIT_CONFIG_COUNT",
    ] {
        assert!(env.remove.contains(&key), "{key}");
    }
    assert!(env.remove_prefixes.contains(&"GIT_CONFIG_KEY_"));
    assert!(env.remove_prefixes.contains(&"GIT_CONFIG_VALUE_"));
}

#[test]
fn fake_tag_wins_over_branch_and_peeled_wins() {
    smol::block_on(async {
        let fake = FakeGit::new();
        let url = fake_url();
        fake.set_refs(
            &url,
            &[
                ("refs/heads/v1", BRANCH),
                ("refs/tags/v1", TAG_OBJECT),
                ("refs/tags/v1^{}", COMMIT),
            ],
        );
        assert_eq!(resolve(&fake, &url, &name("v1")).await.unwrap(), COMMIT);
    });
}

#[test]
fn fake_commit_rev_does_no_network() {
    smol::block_on(async {
        let fake = FakeGit::new();
        let commit = resolve(&fake, &fake_url(), &RevSpec::Commit(COMMIT.to_string()))
            .await
            .unwrap();
        assert_eq!(commit, COMMIT);
        assert_eq!(fake.network_ops(), 0);
    });
}

#[test]
fn fake_ls_remote_failure_names_url_and_message() {
    smol::block_on(async {
        let fake = FakeGit::new();
        let url = fake_url();
        fake.fail_next_ls_remote(&url, "fatal: could not read Username");
        let err = resolve(&fake, &url, &RevSpec::Default)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains(url.as_str()), "{err}");
        assert!(err.contains("could not read Username"), "{err}");
    });
}

#[test]
fn fake_first_use_creates_git_and_staging() {
    smol::block_on(async {
        let temp = TempDir::new().unwrap();
        let layout = Layout::new(temp.path().join("extensions"));
        assert!(!layout.git_dir().exists());
        let fake = FakeGit::new();
        let staged = stage(&fake, &layout, &fake_url(), COMMIT, None)
            .await
            .unwrap();
        assert!(layout.staging_dir().is_dir());
        assert!(staged.dir().starts_with(layout.staging_dir()));
        assert_eq!(staged.root(), staged.dir());
    });
}

#[test]
fn fake_two_stages_of_one_url_get_distinct_folders() {
    smol::block_on(async {
        let temp = TempDir::new().unwrap();
        let layout = Layout::new(temp.path().join("extensions"));
        let fake = FakeGit::new();
        let first = stage(&fake, &layout, &fake_url(), COMMIT, None)
            .await
            .unwrap();
        let second = stage(&fake, &layout, &fake_url(), COMMIT, None)
            .await
            .unwrap();
        assert_ne!(first.dir(), second.dir());
        assert!(first.dir().is_dir() && second.dir().is_dir());
    });
}

#[test]
fn fake_fetch_failure_deletes_staging_and_names_url() {
    smol::block_on(async {
        let temp = TempDir::new().unwrap();
        let layout = Layout::new(temp.path().join("extensions"));
        let fake = FakeGit::new();
        let url = fake_url();
        fake.fail_next_fetch(&url, "fatal: unable to access: Connection reset");
        let err = stage(&fake, &layout, &url, COMMIT, None)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains(url.as_str()), "{err}");
        assert!(err.contains("Connection reset"), "{err}");
        assert_eq!(std::fs::read_dir(layout.staging_dir()).unwrap().count(), 0);

        // The failure was transient: the next attempt succeeds.
        let staged = stage(&fake, &layout, &url, COMMIT, None).await.unwrap();
        assert!(staged.dir().is_dir());
        assert_eq!(fake.network_ops(), 2);
    });
}

#[test]
fn fake_stage_refuses_a_path_outside_the_checkout() {
    smol::block_on(async {
        let temp = TempDir::new().unwrap();
        let layout = Layout::new(temp.path().join("extensions"));
        let fake = FakeGit::new();
        assert!(
            stage(&fake, &layout, &fake_url(), COMMIT, Some("../up"))
                .await
                .is_err()
        );
        assert_eq!(fake.network_ops(), 0);
    });
}

#[test]
fn parses_ls_remote_with_symref() {
    let refs = parse_ls_remote(&format!(
        "ref: refs/heads/main\tHEAD\n{COMMIT}\tHEAD\n{BRANCH}\trefs/heads/main\n"
    ));
    assert_eq!(refs.len(), 2);
    assert_eq!(refs[0].name, "HEAD");
    assert_eq!(refs[1].oid, BRANCH);
}
