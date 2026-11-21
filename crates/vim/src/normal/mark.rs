use std::{ops::Range, path::Path, sync::Arc};

use editor::{
    Anchor, Bias, DisplayPoint, Editor, MultiBuffer,
    display_map::{DisplaySnapshot, ToDisplayPoint},
    movement,
};
use gpui::{Context, Entity, EntityId, TaskExt, UpdateGlobal, Window};
use language::SelectionGoal;
use text::Point;
use ui::App;
use workspace::OpenOptions;

use crate::{
    Vim,
    motion::{self, Motion},
    state::{Mark, Mode, VimGlobals},
};

impl Vim {
    pub fn create_mark(&mut self, text: Arc<str>, window: &mut Window, cx: &mut Context<Self>) {
        self.update_editor(cx, |vim, editor, cx| {
            let anchors = editor
                .selections
                .disjoint_anchors_arc()
                .iter()
                .map(|s| s.head())
                .collect::<Vec<_>>();
            vim.set_mark(text.to_string(), anchors, editor.buffer(), window, cx);
        });
        self.clear_operator(window, cx);
    }

    // When handling an action, you must create visual marks if you will switch to normal
    // mode without the default selection behavior.
    pub(crate) fn store_visual_marks(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.mode.is_visual() {
            self.create_visual_marks(self.mode, window, cx);
        }
    }

    pub(crate) fn create_visual_marks(
        &mut self,
        mode: Mode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut starts = vec![];
        let mut ends = vec![];
        let mut reversed = vec![];

        self.update_editor(cx, |vim, editor, cx| {
            let display_map = editor.display_snapshot(cx);
            let selections = editor.selections.all_display(&display_map);
            for selection in selections {
                let end = movement::saturating_left(&display_map, selection.end);
                ends.push(
                    display_map
                        .buffer_snapshot()
                        .anchor_before(end.to_offset(&display_map, Bias::Left)),
                );
                starts.push(
                    display_map
                        .buffer_snapshot()
                        .anchor_before(selection.start.to_offset(&display_map, Bias::Left)),
                );
                reversed.push(selection.reversed)
            }
            vim.set_mark("<".to_string(), starts, editor.buffer(), window, cx);
            vim.set_mark(">".to_string(), ends, editor.buffer(), window, cx);
        });

        self.stored_visual_mode.replace((mode, reversed));
    }

    fn open_buffer_mark(
        &mut self,
        line: bool,
        entity_id: EntityId,
        anchors: Vec<Anchor>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(workspace) = self.workspace(window, cx) else {
            return;
        };
        workspace.update(cx, |workspace, cx| {
            let item = workspace.items(cx).find(|item| {
                item.act_as::<Editor>(cx)
                    .is_some_and(|editor| editor.read(cx).buffer().entity_id() == entity_id)
            });
            let Some(item) = item.cloned() else {
                return;
            };
            if let Some(pane) = workspace.pane_for(item.as_ref()) {
                pane.update(cx, |pane, cx| {
                    if let Some(index) = pane.index_for_item(item.as_ref()) {
                        pane.activate_item(index, true, true, window, cx);
                    }
                });
            };

            item.act_as::<Editor>(cx).unwrap().update(cx, |editor, cx| {
                let map = editor.snapshot(window, cx);
                let mut ranges: Vec<Range<Anchor>> = Vec::new();
                for mut anchor in anchors {
                    if line {
                        let mut point = anchor.to_display_point(&map.display_snapshot);
                        point = motion::first_non_whitespace(&map.display_snapshot, false, point);
                        anchor = map
                            .display_snapshot
                            .buffer_snapshot()
                            .anchor_before(point.to_point(&map.display_snapshot));
                    }

                    if ranges.last() != Some(&(anchor..anchor)) {
                        ranges.push(anchor..anchor);
                    }
                }

                editor.change_selections(Default::default(), window, cx, |s| {
                    s.select_anchor_ranges(ranges)
                });
            })
        });
    }

