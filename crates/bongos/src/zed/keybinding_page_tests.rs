//! The keybinding page `docs/keybindings.md`: rendered from the keymaps every profile loads, it
//! must match the checked-in file; and the checks behind it: every bongos command resolves under
//! all three profiles on both platforms, is a registered action that the command palette does not
//! hide, and is bound the same way on Linux and macOS under VS Code, bar the listed exceptions.

use std::collections::BTreeMap;
use std::path::PathBuf;

use gpui::{KeyBinding, Keymap, Keystroke, TestAppContext};
use settings::{BaseKeymap, KeybindSource, KeymapFile, KeymapFileLoadResult, Leader};

use super::keybinding_profile_tests::{
    EDITOR, PANE_TERMINAL, PROMPTS_EDITOR, TERMINAL, VIM_NORMAL, WORKSPACE, command_rows,
    context_of, first, is_panel_key, keys_of, load_profile, resolve, stack,
};
use super::tests::init_keymap_test;

/// Set this variable to rewrite the checked-in page from the keymaps.
const UPDATE_VARIABLE: &str = "BONGOS_UPDATE_KEYBINDINGS_PAGE";

/// The prompt editor in vim's normal mode, where the `space` leader of the prompts rows binds.
const PROMPTS_VIM_NORMAL: [&str; 3] = [
    "Workspace",
    "Pane",
    "Editor mode=full prompts vim_mode=normal vim_operator=none VimControl",
];

/// The contexts a derived row is resolved in: the plain stack, and the stack in vim's normal mode.
fn stacks_of(context: &str) -> Option<(&'static [&'static str], &'static [&'static str])> {
    match context {
        "Workspace" => Some((&WORKSPACE, &VIM_NORMAL)),
        "Editor && prompts" => Some((&PROMPTS_EDITOR, &PROMPTS_VIM_NORMAL)),
        _ => None,
    }
}

/// The VS Code keys the profile promises on both platforms: the key as spelled on Linux (a
/// leading `ctrl` reads as `cmd` on macOS), the contexts it resolves in and the command it runs.
const VSCODE_KEYS: &[(&str, &[&str], &str)] = &[
    ("ctrl-p", &WORKSPACE, "file_finder::Toggle"),
    ("ctrl-shift-p", &WORKSPACE, "command_palette::Toggle"),
    ("ctrl-d", &EDITOR, "editor::SelectNext"),
    ("alt-up", &EDITOR, "editor::MoveLineUp"),
    ("alt-down", &EDITOR, "editor::MoveLineDown"),
    ("ctrl-/", &EDITOR, "editor::ToggleComments"),
];

/// Where the VS Code profile's Linux and macOS keys differ in command or in key, and why. Every
/// entry is checked in both directions, and the page lists them.
struct Exception {
    /// What the exception is about, as the page words it.
    what: &'static str,
    linux_key: &'static str,
    macos_key: &'static str,
    linux_action: &'static str,
    macos_action: &'static str,
    why: &'static str,
}

const EXCEPTIONS: &[Exception] = &[
    Exception {
        what: "Terminal: leave for the first editor pane",
        linux_key: "alt-1",
        macos_key: "cmd-1",
        linux_action: "workspace::ActivatePane",
        macos_action: "workspace::ActivatePane",
        why: "the Linux `Pane` context binds `alt-1` to its own item, so the terminal row \
              restores the way out; macOS already has `cmd-1`",
    },
    Exception {
        what: "Terminal: `ctrl-k` / `cmd-k`",
        linux_key: "ctrl-k",
        macos_key: "cmd-k",
        linux_action: "terminal::SendKeystroke",
        macos_action: "terminal::Clear",
        why: "Linux terminals forward `ctrl-k` (readline's kill-line) to the shell instead of \
              holding the `ctrl-k b` leader; on macOS `cmd-k` clears the terminal, as it does in \
              VS Code, and `ctrl-k` is no chord prefix",
    },
];

/// The terminal contexts the exceptions are checked in.
const TERMINAL_STACKS: [&[&str]; 2] = [&TERMINAL, &PANE_TERMINAL];

/// The macOS spelling of a Linux key: a leading `ctrl` reads as `cmd`.
fn macos_spelling(key: &str) -> String {
    match key.strip_prefix("ctrl-") {
        Some(rest) => format!("cmd-{rest}"),
        None => key.to_string(),
    }
}

