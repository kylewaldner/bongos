//! The Linear token: a masked one-line modal, the check against Linear, and the credential store.
//!
//! `linear: set token` opens [`TokenModal`], checks what was typed with exactly one `viewer()`
//! request and, only if Linear accepts it, stores it in the credential store under
//! [`CREDENTIAL_URL`]. `linear: clear token` deletes it. [`read_token`] reads the store first and
//! then the `LINEAR_API_KEY` environment variable.
//!
//! The token never reaches a file bongos writes, a log line, a toast, the clipboard, the primary
//! selection or the kill ring: [`Token`] has no `Display` and a redacted `Debug`, every message built from an error
//! passes through [`Token::redact`] (the HTTP client scrubs its own errors by construction, see
//! `client`), and the modal blocks copy, cut and kill while it is masked, and a masked editor
//! never writes its selection to the primary selection. The only place the
//! bytes are kept at rest is the credential store. Every command reads the token through
//! [`with_token`]; the sync learns from there whether one exists.

use std::fmt;
use std::sync::Arc;

use bongos_linear_model::LinearApi;
use credentials_provider::CredentialsProvider;
use editor::Editor;
use editor::actions::{Copy, CopyAndTrim, Cut, CutToEndOfLine, KillRingCut};
use gpui::{
    App, AppContext as _, AsyncApp, Context, DismissEvent, Entity, EventEmitter, FocusHandle,
    Focusable, Global, IntoElement, Render, Styled, WeakEntity, Window,
};
use ui::{
    ActiveTheme as _, DynamicSpacing, Headline, HeadlineSize, Icon, IconName, IconSize,
    InteractiveElement as _, Label, LabelCommon as _, LabelSize, ParentElement as _,
    StyledExt as _, StyledTypography as _, div, h_flex, v_flex,
};
use workspace::{ModalView, Workspace};

use crate::actions::show_toast;
use crate::sync;

/// The url the token is stored under in the credential store.
pub const CREDENTIAL_URL: &str = "https://api.linear.app";
/// The user name the token is stored under.
pub const CREDENTIAL_USER: &str = "bongos";
/// The environment variable read when the credential store has no token.
pub const ENV_VAR: &str = "LINEAR_API_KEY";
/// The toast of a command that needs a token when there is none.
pub const NO_TOKEN_MESSAGE: &str = "No Linear token: run linear: set token, or set LINEAR_API_KEY";

/// A Linear API key. It has no `Display`, and `Debug` redacts it.
#[derive(Clone, PartialEq, Eq)]
pub struct Token(Arc<str>);

impl Token {
    /// The token in `text` without surrounding white space, or `None` when nothing is left.
    pub fn new(text: &str) -> Option<Self> {
        let text = text.trim();
        (!text.is_empty()).then(|| Self(Arc::from(text)))
    }

    /// The token itself, for the one place that sends or stores it.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// `text` with every occurrence of the token replaced, for messages built from errors that
    /// might quote what was sent.
    pub fn redact(&self, text: &str) -> String {
        text.replace(&*self.0, "[redacted]")
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token(<redacted>)")
    }
}

/// Builds the client that sends requests with a token. The HTTP client installs it; tests install
/// one that returns a fake.
pub type ClientFactory = Arc<dyn Fn(&Token) -> Arc<dyn LinearApi> + Send + Sync>;

/// Reads an environment variable; tests replace it.
type EnvReader = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

struct InstalledCredentials(Arc<dyn CredentialsProvider>);
struct InstalledClientFactory(ClientFactory);
struct InstalledEnv(EnvReader);

impl Global for InstalledCredentials {}
impl Global for InstalledClientFactory {}
impl Global for InstalledEnv {}

/// Replaces the credential store the token is read from and written to. Without a call the
/// application's store is used.
pub fn install_credentials_provider(provider: Arc<dyn CredentialsProvider>, cx: &mut App) {
    cx.set_global(InstalledCredentials(provider));
}

/// Installs the builder of the client that checks a new token.
pub fn install_client_factory(factory: ClientFactory, cx: &mut App) {
    cx.set_global(InstalledClientFactory(factory));
}

/// Replaces the environment lookup, for tests.
pub fn install_env(reader: impl Fn(&str) -> Option<String> + Send + Sync + 'static, cx: &mut App) {
    cx.set_global(InstalledEnv(Arc::new(reader)));
}

fn credentials(cx: &App) -> Arc<dyn CredentialsProvider> {
    match cx.try_global::<InstalledCredentials>() {
        Some(installed) => installed.0.clone(),
        None => zed_credentials_provider::global(cx),
    }
}

