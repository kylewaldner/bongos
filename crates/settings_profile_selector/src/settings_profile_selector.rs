use std::path::{Path, PathBuf};
use std::{cell::Cell, rc::Rc};

use anyhow::{Context as _, Result};
use fuzzy::{StringMatch, StringMatchCandidate, match_strings};
use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, Focusable, Global, Render, Task, WeakEntity,
    Window,
};
use picker::{Picker, PickerDelegate};
use serde::{Deserialize, Serialize};
use settings::{ActiveSettingsProfileName, SettingsStore};
use ui::{HighlightedLabel, ListItem, ListItemSpacing, prelude::*};
use util::ResultExt as _;
use workspace::notifications::simple_message_notification::MessageNotification;
use workspace::notifications::{NotificationId, show_app_notification};
use workspace::{ModalView, Toast, Workspace};

const ACTIVE_PROFILE_FILE: &str = "active-profile.json";
const ACTIVE_PROFILE_FILE_VERSION: u32 = 1;
const NO_PROFILE: &str = "No profile";

#[derive(Serialize, Deserialize)]
struct ActiveProfileFile {
    version: u32,
    profile: String,
}

/// Where the remembered profile lives. Set once by [`restore_active_profile`].
struct ProfileStateDirectory(PathBuf);

impl Global for ProfileStateDirectory {}

struct ProfileToastId;

struct UnknownRememberedProfileId;

/// Reads the remembered profile name. A missing, unreadable or corrupt file means no profile.
pub fn read_remembered_profile(state_directory: &Path) -> Option<String> {
    let contents = std::fs::read(state_directory.join(ACTIVE_PROFILE_FILE)).ok()?;
    let file: ActiveProfileFile = serde_json::from_slice(&contents).ok()?;
    (file.version == ACTIVE_PROFILE_FILE_VERSION && !file.profile.is_empty())
        .then_some(file.profile)
}

/// Writes the remembered profile name through a temporary file and a rename.
pub fn write_remembered_profile(state_directory: &Path, profile: &str) -> Result<()> {
    std::fs::create_dir_all(state_directory)
        .with_context(|| format!("creating {}", state_directory.display()))?;
    let contents = serde_json::to_vec(&ActiveProfileFile {
        version: ACTIVE_PROFILE_FILE_VERSION,
        profile: profile.to_string(),
    })?;
    let path = state_directory.join(ACTIVE_PROFILE_FILE);
    let temporary_path = state_directory.join(format!("{ACTIVE_PROFILE_FILE}.tmp"));
    std::fs::write(&temporary_path, contents)
        .with_context(|| format!("writing {}", temporary_path.display()))?;
    std::fs::rename(&temporary_path, &path)
        .with_context(|| format!("renaming to {}", path.display()))
}

/// Removes the remembered profile. A file that is already gone is not an error.
pub fn delete_remembered_profile(state_directory: &Path) -> Result<()> {
    let path = state_directory.join(ACTIVE_PROFILE_FILE);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("removing {}", path.display())),
    }
}

fn remember_profile(state_directory: &Path, profile: Option<&str>) -> Result<()> {
    match profile {
        Some(profile) => write_remembered_profile(state_directory, profile),
        None => delete_remembered_profile(state_directory),
    }
}

/// Applies the profile remembered on this machine. Call once right after settings init, with
/// `paths::state_dir()`. The user settings file may not be loaded yet, so an unknown name is
/// reported once, after the first load.
pub fn restore_active_profile(state_directory: &Path, cx: &mut App) {
    cx.set_global(ProfileStateDirectory(state_directory.to_path_buf()));

    let Some(profile) = read_remembered_profile(state_directory) else {
        return;
    };
    cx.set_global(ActiveSettingsProfileName(profile.clone()));

    if cx.global::<SettingsStore>().raw_user_settings().is_some() {
        report_if_unknown(&profile, cx);
        return;
    }

    let reported = Rc::new(Cell::new(false));
    cx.observe_global::<SettingsStore>(move |cx| {
        if reported.get() || cx.global::<SettingsStore>().raw_user_settings().is_none() {
            return;
        }
        reported.set(true);
        report_if_unknown(&profile, cx);
    })
    .detach();
}