/// The leader of the VS Code profile.
fn vscode_leader(macos: bool) -> &'static str {
    if macos { "cmd-k b" } else { "ctrl-k b" }
}

/// One bongos command row of the Emacs keymap.
struct Row {
    context: String,
    action: Box<dyn gpui::Action>,
    /// The keys after `ctrl-c`.
    tail: String,
}

/// What a profile binds a row to, per platform (Linux first): the inputs that run the row's
/// command.
type Cells = [Vec<String>; 2];

fn load_text(json: &str, cx: &TestAppContext) -> Vec<KeyBinding> {
    cx.update(|cx| match KeymapFile::load(json, cx) {
        KeymapFileLoadResult::Success { mut key_bindings } => {
            for binding in &mut key_bindings {
                binding.set_meta(KeybindSource::Base.meta());
            }
            key_bindings
        }
        _ => panic!("the keymap text does not load"),
    })
}

fn asset_text(path: &str) -> String {
    util::asset_str::<settings::SettingsAssets>(path).to_string()
}

/// A copy of an Emacs keymap's text with a section of bindings added at its end.
fn with_section(source: &str, section: &str) -> String {
    let end = source.rfind(']').expect("the keymap is a JSON array");
    format!("{}{section},\n]", &source[..end])
}

/// The page and the problems found while rendering it.
struct Rendered {
    page: String,
    problems: Vec<String>,
}

fn input_resolves(keymap: &Keymap, input: &str, contexts: &[&str], row: &Row) -> bool {
    let keystrokes: Vec<Keystroke> = input
        .split(' ')
        .map(|key| Keystroke::parse(key).unwrap())
        .collect();
    let (bindings, pending) = keymap.bindings_for_input(&keystrokes, &stack(contexts));
    !pending
        && bindings
            .first()
            .is_some_and(|binding| binding.action().partial_eq(row.action.as_ref()))
}

/// Renders the page from an Emacs keymap's text on each platform (Linux first). Each profile
/// loads its parts, and the rows of `emacs_sources` are bound on top the way the loader binds
/// them: as they are under Emacs, behind the derived leaders under VS Code and Vim.
fn render_page(emacs_sources: [&str; 2], cx: &TestAppContext) -> Rendered {
    let mut problems = Vec::new();
    // (family, command name, context, tail) -> cells per profile (Emacs, Vim, VS Code).
    let mut table: BTreeMap<(String, String, String, String), [Cells; 3]> = BTreeMap::new();

    for (index, macos) in [false, true].into_iter().enumerate() {
        let source = emacs_sources[index];
        let platform = if macos { "macOS" } else { "Linux" };
        let rows: Vec<Row> = command_rows(load_text(source, cx))
            .into_iter()
            .map(|binding| Row {
                context: context_of(&binding),
                tail: keys_of(&binding)[1..].join(" "),
                action: binding.action().boxed_clone(),
            })
            .collect();
        assert!(!rows.is_empty(), "{platform}: no bongos command rows");

        let leader_rows = |leader| {
            load_text(
                &settings::leader_bindings(source, leader, macos).unwrap(),
                cx,
            )
        };
        let vscode_rows = leader_rows(Leader::VsCode);
        let mut vim_rows = vscode_rows.clone();
        vim_rows.extend(leader_rows(Leader::VimSpace));
        let mut emacs_bindings = load_profile(BaseKeymap::Emacs, false, macos, cx);
        emacs_bindings.extend(load_text(source, cx));
        let mut vscode_bindings = load_profile(BaseKeymap::VSCode, false, macos, cx);
        vscode_bindings.extend(vscode_rows);
        let mut vim_bindings = load_profile(BaseKeymap::Vim, true, macos, cx);
        vim_bindings.extend(vim_rows);
        let keymaps = [
            Keymap::new(emacs_bindings),
            Keymap::new(vim_bindings),
            Keymap::new(vscode_bindings),
        ];

        for row in &rows {
            let name = row.action.name();
            let Some((plain, vim)) = stacks_of(&row.context) else {
                problems.push(format!(
                    "{platform}: no context stack for `{}` rows ({name}): add one to `stacks_of`",
                    row.context
                ));
                continue;
            };
            let leader = vscode_leader(macos);
            let inputs: [Vec<(String, &[&str])>; 3] = [
                vec![(format!("ctrl-c {}", row.tail), plain)],
                vec![
                    (format!("{leader} {}", row.tail), plain),
                    (format!("space {}", row.tail), vim),
                ],
                vec![(format!("{leader} {}", row.tail), plain)],
            ];
            let family = name.split("::").next().unwrap_or(name).to_string();
            let cells = table
                .entry((
                    family,
                    name.to_string(),
                    row.context.clone(),
                    row.tail.clone(),
                ))
                .or_default();
            for (profile, (keymap, inputs)) in keymaps.iter().zip(inputs).enumerate() {
                for (input, contexts) in inputs {
                    if input_resolves(keymap, &input, contexts, row) {
                        cells[profile][index].push(input);
                    } else {
                        problems.push(format!(
                            "{platform}: `{input}` does not resolve to {name} in {contexts:?} \
                             under {}",
                            ["Emacs", "Vim", "VS Code"][profile]
                        ));
                    }
                }
            }
        }
    }
    for ((_, name, _, tail), cells) in &table {
        for (profile, cells) in cells.iter().enumerate() {
            if cells[0].is_empty() != cells[1].is_empty() {
                problems.push(format!(
                    "{name} `{tail}` is bound on one platform only under {}",
                    ["Emacs", "Vim", "VS Code"][profile]
                ));
            }
        }
    }
    Rendered {
        page: page_text(&table, cx),
        problems,
    }
}

