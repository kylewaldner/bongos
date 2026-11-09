use std::time::{Duration, Instant};

use crate::engine_tests::Lcg;
use crate::{Cancel, Narrower, texts};

/// Directory names of a monorepo, a few hundred of them so that no one name is common.
const DIRECTORIES: &str = "crates src lib bin docs assets script scripts tests benches examples \
    components editor view views model models controller service services handler handlers util utils \
    core common shared base platform terminal picker workspace settings project projects language \
    languages lsp rpc client server proto protocol collab call channel channels chat notifications \
    auth user users accounts billing git diff blame branch commit history merge rebase stash remote \
    remotes search replace find query index indexer indexing cache store storage db database sql \
    schema migrations queries fixtures mocks stubs fakes snapshots golden data json yaml toml config \
    configs env build builder compile compiler parser parsers lexer syntax tree treesitter grammar \
    grammars highlight theme themes color colors fonts font icons icon images image svg audio video \
    media net http https grpc websocket socket stream streams channel async executor runtime tasks \
    task jobs job queue scheduler timer clock time date i18n locale locales translations strings text \
    buffer buffers rope anchor selection selections cursor cursors keymap keymaps keybinding bindings \
    commands command actions action menus menu toolbar statusbar tabbar sidebar panel panels dock \
    pane panes layout geometry shapes render renderer paint scene shaders gpu vulkan metal wayland \
    x11 windows macos linux android ios web wasm browser extension extensions plugins plugin api \
    sdk cli tools tooling devtools debug debugger profiler tracing logging log metrics telemetry \
    crash reporter updater installer release releases packaging bundle dist vendor third_party \
    deps external internal private public stable nightly preview beta canary legacy compat shim \
    kotlin java gradle maven android_studio jvm bytecode decompiler refactor refactoring inspections \
    intentions completion completions snippets templates formatter format lint linter analysis \
    inference types typeck resolve resolver symbols symbol references usages hierarchy outline \
    structure navigation bookmarks marks registers kill_ring macros minibuffer modeline echo area \
    window frame frames buffer_list dired ibuffer magit org_mode vc project_el eglot company ivy";

/// File name stems, as many again.
const STEMS: &str = "main lib mod init setup teardown app application builder config context controller \
    client server handler helper manager provider factory registry repository service store state \
    action command event listener observer subscriber publisher dispatcher router route routes \
    request response error errors result option options settings defaults constants types traits \
    impls extensions ext utils util helpers common base core api schema model models entity entities \
    parser lexer tokenizer scanner printer writer reader loader saver importer exporter serializer \
    encoder decoder codec stream buffer cache pool queue stack list map set tree graph node edge \
    item items entry entries record records row rows column columns cell cells table tables field \
    fields value values key keys index indexes cursor iterator visitor walker mapper reducer filter \
    sorter matcher finder searcher resolver binder linker compiler runner executor scheduler worker \
    thread task job timer clock tracker monitor watcher poller notifier logger tracer profiler \
    metrics counter gauge histogram bench fixture mock stub fake harness runner session transport \
    channel socket connection listener acceptor handshake auth token credentials permissions roles \
    account profile preferences theme palette color font icon image sprite atlas texture shader \
    pipeline renderer painter canvas layer surface window frame panel pane dock tab tabs sidebar \
    toolbar menu popup tooltip dialog modal picker prompt input output console terminal pty shell \
    keymap keybinding gesture mouse touch scroll scrollbar zoom focus selection highlight diagnostics \
    completion hover signature rename format symbols outline folding indent layout wrap display \
    gutter minimap breadcrumb statusline modeline tabline ruler inlay lens action quickfix \
    snippet template macro register mark bookmark history undo redo transaction patch diff merge \
    blame commit branch stash remote fetch push pull rebase status staging hunks conflicts review \
    comments threads notifications presence collab session room call share follow project worktree \
    workspace settings extension plugin theme grammar language server client tool task debug \
    breakpoint stack_frame variables watch evaluate attach launch adapter";