    fn open_path_mark(
        &mut self,
        line: bool,
        path: Arc<Path>,
        points: Vec<Point>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(workspace) = self.workspace(window, cx) else {
            return;
        };
        let task = workspace.update(cx, |workspace, cx| {
            workspace.open_abs_path(
                path.to_path_buf(),
                OpenOptions {
                    visible: Some(workspace::OpenVisible::All),
                    focus: Some(true),
                    ..Default::default()
                },
                window,
                cx,
            )
        });
        cx.spawn_in(window, async move |this, cx| {
            let editor = task.await?;
            this.update_in(cx, |_, window, cx| {
                if let Some(editor) = editor.act_as::<Editor>(cx) {
                    editor.update(cx, |editor, cx| {
                        let map = editor.snapshot(window, cx);
                        let points: Vec<_> = points
                            .into_iter()
                            .map(|p| {
                                if line {
                                    let point = p.to_display_point(&map.display_snapshot);
                                    motion::first_non_whitespace(
                                        &map.display_snapshot,
                                        false,
                                        point,
                                    )
                                    .to_point(&map.display_snapshot)
                                } else {
                                    p
                                }
                            })
                            .collect();
                        editor.change_selections(Default::default(), window, cx, |s| {
                            s.select_ranges(points.into_iter().map(|p| p..p))
                        })
                    })
                }
            })
        })
        .detach_and_log_err(cx);
    }

    pub fn jump(
        &mut self,
        text: Arc<str>,
        line: bool,
        should_pop_operator: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if should_pop_operator {
            self.pop_operator(window, cx);
        }
        let mark = self
            .update_editor(cx, |vim, editor, cx| {
                vim.get_mark(&text, editor, window, cx)
            })
            .flatten();
        let anchors = match mark {
            None => None,
            Some(Mark::Local(anchors)) => Some(anchors),
            Some(Mark::Buffer(entity_id, anchors)) => {
                self.open_buffer_mark(line, entity_id, anchors, window, cx);
                return;
            }
            Some(Mark::Path(path, points)) => {
                self.open_path_mark(line, path, points, window, cx);
                return;
            }
        };

        let Some(mut anchors) = anchors else { return };

        self.update_editor(cx, |_, editor, cx| {
            editor.create_nav_history_entry(cx);
        });
        let is_active_operator = self.active_operator().is_some();
        if is_active_operator {
            if let Some(anchor) = anchors.last() {
                self.motion(
                    Motion::Jump {
                        anchor: *anchor,
                        line,
                    },
                    window,
                    cx,
                )
            }
        } else {
            // Save the last anchor so as to jump to it later.
            let anchor: Option<Anchor> = anchors.last_mut().map(|anchor| *anchor);
            let should_jump = self.mode == Mode::Visual
                || self.mode == Mode::VisualLine
                || self.mode == Mode::VisualBlock;

            self.update_editor(cx, |_, editor, cx| {
                let map = editor.snapshot(window, cx);
                let mut ranges: Vec<Range<Anchor>> = Vec::new();
                for mut anchor in anchors {
                    if line {
                        let mut point = anchor.to_display_point(&map.display_snapshot);
                        point = motion::first_non_whitespace(&map.display_snapshot, false, point);
                        anchor = map
                            .display_snapshot
                            .buffer_snapshot()
                            .anchor_before(point.to_point(&map.display_snapshot));
                    }

                    if ranges.last() != Some(&(anchor..anchor)) {
                        ranges.push(anchor..anchor);
                    }
                }

                if !should_jump && !ranges.is_empty() {
                    editor.change_selections(Default::default(), window, cx, |s| {
                        s.select_anchor_ranges(ranges)
                    });
                }
            });

            if should_jump && let Some(anchor) = anchor {
                self.motion(Motion::Jump { anchor, line }, window, cx)
            }
        }
    }

    pub fn set_mark(
        &mut self,
        mut name: String,
        anchors: Vec<Anchor>,
        buffer_entity: &Entity<MultiBuffer>,
        window: &mut Window,
        cx: &mut App,
    ) {
        let Some(workspace) = self.workspace(window, cx) else {
            return;
        };
        if name == "`" {
            name = "'".to_string();
        }
        if matches!(&name[..], "-" | " ") {
            // Not allowed marks
            return;
        }
        let entity_id = workspace.entity_id();
        Vim::update_globals(cx, |vim_globals, cx| {
            let Some(marks_state) = vim_globals.marks.get(&entity_id) else {
                return;
            };
            marks_state.update(cx, |ms, cx| {
                ms.set_mark(name.clone(), buffer_entity, anchors, cx);
            });
        });
    }

