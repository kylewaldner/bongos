mod components;

use std::any::TypeId;
use std::sync::Arc;

use cloud_api_types::ExtensionProvides;
use command_palette_hooks::CommandPaletteFilter;
use editor::{Editor, EditorElement, EditorStyle};
use extension_host::{ExtensionManifest, ExtensionStore};
use fuzzy::{StringMatch, StringMatchCandidate, match_strings};
use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, Focusable, InteractiveElement, KeyContext,
    ParentElement, Render, Styled, Task, TaskExt, TextStyle, UniformListScrollHandle, Window,
    actions, point, uniform_list,
};
use picker::{Picker, PickerDelegate};
use project::DirectoryLister;
use schemars::JsonSchema;
use serde::Deserialize;
use settings::Settings;
use strum::IntoEnumIterator as _;
use theme_settings::ThemeSettings;
use ui::{ListItem, ListItemSpacing, ScrollableHandle as _, Tooltip, WithScrollbar, prelude::*};
use util::ResultExt;
use workspace::{
    Workspace,
    item::{Item, ItemEvent},
    workspace_error::{ErrorAction, ErrorSeverity, WorkspaceError},
};
use zed_actions::ExtensionCategoryFilter;

use crate::components::ExtensionCard;

actions!(
    zed,
    [
        /// Installs an extension from a local directory for development.
        InstallDevExtension,
    ]
);

/// Rebuilds an installed dev extension.
#[derive(Clone, Debug, Default, PartialEq, Deserialize, JsonSchema, gpui::Action)]
#[action(namespace = zed)]
#[serde(deny_unknown_fields)]
pub struct RebuildDevExtension {
    /// The ID of the dev extension to rebuild.
    ///
    /// Default: opens a picker if multiple dev extensions are installed.
    #[serde(default)]
    pub extension_id: Option<String>,
}

#[derive(Default)]
struct DevExtensionNotInstalledError {
    extension_id: Option<SharedString>,
}

impl WorkspaceError for DevExtensionNotInstalledError {
    fn primary_message(&self) -> SharedString {
        match &self.extension_id {
            Some(extension_id) => {
                format!("Dev extension '{extension_id}' is not installed.").into()
            }
            None => "No dev extensions are installed.".into(),
        }
    }

    fn primary_action(&self) -> ErrorAction {
        ErrorAction::new("Install Dev Extension", InstallDevExtension)
    }

    fn severity(&self) -> ErrorSeverity {
        ErrorSeverity::Warning
    }
}

fn update_rebuild_dev_extension_visibility(store: &Entity<ExtensionStore>, cx: &mut App) {
    let has_dev_extensions = store.read(cx).dev_extensions().next().is_some();
    CommandPaletteFilter::update_global(cx, |filter, _cx| {
        if has_dev_extensions {
            filter.show_action_types(&[TypeId::of::<RebuildDevExtension>()]);
        } else {
            filter.hide_action_types(&[TypeId::of::<RebuildDevExtension>()]);
        }
    });
}

