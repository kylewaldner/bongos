# Upstream patches

Every file that differs from upstream Zed, as of the commit in `UPSTREAM`. `script/bongos/check-patches`
compares this table with `git diff` against that commit, both ways: a changed upstream file with no row
fails, and a row that matches no changed file fails. Files that bongos adds under `crates/bongos_*`,
`script/bongos/`, `plans/`, `.claude/` and a few root files are ours and need no row.

A step that patches an upstream file adds its row in the same commit. The first column takes
backticked paths, `dir/**` for a whole tree and `{a,b}` for alternatives.

| Files | Lines | Reason |
|---|---|---|
| `.github/**` | deleted | Upstream workflows must not run here; `check` fails if `.github` reappears |
| `README.md` | ours | Root clash: our README, plus a derived-from-Zed and licence notice |
| `crates/paths/src/paths.rs` | 5 lines, plus a test | `APP_NAME = "bongos"`: config, data, cache, state and log paths become bongos's; macOS config falls back to `~/.config/bongos`; `.zed` becomes `.bongos` in `local_settings_folder_name` and the settings, tasks and debug relative paths |
| `crates/release_channel/src/lib.rs` | 8 lines, plus a test | `display_name` and `app_id` arms: `dev.bongos.Bongos`, `-Dev`, `-Nightly`, `-Preview` |
| `crates/zed/Cargo.toml` | 2 lines | `[[bin]] name = "bongos"` and `default-run`; the package stays `zed` (D2) |
| `crates/zed/src/zed/open_listener.rs` | 1 line | CLI socket `zed-<channel>.sock` becomes `bongos-<channel>.sock`, so the data directory holds no zed-named file; the two files must agree |
| `crates/cli/src/main.rs` | 1 line | Same socket rename, as the CLI connects to the app by this path |
| `crates/workspace/src/workspace.rs` | 3 lines | Window title ends in ` — bongos` in non-test builds, so the app name shows; the `cfg` keeps upstream's `window_title()` test assertions |
| `assets/settings/default.json` | 2 lines | `.zed` globs become `.bongos`: the sensitive-file and JSONC `file_types` entries |
| `crates/debugger_ui/src/{new_process_modal,debugger_panel}.rs` | 3 lines | `.zed` literal becomes `.bongos`, so the debug config picker finds `.bongos/debug.json` |
| `crates/project/src/task_inventory.rs` | 2 lines | `.zed` literal compared against `paths::local_settings_folder_name()` |
| `crates/project/tests/integration/{project_tests,task_inventory}.rs` | fixtures, plus a test | `.zed` fixtures become `.bongos`; new test that `.bongos/settings.json` applies and `.zed` does not |
| `crates/debugger_ui/src/tests/new_process_modal.rs` | test fixtures | `.zed` fixtures become `.bongos` |
| `crates/tasks_ui/src/modal.rs` | test fixtures | `.zed` fixtures become `.bongos` |
| `crates/agent/src/tools/{edit_file_tool,list_directory_tool,grep_tool,read_file_tool}.rs` | test fixtures | `.zed` fixtures become `.bongos`, as the agent's local-settings protection reads `paths::local_settings_folder_name()` |
| `crates/agent/src/tests/mod.rs` | test fixtures | Same: `.zed` fixture in one edit-tool test becomes `.bongos` |
| `crates/zed/src/zed.rs` | test fixtures | `.zed` fixtures in `test_opening_project_settings_when_excluded` become `.bongos` |
