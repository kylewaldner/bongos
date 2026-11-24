//! Checks the bundled The Inner Light theme: its exact token values, the contrast of its derived
//! text colors on white, and that it loads through the bundled theme loader.

use std::borrow::Cow;

use anyhow::Result;
use gpui::{AssetSource, SharedString};
use serde_json::Value;
use theme::{Appearance, ThemeRegistry};

use crate::true_dark_tests::contrast_ratio;
use crate::{deserialize_user_theme, load_bundled_themes, refine_theme_family};

const INNER_LIGHT_NAME: &str = "The Inner Light";
const INNER_LIGHT_PATH: &str = "themes/inner-light/inner-light.json";
const INNER_LIGHT_JSON: &str = include_str!("../../../assets/themes/inner-light/inner-light.json");
const ONE_JSON: &str = include_str!("../../../assets/themes/one/one.json");

fn inner_light_style() -> Value {
    let family: Value =
        serde_json::from_str(INNER_LIGHT_JSON).expect("The Inner Light is valid JSON");
    family["themes"][0]["style"].clone()
}

fn color(style: &Value, key: &str) -> String {
    style[key]
        .as_str()
        .unwrap_or_else(|| panic!("The Inner Light has no color for {key}"))
        .to_string()
}

fn syntax_color(style: &Value, key: &str) -> String {
    style["syntax"][key]["color"]
        .as_str()
        .unwrap_or_else(|| panic!("The Inner Light has no syntax color for {key}"))
        .to_string()
}

#[test]
fn inner_light_parses_refines_and_is_named_the_inner_light() {
    let content =
        deserialize_user_theme(INNER_LIGHT_JSON.as_bytes()).expect("The Inner Light parses");
    assert_eq!(content.name, INNER_LIGHT_NAME);
    assert_eq!(content.author, "bongos");
    assert_eq!(content.themes.len(), 1);

    let family = refine_theme_family(content);
    assert_eq!(family.name.as_ref(), INNER_LIGHT_NAME);
    let theme = &family.themes[0];
    assert_eq!(theme.name.as_ref(), INNER_LIGHT_NAME);
    assert_eq!(theme.appearance(), Appearance::Light);

    // The text cursor is the accent.
    assert_eq!(theme.players().local().cursor, theme.colors().text_accent);
}

#[test]
fn inner_light_keeps_one_light_key_set() {
    let one: Value = serde_json::from_str(ONE_JSON).expect("One is valid JSON");
    let one_light = one["themes"]
        .as_array()
        .expect("themes array")
        .iter()
        .find(|theme| theme["name"] == "One Light")
        .expect("One Light exists")["style"]
        .clone();
    let style = inner_light_style();

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
    let style_keys = keys(&style);
    let one_keys = keys(&one_light);
    for key in &one_keys {
        assert!(style_keys.contains(key), "The Inner Light is missing {key}");
    }
    let extra = style_keys
        .iter()
        .filter(|key| !one_keys.contains(key))
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(
        extra,
        [
            "editor.indent_guide",
            "editor.indent_guide_active",
            "pane_group.border",
            "version_control.conflict_marker.ours",
            "version_control.conflict_marker.theirs",
        ]
    );
    assert_eq!(color(&style, "pane_group.border"), "#dfe1e5ff");
    assert_eq!(color(&style, "editor.indent_guide"), "#ebecf0ff");
    assert_eq!(color(&style, "editor.indent_guide_active"), "#dfe1e5ff");
    assert_eq!(
        color(&style, "version_control.conflict_marker.ours"),
        "#208a3c1a"
    );
    assert_eq!(
        color(&style, "version_control.conflict_marker.theirs"),
        "#3574f01a"
    );
    assert_eq!(keys(&style["syntax"]), keys(&one_light["syntax"]));
    assert_eq!(
        style["players"].as_array().map(Vec::len),
        one_light["players"].as_array().map(Vec::len)
    );
    for (key, value) in one_light.as_object().expect("object") {
        assert_eq!(
            value.is_null(),
            style[key].is_null(),
            "{key} must be null exactly where One Light is"
        );
    }
    for (key, highlight) in one_light["syntax"].as_object().expect("syntax object") {
        for field in ["font_style", "font_weight"] {
            assert_eq!(
                style["syntax"][key][field], highlight[field],
                "syntax.{key}.{field} must stay One Light's"
            );
        }
    }
}

