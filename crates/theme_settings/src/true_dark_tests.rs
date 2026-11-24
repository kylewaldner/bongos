//! Checks the bundled True Dark theme: its exact token values, the contrast of its text colors on
//! black, and that it loads through the bundled theme loader.

use std::borrow::Cow;

use anyhow::Result;
use gpui::{AssetSource, SharedString};
use serde_json::Value;
use theme::{Appearance, ThemeRegistry};

use crate::{deserialize_user_theme, load_bundled_themes, refine_theme_family};

const TRUE_DARK_PATH: &str = "themes/true-dark/true-dark.json";
const TRUE_DARK_JSON: &str = include_str!("../../../assets/themes/true-dark/true-dark.json");
const ONE_JSON: &str = include_str!("../../../assets/themes/one/one.json");

/// The WCAG relative luminance of a `#rrggbb` or `#rrggbbaa` color (alpha ignored).
pub(super) fn relative_luminance(hex: &str) -> f64 {
    let hex = hex.strip_prefix('#').expect("color starts with #");
    assert!(hex.len() == 6 || hex.len() == 8, "bad color {hex}");
    let channel = |range: std::ops::Range<usize>| {
        let value = f64::from(u8::from_str_radix(&hex[range], 16).expect("hex digits")) / 255.0;
        if value <= 0.04045 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * channel(0..2) + 0.7152 * channel(2..4) + 0.0722 * channel(4..6)
}

/// The WCAG contrast ratio between two opaque colors, from 1.0 to 21.0.
pub(super) fn contrast_ratio(foreground: &str, background: &str) -> f64 {
    let a = relative_luminance(foreground);
    let b = relative_luminance(background);
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

fn true_dark_style() -> Value {
    let family: Value = serde_json::from_str(TRUE_DARK_JSON).expect("True Dark is valid JSON");
    family["themes"][0]["style"].clone()
}

fn color(style: &Value, key: &str) -> String {
    style[key]
        .as_str()
        .unwrap_or_else(|| panic!("True Dark has no color for {key}"))
        .to_string()
}

fn syntax_color(style: &Value, key: &str) -> String {
    style["syntax"][key]["color"]
        .as_str()
        .unwrap_or_else(|| panic!("True Dark has no syntax color for {key}"))
        .to_string()
}

#[test]
fn true_dark_parses_refines_and_is_named_true_dark() {
    let content = deserialize_user_theme(TRUE_DARK_JSON.as_bytes()).expect("True Dark parses");
    assert_eq!(content.name, "True Dark");
    assert_eq!(content.author, "bongos");
    assert_eq!(content.themes.len(), 1);

    let family = refine_theme_family(content);
    assert_eq!(family.name.as_ref(), "True Dark");
    let theme = &family.themes[0];
    assert_eq!(theme.name.as_ref(), "True Dark");
    assert_eq!(theme.appearance(), Appearance::Dark);

    // The text cursor is the accent.
    assert_eq!(theme.players().local().cursor, theme.colors().text_accent);
}

#[test]
fn true_dark_keeps_one_dark_key_set() {
    let one: Value = serde_json::from_str(ONE_JSON).expect("One is valid JSON");
    let one_dark = one["themes"]
        .as_array()
        .expect("themes array")
        .iter()
        .find(|theme| theme["name"] == "One Dark")
        .expect("One Dark exists")["style"]
        .clone();
    let style = true_dark_style();

    let keys = |value: &Value| {
        let mut keys = value
            .as_object()
            .expect("object")
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        keys.sort();
        keys
    };
    assert_eq!(keys(&style), keys(&one_dark));
    assert_eq!(keys(&style["syntax"]), keys(&one_dark["syntax"]));
    assert_eq!(
        style["players"].as_array().map(Vec::len),
        one_dark["players"].as_array().map(Vec::len)
    );
    for (key, value) in one_dark.as_object().expect("object") {
        assert_eq!(
            value.is_null(),
            style[key].is_null(),
            "{key} must be null exactly where One Dark is"
        );
    }
}

#[test]
fn true_dark_tokens_match_the_table() {
    let style = true_dark_style();
    let table: &[(&[&str], &str)] = &[
        (
            &[
                "background",
                "title_bar.background",
                "status_bar.background",
                "editor.background",
                "editor.gutter.background",
                "toolbar.background",
                "tab_bar.background",
                "tab.inactive_background",
                "tab.active_background",
            ],
            "#000000ff",
        ),
        (&["panel.background", "surface.background"], "#0a0a0aff"),
        (&["elevated_surface.background"], "#141414ff"),
        (&["editor.active_line.background"], "#0a0a0abf"),
        (&["border"], "#2a2a2aff"),
        (&["border.variant"], "#1a1a1aff"),
        (&["border.focused"], "#5c9affff"),
        (&["ghost_element.hover"], "#151515ff"),
        (&["ghost_element.selected", "element.selected"], "#1d2f52ff"),
        (&["text"], "#ffffffff"),
        (&["text.muted"], "#a0a0a0ff"),
        (&["text.placeholder"], "#6a6a6aff"),
        (&["text.accent"], "#5c9affff"),
        (&["icon.muted"], "#a0a0a0ff"),
        (&["icon.accent"], "#5c9affff"),
        (&["editor.foreground"], "#f0f0f0ff"),
        (&["editor.line_number"], "#4a4a4aff"),
        (&["editor.active_line_number"], "#ffffffff"),
        (&["success"], "#5fb865ff"),
        (&["warning"], "#f2c55cff"),
        (&["error"], "#ff6b6bff"),
        (&["info"], "#5c9affff"),
        (&["success.background"], "#5fb86522"),
        (&["warning.background"], "#f2c55c22"),
        (&["error.background"], "#ff6b6b22"),
        (&["info.background"], "#5c9aff22"),
        (&["scrollbar.thumb.background"], "#a0a0a040"),
        (&["scrollbar.track.border"], "#1a1a1aff"),
    ];
    for (keys, value) in table {
        for key in *keys {
            assert_eq!(color(&style, key), *value, "{key}");
        }
    }

    let syntax = [
        ("keyword", "#e5976fff"),
        ("string", "#7fc98aff"),
        ("function", "#5fb0ffff"),
        ("type", "#f0f0f0ff"),
        ("comment", "#7a7a7aff"),
        ("number", "#36c4d0ff"),
        ("property", "#d68ad0ff"),
        ("punctuation", "#a0a0a0ff"),
    ];
    for (key, value) in syntax {
        assert_eq!(syntax_color(&style, key), value, "syntax.{key}");
    }
}

#[test]
fn true_dark_text_colors_read_on_black() {
    let style = true_dark_style();
    let mut foregrounds: Vec<(String, String)> = Vec::new();

    // Faint by design: predictive is the one faint syntax color.
    for (key, highlight) in style["syntax"].as_object().expect("syntax object") {
        if key != "predictive" {
            let value = highlight["color"].as_str().expect("syntax color");
            foregrounds.push((format!("syntax.{key}"), value.to_string()));
        }
    }

    for name in ["red", "green", "yellow", "blue", "magenta", "cyan", "white"] {
        for prefix in ["", "bright_"] {
            let key = format!("terminal.ansi.{prefix}{name}");
            foregrounds.push((key.clone(), color(&style, &key)));
        }
    }
    for key in ["terminal.foreground", "terminal.bright_foreground"] {
        foregrounds.push((key.to_string(), color(&style, key)));
    }

    for (index, player) in style["players"]
        .as_array()
        .expect("players")
        .iter()
        .enumerate()
    {
        let cursor = player["cursor"].as_str().expect("player cursor");
        foregrounds.push((format!("players[{index}].cursor"), cursor.to_string()));
    }

    // Hidden and ignored are faint by design.
    for key in [
        "created",
        "modified",
        "conflict",
        "deleted",
        "renamed",
        "hint",
        "predictive",
        "unreachable",
        "success",
        "warning",
        "error",
        "info",
        "version_control.added",
        "version_control.modified",
        "version_control.deleted",
        "text",
        "text.muted",
        "text.accent",
        "icon",
        "icon.muted",
        "icon.accent",
        "editor.foreground",
        "editor.active_line_number",
        "editor.hover_line_number",
        "link_text.hover",
    ] {
        foregrounds.push((key.to_string(), color(&style, key)));
    }

    for (key, value) in &foregrounds {
        assert!(
            value.len() == 9 && value.ends_with("ff"),
            "{key} is drawn as text and must be opaque, got {value}"
        );
        let ratio = contrast_ratio(value, "#000000");
        assert!(
            ratio >= 4.5,
            "{key} {value} has contrast {ratio:.2} on black, under 4.5"
        );
    }
}

#[test]
fn true_dark_player_zero_cursor_is_the_accent() {
    let style = true_dark_style();
    assert_eq!(
        style["players"][0]["cursor"].as_str(),
        Some(color(&style, "text.accent").as_str())
    );
}

struct TrueDarkAsset;

impl AssetSource for TrueDarkAsset {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        Ok((path == TRUE_DARK_PATH).then_some(Cow::Borrowed(TRUE_DARK_JSON.as_bytes())))
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        Ok(if TRUE_DARK_PATH.starts_with(path) {
            vec![SharedString::from(TRUE_DARK_PATH)]
        } else {
            Vec::new()
        })
    }
}

#[test]
fn true_dark_loads_through_the_bundled_loader() {
    let registry = ThemeRegistry::new(Box::new(TrueDarkAsset));
    assert!(registry.get("True Dark").is_err());

    load_bundled_themes(&registry);

    // The loader skips a file it cannot read or parse, so finding the theme means no error.
    let theme = registry.get("True Dark").expect("True Dark is registered");
    assert_eq!(theme.appearance(), Appearance::Dark);
    assert_eq!(theme.players().local().cursor, theme.colors().text_accent);
}
