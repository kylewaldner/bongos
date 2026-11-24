//! The Slack connection: checking a pasted token, storing it, and the two files beside it.
//!
//! The token lives only in the credential store, under [`CREDENTIAL_URL`] and
//! [`CREDENTIAL_USER`]. `connection.toml` and `allowed.toml`, in the Slack directory of the data
//! directory, hold no secret. A connect runs `auth.test` once with the pasted token, checks the
//! scopes against [`SCOPES`], then writes the secret, then `connection.toml`; any failure before
//! the secret is written stores nothing, and a failure of the store writes no file. A disconnect
//! deletes the secret first (a failure stops there and the connection stays), then
//! `allowed.toml`, then `connection.toml`. The state at startup comes from `connection.toml`
//! alone, so it never touches the credential store: a secret without the file is not connected,
//! and the next connect overwrites it. There is no environment-variable fallback.
//!
//! Every message built from an error passes through [`SlackToken::redact`] or [`redact`].

use std::fmt;
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use bongos_slack_model::scopes::{SCOPES, missing_scopes};
use bongos_slack_model::{ApiError, AuthInfo, SlackToken, TokenError, TokenKind, redact};
use chrono::{DateTime, Utc};
use credentials_provider::CredentialsProvider;
use gpui::{App, Global, Task};
use serde::{Deserialize, Serialize};

use crate::client;

/// The url the token is stored under in the credential store.
pub const CREDENTIAL_URL: &str = "https://slack.com";
/// The user name the token is stored under.
pub const CREDENTIAL_USER: &str = "bongos";
/// The file of the connection, in the Slack directory.
pub const CONNECTION_FILE: &str = "connection.toml";
/// The file of the allowed conversations, beside [`CONNECTION_FILE`].
pub const ALLOWED_FILE: &str = "allowed.toml";
/// Slack error codes that mean a workspace admin has not approved the app.
pub const NOT_APPROVED_CODES: &[&str] = &["team_access_not_granted", "access_denied"];

/// The Slack directory: `slack` in the data directory.
pub fn default_dir() -> PathBuf {
    paths::data_dir().join("slack")
}

pub fn connection_path(dir: &Path) -> PathBuf {
    dir.join(CONNECTION_FILE)
}

pub fn allowed_path(dir: &Path) -> PathBuf {
    dir.join(ALLOWED_FILE)
}

/// The kind of the stored token, as written in `connection.toml`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StoredKind {
    User,
    Bot,
}

impl From<TokenKind> for StoredKind {
    fn from(kind: TokenKind) -> Self {
        match kind {
            TokenKind::User => Self::User,
            TokenKind::Bot => Self::Bot,
        }
    }
}

/// What `connection.toml` holds. No field is a secret.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Connection {
    /// The workspace name.
    pub workspace: String,
    pub team_id: String,
    pub workspace_url: String,
    pub user: String,
    pub user_id: String,
    pub token_kind: StoredKind,
    pub scopes: Vec<String>,
    pub connected_at: DateTime<Utc>,
}

impl Connection {
    pub fn new(info: &AuthInfo, kind: TokenKind, connected_at: DateTime<Utc>) -> Self {
        Self {
            workspace: info.team.clone(),
            team_id: info.team_id.clone(),
            workspace_url: info.url.clone(),
            user: info.user.clone(),
            user_id: info.user_id.0.clone(),
            token_kind: kind.into(),
            scopes: info.scopes.clone(),
            connected_at,
        }
    }
}

/// The connection in `dir`, or `None` when `connection.toml` is missing or unreadable.
pub fn read_connection(dir: &Path) -> Option<Connection> {
    let path = connection_path(dir);
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return None,
        Err(error) => {
            log::warn!("could not read the Slack connection file: {error}");
            return None;
        }
    };
    match toml::from_str(&text) {
        Ok(connection) => Some(connection),
        Err(error) => {
            log::warn!("the Slack connection file is not valid: {error}");
            None
        }
    }
}

/// Writes `connection` to `connection.toml` in `dir`.
pub fn write_connection(dir: &Path, connection: &Connection) -> io::Result<()> {
    let text = toml::to_string(connection).map_err(io::Error::other)?;
    write_atomic(dir, CONNECTION_FILE, text.as_bytes())
}

static TEMPORARY_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Whether `file_name` is a temporary file of `name`, as [`write_atomic`] names them.
fn is_temporary_of(file_name: &str, name: &str) -> bool {
    file_name
        .strip_prefix(&format!(".{name}."))
        .is_some_and(|rest| rest.ends_with(".tmp"))
}

/// Writes `bytes` to `dir/name` by writing a temporary file in `dir` and renaming it over the
/// file, so a reader sees the old file or the new one. Temporary files left by an interrupted
/// write are removed first.
pub fn write_atomic(dir: &Path, name: &str, bytes: &[u8]) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if entry
            .file_name()
            .to_str()
            .is_some_and(|file_name| is_temporary_of(file_name, name))
        {
            fs::remove_file(entry.path())?;
        }
    }
    let unique = TEMPORARY_COUNTER.fetch_add(1, Ordering::Relaxed);
    let temporary = dir.join(format!(".{name}.{}-{unique}.tmp", std::process::id()));
    let written = (|| {
        let mut file = fs::File::create(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        fs::rename(&temporary, dir.join(name))
    })();
    if written.is_err() {
        fs::remove_file(&temporary).ok();
    }
    written
}