fn cell(cells: &Cells) -> String {
    let platform = |inputs: &Vec<String>| {
        if inputs.is_empty() {
            "none".to_string()
        } else {
            inputs
                .iter()
                .map(|input| format!("`{input}`"))
                .collect::<Vec<_>>()
                .join(", ")
        }
    };
    format!("{} / {}", platform(&cells[0]), platform(&cells[1]))
}

fn page_text(
    table: &BTreeMap<(String, String, String, String), [Cells; 3]>,
    cx: &TestAppContext,
) -> String {
    let mut page = String::new();
    page.push_str(
        "# Keybindings\n\n\
         <!-- Generated by the test `keybinding_page_tests` in `crates/bongos/src/zed`. Do not \
         edit by hand: run that test with BONGOS_UPDATE_KEYBINDINGS_PAGE=1 to rewrite this file. \
         -->\n\n\
         bongos has three keybinding profiles: Emacs, Vim and VS Code (the `base_keymap` \
         setting, or the keybindings picker). Every bongos command is bound under all three. \
         The commands are bound once, in the Emacs keymaps under `ctrl-c`; the Vim and VS Code \
         keys are derived from those rows when the keymap loads, so a command added to the Emacs \
         keymap gets its Vim and VS Code keys with no further work.\n\n\
         Each cell reads `Linux / macOS`. Every command is also in the command palette and in \
         `M-x` under every profile.\n",
    );
    let mut family = "";
    for ((fam, name, context, tail), cells) in table {
        if fam != family {
            page.push_str(&format!(
                "\n## {fam}\n\n| Command | Emacs | Vim | VS Code |\n| --- | --- | --- | --- |\n"
            ));
            family = fam;
        }
        let command = if context == "Workspace" {
            format!("`{name}` (`{tail}`)")
        } else {
            format!("`{name}` (`{tail}`, in `{context}`)")
        };
        page.push_str(&format!(
            "| {command} | {} | {} | {} |\n",
            cell(&cells[0]),
            cell(&cells[1]),
            cell(&cells[2])
        ));
    }
    page.push_str(&rules_text(cx));
    page
}

/// Contexts of the overrides files that hold a session's own keys, not a panel's.
const SESSION_CONTEXTS: &[&str] = &["Minibuffer", "QueryReplace"];

/// The panel keys of the specific overrides files, as `(context, key, command)`, sorted; the
/// Linux and macOS files must hold the same ones.
fn panel_keys(cx: &TestAppContext) -> Vec<(String, String, String)> {
    let load = |path: &str| {
        let mut keys: Vec<(String, String, String)> = cx
            .update(|cx| KeymapFile::load_asset(path, Some(KeybindSource::Base), cx).unwrap())
            .iter()
            .filter(|binding| is_panel_key(binding))
            .filter(|binding| !SESSION_CONTEXTS.contains(&context_of(binding).as_str()))
            .map(|binding| {
                (
                    context_of(binding),
                    keys_of(binding).join(" "),
                    binding.action().name().to_string(),
                )
            })
            .collect();
        keys.sort();
        keys
    };
    let linux = load("keymaps/specific-overrides.json");
    assert_eq!(
        linux,
        load("keymaps/specific-overrides-macos.json"),
        "the panel keys of the two overrides files differ"
    );
    linux
}

