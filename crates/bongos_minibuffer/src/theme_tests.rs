//! Tests that the strip takes its colors from the theme, in One Light and in One Dark: the line
//! uses `status_bar_background` and `border`, the candidate list `elevated_surface_background`,
//! and the input text is readable on the line.

use std::sync::Arc;

use gpui::{
    BorrowAppContext as _, Entity, Focusable as _, Hsla, TestAppContext, VisualTestContext,
};
use project::Project;
use settings::{SettingsStore, ThemeName, ThemeSelection};
use theme::{ActiveTheme as _, ThemeRegistry};
use workspace::{AppState, MultiWorkspace, Workspace};

use crate::strip::StripColors;
use crate::{Candidate, Session, Source, open};

const HOST: &str = "test_host";

/// The themes of the One family, as they ship with the app.
const ONE_THEMES: &str = include_str!("../../../assets/themes/one/one.json");

/// The colors a theme gives the strip.
#[derive(Debug)]
struct Expected {
    line: Hsla,
    border: Hsla,
    list: Hsla,
    text: Hsla,
}

fn init(cx: &mut TestAppContext) -> Arc<AppState> {
    let app_state = cx.update(|cx| {
        let app_state = AppState::test(cx);
        command_palette_hooks::init(cx);
        editor::init(cx);
        crate::init(cx);
        let registry = ThemeRegistry::global(cx);
        theme_settings::load_user_theme(&registry, ONE_THEMES.as_bytes())
            .expect("the One themes load");
        app_state
    });
    cx.update(|cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |content| {
                content.minibuffer.get_or_insert_default().enabled_for = Some(vec![HOST.into()]);
            });
        });
    });
    app_state
}

fn use_theme(name: &str, cx: &mut VisualTestContext) {
    cx.update(|_, cx| {
        cx.update_global::<SettingsStore, _>(|store, cx| {
            store.update_user_settings(cx, |content| {
                content.theme.theme = Some(ThemeSelection::Static(ThemeName(name.into())));
            });
        });
    });
    cx.run_until_parked();
    cx.update(|_, cx| assert_eq!(cx.theme().name.as_ref(), name, "the theme is active"));
}

fn expected(cx: &mut VisualTestContext) -> Expected {
    cx.update(|_, cx| {
        let colors = cx.theme().colors();
        Expected {
            line: colors.status_bar_background,
            border: colors.border,
            list: colors.elevated_surface_background,
            text: colors.text,
        }
    })
}

async fn window_with_session(cx: &mut TestAppContext) -> (Entity<Workspace>, VisualTestContext) {
    let app_state = init(cx);
    let project = Project::test(app_state.fs.clone(), [], cx).await;
    let window = cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
    let workspace = window
        .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
        .unwrap();
    let mut cx = VisualTestContext::from_window(window.into(), cx);
    workspace.update_in(&mut cx, |workspace, window, cx| {
        window.focus(&workspace.focus_handle(cx), cx);
    });
    cx.run_until_parked();
    workspace.update_in(&mut cx, |workspace, window, cx| {
        let session = Session::completing_read(HOST, "Pick: ").source(Source::Fixed(
            ["alpha", "beta", "gamma"]
                .into_iter()
                .map(Candidate::text)
                .collect(),
        ));
        open(workspace, session, window, cx);
    });
    cx.run_until_parked();
    (workspace, cx)
}

/// Distance in lightness between two colors.
fn lightness_gap(a: Hsla, b: Hsla) -> f32 {
    (a.l - b.l).abs()
}

#[gpui::test]
async fn minibuffer_strip_colors_come_from_the_theme_in_one_light_and_one_dark(
    cx: &mut TestAppContext,
) {
    let (workspace, mut cx) = window_with_session(cx).await;
    let cx = &mut cx;
    assert!(workspace.read_with(cx, |workspace, cx| {
        workspace.modal_layer().read(cx).bottom_modal().is_some()
    }));

    let mut seen = Vec::new();
    for name in ["One Light", "One Dark"] {
        use_theme(name, cx);
        let theme = expected(cx);

        // The strip's line reads its colors from the theme, through the same accessor that
        // its render uses.
        let strip = cx.update(|_, cx| StripColors::of(cx));
        assert_eq!(strip.background, theme.line, "{name}: line background");
        assert_eq!(strip.border, theme.border, "{name}: line border");

        // The strip and the candidate list are drawn, over a theme that is not the default.
        assert!(
            cx.debug_bounds("minibuffer-input").is_some(),
            "{name}: the input row is drawn"
        );
        assert!(
            cx.debug_bounds("minibuffer-list").is_some(),
            "{name}: the candidate list is drawn"
        );

        // The list sits on the elevated surface, apart from the line it rises from.
        assert_ne!(theme.list, theme.line, "{name}: list and line differ");
        // The input has no background of its own, so its text must read on the line.
        assert_ne!(theme.text, theme.line, "{name}: input text and line differ");
        assert!(
            lightness_gap(theme.text, theme.line) > 0.3,
            "{name}: the input text is readable: {theme:?}"
        );
        seen.push(theme);
    }

    // The two themes really differ, so the checks above are not vacuous.
    let [light, dark] = &seen[..] else {
        unreachable!()
    };
    assert_ne!(light.line, dark.line);
    assert_ne!(light.border, dark.border);
    assert_ne!(light.list, dark.list);
    assert_ne!(light.text, dark.text);
    assert!(light.line.l > dark.line.l, "One Light is the light theme");
}