#[test]
fn inner_light_tokens_are_exact() {
    let style = inner_light_style();
    let table: &[(&[&str], &str)] = &[
        (
            &[
                "background",
                "title_bar.background",
                "status_bar.background",
                "panel.background",
                "surface.background",
                "tab_bar.background",
                "tab.inactive_background",
            ],
            "#f7f8faff",
        ),
        (
            &[
                "elevated_surface.background",
                "editor.background",
                "editor.gutter.background",
                "toolbar.background",
                "tab.active_background",
            ],
            "#ffffffff",
        ),
        (&["editor.active_line.background"], "#f7f8fabf"),
        (&["border"], "#dfe1e5ff"),
        (&["border.variant"], "#ebecf0ff"),
        (&["border.focused"], "#3574f0ff"),
        (&["ghost_element.hover"], "#edeff3ff"),
        (&["ghost_element.selected", "element.selected"], "#d4e2ffff"),
        (&["text"], "#080808ff"),
        (&["text.muted"], "#6c707eff"),
        (&["text.placeholder"], "#a0a4aeff"),
        (&["text.accent"], "#3574f0ff"),
        (&["icon.muted"], "#6c707eff"),
        (&["icon.accent"], "#3574f0ff"),
        (&["editor.foreground"], "#080808ff"),
        (&["editor.line_number"], "#aeb3c2ff"),
        (&["editor.active_line_number"], "#080808ff"),
        (&["success"], "#208a3cff"),
        (&["warning"], "#a46704ff"),
        (&["error"], "#db3b4bff"),
        (&["info"], "#3574f0ff"),
        (&["success.background"], "#208a3c22"),
        (&["warning.background"], "#a4670422"),
        (&["error.background"], "#db3b4b22"),
        (&["info.background"], "#3574f022"),
        (&["scrollbar.thumb.background"], "#6c707e40"),
        (&["scrollbar.track.border"], "#ebecf0ff"),
    ];
    for (keys, value) in table {
        for key in *keys {
            assert_eq!(color(&style, key), *value, "{key}");
        }
    }

    let syntax = [
        ("keyword", "#0033b3ff"),
        ("string", "#067d17ff"),
        ("function", "#00627aff"),
        ("type", "#080808ff"),
        ("comment", "#8c8c8cff"),
        ("number", "#1750ebff"),
        ("property", "#871094ff"),
        ("punctuation", "#6c707eff"),
    ];
    for (key, value) in syntax {
        assert_eq!(syntax_color(&style, key), value, "syntax.{key}");
    }
}

#[test]
fn inner_light_derived_text_colors_read_on_white() {
    let style = inner_light_style();
    let mut foregrounds: Vec<(String, String)> = Vec::new();

    // The table's own syntax colors ship as drawn and are not held to 4.5; of the derived ones,
    // comment.doc and predictive are faint by design and emphasis is the accent.
    let table_syntax = [
        "keyword",
        "string",
        "function",
        "type",
        "comment",
        "number",
        "property",
        "punctuation",
    ];
    for (key, highlight) in style["syntax"].as_object().expect("syntax object") {
        if !table_syntax.contains(&key.as_str())
            && !["comment.doc", "predictive", "emphasis"].contains(&key.as_str())
        {
            let value = highlight["color"].as_str().expect("syntax color");
            foregrounds.push((format!("syntax.{key}"), value.to_string()));
        }
    }

    // White and every dim are faint by design.
    for name in ["black", "red", "green", "yellow", "blue", "magenta", "cyan"] {
        for prefix in ["", "bright_"] {
            let key = format!("terminal.ansi.{prefix}{name}");
            foregrounds.push((key.clone(), color(&style, &key)));
        }
    }
    for key in ["terminal.foreground", "terminal.bright_foreground"] {
        foregrounds.push((key.to_string(), color(&style, key)));
    }

    // Player 0's cursor is the accent, checked by its own test.
    for (index, player) in style["players"]
        .as_array()
        .expect("players")
        .iter()
        .enumerate()
        .skip(1)
    {
        let cursor = player["cursor"].as_str().expect("player cursor");
        foregrounds.push((format!("players[{index}].cursor"), cursor.to_string()));
    }

    // Predictive, unreachable, hidden and ignored are faint by design.
    for key in [
        "created",
        "modified",
        "conflict",
        "deleted",
        "renamed",
        "hint",
        "version_control.added",
        "version_control.modified",
        "version_control.deleted",
        "icon",
        "editor.hover_line_number",
    ] {
        foregrounds.push((key.to_string(), color(&style, key)));
    }

    for (key, value) in &foregrounds {
        assert!(
            value.len() == 9 && value.ends_with("ff"),
            "{key} is drawn as text and must be opaque, got {value}"
        );
        let ratio = contrast_ratio(value, "#ffffff");
        assert!(
            ratio >= 4.5,
            "{key} {value} has contrast {ratio:.2} on white, under 4.5"
        );
    }
}

#[test]
fn inner_light_player_zero_cursor_is_the_accent() {
    let style = inner_light_style();
    assert_eq!(
        style["players"][0]["cursor"].as_str(),
        Some(color(&style, "text.accent").as_str())
    );
}

struct InnerLightAsset;

impl AssetSource for InnerLightAsset {
    fn load(&self, path: &str) -> Result<Option<Cow<'static, [u8]>>> {
        Ok((path == INNER_LIGHT_PATH).then_some(Cow::Borrowed(INNER_LIGHT_JSON.as_bytes())))
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        Ok(if INNER_LIGHT_PATH.starts_with(path) {
            vec![SharedString::from(INNER_LIGHT_PATH)]
        } else {
            Vec::new()
        })
    }
}

#[test]
fn inner_light_loads_through_the_bundled_loader() {
    let registry = ThemeRegistry::new(Box::new(InnerLightAsset));
    assert!(registry.get(INNER_LIGHT_NAME).is_err());

    load_bundled_themes(&registry);

    // The loader skips a file it cannot read or parse, so finding the theme means no error.
    let theme = registry
        .get(INNER_LIGHT_NAME)
        .expect("The Inner Light is registered");
    assert_eq!(theme.appearance(), Appearance::Light);
    assert_eq!(theme.players().local().cursor, theme.colors().text_accent);
}