fn unknown_profile_message(profile: &str, store: &SettingsStore) -> Option<String> {
    let defined = store
        .configured_settings_profiles()
        .any(|name| name == profile);
    (!defined).then(|| {
        format!(
            "Profile {profile} is remembered on this machine but not defined; using base settings"
        )
    })
}

fn report_if_unknown(profile: &str, cx: &mut App) {
    let Some(message) = unknown_profile_message(profile, cx.global::<SettingsStore>()) else {
        return;
    };
    log::warn!("{message}");
    show_app_notification(
        NotificationId::unique::<UnknownRememberedProfileId>(),
        cx,
        move |cx| cx.new(|cx| MessageNotification::new(message.clone(), cx)),
    );
}

pub fn init(cx: &mut App) {
    cx.on_action(|_: &zed_actions::settings::SwitchProfile, cx| {
        workspace::with_active_or_new_workspace(cx, |workspace, window, cx| {
            toggle_settings_profile_selector(workspace, window, cx);
        });
    });
}

fn toggle_settings_profile_selector(
    workspace: &mut Workspace,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let workspace_handle = cx.entity().downgrade();
    workspace.toggle_modal(window, cx, |window, cx| {
        let delegate = SettingsProfileSelectorDelegate::new(
            cx.entity().downgrade(),
            workspace_handle,
            window,
            cx,
        );
        SettingsProfileSelector::new(delegate, window, cx)
    });
}

pub struct SettingsProfileSelector {
    picker: Entity<Picker<SettingsProfileSelectorDelegate>>,
}

impl ModalView for SettingsProfileSelector {}

impl EventEmitter<DismissEvent> for SettingsProfileSelector {}

impl Focusable for SettingsProfileSelector {
    fn focus_handle(&self, cx: &App) -> gpui::FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl Render for SettingsProfileSelector {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        v_flex().child(self.picker.clone())
    }
}

impl SettingsProfileSelector {
    pub fn new(
        delegate: SettingsProfileSelectorDelegate,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let picker =
            cx.new(|cx| Picker::uniform_list(delegate, window, cx).initial_width(rems(22.)));
        Self { picker }
    }
}

pub struct SettingsProfileSelectorDelegate {
    matches: Vec<StringMatch>,
    profile_names: Vec<Option<String>>,
    selected_index: usize,
    selector: WeakEntity<SettingsProfileSelector>,
    workspace: WeakEntity<Workspace>,
}

impl SettingsProfileSelectorDelegate {
    fn new(
        selector: WeakEntity<SettingsProfileSelector>,
        workspace: WeakEntity<Workspace>,
        _: &mut Window,
        cx: &mut Context<SettingsProfileSelector>,
    ) -> Self {
        let settings_store = cx.global::<SettingsStore>();
        let mut profile_names: Vec<Option<String>> = settings_store
            .configured_settings_profiles()
            .map(|s| Some(s.to_string()))
            .collect();
        profile_names.insert(0, None);

        let matches = profile_names
            .iter()
            .enumerate()
            .map(|(ix, profile_name)| StringMatch {
                candidate_id: ix,
                score: 0.0,
                positions: Default::default(),
                string: display_name(profile_name),
            })
            .collect();

        let profile_name = cx
            .try_global::<ActiveSettingsProfileName>()
            .map(|p| p.0.clone());

        let mut this = Self {
            matches,
            profile_names,
            selected_index: 0,
            selector,
            workspace,
        };

        if let Some(profile_name) = profile_name {
            this.select_if_matching(&profile_name);
        }

        this
    }

    fn select_if_matching(&mut self, profile_name: &str) {
        self.selected_index = self
            .matches
            .iter()
            .position(|mat| mat.string == profile_name)
            .unwrap_or(self.selected_index);
    }

    fn selected_profile(&self) -> Option<Option<String>> {
        let mat = self.matches.get(self.selected_index)?;
        self.profile_names.get(mat.candidate_id).cloned()
    }

    fn apply_profile(profile_name: Option<String>, cx: &mut App) {
        match profile_name {
            Some(profile_name) => cx.set_global(ActiveSettingsProfileName(profile_name)),
            None => {
                if cx.has_global::<ActiveSettingsProfileName>() {
                    cx.remove_global::<ActiveSettingsProfileName>();
                }
            }
        }
    }