fn env_reader(cx: &App) -> EnvReader {
    match cx.try_global::<InstalledEnv>() {
        Some(installed) => installed.0.clone(),
        None => Arc::new(|name| std::env::var(name).ok()),
    }
}

/// The token: the credential store first, then `LINEAR_API_KEY`. `None` when neither has one.
pub async fn read_token(
    provider: &dyn CredentialsProvider,
    env: &(dyn Fn(&str) -> Option<String> + Send + Sync),
    cx: &AsyncApp,
) -> Option<Token> {
    match provider.read_credentials(CREDENTIAL_URL, cx).await {
        Ok(Some((_, bytes))) => {
            if let Some(token) = String::from_utf8(bytes).ok().and_then(|t| Token::new(&t)) {
                return Some(token);
            }
        }
        Ok(None) => {}
        Err(_) => log::warn!("could not read the Linear token from the credential store"),
    }
    env(ENV_VAR).and_then(|value| Token::new(&value))
}

/// Hands the current token, or `None`, to `then`, and tells the sync whether one exists and the
/// client that uses it. The one place the token is read for a command.
fn read_current(
    cx: &mut Context<Workspace>,
    then: impl FnOnce(&mut Workspace, Option<Token>, &mut Context<Workspace>) + 'static,
) {
    let provider = credentials(cx);
    let env = env_reader(cx);
    cx.spawn(async move |workspace, cx| {
        let token = read_token(&*provider, &*env, cx).await;
        workspace
            .update(cx, |workspace, cx| {
                connect(token.as_ref(), cx);
                then(workspace, token, cx)
            })
            .ok();
    })
    .detach();
}

/// Tells the sync about `token`: that one exists, and the client built with it.
fn connect(token: Option<&Token>, cx: &mut App) {
    let client = token.and_then(|token| installed_factory(cx).map(|factory| factory(token)));
    if let Some(sync) = sync::global(cx) {
        sync.update(cx, |sync, _| sync.set_token(token.is_some(), client));
    }
}

/// Reads the token without a workspace and tells the sync about it, for a refresh that starts
/// before any command ran. Returns whether a token exists. Nothing is sent.
pub(crate) async fn load(cx: &mut gpui::AsyncApp) -> bool {
    let (provider, env) = cx.update(|cx| (credentials(cx), env_reader(cx)));
    let token = read_token(&*provider, &*env, cx).await;
    cx.update(|cx| connect(token.as_ref(), cx));
    token.is_some()
}

fn installed_factory(cx: &App) -> Option<ClientFactory> {
    cx.try_global::<InstalledClientFactory>()
        .map(|installed| installed.0.clone())
}

/// Reads the token and hands it to `then`, or toasts [`NO_TOKEN_MESSAGE`] and does nothing else.
/// Every Linear command that sends a request goes through this.
pub fn with_token(
    cx: &mut Context<Workspace>,
    then: impl FnOnce(&mut Workspace, Token, &mut Context<Workspace>) + 'static,
) {
    read_current(cx, |workspace, token, cx| match token {
        Some(token) => then(workspace, token, cx),
        None => show_toast(workspace, NO_TOKEN_MESSAGE.to_string(), cx),
    });
}

/// Hands `then` whether a token exists, and nothing else about it.
pub(crate) fn with_token_state(
    cx: &mut Context<Workspace>,
    then: impl FnOnce(&mut Workspace, bool, &mut Context<Workspace>) + 'static,
) {
    read_current(cx, |workspace, token, cx| {
        then(workspace, token.is_some(), cx)
    });
}

/// Checks `token` with one `viewer()` request, through the sync's gate, and stores it when Linear
/// accepts it, then toasts the outcome. A rejected token stores nothing.
fn check_and_store(workspace: WeakEntity<Workspace>, token: Token, cx: &mut App) {
    let provider = credentials(cx);
    let toast = |message: String, cx: &mut App| {
        workspace
            .update(cx, |workspace, cx| show_toast(workspace, message, cx))
            .ok();
    };
    let (Some(factory), Some(sync)) = (installed_factory(cx), sync::global(cx)) else {
        toast(
            "The Linear client is not available, so the token was not checked or saved".to_string(),
            cx,
        );
        return;
    };
    let client = factory(&token);
    let started = sync.update(cx, |sync, cx| sync.verify_token(&*client, cx));
    let check = match started {
        Ok(check) => check,
        Err(refusal) => {
            toast(refusal.to_string(), cx);
            return;
        }
    };
    cx.spawn(async move |cx| {
        let message = match check.await {
            Ok(organization) => {
                let stored = provider
                    .write_credentials(
                        CREDENTIAL_URL,
                        CREDENTIAL_USER,
                        token.expose().as_bytes(),
                        cx,
                    )
                    .await;
                match stored {
                    Ok(()) => {
                        log::info!("stored the Linear token");
                        sync.update(cx, |sync, _| sync.accept_token(client));
                        format!("Linear token saved ({organization})")
                    }
                    Err(error) => {
                        token.redact(&format!("Could not save the Linear token: {error}"))
                    }
                }
            }
            Err(error) => token.redact(&format!("Linear token not saved: {error}")),
        };
        workspace
            .update(cx, |workspace, cx| show_toast(workspace, message, cx))
            .ok();
    })
    .detach();
}