pub fn init(cx: &mut App) {
    let store = ExtensionStore::global(cx);
    update_rebuild_dev_extension_visibility(&store, cx);
    cx.observe(&store, |store, cx| {
        update_rebuild_dev_extension_visibility(&store, cx);
    })
    .detach();

    cx.observe_new(move |workspace: &mut Workspace, window, _cx| {
        if window.is_none() {
            return;
        }
        workspace
            .register_action(
                move |workspace, action: &zed_actions::Extensions, window, cx| {
                    let provides_filter = action.category_filter.map(|category| match category {
                        ExtensionCategoryFilter::Themes => ExtensionProvides::Themes,
                        ExtensionCategoryFilter::IconThemes => ExtensionProvides::IconThemes,
                        ExtensionCategoryFilter::Languages => ExtensionProvides::Languages,
                        ExtensionCategoryFilter::Grammars => ExtensionProvides::Grammars,
                        ExtensionCategoryFilter::LanguageServers => {
                            ExtensionProvides::LanguageServers
                        }
                        ExtensionCategoryFilter::ContextServers => {
                            ExtensionProvides::ContextServers
                        }
                        ExtensionCategoryFilter::Snippets => ExtensionProvides::Snippets,
                        ExtensionCategoryFilter::DebugAdapters => ExtensionProvides::DebugAdapters,
                    });

                    let existing = workspace
                        .active_pane()
                        .read(cx)
                        .items()
                        .find_map(|item| item.downcast::<ExtensionsPage>());

                    if let Some(existing) = existing {
                        existing.update(cx, |extensions_page, cx| {
                            if provides_filter.is_some() {
                                extensions_page.change_provides_filter(provides_filter, cx);
                            }
                            if let Some(id) = action.id.as_ref() {
                                extensions_page.focus_extension(id, window, cx);
                            }
                        });

                        workspace.activate_item(&existing, true, true, window, cx);
                    } else {
                        let extensions_page =
                            ExtensionsPage::new(provides_filter, action.id.as_deref(), window, cx);
                        workspace.add_item_to_active_pane(
                            Box::new(extensions_page),
                            None,
                            true,
                            window,
                            cx,
                        )
                    }
                },
            )
            .register_action(move |workspace, _: &InstallDevExtension, window, cx| {
                let store = ExtensionStore::global(cx);
                let prompt = workspace.prompt_for_open_path(
                    gpui::PathPromptOptions {
                        files: false,
                        directories: true,
                        multiple: false,
                        prompt: None,
                    },
                    DirectoryLister::Local(
                        workspace.project().clone(),
                        workspace.app_state().fs.clone(),
                    ),
                    window,
                    cx,
                );

                let workspace_handle = cx.entity().downgrade();
                window
                    .spawn(cx, async move |cx| {
                        let extension_path = match prompt.await.map_err(anyhow::Error::from) {
                            Ok(Some(mut paths)) => paths.pop()?,
                            Ok(None) => return None,
                            Err(err) => {
                                workspace_handle
                                    .update(cx, |workspace, cx| {
                                        workspace.show_error(
                                            workspace::workspace_error::PortalError::new(
                                                err.to_string(),
                                            ),
                                            cx,
                                        );
                                    })
                                    .ok();
                                return None;
                            }
                        };

                        let install_task = store.update(cx, |store, cx| {
                            store.install_dev_extension(extension_path, cx)
                        });

                        match install_task.await {
                            Ok(_) => {}
                            Err(err) => {
                                log::error!("Failed to install dev extension: {:?}", err);
                                workspace_handle
                                    .update(cx, |workspace, cx| {
                                        // NOTE: using `anyhow::context` here ends up not printing
                                        // the error
                                        workspace.show_error(
                                            format!("Failed to install dev extension: {}", err),
                                            cx,
                                        );
                                    })
                                    .ok();
                            }
                        }

                        Some(())
                    })
                    .detach();
            })
            .register_action(move |workspace, action: &RebuildDevExtension, window, cx| {
                if let Some(target_id) = action.extension_id.as_deref() {
                    let extension_id = ExtensionStore::global(cx)
                        .read(cx)
                        .dev_extensions()
                        .find_map(|m| {
                            if m.id.as_ref() == target_id {
                                Some(m.id.clone())
                            } else {
                                None
                            }
                        });
                    if let Some(extension_id) = extension_id {
                        ExtensionStore::global(cx).update(cx, |store, cx| {
                            store.rebuild_dev_extension(extension_id, cx);
                        });
                    } else {
                        workspace.show_error(
                            DevExtensionNotInstalledError {
                                extension_id: Some(SharedString::from(target_id.to_owned())),
                            },
                            cx,
                        );
                    }
                    return;
                }

                let dev_extensions = ExtensionStore::global(cx)
                    .read(cx)
                    .dev_extensions()
                    .cloned()
                    .collect::<Vec<_>>();

                match dev_extensions.len() {
                    0 => {
                        workspace.show_error(DevExtensionNotInstalledError::default(), cx);
                    }
                    1 => {
                        let extension_id = dev_extensions[0].id.clone();
                        ExtensionStore::global(cx).update(cx, |store, cx| {
                            store.rebuild_dev_extension(extension_id, cx);
                        });
                    }
                    _ => {
                        workspace.toggle_modal(window, cx, |window, cx| {
                            let delegate = DevExtensionRebuildPickerDelegate::new(dev_extensions);
                            Picker::uniform_list(delegate, window, cx)
                        });
                    }
                }
            });
    })
    .detach();
}
fn extension_provides_label(provides: ExtensionProvides) -> &'static str {
    match provides {
        ExtensionProvides::Themes => "Themes",
        ExtensionProvides::IconThemes => "Icon Themes",
        ExtensionProvides::Languages => "Languages",
        ExtensionProvides::Grammars => "Grammars",
        ExtensionProvides::LanguageServers => "Language Servers",
        ExtensionProvides::ContextServers => "MCP Servers",
        ExtensionProvides::AgentServers => "Agent Servers",
        ExtensionProvides::SlashCommands => "Slash Commands",
        ExtensionProvides::IndexedDocsProviders => "Indexed Docs Providers",
        ExtensionProvides::Snippets => "Snippets",
        ExtensionProvides::DebugAdapters => "Debug Adapters",
    }
}