    fn show_active_toast(
        &self,
        profile_name: Option<String>,
        remembered: bool,
        cx: &mut Context<Picker<Self>>,
    ) {
        let state_directory = cx
            .try_global::<ProfileStateDirectory>()
            .map(|directory| directory.0.clone());
        let message = match (&profile_name, remembered) {
            (Some(name), false) => format!("Profile {name} active"),
            (Some(name), true) => format!("Profile {name} active and remembered on this machine"),
            (None, false) => "No profile active".to_string(),
            (None, true) => "No profile active and remembered on this machine".to_string(),
        };
        let mut toast = Toast::new(NotificationId::unique::<ProfileToastId>(), message).autohide();
        if !remembered && let Some(state_directory) = state_directory {
            toast = toast.on_click("Remember on this machine", move |_, _| {
                remember_profile(&state_directory, profile_name.as_deref()).log_err();
            });
        }
        self.workspace
            .update(cx, |workspace, cx| workspace.show_toast(toast, cx))
            .ok();
    }
}

impl PickerDelegate for SettingsProfileSelectorDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "settings profile selector"
    }

    fn placeholder_text(&self, _: &mut Window, _: &mut App) -> std::sync::Arc<str> {
        "Select a settings profile...".into()
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(
        &mut self,
        ix: usize,
        _: &mut Window,
        _: &mut Context<Picker<SettingsProfileSelectorDelegate>>,
    ) {
        self.selected_index = ix;
    }

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<SettingsProfileSelectorDelegate>>,
    ) -> Task<()> {
        let background = cx.background_executor().clone();
        let candidates = self
            .profile_names
            .iter()
            .enumerate()
            .map(|(id, profile_name)| StringMatchCandidate::new(id, &display_name(profile_name)))
            .collect::<Vec<_>>();

        cx.spawn_in(window, async move |this, cx| {
            let matches = if query.is_empty() {
                candidates
                    .into_iter()
                    .enumerate()
                    .map(|(index, candidate)| StringMatch {
                        candidate_id: index,
                        string: candidate.string,
                        positions: Vec::new(),
                        score: 0.0,
                    })
                    .collect()
            } else {
                match_strings(
                    &candidates,
                    &query,
                    false,
                    true,
                    100,
                    &Default::default(),
                    background,
                )
                .await
            };

            this.update_in(cx, |this, _, _| {
                this.delegate.matches = matches;
                this.delegate.selected_index = this
                    .delegate
                    .selected_index
                    .min(this.delegate.matches.len().saturating_sub(1));
            })
            .ok();
        })
    }

    fn confirm(
        &mut self,
        secondary: bool,
        _: &mut Window,
        cx: &mut Context<Picker<SettingsProfileSelectorDelegate>>,
    ) {
        if let Some(profile_name) = self.selected_profile() {
            Self::apply_profile(profile_name.clone(), cx);
            let mut remembered = false;
            if secondary {
                if let Some(state_directory) = cx.try_global::<ProfileStateDirectory>() {
                    remembered = remember_profile(&state_directory.0, profile_name.as_deref())
                        .log_err()
                        .is_some();
                }
            }
            self.show_active_toast(profile_name, remembered, cx);
        }
        self.selector
            .update(cx, |_, cx| {
                cx.emit(DismissEvent);
            })
            .ok();
    }

    fn dismissed(
        &mut self,
        _: &mut Window,
        cx: &mut Context<Picker<SettingsProfileSelectorDelegate>>,
    ) {
        self.selector.update(cx, |_, cx| cx.emit(DismissEvent)).ok();
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _: &mut Window,
        _: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let mat = &self.matches.get(ix)?;
        let profile_name = &self.profile_names.get(mat.candidate_id)?;

        Some(
            ListItem::new(ix)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .child(HighlightedLabel::new(
                    display_name(profile_name),
                    mat.positions.clone(),
                )),
        )
    }
}