    pub fn get_mark(
        &self,
        mut name: &str,
        editor: &mut Editor,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<Mark> {
        if name == "`" {
            name = "'";
        }
        if matches!(name, "{" | "}" | "(" | ")") {
            let display_map = editor.display_snapshot(cx);
            let selections = editor.selections.all_display(&display_map);
            let anchors = selections
                .into_iter()
                .map(|selection| {
                    let point = match name {
                        "{" => movement::start_of_paragraph(&display_map, selection.head(), 1),
                        "}" => movement::end_of_paragraph(&display_map, selection.head(), 1),
                        "(" => motion::sentence_backwards(&display_map, selection.head(), 1),
                        ")" => motion::sentence_forwards(&display_map, selection.head(), 1),
                        _ => unreachable!(),
                    };
                    display_map
                        .buffer_snapshot()
                        .anchor_before(point.to_offset(&display_map, Bias::Left))
                })
                .collect::<Vec<Anchor>>();
            return Some(Mark::Local(anchors));
        }
        VimGlobals::update_global(cx, |globals, cx| {
            let workspace_id = self.workspace(window, cx)?.entity_id();
            globals
                .marks
                .get_mut(&workspace_id)?
                .update(cx, |ms, cx| ms.get_mark(name, editor.buffer(), cx))
        })
    }

    pub fn delete_mark(
        &self,
        name: String,
        editor: &mut Editor,
        window: &mut Window,
        cx: &mut App,
    ) {
        let Some(workspace) = self.workspace(window, cx) else {
            return;
        };
        if name == "`" || name == "'" {
            return;
        }
        let entity_id = workspace.entity_id();
        Vim::update_globals(cx, |vim_globals, cx| {
            let Some(marks_state) = vim_globals.marks.get(&entity_id) else {
                return;
            };
            marks_state.update(cx, |ms, cx| {
                ms.delete_mark(name.clone(), editor.buffer(), cx);
            });
        });
    }
}

pub fn jump_motion(
    map: &DisplaySnapshot,
    anchor: Anchor,
    line: bool,
) -> (DisplayPoint, SelectionGoal) {
    let mut point = anchor.to_display_point(map);
    if line {
        point = motion::first_non_whitespace(map, false, point)
    }

    (point, SelectionGoal::None)
}

#[cfg(test)]
mod test {
    use crate::{
        VimAddon,
        state::{Mark, Mode},
        test::{NeovimBackedTestContext, VimTestContext},
    };
    use editor::{Editor, ToPoint as _, test::editor_lsp_test_context::EditorLspTestContext};
    use gpui::{TestAppContext, UpdateGlobal as _};
    use settings::SettingsStore;
    use std::path::Path;
    use util::path;
    use workspace::{CloseActiveItem, OpenOptions};

    #[gpui::test]
    async fn test_quote_mark(cx: &mut TestAppContext) {
        let mut cx = NeovimBackedTestContext::new(cx).await;

        cx.set_shared_state("ˇHello, world!").await;
        cx.simulate_shared_keystrokes("w m o").await;
        cx.shared_state().await.assert_eq("Helloˇ, world!");
        cx.simulate_shared_keystrokes("$ ` o").await;
        cx.shared_state().await.assert_eq("Helloˇ, world!");
        cx.simulate_shared_keystrokes("` `").await;
        cx.shared_state().await.assert_eq("Hello, worldˇ!");
        cx.simulate_shared_keystrokes("` `").await;
        cx.shared_state().await.assert_eq("Helloˇ, world!");
        cx.simulate_shared_keystrokes("$ m '").await;
        cx.shared_state().await.assert_eq("Hello, worldˇ!");
        cx.simulate_shared_keystrokes("^ ` `").await;
        cx.shared_state().await.assert_eq("Hello, worldˇ!");
    }

    #[gpui::test]
    async fn test_global_mark_overwrite(cx: &mut TestAppContext) {
        let mut cx = VimTestContext::new(cx, true).await;

        let path = Path::new(path!("/first.rs"));
        let fs = cx.workspace(|workspace, _, cx| workspace.project().read(cx).fs().clone());
        fs.as_fake().insert_file(path, "one".into()).await;
        let path = Path::new(path!("/second.rs"));
        fs.as_fake().insert_file(path, "two".into()).await;

        let _ = cx
            .workspace(|workspace, window, cx| {
                workspace.open_abs_path(
                    path!("/first.rs").into(),
                    OpenOptions::default(),
                    window,
                    cx,
                )
            })
            .await;

        cx.simulate_keystrokes("m A");

        let _ = cx
            .workspace(|workspace, window, cx| {
                workspace.open_abs_path(
                    path!("/second.rs").into(),
                    OpenOptions::default(),
                    window,
                    cx,
                )
            })
            .await;

        cx.simulate_keystrokes("m A");

        let _ = cx
            .workspace(|workspace, window, cx| {
                workspace.active_pane().update(cx, |pane, cx| {
                    pane.close_active_item(&CloseActiveItem::default(), window, cx)
                })
            })
            .await;

        cx.simulate_keystrokes("m B");

        cx.simulate_keystrokes("' A");

        cx.workspace(|workspace, _, cx| {
            let active_editor = workspace.active_item_as::<Editor>(cx).unwrap();

            let buffer = active_editor
                .read(cx)
                .buffer()
                .read(cx)
                .as_singleton()
                .unwrap();

            let file = buffer.read(cx).file().unwrap();
            let file_path = file.as_local().unwrap().abs_path(cx);

            assert_eq!(file_path.to_str().unwrap(), path!("/second.rs"));
        })
    }