/// Removes `path`; a missing file is not an error.
fn remove_if_present(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(error) if error.kind() != io::ErrorKind::NotFound => Err(error),
        _ => Ok(()),
    }
}

/// Why a connect stored nothing, or stored the secret but not the file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnectError {
    AlreadyRunning,
    Token(TokenError),
    NoClient,
    /// The workspace has not approved the app; offer the admin request text.
    NeedsAdmin,
    /// Slack refused the token or the check failed; the text is redacted.
    Check(String),
    /// Slack did not say which scopes the token has.
    ScopesUnknown,
    MissingScopes(Vec<&'static str>),
    /// The credential store failed; the cause is redacted.
    SecretStore(String),
    /// The secret was stored but `connection.toml` was not written; the cause is redacted.
    File(String),
}

impl fmt::Display for ConnectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyRunning => f.write_str("Slack: a connect is already running"),
            Self::Token(error) => write!(f, "Slack token refused: {error}"),
            Self::NoClient => f.write_str(
                "The Slack client is not available, so the token was not checked or saved",
            ),
            Self::NeedsAdmin => f.write_str("This workspace needs an admin to approve the app"),
            Self::Check(reason) => write!(f, "Slack token not saved: {reason}"),
            Self::ScopesUnknown => write!(
                f,
                "Slack did not say which scopes the token has, so it was not saved (it needs {})",
                SCOPES.join(", ")
            ),
            Self::MissingScopes(missing) => write!(
                f,
                "The Slack token lacks the scopes {}; nothing was saved",
                missing.join(", ")
            ),
            Self::SecretStore(cause) => {
                write!(
                    f,
                    "No secret store is available ({cause}); nothing was saved"
                )
            }
            Self::File(cause) => write!(
                f,
                "The Slack token was stored but {CONNECTION_FILE} could not be written ({cause}); \
                 not connected"
            ),
        }
    }
}

/// Why a disconnect stopped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DisconnectError {
    AlreadyRunning,
    /// Deleting the secret failed; the connection stays. The cause is redacted.
    SecretStore(String),
    /// The secret is gone but a file could not be removed; the cause is redacted.
    File(String),
}

impl fmt::Display for DisconnectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyRunning => f.write_str("Slack: a connect is already running"),
            Self::SecretStore(cause) => write!(
                f,
                "Could not delete the Slack token from the secret store ({cause}); still connected"
            ),
            Self::File(cause) => write!(
                f,
                "The Slack token was deleted but a connection file could not be removed ({cause})"
            ),
        }
    }
}

/// The error of a failed `auth.test`, redacted.
fn check_error(error: ApiError, token: &SlackToken) -> ConnectError {
    match &error {
        ApiError::NeedsAdmin => ConnectError::NeedsAdmin,
        ApiError::Other { code } if NOT_APPROVED_CODES.contains(&code.as_str()) => {
            ConnectError::NeedsAdmin
        }
        _ => ConnectError::Check(token.redact(&error.to_string())),
    }
}

/// Refuses a token whose scopes are unknown or lack one of [`SCOPES`]. An empty list (no
/// `x-oauth-scopes` header) is unknown, never all granted.
pub fn check_scopes(granted: &[String]) -> Result<(), ConnectError> {
    if granted.is_empty() {
        return Err(ConnectError::ScopesUnknown);
    }
    let missing = missing_scopes(granted);
    if missing.is_empty() {
        Ok(())
    } else {
        Err(ConnectError::MissingScopes(missing))
    }
}

/// The connection state of the application.
struct SlackConnection {
    dir: PathBuf,
    connection: Option<Connection>,
    /// A connect or a disconnect is running.
    busy: bool,
}

impl Global for SlackConnection {}

struct InstalledCredentials(Arc<dyn CredentialsProvider>);

impl Global for InstalledCredentials {}

/// Sets the Slack directory and reads the state from its `connection.toml` alone; the credential
/// store is not touched.
pub fn install(dir: PathBuf, cx: &mut App) {
    let connection = read_connection(&dir);
    cx.set_global(SlackConnection {
        dir,
        connection,
        busy: false,
    });
}

/// Replaces the credential store the token is written to and deleted from. Without a call the
/// application's store is used.
pub fn install_credentials_provider(provider: Arc<dyn CredentialsProvider>, cx: &mut App) {
    cx.set_global(InstalledCredentials(provider));
}

fn credentials(cx: &App) -> Arc<dyn CredentialsProvider> {
    match cx.try_global::<InstalledCredentials>() {
        Some(installed) => installed.0.clone(),
        None => zed_credentials_provider::global(cx),
    }
}

fn state_mut(cx: &mut App) -> &mut SlackConnection {
    if !cx.has_global::<SlackConnection>() {
        install(default_dir(), cx);
    }
    cx.global_mut::<SlackConnection>()
}