fn rules_text(cx: &TestAppContext) -> String {
    let mut text = String::new();
    text.push_str(
        "\n## Profile rules\n\n\
         - **The leaders.** Emacs binds the commands under `C-c`. VS Code binds them under \
         `ctrl-k b` (`cmd-k b` on macOS), in the same contexts. Vim binds both of those and \
         also `space` in normal and visual mode (and in the prompt editor's normal mode for \
         the prompts commands).\n\
         - **The `space` delay.** Vim's own `space` moves the cursor right. Because `space w w` \
         and the other chords start with `space`, a lone `space` waits for the chord timeout \
         before it moves; type the chord or wait.\n\
         - **Prompt editor.** The chat prompt editor is a full editor. Under Vim it opens in \
         normal mode: `i` enters insert, `Escape` returns to normal, and `ctrl-k b ctrl-c` \
         (`space ctrl-c` in normal mode) sends. Under VS Code the default editing keys apply, \
         so `ctrl-c` copies, and `ctrl-k b ctrl-c` sends.\n\
         - **Terminals.** Under VS Code and Vim a terminal forwards keys to the shell: the \
         bongos leaders are not bound there, and `ctrl-c`, `ctrl-d`, `ctrl-w`, `ctrl-r` and \
         `escape` reach the shell. The one way out is `alt-1` (`cmd-1` on macOS), which \
         focuses the first editor pane. Chat terminals follow the same rule. Commands from a \
         terminal go through the command palette (`ctrl-shift-p`, `cmd-shift-p`).\n\
         - **Panel keys.** The single keys of the bongos panels live in the specific overrides \
         files (`assets/keymaps/specific-overrides.json` and `specific-overrides-macos.json`), \
         which every profile loads after its base keymap, so they work under every profile. A \
         panel key added later goes in those files, not in the Emacs keymap. They are:\n\n\
         | Panel context | Key | Command |\n| --- | --- | --- |\n",
    );
    for (context, key, action) in panel_keys(cx) {
        text.push_str(&format!("| `{context}` | `{key}` | `{action}` |\n"));
    }
    text.push_str(
        "\n- **Your `keymap.json`.** It loads last, so a binding of yours wins over every \
         profile at the same context depth.\n\
         - **Linux exceptions.** Under VS Code the Linux and macOS keymaps bind the same \
         commands, with `ctrl` read as `cmd`; these are the differences, because Linux \
         terminals and window managers reserve some chords:\n\n\
         | Where | Linux | macOS | Why |\n| --- | --- | --- | --- |\n",
    );
    for exception in EXCEPTIONS {
        text.push_str(&format!(
            "| {} | `{}` runs `{}` | `{}` runs `{}` | {} |\n",
            exception.what,
            exception.linux_key,
            exception.linux_action,
            exception.macos_key,
            exception.macos_action,
            exception.why
        ));
    }
    text
}

fn page_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/keybindings.md")
}

fn emacs_sources() -> [String; 2] {
    [
        asset_text("keymaps/linux/emacs.json"),
        asset_text("keymaps/macos/emacs.json"),
    ]
}

fn render_current(cx: &TestAppContext) -> Rendered {
    let sources = emacs_sources();
    render_page([&sources[0], &sources[1]], cx)
}

#[gpui::test]
async fn the_checked_in_page_matches_the_keymaps(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    let rendered = render_current(cx);
    assert!(rendered.problems.is_empty(), "{:#?}", rendered.problems);
    let path = page_path();
    if std::env::var_os(UPDATE_VARIABLE).is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &rendered.page).unwrap();
        return;
    }
    let checked_in = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(
        checked_in == rendered.page,
        "docs/keybindings.md is out of date with the keymaps: rerun this test with \
         {UPDATE_VARIABLE}=1 to rewrite it, then commit the file"
    );
}

#[gpui::test]
async fn every_command_resolves_under_all_three_profiles(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    let rendered = render_current(cx);
    assert!(rendered.problems.is_empty(), "{:#?}", rendered.problems);
}

