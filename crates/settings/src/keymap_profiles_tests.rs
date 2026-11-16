//! The leader rows derived from an Emacs keymap: which rows qualify and how each leader rewrites
//! keys and contexts.

use serde_json::{Value, json};

use crate::{Leader, leader_bindings};

const FIXTURE: &str = r#"[
  // A comment, as the real assets have.
  {
    "context": "Editor",
    "bindings": {
      "ctrl-c ;": "emacs::CommentDwim",
      "ctrl-x ctrl-s": "workspace::Save",
    },
  },
  {
    "context": "Editor && prompts",
    "bindings": {
      "ctrl-c ctrl-c": "prompts::Send",
      "ctrl-c ctrl-n": "prompts::New",
    },
  },
  {
    "context": "Workspace",
    "bindings": {
      "alt-x": "command_palette::Toggle",
      "ctrl-c": "menu::Cancel",
      "ctrl-c w w": "work::TogglePanel",
      "ctrl-c w g e": "project::Edit",
      "ctrl-c c n": "chat::New",
      "ctrl-c x": null,
      "ctrl-c f r": ["workflow::Run", { "id": 3 }],
      "ctrl-c t": ["terminal::SendKeystroke", "ctrl-c"],
      "ctrl-x ctrl-c": "zed::Quit",
    },
  },
  {
    "context": "Terminal",
    "bindings": {
      "ctrl-c": ["terminal::SendKeystroke", "ctrl-c"],
      "ctrl-c ctrl-x": "workspace::Save",
    },
  },
  {
    "context": "ChatPanel > Terminal",
    "bindings": {
      "ctrl-c c n": "chat::New",
    },
  },
]"#;

fn derive(leader: Leader, macos: bool) -> Value {
    let json = leader_bindings(FIXTURE, leader, macos).unwrap();
    serde_json::from_str(&json).unwrap()
}

#[test]
fn vscode_rows_on_linux() {
    assert_eq!(
        derive(Leader::VsCode, false),
        json!([
            {
                "context": "Editor && prompts",
                "bindings": {
                    "ctrl-k b ctrl-c": "prompts::Send",
                    "ctrl-k b ctrl-n": "prompts::New",
                },
            },
            {
                "context": "Workspace",
                "bindings": {
                    "ctrl-k b w w": "work::TogglePanel",
                    "ctrl-k b w g e": "project::Edit",
                    "ctrl-k b c n": "chat::New",
                    "ctrl-k b f r": ["workflow::Run", { "id": 3 }],
                },
            },
        ])
    );
}

#[test]
fn vscode_rows_on_macos_use_cmd_k_b() {
    let derived = derive(Leader::VsCode, true);
    let keys: Vec<(&str, &str)> = derived
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|section| {
            let context = section["context"].as_str().unwrap();
            section["bindings"]
                .as_object()
                .unwrap()
                .keys()
                .map(move |key| (context, key.as_str()))
        })
        .collect();
    assert_eq!(
        keys,
        [
            ("Editor && prompts", "cmd-k b ctrl-c"),
            ("Editor && prompts", "cmd-k b ctrl-n"),
            ("Workspace", "cmd-k b w w"),
            ("Workspace", "cmd-k b w g e"),
            ("Workspace", "cmd-k b c n"),
            ("Workspace", "cmd-k b f r"),
        ]
    );
}

#[test]
fn vim_space_rows_scope_to_vim_control() {
    for macos in [false, true] {
        assert_eq!(
            derive(Leader::VimSpace, macos),
            json!([
                {
                    "context": "(Editor && prompts) && VimControl && !menu",
                    "bindings": {
                        "space ctrl-c": "prompts::Send",
                        "space ctrl-n": "prompts::New",
                    },
                },
                {
                    "context": "VimControl && !menu",
                    "bindings": {
                        "space w w": "work::TogglePanel",
                        "space w g e": "project::Edit",
                        "space c n": "chat::New",
                        "space f r": ["workflow::Run", { "id": 3 }],
                    },
                },
            ]),
            "macos={macos}"
        );
    }
}

#[test]
fn skipped_rows_derive_nothing() {
    for leader in [Leader::VsCode, Leader::VimSpace] {
        let json = leader_bindings(FIXTURE, leader, false).unwrap();
        for absent in [
            "emacs::",
            "terminal::",
            "Terminal",
            "null",
            "menu::Cancel",
            "zed::Quit",
            "workspace::Save",
            "command_palette",
        ] {
            assert!(!json.contains(absent), "{leader:?}: {absent} in {json}");
        }
    }
}

#[test]
fn output_is_deterministic_and_empty_input_derives_nothing() {
    for leader in [Leader::VsCode, Leader::VimSpace] {
        assert_eq!(
            leader_bindings(FIXTURE, leader, false).unwrap(),
            leader_bindings(FIXTURE, leader, false).unwrap()
        );
        assert_eq!(
            serde_json::from_str::<Value>(&leader_bindings("", leader, false).unwrap()).unwrap(),
            json!([])
        );
    }
}