/// The current connection, or `None` when not connected.
pub fn current(cx: &App) -> Option<Connection> {
    cx.try_global::<SlackConnection>()
        .and_then(|state| state.connection.clone())
}

/// Whether a connect or a disconnect is running.
pub fn is_busy(cx: &App) -> bool {
    cx.try_global::<SlackConnection>()
        .is_some_and(|state| state.busy)
}

/// The Slack directory in use.
pub fn dir(cx: &mut App) -> PathBuf {
    state_mut(cx).dir.clone()
}

/// Checks the token in `text` and stores it, with `now` as the time of the connection. The kind
/// of token is checked before any request; then one `auth.test`, the scope check, the secret,
/// then `connection.toml`. Replacing a connection to another team clears `allowed.toml`.
pub fn connect(
    text: &str,
    now: DateTime<Utc>,
    cx: &mut App,
) -> Task<Result<Connection, ConnectError>> {
    let token = match SlackToken::parse(text) {
        Ok(token) => token,
        Err(error) => return Task::ready(Err(ConnectError::Token(error))),
    };
    if state_mut(cx).busy {
        return Task::ready(Err(ConnectError::AlreadyRunning));
    }
    let Ok(client) = client::client(Some(&token), cx) else {
        return Task::ready(Err(ConnectError::NoClient));
    };
    let provider = credentials(cx);
    let state = state_mut(cx);
    state.busy = true;
    let dir = state.dir.clone();
    let previous_team = state
        .connection
        .as_ref()
        .map(|connection| connection.team_id.clone());
    cx.spawn(async move |cx| {
        let result = async {
            let info = client
                .auth_test()
                .await
                .map_err(|error| check_error(error, &token))?;
            check_scopes(&info.scopes)?;
            provider
                .write_credentials(
                    CREDENTIAL_URL,
                    CREDENTIAL_USER,
                    token.secret().as_bytes(),
                    cx,
                )
                .await
                .map_err(|error| ConnectError::SecretStore(token.redact(&format!("{error:#}"))))?;
            let connection = Connection::new(&info, token.kind(), now);
            write_connection(&dir, &connection)
                .map_err(|error| ConnectError::File(token.redact(&error.to_string())))?;
            if previous_team.is_some_and(|team| team != connection.team_id)
                && let Err(error) = remove_if_present(&allowed_path(&dir))
            {
                log::warn!(
                    "could not clear the Slack allowed list of the previous workspace: {}",
                    token.redact(&error.to_string())
                );
            }
            Ok(connection)
        }
        .await;
        cx.update(|cx| {
            let state = state_mut(cx);
            state.busy = false;
            if let Ok(connection) = &result {
                state.connection = Some(connection.clone());
            }
        });
        result
    })
}

/// Deletes the secret, then `allowed.toml`, then `connection.toml`. A failure to delete the
/// secret stops there and keeps the connection.
pub fn disconnect(cx: &mut App) -> Task<Result<(), DisconnectError>> {
    if state_mut(cx).busy {
        return Task::ready(Err(DisconnectError::AlreadyRunning));
    }
    let provider = credentials(cx);
    let state = state_mut(cx);
    state.busy = true;
    let dir = state.dir.clone();
    cx.spawn(async move |cx| {
        let result = async {
            provider
                .delete_credentials(CREDENTIAL_URL, cx)
                .await
                .map_err(|error| DisconnectError::SecretStore(redact(&format!("{error:#}"))))?;
            let removed = remove_if_present(&allowed_path(&dir))
                .and_then(|()| remove_if_present(&connection_path(&dir)));
            // The secret is gone, so the connection is over whatever happens to the files.
            removed.map_err(|error| DisconnectError::File(redact(&error.to_string())))
        }
        .await;
        cx.update(|cx| {
            let state = state_mut(cx);
            state.busy = false;
            if !matches!(result, Err(DisconnectError::SecretStore(_))) {
                state.connection = None;
            }
        });
        result
    })
}

/// The steps to make the Slack app, with its manifest, for the read-only buffer.
pub fn how_to_make_the_app() -> String {
    let scopes = SCOPES
        .iter()
        .map(|scope| format!("      - {scope}\n"))
        .collect::<String>();
    format!(
        "How to make the Slack app for bongos\n\
         \n\
         1. Open https://api.slack.com/apps and choose Create New App, then From a manifest.\n\
         2. Pick the workspace and paste the manifest below.\n\
         3. Create the app, then Install to Workspace (an admin may have to approve it).\n\
         4. Under OAuth & Permissions, leave token rotation off and copy the User OAuth Token\n   \
            (it starts with xoxp-).\n\
         5. Run slack: connect, choose Paste a token, and paste it.\n\
         \n\
         The app asks for no admin scope.\n\
         \n\
         Manifest (YAML):\n\
         \n\
         display_information:\n  \
           name: bongos\n  \
           description: Read and post Slack messages from the bongos editor.\n\
         oauth_config:\n  \
           scopes:\n    \
             user:\n\
         {scopes}\
         settings:\n  \
           token_rotation_enabled: false\n"
    )
}
