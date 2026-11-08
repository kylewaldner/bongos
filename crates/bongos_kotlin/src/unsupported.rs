//! "Not supported by <server>": a Kotlin editor action whose language feature the running Kotlin
//! server does not offer shows one toast instead of silently doing nothing. The handlers are
//! registered ahead of the stock ones on the editor, so they run first and hand the action on
//! (`cx.propagate()`) whenever the server supports it, or no Kotlin server runs to ask.

use editor::{
    Editor,
    actions::{FindAllReferences, GoToDefinition, GoToTypeDefinition, Hover, Rename},
};
use gpui::{Action, App, Entity, WeakEntity, Window};
use lsp::{OneOf, ServerCapabilities};
use workspace::{Toast, notifications::NotificationId};

use crate::{
    installed::server_name,
    manager::{ADAPTER_NAME, KotlinRegistry},
};

const KOTLIN_LANGUAGE: &str = "Kotlin";

/// The editor features that need a server capability.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Feature {
    Definition,
    TypeDefinition,
    References,
    Rename,
    Hover,
}

impl Feature {
    /// The name used in the toast.
    pub fn label(self) -> &'static str {
        match self {
            Self::Definition => "Go to definition",
            Self::TypeDefinition => "Go to type definition",
            Self::References => "Find all references",
            Self::Rename => "Rename",
            Self::Hover => "Hover",
        }
    }

    fn index(self) -> usize {
        self as usize
    }

    /// Whether `capabilities` offer the feature, by the same rules as the stock requests.
    pub fn supported_by(self, capabilities: &ServerCapabilities) -> bool {
        match self {
            Self::Definition => match &capabilities.definition_provider {
                Some(OneOf::Left(supported)) => *supported,
                Some(OneOf::Right(_)) => true,
                None => false,
            },
            Self::TypeDefinition => !matches!(
                &capabilities.type_definition_provider,
                None | Some(lsp::TypeDefinitionProviderCapability::Simple(false))
            ),
            Self::References => match &capabilities.references_provider {
                Some(OneOf::Left(supported)) => *supported,
                Some(OneOf::Right(_)) => true,
                None => false,
            },
            Self::Rename => match &capabilities.rename_provider {
                Some(OneOf::Left(supported)) => *supported,
                Some(OneOf::Right(_)) => true,
                None => false,
            },
            Self::Hover => match &capabilities.hover_provider {
                Some(lsp::HoverProviderCapability::Simple(supported)) => *supported,
                Some(lsp::HoverProviderCapability::Options(_)) => true,
                None => false,
            },
        }
    }
}

/// The text of the toast.
pub fn not_supported_message(feature: Feature, server: &str) -> String {
    format!("{} is not supported by {server}", feature.label())
}

/// The id of a feature's toast: pressing the key again replaces the toast, never stacks.
struct NotSupportedNotice;

pub fn not_supported_notice_id(feature: Feature) -> NotificationId {
    NotificationId::composite::<NotSupportedNotice>(feature.index())
}

/// Gives every editor the handlers. Call before any editor is created.
pub fn attach_unsupported_notices(cx: &mut App) {
    cx.observe_new::<Editor>(|editor, _window, cx| {
        let weak = cx.weak_entity();
        register::<GoToDefinition>(editor, &weak, Feature::Definition);
        register::<GoToTypeDefinition>(editor, &weak, Feature::TypeDefinition);
        register::<FindAllReferences>(editor, &weak, Feature::References);
        register::<Rename>(editor, &weak, Feature::Rename);
        register::<Hover>(editor, &weak, Feature::Hover);
    })
    .detach();
}

fn register<A: Action>(editor: &mut Editor, weak: &WeakEntity<Editor>, feature: Feature) {
    let weak = weak.clone();
    editor
        .register_action::<A>(move |_, window, cx| handle(&weak, feature, window, cx))
        .detach();
}

/// Runs before the stock action: propagates unless the Kotlin server is known to lack `feature`.
fn handle(editor: &WeakEntity<Editor>, feature: Feature, _window: &mut Window, cx: &mut App) {
    let Some(editor) = editor.upgrade() else {
        cx.propagate();
        return;
    };
    let Some(message) = not_supported_text(&editor, feature, cx) else {
        cx.propagate();
        return;
    };
    // Stops here: the action is consumed, the stock handler never runs.
    if let Some(workspace) = editor.read(cx).workspace() {
        let toast = Toast::new(not_supported_notice_id(feature), message);
        workspace.update(cx, |workspace, cx| workspace.show_toast(toast, cx));
    }
}

/// The toast text when the editor's running Kotlin server lacks `feature`, else `None`.
pub fn not_supported_text(
    editor: &Entity<Editor>,
    feature: Feature,
    cx: &mut App,
) -> Option<String> {
    let server = server_lacking(editor, feature, cx)?;
    Some(not_supported_message(feature, &server))
}

/// The name of the running Kotlin server of the editor's buffer when it lacks `feature`.
/// `None` for a buffer that is not Kotlin, or has no running Kotlin server.
fn server_lacking(editor: &Entity<Editor>, feature: Feature, cx: &mut App) -> Option<String> {
    let (buffer, project) = {
        let editor = editor.read(cx);
        (editor.active_buffer(cx)?, editor.project()?.clone())
    };
    let is_kotlin = buffer
        .read(cx)
        .language()
        .is_some_and(|language| language.name().as_ref() == KOTLIN_LANGUAGE);
    if !is_kotlin {
        return None;
    }
    let lsp_store = project.read(cx).lsp_store();
    let capabilities = lsp_store.update(cx, |lsp_store, cx| {
        buffer.update(cx, |buffer, cx| {
            lsp_store
                .running_language_servers_for_local_buffer(buffer, cx)
                .find(|(adapter, _)| adapter.name.0.as_ref() == ADAPTER_NAME)
                .map(|(_, server)| server.capabilities())
        })
    })?;
    if feature.supported_by(&capabilities) {
        return None;
    }
    let worktree_id = buffer.read(cx).file().map(|file| file.worktree_id(cx));
    let effective = cx
        .try_global::<KotlinRegistry>()
        .and_then(|registry| registry.manager_of_project(project.entity_id()))
        .zip(worktree_id)
        .and_then(|(manager, id)| {
            manager
                .read(cx)
                .worktree(id)
                .map(|state| server_name(state.effective))
        });
    Some(effective.unwrap_or(ADAPTER_NAME).to_string())
}