fn display_name(profile_name: &Option<String>) -> String {
    profile_name.clone().unwrap_or(NO_PROFILE.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use editor;
    use gpui::{TestAppContext, UpdateGlobal, VisualTestContext};
    use menu::{Cancel, Confirm, SecondaryConfirm, SelectNext, SelectPrevious};
    use project::{FakeFs, Project};
    use serde_json::json;
    use settings::Settings;
    use tempfile::TempDir;
    use theme_settings::ThemeSettings;
    use workspace::{self, AppState, MultiWorkspace};
    use zed_actions::settings::SwitchProfile;

    async fn init_test(
        user_settings_json: Option<serde_json::Value>,
        cx: &mut TestAppContext,
    ) -> (Entity<Workspace>, &mut VisualTestContext) {
        cx.update(|cx| {
            let state = AppState::test(cx);
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            settings::init(cx);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            super::init(cx);
            editor::init(cx);
            state
        });

        if let Some(user_settings_json) = user_settings_json {
            set_user_settings(&user_settings_json, cx);
        }

        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, ["/test".as_ref()], cx).await;
        let window = cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let cx = VisualTestContext::from_window(*window, cx).into_mut();
        let workspace = window
            .read_with(cx, |mw, _| mw.workspace().clone())
            .unwrap();

        cx.update(|_, cx| {
            assert!(!cx.has_global::<ActiveSettingsProfileName>());
        });

        (workspace, cx)
    }

    fn set_user_settings(user_settings_json: &serde_json::Value, cx: &mut TestAppContext) {
        cx.update(|cx| {
            SettingsStore::update_global(cx, |store, cx| {
                store
                    .set_user_settings(&user_settings_json.to_string(), cx)
                    .unwrap();
            });
        });
    }

    fn active_profile(cx: &mut VisualTestContext) -> Option<String> {
        cx.update(|_, cx| {
            cx.try_global::<ActiveSettingsProfileName>()
                .map(|profile| profile.0.clone())
        })
    }

    fn font_size(cx: &mut VisualTestContext) -> Pixels {
        cx.update(|_, cx| ThemeSettings::get_global(cx).buffer_font_size(cx))
    }

    fn has_profile_toast(workspace: &Entity<Workspace>, cx: &mut VisualTestContext) -> bool {
        workspace.update(cx, |workspace, _| {
            workspace
                .notification_ids()
                .contains(&NotificationId::unique::<ProfileToastId>())
        })
    }

    #[track_caller]
    fn active_settings_profile_picker(
        workspace: &Entity<Workspace>,
        cx: &mut VisualTestContext,
    ) -> Entity<Picker<SettingsProfileSelectorDelegate>> {
        workspace.update(cx, |workspace, cx| {
            workspace
                .active_modal::<SettingsProfileSelector>(cx)
                .expect("settings profile selector is not open")
                .read(cx)
                .picker
                .clone()
        })
    }

    fn two_profiles() -> serde_json::Value {
        json!({
            "buffer_font_size": 10.0,
            "profiles": {
                "Classroom / Streaming": { "settings": { "buffer_font_size": 20.0 } },
                "Demo Videos": { "settings": { "buffer_font_size": 15.0 } }
            }
        })
    }

    #[gpui::test]
    async fn test_settings_profile_selector_state(cx: &mut TestAppContext) {
        let (workspace, cx) = init_test(Some(two_profiles()), cx).await;

        cx.dispatch_action(SwitchProfile);
        let picker = active_settings_profile_picker(&workspace, cx);
        picker.read_with(cx, |picker, _| {
            assert_eq!(picker.delegate.matches.len(), 3);
            assert_eq!(picker.delegate.matches[0].string, display_name(&None));
            assert_eq!(picker.delegate.matches[1].string, "Classroom / Streaming");
            assert_eq!(picker.delegate.matches[2].string, "Demo Videos");
            assert_eq!(picker.delegate.selected_index, 0);
        });

        cx.dispatch_action(Confirm);
        assert_eq!(active_profile(cx), None);
        assert!(has_profile_toast(&workspace, cx));

        // Moving through the list changes nothing; cancelling keeps the active profile.
        cx.dispatch_action(SwitchProfile);
        cx.dispatch_action(SelectNext);
        assert_eq!(active_profile(cx), None);
        assert_eq!(font_size(cx), px(10.0));
        cx.dispatch_action(Cancel);
        assert_eq!(active_profile(cx), None);
        assert_eq!(font_size(cx), px(10.0));

        cx.dispatch_action(SwitchProfile);
        cx.dispatch_action(SelectNext);
        cx.dispatch_action(Confirm);
        assert_eq!(active_profile(cx).as_deref(), Some("Classroom / Streaming"));
        assert_eq!(font_size(cx), px(20.0));

        // Reopening selects the active profile; moving and cancelling keeps it.
        cx.dispatch_action(SwitchProfile);
        let picker = active_settings_profile_picker(&workspace, cx);
        picker.read_with(cx, |picker, _| {
            assert_eq!(picker.delegate.selected_index, 1)
        });
        cx.dispatch_action(SelectNext);
        picker.read_with(cx, |picker, _| {
            assert_eq!(picker.delegate.selected_index, 2)
        });
        assert_eq!(active_profile(cx).as_deref(), Some("Classroom / Streaming"));
        assert_eq!(font_size(cx), px(20.0));
        cx.dispatch_action(Cancel);
        assert_eq!(active_profile(cx).as_deref(), Some("Classroom / Streaming"));
        assert_eq!(font_size(cx), px(20.0));

        cx.dispatch_action(SwitchProfile);
        cx.dispatch_action(SelectNext);
        cx.dispatch_action(Confirm);
        assert_eq!(active_profile(cx).as_deref(), Some("Demo Videos"));
        assert_eq!(font_size(cx), px(15.0));

        cx.dispatch_action(SwitchProfile);
        cx.dispatch_action(SelectPrevious);
        cx.dispatch_action(SelectPrevious);
        assert_eq!(active_profile(cx).as_deref(), Some("Demo Videos"));
        cx.dispatch_action(Confirm);
        assert_eq!(active_profile(cx), None);
        assert_eq!(font_size(cx), px(10.0));
    }

    #[gpui::test]
    async fn test_selection_move_changes_nothing(cx: &mut TestAppContext) {
        let (workspace, cx) = init_test(Some(two_profiles()), cx).await;

        cx.dispatch_action(SwitchProfile);
        let picker = active_settings_profile_picker(&workspace, cx);
        cx.dispatch_action(SelectNext);
        cx.dispatch_action(SelectNext);
        cx.dispatch_action(SelectPrevious);
        picker.read_with(cx, |picker, _| {
            assert_eq!(picker.delegate.selected_index, 1)
        });
        assert_eq!(active_profile(cx), None);
        assert_eq!(font_size(cx), px(10.0));
        assert!(!has_profile_toast(&workspace, cx));

        cx.dispatch_action(Cancel);
        assert_eq!(active_profile(cx), None);
        assert!(!has_profile_toast(&workspace, cx));
    }

    #[gpui::test]
    async fn test_confirm_remembers_only_on_request(cx: &mut TestAppContext) {
        let state_directory = TempDir::new().unwrap();
        let (workspace, cx) = init_test(Some(two_profiles()), cx).await;
        cx.update(|_, cx| restore_active_profile(state_directory.path(), cx));

        cx.dispatch_action(SwitchProfile);
        cx.dispatch_action(SelectNext);
        cx.dispatch_action(Confirm);
        assert_eq!(active_profile(cx).as_deref(), Some("Classroom / Streaming"));
        assert!(has_profile_toast(&workspace, cx));
        assert_eq!(read_remembered_profile(state_directory.path()), None);

        // Secondary confirm applies and remembers in one step.
        cx.dispatch_action(SwitchProfile);
        cx.dispatch_action(SelectNext);
        cx.dispatch_action(SecondaryConfirm);
        assert_eq!(active_profile(cx).as_deref(), Some("Demo Videos"));
        assert_eq!(
            read_remembered_profile(state_directory.path()).as_deref(),
            Some("Demo Videos")
        );

        // Choosing "No profile" the same way removes the remembered profile.
        cx.dispatch_action(SwitchProfile);
        cx.dispatch_action(SelectPrevious);
        cx.dispatch_action(SelectPrevious);
        cx.dispatch_action(SecondaryConfirm);
        assert_eq!(active_profile(cx), None);
        assert_eq!(read_remembered_profile(state_directory.path()), None);
        assert!(!state_directory.path().join(ACTIVE_PROFILE_FILE).exists());
    }

    #[gpui::test]
    async fn test_settings_profile_with_user_base(cx: &mut TestAppContext) {
        let user_settings_json = json!({
            "buffer_font_size": 10.0,
            "profiles": {
                "Explicit User": {
                    "base": "user",
                    "settings": {
                        "buffer_font_size": 20.0
                    }
                },
                "Implicit User": {
                    "settings": {
                        "buffer_font_size": 25.0
                    }
                }
            }
        });
        let (_, cx) = init_test(Some(user_settings_json), cx).await;

        cx.dispatch_action(SwitchProfile);
        cx.dispatch_action(SelectNext);
        assert_eq!(font_size(cx), px(10.0));
        cx.dispatch_action(Confirm);
        assert_eq!(active_profile(cx).as_deref(), Some("Explicit User"));
        assert_eq!(font_size(cx), px(20.0));

        // No base specified behaves the same as "user".
        cx.dispatch_action(SwitchProfile);
        cx.dispatch_action(SelectNext);
        cx.dispatch_action(Confirm);
        assert_eq!(active_profile(cx).as_deref(), Some("Implicit User"));
        assert_eq!(font_size(cx), px(25.0));
    }

    #[gpui::test]
    async fn test_settings_profile_with_default_base(cx: &mut TestAppContext) {
        let user_settings_json = json!({
            "buffer_font_size": 10.0,
            "profiles": {
                "Clean Slate": {
                    "base": "default"
                },
                "Custom on Defaults": {
                    "base": "default",
                    "settings": {
                        "buffer_font_size": 30.0
                    }
                }
            }
        });
        let (_, cx) = init_test(Some(user_settings_json), cx).await;

        // User has buffer_font_size: 10, factory default is 15.
        assert_eq!(font_size(cx), px(10.0));

        // "Clean Slate" has base: "default" with no settings overrides,
        // so we get the factory default (15), not the user's value (10).
        cx.dispatch_action(SwitchProfile);
        cx.dispatch_action(SelectNext);
        cx.dispatch_action(Confirm);
        assert_eq!(active_profile(cx).as_deref(), Some("Clean Slate"));
        assert_eq!(font_size(cx), px(15.0));

        // "Custom on Defaults" has base: "default" with buffer_font_size: 30,
        // so the profile's override (30) applies on top of the factory default,
        // not on top of the user's value (10).
        cx.dispatch_action(SwitchProfile);
        cx.dispatch_action(SelectNext);
        cx.dispatch_action(Confirm);
        assert_eq!(active_profile(cx).as_deref(), Some("Custom on Defaults"));
        assert_eq!(font_size(cx), px(30.0));
    }

    #[gpui::test]
    async fn test_settings_profile_selector_is_in_user_configuration_order(
        cx: &mut TestAppContext,
    ) {
        // Must be unique names (HashMap)
        let user_settings_json = json!({
            "profiles": {
                "z": { "settings": {} },
                "e": { "settings": {} },
                "d": { "settings": {} },
                " ": { "settings": {} },
                "r": { "settings": {} },
                "u": { "settings": {} },
                "l": { "settings": {} },
                "3": { "settings": {} },
                "s": { "settings": {} },
                "!": { "settings": {} },
            }
        });
        let (workspace, cx) = init_test(Some(user_settings_json), cx).await;

        cx.dispatch_action(SwitchProfile);
        let picker = active_settings_profile_picker(&workspace, cx);

        picker.read_with(cx, |picker, _| {
            assert_eq!(picker.delegate.matches.len(), 11);
            assert_eq!(picker.delegate.matches[0].string, display_name(&None));
            assert_eq!(picker.delegate.matches[1].string, "z");
            assert_eq!(picker.delegate.matches[2].string, "e");
            assert_eq!(picker.delegate.matches[3].string, "d");
            assert_eq!(picker.delegate.matches[4].string, " ");
            assert_eq!(picker.delegate.matches[5].string, "r");
            assert_eq!(picker.delegate.matches[6].string, "u");
            assert_eq!(picker.delegate.matches[7].string, "l");
            assert_eq!(picker.delegate.matches[8].string, "3");
            assert_eq!(picker.delegate.matches[9].string, "s");
            assert_eq!(picker.delegate.matches[10].string, "!");
        });
    }

    #[test]
    fn test_state_file_round_trip() {
        let directory = TempDir::new().unwrap();
        let state_directory = directory.path().join("state");

        assert_eq!(read_remembered_profile(&state_directory), None);

        write_remembered_profile(&state_directory, "kyle-job").unwrap();
        assert_eq!(
            read_remembered_profile(&state_directory).as_deref(),
            Some("kyle-job")
        );
        let contents = std::fs::read_to_string(state_directory.join(ACTIVE_PROFILE_FILE)).unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&contents).unwrap(),
            json!({ "version": 1, "profile": "kyle-job" })
        );
        assert!(
            !state_directory
                .join(format!("{ACTIVE_PROFILE_FILE}.tmp"))
                .exists()
        );

        write_remembered_profile(&state_directory, "home").unwrap();
        assert_eq!(
            read_remembered_profile(&state_directory).as_deref(),
            Some("home")
        );

        delete_remembered_profile(&state_directory).unwrap();
        assert_eq!(read_remembered_profile(&state_directory), None);
        delete_remembered_profile(&state_directory).unwrap();
    }

    #[test]
    fn test_corrupt_state_file_is_ignored() {
        let directory = TempDir::new().unwrap();
        let path = directory.path().join(ACTIVE_PROFILE_FILE);

        for contents in [
            "",
            "not json",
            "{\"version\": 1}",
            "{\"version\": 2, \"profile\": \"kyle-job\"}",
            "{\"version\": 1, \"profile\": \"\"}",
            "{\"version\": 1, \"profile\": 7}",
        ] {
            std::fs::write(&path, contents).unwrap();
            assert_eq!(
                read_remembered_profile(directory.path()),
                None,
                "{contents}"
            );
        }

        // A directory in place of the file is unreadable, not an error.
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert_eq!(read_remembered_profile(directory.path()), None);
    }

    #[gpui::test]
    async fn test_restore_applies_remembered_profile(cx: &mut TestAppContext) {
        let state_directory = TempDir::new().unwrap();
        write_remembered_profile(state_directory.path(), "Demo Videos").unwrap();
        let (_, cx) = init_test(None, cx).await;

        cx.update(|_, cx| restore_active_profile(state_directory.path(), cx));
        assert_eq!(active_profile(cx).as_deref(), Some("Demo Videos"));

        // The profile is defined once the user settings load.
        cx.update(|_, cx| {
            SettingsStore::update_global(cx, |store, cx| {
                store
                    .set_user_settings(&two_profiles().to_string(), cx)
                    .unwrap();
            });
        });
        assert_eq!(font_size(cx), px(15.0));
    }

    #[gpui::test]
    async fn test_unknown_remembered_profile_is_reported_once(cx: &mut TestAppContext) {
        let state_directory = TempDir::new().unwrap();
        write_remembered_profile(state_directory.path(), "ghost").unwrap();
        let (workspace, cx) = init_test(None, cx).await;
        let notification_id = NotificationId::unique::<UnknownRememberedProfileId>();

        cx.update(|_, cx| restore_active_profile(state_directory.path(), cx));
        cx.run_until_parked();

        // The user settings have not loaded yet, so nothing is reported.
        assert_eq!(active_profile(cx).as_deref(), Some("ghost"));
        let reported = |workspace: &Entity<Workspace>, cx: &mut VisualTestContext| {
            workspace.update(cx, |workspace, _| {
                workspace.notification_ids().contains(&notification_id)
            })
        };
        assert!(!reported(&workspace, cx));

        cx.update(|_, cx| {
            SettingsStore::update_global(cx, |store, cx| {
                store
                    .set_user_settings(&two_profiles().to_string(), cx)
                    .unwrap();
            });
        });
        cx.run_until_parked();
        assert!(reported(&workspace, cx));
        // Base settings apply while the name is unknown.
        assert_eq!(font_size(cx), px(10.0));

        workspace.update(cx, |workspace, cx| {
            workspace.dismiss_notification(&notification_id, cx)
        });
        cx.update(|_, cx| workspace::notifications::dismiss_app_notification(&notification_id, cx));
        cx.run_until_parked();
        cx.update(|_, cx| {
            SettingsStore::update_global(cx, |store, cx| {
                store
                    .set_user_settings(&two_profiles().to_string(), cx)
                    .unwrap();
            });
        });
        cx.run_until_parked();
        assert!(!reported(&workspace, cx));
    }

    #[gpui::test]
    async fn test_known_remembered_profile_is_not_reported(cx: &mut TestAppContext) {
        let (_, cx) = init_test(Some(two_profiles()), cx).await;
        cx.update(|_, cx| {
            let store = cx.global::<SettingsStore>();
            assert_eq!(unknown_profile_message("Demo Videos", store), None);
            let message = unknown_profile_message("ghost", store).unwrap();
            assert_eq!(
                message,
                "Profile ghost is remembered on this machine but not defined; using base settings"
            );
        });
    }
}