    /// Twenty lines, `line <row>`, with the caret on row 5.
    fn twenty_lines_with_caret_on_row_5() -> String {
        (0..20)
            .map(|row| {
                if row == 5 {
                    format!("ˇline {row}\n")
                } else {
                    format!("line {row}\n")
                }
            })
            .collect()
    }

    /// Writes `dir/other.rs` next to the test file and opens it in the active pane.
    async fn switch_to_another_file(cx: &mut VimTestContext) -> gpui::EntityId {
        let path = EditorLspTestContext::root_path()
            .join("dir")
            .join("other.rs");
        let fs = cx.workspace(|workspace, _, cx| workspace.project().read(cx).fs().clone());
        fs.as_fake().insert_file(&path, "other\n".into()).await;
        let item = cx
            .workspace(|workspace, window, cx| {
                workspace.open_abs_path(path.clone(), OpenOptions::default(), window, cx)
            })
            .await
            .unwrap();
        cx.run_until_parked();
        item.item_id()
    }

    /// The row of the test editor's `"` mark, the place it was left.
    fn quote_mark_row(cx: &mut VimTestContext) -> Option<u32> {
        cx.update_editor(|editor, window, cx| {
            let vim = editor.addon::<VimAddon>().unwrap().entity.clone();
            let mark = vim.update(cx, |vim, cx| vim.get_mark("\"", editor, window, cx))?;
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            match mark {
                Mark::Local(anchors) | Mark::Buffer(_, anchors) => {
                    anchors.first().map(|anchor| anchor.to_point(&snapshot).row)
                }
                Mark::Path(_, points) => points.first().map(|point| point.row),
            }
        })
    }

    fn active_item_id(cx: &mut VimTestContext) -> gpui::EntityId {
        cx.workspace(|workspace, _, cx| workspace.active_item(cx).unwrap().item_id())
    }

    #[gpui::test]
    async fn test_profile_nav_vim_file_switch_sets_quote_mark_and_ctrl_o_returns(
        cx: &mut TestAppContext,
    ) {
        let mut cx = VimTestContext::new(cx, true).await;
        cx.set_state(&twenty_lines_with_caret_on_row_5(), Mode::Normal);
        let original = cx.editor.entity_id();

        let other = switch_to_another_file(&mut cx).await;
        assert_ne!(other, original);
        assert_eq!(quote_mark_row(&mut cx), Some(5));

        cx.simulate_keystrokes("ctrl-o");
        cx.run_until_parked();
        assert_eq!(active_item_id(&mut cx), original);
        cx.assert_state(&twenty_lines_with_caret_on_row_5(), Mode::Normal);
        assert_eq!(quote_mark_row(&mut cx), Some(5));
    }

    #[gpui::test]
    async fn test_profile_nav_vim_quote_mark_without_item_switch_recording(
        cx: &mut TestAppContext,
    ) {
        let mut cx = VimTestContext::new(cx, true).await;
        cx.update(|_, cx| {
            SettingsStore::update_global(cx, |store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.editor.navigation_history = Some(settings::NavigationHistoryContent {
                        // Off too, so setting up the text leaves the history empty.
                        record_large_moves: Some(false),
                        record_item_switches: Some(false),
                    });
                });
            });
        });
        cx.set_state(&twenty_lines_with_caret_on_row_5(), Mode::Normal);

        switch_to_another_file(&mut cx).await;
        // The switch is not in the history, but the `"` mark still records where it left.
        cx.workspace(|workspace, _, cx| {
            assert!(!workspace.active_pane().read(cx).can_navigate_backward());
        });
        assert_eq!(quote_mark_row(&mut cx), Some(5));
    }
}