pub struct ExtensionsPage {
    list: UniformListScrollHandle,
    entries: Vec<(Arc<ExtensionManifest>, bool)>,
    query_editor: Entity<Editor>,
    provides_filter: Option<ExtensionProvides>,
    _subscriptions: [gpui::Subscription; 2],
}

impl ExtensionsPage {
    pub fn new(
        provides_filter: Option<ExtensionProvides>,
        focus_extension_id: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<Self> {
        cx.new(|cx| {
            let store = ExtensionStore::global(cx);
            let subscriptions = [
                cx.observe(&store, |_: &mut Self, _, cx| cx.notify()),
                cx.subscribe(&store, |this, _, event, cx| {
                    if let extension_host::Event::ExtensionsUpdated = event {
                        this.refresh_entries(cx);
                    }
                }),
            ];

            let query_editor = cx.new(|cx| {
                let mut input = Editor::single_line(window, cx);
                input.set_placeholder_text("Search installed extensions...", window, cx);
                if let Some(id) = focus_extension_id {
                    input.set_text(id, window, cx);
                }
                input
            });
            cx.subscribe(&query_editor, Self::on_query_change).detach();

            let mut this = Self {
                list: UniformListScrollHandle::new(),
                entries: Vec::new(),
                query_editor,
                provides_filter,
                _subscriptions: subscriptions,
            };
            this.refresh_entries(cx);
            this
        })
    }

    fn refresh_entries(&mut self, cx: &mut Context<Self>) {
        let query = self.search_query(cx).map(|query| query.to_lowercase());
        let provides_filter = self.provides_filter;
        let entries = ExtensionStore::global(cx)
            .read(cx)
            .installed_extensions()
            .values()
            .filter(|entry| match provides_filter {
                Some(provides) => entry.manifest.provides().contains(&provides),
                None => true,
            })
            .filter(|entry| match &query {
                Some(query) => {
                    entry.manifest.name.to_lowercase().contains(query)
                        || entry.manifest.id.to_lowercase().contains(query)
                }
                None => true,
            })
            .map(|entry| (entry.manifest.clone(), entry.dev))
            .collect();
        self.entries = entries;
        self.list.set_offset(point(px(0.), px(0.)));
        cx.notify();
    }

    fn render_extensions(
        &mut self,
        range: std::ops::Range<usize>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> Vec<ExtensionCard> {
        range
            .filter_map(|ix| {
                let (manifest, is_dev) = self.entries.get(ix)?.clone();
                Some(self.render_installed_extension(&manifest, is_dev, cx))
            })
            .collect()
    }

    fn render_installed_extension(
        &self,
        extension: &ExtensionManifest,
        is_dev: bool,
        cx: &mut Context<Self>,
    ) -> ExtensionCard {
        let busy = ExtensionStore::global(cx)
            .read(cx)
            .outstanding_operations()
            .contains_key(extension.id.as_ref());

        let repository_url = extension.repository.clone();

        let can_configure = !extension.context_servers.is_empty();

        ExtensionCard::new()
            .child(
                h_flex()
                    .justify_between()
                    .child(
                        h_flex()
                            .gap_2()
                            .items_end()
                            .child(Headline::new(extension.name.clone()).size(HeadlineSize::Medium))
                            .child(
                                Headline::new(format!("v{}", extension.version))
                                    .size(HeadlineSize::XSmall),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .justify_between()
                            .when(is_dev, |this| {
                                this.child(
                                    Button::new(
                                        SharedString::from(format!("rebuild-{}", extension.id)),
                                        "Rebuild",
                                    )
                                    .color(Color::Accent)
                                    .disabled(busy)
                                    .on_click({
                                        let extension_id = extension.id.clone();
                                        move |_, _, cx| {
                                            ExtensionStore::global(cx).update(cx, |store, cx| {
                                                store.rebuild_dev_extension(extension_id.clone(), cx)
                                            });
                                        }
                                    }),
                                )
                            })
                            .child(
                                Button::new(SharedString::from(format!("uninstall-{}", extension.id)), "Uninstall")
                                    .color(Color::Accent)
                                    .disabled(busy)
                                    .on_click({
                                        let extension_id = extension.id.clone();
                                        move |_, _, cx| {
                                            ExtensionStore::global(cx).update(cx, |store, cx| {
                                                store.uninstall_extension(extension_id.clone(), cx).detach_and_log_err(cx);
                                            });
                                        }
                                    }),
                            )
                            .when(can_configure, |this| {
                                this.child(
                                    Button::new(
                                        SharedString::from(format!("configure-{}", extension.id)),
                                        "Configure",
                                    )
                                    .color(Color::Accent)
                                    .disabled(busy)
                                    .on_click({
                                        let manifest = Arc::new(extension.clone());
                                        move |_, _, cx| {
                                            if let Some(events) =
                                                extension::ExtensionEvents::try_global(cx)
                                            {
                                                events.update(cx, |this, cx| {
                                                    this.emit(
                                                        extension::Event::ConfigureExtensionRequested(
                                                            manifest.clone(),
                                                        ),
                                                        cx,
                                                    )
                                                });
                                            }
                                        }
                                    }),
                                )
                            }),
                    ),
            )
            .child(
                h_flex()
                    .gap_2()
                    .justify_between()
                    .child(
                        Label::new(format!(
                            "{}: {}",
                            if extension.authors.len() > 1 {
                                "Authors"
                            } else {
                                "Author"
                            },
                            extension.authors.join(", ")
                        ))
                        .size(LabelSize::Small)
                        .color(Color::Muted)
                        .truncate(),
                    )
                    .child(Label::new("<>").size(LabelSize::Small)),
            )
            .child(
                h_flex()
                    .gap_2()
                    .justify_between()
                    .children(extension.description.as_ref().map(|description| {
                        Label::new(description.clone())
                            .size(LabelSize::Small)
                            .color(Color::Default)
                            .truncate()
                    }))
                    .children(repository_url.map(|repository_url| {
                        IconButton::new(
                            SharedString::from(format!("repository-{}", extension.id)),
                            IconName::Github,
                        )
                        .icon_color(Color::Accent)
                        .icon_size(IconSize::Small)
                        .on_click(cx.listener({
                            let repository_url = repository_url.clone();
                            move |_, _, _, cx| {
                                cx.open_url(&repository_url);
                            }
                        }))
                        .tooltip(Tooltip::text(repository_url))
                    })),
            )
    }

    fn render_search(&self, cx: &mut Context<Self>) -> Div {
        let mut key_context = KeyContext::new_with_defaults();
        key_context.add("BufferSearchBar");

        h_flex()
            .key_context(key_context)
            .h_8()
            .min_w(rems_from_px(384.))
            .flex_1()
            .pl_1p5()
            .pr_2()
            .gap_2()
            .border_1()
            .border_color(cx.theme().colors().border)
            .rounded_md()
            .child(Icon::new(IconName::MagnifyingGlass).color(Color::Muted))
            .child(self.render_text_input(&self.query_editor, cx))
    }

    fn render_text_input(
        &self,
        editor: &Entity<Editor>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let settings = ThemeSettings::get_global(cx);
        let text_style = TextStyle {
            color: if editor.read(cx).read_only(cx) {
                cx.theme().colors().text_disabled
            } else {
                cx.theme().colors().text
            },
            font_family: settings.ui_font.family.clone(),
            font_features: settings.ui_font.features.clone(),
            font_fallbacks: settings.ui_font.fallbacks.clone(),
            font_size: rems(0.875).into(),
            font_weight: settings.ui_font.weight,
            line_height: relative(1.3),
            ..Default::default()
        };

        EditorElement::new(
            editor,
            EditorStyle {
                background: cx.theme().colors().editor_background,
                local_player: cx.theme().players().local(),
                text: text_style,
                ..Default::default()
            },
        )
    }

    fn on_query_change(
        &mut self,
        _: Entity<Editor>,
        event: &editor::EditorEvent,
        cx: &mut Context<Self>,
    ) {
        if let editor::EditorEvent::Edited { .. } = event {
            self.refresh_entries(cx);
        }
    }

    pub fn focus_extension(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.query_editor
            .update(cx, |editor, cx| editor.set_text(id, window, cx));
        self.refresh_entries(cx);
    }

    pub fn change_provides_filter(
        &mut self,
        provides_filter: Option<ExtensionProvides>,
        cx: &mut Context<Self>,
    ) {
        self.provides_filter = provides_filter;
        self.refresh_entries(cx);
    }

    pub fn search_query(&self, cx: &mut App) -> Option<String> {
        let search = self.query_editor.read(cx).text(cx);
        if search.trim().is_empty() {
            None
        } else {
            Some(search)
        }
    }

    fn render_empty_state(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let message = if self.search_query(cx).is_some() || self.provides_filter.is_some() {
            "No installed extensions that match your search."
        } else {
            "No installed extensions."
        };

        h_flex().py_4().gap_1p5().child(Label::new(message))
    }
}

struct DevExtensionRebuildPickerDelegate {
    entries: Vec<Arc<ExtensionManifest>>,
    matches: Vec<StringMatch>,
    selected_index: usize,
}

impl DevExtensionRebuildPickerDelegate {
    fn new(manifests: Vec<Arc<ExtensionManifest>>) -> Self {
        let matches = manifests
            .iter()
            .enumerate()
            .map(|(ix, manifest)| StringMatch {
                candidate_id: ix,
                score: 0.0,
                positions: Vec::new(),
                string: manifest.name.clone(),
            })
            .collect();

        Self {
            entries: manifests,
            matches,
            selected_index: 0,
        }
    }
}

impl PickerDelegate for DevExtensionRebuildPickerDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "dev-extension-rebuild"
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
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) {
        self.selected_index = ix;
    }

    fn selected_index_changed(
        &self,
        _ix: usize,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Option<Box<dyn Fn(&mut Window, &mut App) + 'static>> {
        None
    }

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let background = cx.background_executor().clone();
        let candidates = self
            .entries
            .iter()
            .enumerate()
            .map(|(ix, manifest)| StringMatchCandidate::new(ix, manifest.name.as_ref()))
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

            this.update(cx, |this, _cx| {
                this.delegate.matches = matches;
                this.delegate.selected_index = this
                    .delegate
                    .selected_index
                    .min(this.delegate.matches.len().saturating_sub(1));
            })
            .log_err();
        })
    }

    fn confirm(&mut self, _secondary: bool, _window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(mat) = self.matches.get(self.selected_index) else {
            return;
        };

        let extension_id = self.entries[mat.candidate_id].id.clone();
        ExtensionStore::global(cx).update(cx, |store, cx| {
            store.rebuild_dev_extension(extension_id, cx);
        });

        cx.emit(DismissEvent);
    }

    fn dismissed(&mut self, _window: &mut Window, _cx: &mut Context<Picker<Self>>) {}

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        Arc::from("Rebuild dev extension…")
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let mat = self.matches.get(ix)?;
        let entry = self.entries.get(mat.candidate_id)?;

        let item = ListItem::new(("dev-extension-list-item", mat.candidate_id))
            .inset(true)
            .spacing(ListItemSpacing::Sparse)
            .toggle_state(selected)
            .child(
                h_flex()
                    .w_full()
                    .py_px()
                    .justify_between()
                    .gap_2()
                    .child(Label::new(entry.name.clone()))
                    .child(
                        Label::new(format!("{} • v{}", entry.id, entry.version))
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
            );

        Some(item)
    }

    fn no_matches_text(&self, _window: &mut Window, _cx: &mut App) -> Option<SharedString> {
        Some("No dev extensions found".into())
    }
}
impl Render for ExtensionsPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .size_full()
            .bg(cx.theme().colors().editor_background)
            .child(
                v_flex()
                    .gap_4()
                    .pt_4()
                    .px_4()
                    .bg(cx.theme().colors().editor_background)
                    .child(
                        h_flex()
                            .w_full()
                            .gap_1p5()
                            .justify_between()
                            .child(Headline::new("Extensions").size(HeadlineSize::Large))
                            .child(
                                Button::new("install-dev-extension", "Install Dev Extension")
                                    .style(ButtonStyle::Outlined)
                                    .size(ButtonSize::Medium)
                                    .on_click(|_event, window, cx| {
                                        window.dispatch_action(Box::new(InstallDevExtension), cx)
                                    }),
                            ),
                    )
                    .child(
                        h_flex()
                            .w_full()
                            .flex_wrap()
                            .gap_2()
                            .child(self.render_search(cx)),
                    ),
            )
            .child(
                h_flex()
                    .id("filter-row")
                    .gap_2()
                    .py_2p5()
                    .px_4()
                    .border_b_1()
                    .border_color(cx.theme().colors().border_variant)
                    .overflow_x_scroll()
                    .child(
                        Button::new("filter-all-categories", "All")
                            .when(self.provides_filter.is_none(), |button| {
                                button.style(ButtonStyle::Filled)
                            })
                            .when(self.provides_filter.is_some(), |button| {
                                button.style(ButtonStyle::Subtle)
                            })
                            .toggle_state(self.provides_filter.is_none())
                            .on_click(cx.listener(|this, _event, _, cx| {
                                this.change_provides_filter(None, cx);
                            })),
                    )
                    .children(
                        ExtensionProvides::iter()
                            .filter(|provides| match provides {
                                ExtensionProvides::AgentServers
                                | ExtensionProvides::Grammars // grammars do not add anything of value to users currently
                                | ExtensionProvides::IndexedDocsProviders
                                | ExtensionProvides::SlashCommands => false,
                                _ => true,
                            })
                            .map(|provides| {
                                let label = extension_provides_label(provides);
                                let button_id =
                                    SharedString::from(format!("filter-category-{}", label));

                                Button::new(button_id, label)
                                    .style(if self.provides_filter == Some(provides) {
                                        ButtonStyle::Filled
                                    } else {
                                        ButtonStyle::Subtle
                                    })
                                    .toggle_state(self.provides_filter == Some(provides))
                                    .on_click({
                                        cx.listener(move |this, _event, _, cx| {
                                            this.change_provides_filter(Some(provides), cx);
                                        })
                                    })
                            }),
                    ),
            )
            .child(v_flex().px_4().size_full().overflow_y_hidden().map(|this| {
                let count = self.entries.len();

                if count == 0 {
                    this.child(self.render_empty_state(cx)).into_any_element()
                } else {
                    let scroll_handle = &self.list;
                    this.child(
                        uniform_list("entries", count, cx.processor(Self::render_extensions))
                            .flex_grow_1()
                            .pb_4()
                            .track_scroll(scroll_handle),
                    )
                    .vertical_scrollbar_for(scroll_handle, window, cx)
                    .into_any_element()
                }
            }))
    }
}

impl EventEmitter<ItemEvent> for ExtensionsPage {}

impl Focusable for ExtensionsPage {
    fn focus_handle(&self, cx: &App) -> gpui::FocusHandle {
        self.query_editor.read(cx).focus_handle(cx)
    }
}

impl Item for ExtensionsPage {
    type Event = ItemEvent;

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        "Extensions".into()
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        Some("Extensions Page Opened")
    }

    fn show_toolbar(&self) -> bool {
        false
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(workspace::item::ItemEvent)) {
        f(*event)
    }
}