#[gpui::test]
async fn a_row_added_to_the_emacs_file_reaches_every_profile(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    let fixture = r#"{"context": "Workspace", "bindings": {"ctrl-c z z": "chat::New"}}"#;
    let sources = emacs_sources().map(|source| with_section(&source, fixture));
    let rendered = render_page([&sources[0], &sources[1]], cx);
    assert!(rendered.problems.is_empty(), "{:#?}", rendered.problems);
    let line = rendered
        .page
        .lines()
        .find(|line| line.contains("(`z z`)"))
        .expect("the fixture row is on the page");
    for expected in [
        "`ctrl-c z z` / `ctrl-c z z`",
        "`ctrl-k b z z`, `space z z` / `cmd-k b z z`, `space z z`",
        "`ctrl-k b z z` / `cmd-k b z z`",
    ] {
        assert!(line.contains(expected), "{expected} is not in {line}");
    }
}

#[gpui::test]
async fn every_command_is_registered_and_not_hidden_by_the_palette_filter(cx: &mut TestAppContext) {
    use settings::Settings as _;

    init_keymap_test(cx);
    cx.update(|cx| {
        command_palette::init(cx);
        bongos_work::WorkSettings::register(cx);
        bongos_chat::ChatsSettings::register(cx);
        bongos_prompts::PromptsSettings::register(cx);
        bongos_builder::TaskBuilderSettings::register(cx);
        bongos_linear::LinearSettings::register(cx);
        bongos_workflow::WorkflowsSettings::register(cx);
        bongos_work::init_palette(cx);
        bongos_chat::init_palette(cx);
        bongos_prompts::init_palette(cx);
        bongos_builder::init_palette(cx);
        bongos_linear::init_palette(cx);
        bongos_workflow::init_palette(cx);
    });
    for (index, source) in emacs_sources().iter().enumerate() {
        let rows = command_rows(load_text(source, cx));
        assert!(!rows.is_empty());
        cx.update(|cx| {
            let registered = cx.all_action_names().to_vec();
            let filter = command_palette_hooks::CommandPaletteFilter::try_global(cx)
                .expect("the command palette filter is set");
            for row in &rows {
                let name = row.action().name();
                assert!(
                    registered.contains(&name),
                    "platform {index}: {name} is not a registered action"
                );
                assert!(
                    !filter.is_hidden(row.action()),
                    "platform {index}: {name} is hidden from the command palette"
                );
            }
        });
    }
}

#[gpui::test]
async fn vscode_binds_the_same_commands_on_both_platforms(cx: &mut TestAppContext) {
    init_keymap_test(cx);
    let linux = Keymap::new(load_profile(BaseKeymap::VSCode, false, false, cx));
    let macos = Keymap::new(load_profile(BaseKeymap::VSCode, false, true, cx));

    for (key, contexts, action) in VSCODE_KEYS {
        assert_eq!(first(&linux, key, contexts), Some(*action), "Linux: {key}");
        assert_eq!(
            first(&macos, &macos_spelling(key), contexts),
            Some(*action),
            "macOS: {key}"
        );
    }

    // The derived rows: the same commands behind `ctrl-k b` and `cmd-k b`.
    let sources = emacs_sources();
    let linux_rows = command_rows(load_text(&sources[0], cx));
    let macos_rows = command_rows(load_text(&sources[1], cx));
    assert_eq!(linux_rows.len(), macos_rows.len());
    for row in &linux_rows {
        let tail = keys_of(row)[1..].join(" ");
        let (plain, _) = stacks_of(&context_of(row)).expect("a context stack");
        let on_linux = first(&linux, &format!("ctrl-k b {tail}"), plain);
        let on_macos = first(&macos, &format!("cmd-k b {tail}"), plain);
        assert_eq!(on_linux, Some(row.action().name()), "Linux: {tail}");
        assert_eq!(on_macos, on_linux, "macOS: {tail}");
    }

    // The listed exceptions are exactly as listed.
    for exception in EXCEPTIONS {
        for contexts in TERMINAL_STACKS {
            assert_eq!(
                resolve(&linux, exception.linux_key, contexts).0.first(),
                Some(&exception.linux_action),
                "Linux: {}",
                exception.what
            );
            assert_eq!(
                resolve(&macos, exception.macos_key, contexts).0.first(),
                Some(&exception.macos_action),
                "macOS: {}",
                exception.what
            );
        }
    }
}