/// Realistic synthetic paths, depth 3 to 8, each with a file name and an extension. Segments are
/// drawn from a vocabulary of several hundred directory names and file stems, as in a real
/// monorepo, so the finder's query words (`src`, `components`, `editor`, `view`) are four common
/// names among many rather than the whole word list. The final query `src comp edit view` matches
/// about half a percent of the 100,000 paths (494 when written), which the test asserts stays a
/// small minority.
fn realistic_paths(count: usize, seed: u64) -> Vec<String> {
    let directories: Vec<&str> = DIRECTORIES.split_whitespace().collect();
    let stems: Vec<&str> = STEMS.split_whitespace().collect();
    let mut rng = Lcg::new(seed);
    (0..count)
        .map(|_| {
            let depth = 3 + rng.below(6);
            let mut path = String::new();
            for _ in 0..depth - 1 {
                path.push_str(directories[rng.below(directories.len())]);
                path.push('/');
            }
            path.push_str(stems[rng.below(stems.len())]);
            path.push(['_', '-', '.'][rng.below(3)]);
            path.push_str(stems[rng.below(stems.len())]);
            path.push_str([".rs", ".kt", ".json", ".md"][rng.below(4)]);
            path
        })
        .collect()
}

/// Half a frame at 60 Hz; the other half is for render.
const BUDGET: Duration = Duration::from_millis(8);

/// Typing `src comp edit view` one character at a time over 100,000 paths, then backspacing it
/// away: every pass, best of three, must fit the budget. Dev builds optimize this crate and
/// `nucleo` (the workspace `[profile.dev.package]` table) so the gate measures real code.
#[test]
fn a_keystroke_over_100k_paths_fits_half_a_frame() {
    let mut engine = Narrower::new(texts(realistic_paths(100_000, 2026)));
    let cancel = Cancel::new();
    let typed = "src comp edit view";

    let mut worst = Duration::ZERO;
    let mut final_matches = 0;
    let mut report = Vec::new();
    for end in 1..=typed.len() {
        let query = &typed[..end];
        let previous = &typed[..end - 1];
        let mut best = Duration::MAX;
        let mut matched = 0;
        for _ in 0..3 {
            // Back to the state before this keystroke, which is a stack pop and costs no pass.
            engine.narrow(previous, &cancel).expect("not cancelled");
            let started = Instant::now();
            let matches = engine.narrow(query, &cancel).expect("not cancelled");
            let elapsed = started.elapsed();
            assert!(
                query.ends_with(' ') || engine.last_scored() > 0,
                "the pass ran"
            );
            matched = matches.len();
            std::hint::black_box(&matches);
            best = best.min(elapsed);
        }
        final_matches = matched;
        report.push(format!(
            "{query:?}: {best:?} ({matched} matched of {} scored)",
            engine.last_scored()
        ));
        worst = worst.max(best);
    }
    assert!(
        (100..5_000).contains(&final_matches),
        "the final query matches {final_matches} of 100000 paths, which is not a realistic minority"
    );
    // Backspace: each step reuses a stacked pass. Best of three here too, so that one scheduler
    // stall on a loaded machine cannot fail the gate; a real slowdown shows in every run.
    for end in (0..typed.len()).rev() {
        let mut best = Duration::MAX;
        for _ in 0..3 {
            // Back to the state before this backspace, which costs no measured time.
            engine
                .narrow(&typed[..=end], &cancel)
                .expect("not cancelled");
            let started = Instant::now();
            engine
                .narrow(&typed[..end], &cancel)
                .expect("not cancelled");
            best = best.min(started.elapsed());
        }
        report.push(format!("backspace to {:?}: {best:?}", &typed[..end]));
        worst = worst.max(best);
    }
    eprintln!(
        "100k keystroke timings (best of three):\n{}",
        report.join("\n")
    );
    eprintln!("worst pass: {worst:?} (budget {BUDGET:?})");
    assert!(
        worst < BUDGET,
        "worst pass {worst:?} is over {BUDGET:?}:\n{}",
        report.join("\n")
    );
}