/// Opens the masked token modal.
pub(crate) fn set_token(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let weak = cx.entity().downgrade();
    workspace.toggle_modal(window, cx, |window, cx| TokenModal::new(weak, window, cx));
}

/// Deletes the stored token. A token in `LINEAR_API_KEY` stays, as bongos does not touch it.
pub(crate) fn clear_token(_: &mut Workspace, _: &mut Window, cx: &mut Context<Workspace>) {
    let provider = credentials(cx);
    cx.spawn(async move |workspace, cx| {
        let deleted = provider.delete_credentials(CREDENTIAL_URL, cx).await;
        workspace
            .update(cx, |workspace, cx| {
                let message = match deleted {
                    Ok(()) => {
                        log::info!("removed the Linear token");
                        // The next command reads the environment again and connects if it has one.
                        if let Some(sync) = sync::global(cx) {
                            sync.update(cx, |sync, _| sync.set_token(false, None));
                        }
                        "Linear token removed".to_string()
                    }
                    Err(error) => format!("Could not remove the Linear token: {error}"),
                };
                show_toast(workspace, message, cx)
            })
            .ok();
    })
    .detach();
}

/// The masked one-line modal. Its own view: no history, and not a picker or a prompt of the
/// minibuffer.
pub(crate) struct TokenModal {
    editor: Entity<Editor>,
    workspace: WeakEntity<Workspace>,
}

impl TokenModal {
    fn new(workspace: WeakEntity<Workspace>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_masked(true, cx);
            editor
        });
        Self { editor, workspace }
    }

    #[cfg(test)]
    pub(crate) fn editor(&self) -> &Entity<Editor> {
        &self.editor
    }

    fn cancel(&mut self, _: &menu::Cancel, window: &mut Window, cx: &mut Context<Self>) {
        self.editor
            .update(cx, |editor, cx| editor.clear(window, cx));
        cx.emit(DismissEvent);
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.editor.update(cx, |editor, cx| {
            let text = editor.text(cx);
            editor.clear(window, cx);
            text
        });
        cx.emit(DismissEvent);
        if let Some(token) = Token::new(&text) {
            check_and_store(self.workspace.clone(), token, cx);
        }
    }
}

impl EventEmitter<DismissEvent> for TokenModal {}
impl ModalView for TokenModal {}

impl Focusable for TokenModal {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl Render for TokenModal {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("LinearTokenPrompt")
            // The editor masks what it shows, not what it copies: while the token is typed, the
            // copy, cut and Emacs kill commands do nothing, so neither the clipboard nor the kill
            // ring can receive it. Capture runs before the editor's own handlers.
            .capture_action(|_: &Copy, _, cx| cx.stop_propagation())
            .capture_action(|_: &CopyAndTrim, _, cx| cx.stop_propagation())
            .capture_action(|_: &Cut, _, cx| cx.stop_propagation())
            .capture_action(|_: &CutToEndOfLine, _, cx| cx.stop_propagation())
            .capture_action(|_: &KillRingCut, _, cx| cx.stop_propagation())
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::confirm))
            .elevation_2(cx)
            .size_full()
            .child(
                h_flex()
                    .font_buffer(cx)
                    .px(DynamicSpacing::Base12.rems(cx))
                    .pt(DynamicSpacing::Base08.rems(cx))
                    .pb(DynamicSpacing::Base04.rems(cx))
                    .w_full()
                    .gap_1p5()
                    .child(Icon::new(IconName::LockOutlined).size(IconSize::XSmall))
                    .child(Headline::new("Linear API key").size(HeadlineSize::XSmall)),
            )
            .child(
                div()
                    .font_buffer(cx)
                    .text_buffer(cx)
                    .py_2()
                    .px_3()
                    .bg(cx.theme().colors().editor_background)
                    .border_t_1()
                    .border_color(cx.theme().colors().border_variant)
                    .size_full()
                    .overflow_hidden()
                    .child(self.editor.clone())
                    .child(
                        Label::new("Checked with Linear, then stored in the credential store")
                            .size(LabelSize::Small),
                    ),
            )
    }
}
