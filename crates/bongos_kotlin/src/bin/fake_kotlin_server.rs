//! A stand-in for kotlin-lsp and the IntelliJ server, used by tests and smoke scenarios. It speaks
//! LSP over stdio with Content-Length framing. `BONGOS_FAKE_LSP_MODE` holds comma-separated modes:
//! `progress`, `push`, `pull`, `crash-after=<ms>` (exit 1 that long after `initialized`),
//! `hang-shutdown`, `license-lost-after=<ms>`, `slow-init=<ms>`, `stderr` (one line on stderr at start). `BONGOS_FAKE_LSP_PIDS` names a directory that gets one file per live process.
//! `license status` prints per `BONGOS_FAKE_LICENSE=licensed|unlicensed|offline`.

use serde_json::{Value, json};
use std::{
    collections::HashMap,
    fs,
    io::{self, BufRead, Write},
    path::PathBuf,
    process::ExitCode,
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

const EULA_TEXT: &str = "FAKE USER AGREEMENT\nVersion 1.0\n";
const STARTUP_STDERR: &str = "fake server starting up";
const LICENSE_LOST_TEXT: &str = "No valid license found";

#[derive(Default)]
struct Modes {
    progress: bool,
    push: bool,
    pull: bool,
    hang_shutdown: bool,
    stderr: bool,
    crash_after: Option<u64>,
    license_lost_after: Option<u64>,
    slow_init: Option<u64>,
}

impl Modes {
    fn from_env() -> Self {
        let mut modes = Self::default();
        let value = std::env::var("BONGOS_FAKE_LSP_MODE").unwrap_or_default();
        for mode in value
            .split(',')
            .map(str::trim)
            .filter(|mode| !mode.is_empty())
        {
            let (name, number) = match mode.split_once('=') {
                Some((name, number)) => (name, number.parse::<u64>().ok()),
                None => (mode, None),
            };
            match name {
                "progress" => modes.progress = true,
                "push" => modes.push = true,
                "pull" => modes.pull = true,
                "hang-shutdown" => modes.hang_shutdown = true,
                "stderr" => modes.stderr = true,
                "crash-after" => modes.crash_after = number,
                "license-lost-after" => modes.license_lost_after = number,
                "slow-init" => modes.slow_init = number,
                _ => {}
            }
        }
        modes
    }
}

type Output = Arc<Mutex<io::Stdout>>;

fn send(output: &Output, message: &Value) -> io::Result<()> {
    let body = serde_json::to_vec(message)?;
    let mut stdout = output
        .lock()
        .map_err(|_| io::Error::other("stdout lock poisoned"))?;
    write!(stdout, "Content-Length: {}\r\n\r\n", body.len())?;
    stdout.write_all(&body)?;
    stdout.flush()
}

fn read_message(input: &mut impl BufRead) -> io::Result<Option<Value>> {
    let mut length = None;
    loop {
        let mut line = String::new();
        if input.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some(value) = line.strip_prefix("Content-Length:") {
            length = value.trim().parse::<usize>().ok();
        }
    }
    let length = length.ok_or_else(|| io::Error::other("missing Content-Length"))?;
    let mut body = vec![0; length];
    input.read_exact(&mut body)?;
    Ok(Some(serde_json::from_slice(&body)?))
}

fn pid_file() -> Option<PathBuf> {
    let directory = std::env::var_os("BONGOS_FAKE_LSP_PIDS")?;
    Some(PathBuf::from(directory).join(std::process::id().to_string()))
}

fn finish(code: u8) -> ! {
    if let Some(path) = pid_file() {
        fs::remove_file(path).ok();
    }
    std::process::exit(code.into())
}

fn license_status() -> ExitCode {
    let (text, code) = match std::env::var("BONGOS_FAKE_LICENSE").as_deref() {
        Ok("unlicensed") => (
            format!("{LICENSE_LOST_TEXT}\nLicense: not licensed\nAccount: not signed in"),
            2,
        ),
        Ok("offline") => ("License server unreachable".to_string(), 3),
        _ => ("License: licensed".to_string(), 0),
    };
    println!("{text}");
    ExitCode::from(code)
}

fn diagnostics_for(text: &str) -> Vec<Value> {
    text.lines()
        .enumerate()
        .filter(|(_, line)| line.contains("BROKEN"))
        .map(|(index, line)| {
            json!({
                "range": {
                    "start": { "line": index, "character": 0 },
                    "end": { "line": index, "character": line.chars().count() },
                },
                "severity": 1,
                "source": "fake-kotlin",
                "message": "BROKEN line",
            })
        })
        .collect()
}

fn run_progress(output: Output) {
    let steps = [
        ("Importing", "importing Gradle project"),
        ("Indexing", "indexing 3/5 modules"),
    ];
    for (index, (title, report)) in steps.into_iter().enumerate() {
        let token = format!("fake-progress-{index}");
        let notify = |value: Value| {
            send(
                &output,
                &json!({ "jsonrpc": "2.0", "method": "$/progress", "params": { "token": token, "value": value } }),
            )
        };
        let result = send(
            &output,
            &json!({ "jsonrpc": "2.0", "id": format!("create-{index}"), "method": "window/workDoneProgress/create", "params": { "token": token } }),
        )
        .and_then(|()| notify(json!({ "kind": "begin", "title": title, "percentage": 0 })))
        .and_then(|()| {
            thread::sleep(Duration::from_millis(250));
            notify(json!({ "kind": "report", "message": report, "percentage": 50 }))
        })
        .and_then(|()| {
            thread::sleep(Duration::from_millis(250));
            notify(json!({ "kind": "end" }))
        });
        if result.is_err() {
            return;
        }
    }
}

fn serve(modes: Modes) -> io::Result<()> {
    let output: Output = Arc::new(Mutex::new(io::stdout()));
    let mut input = io::BufReader::new(io::stdin());
    let mut documents: HashMap<String, String> = HashMap::new();
    let mut shut_down = false;

    if modes.stderr {
        eprintln!("{STARTUP_STDERR}");
    }

    if let Some(milliseconds) = modes.license_lost_after {
        let output = output.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(milliseconds));
            send(
                &output,
                &json!({ "jsonrpc": "2.0", "method": "window/showMessage", "params": { "type": 1, "message": LICENSE_LOST_TEXT } }),
            )
            .ok();
        });
    }

    while let Some(message) = read_message(&mut input)? {
        let id = message.get("id").cloned();
        let Some(method) = message.get("method").and_then(Value::as_str) else {
            continue;
        };
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        let uri = params
            .pointer("/textDocument/uri")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        // Timed from the end of the handshake, not from the start of the process: a client that is
        // slow to send `initialize` must not find the server already gone.
        if method == "initialized"
            && let Some(milliseconds) = modes.crash_after
        {
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(milliseconds));
                // A crash leaves the pid file behind, as a real crash would.
                std::process::exit(1)
            });
        }
        let mut result = Value::Null;
        match method {
            "initialize" => {
                if let Some(milliseconds) = modes.slow_init {
                    thread::sleep(Duration::from_millis(milliseconds));
                }
                result = json!({
                    "capabilities": {
                        "textDocumentSync": 1,
                        "hoverProvider": true,
                        "diagnosticProvider": modes.pull.then(|| json!({ "interFileDependencies": false, "workspaceDiagnostics": false })),
                    },
                    "serverInfo": { "name": "bongos-fake-kotlin-server", "version": "0.0.0" },
                });
            }
            "initialized" if modes.progress => {
                let output = output.clone();
                thread::spawn(move || run_progress(output));
            }
            "textDocument/didOpen" | "textDocument/didChange" => {
                let text = params
                    .pointer("/textDocument/text")
                    .or_else(|| params.pointer("/contentChanges/0/text"))
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                if modes.push {
                    send(
                        &output,
                        &json!({ "jsonrpc": "2.0", "method": "textDocument/publishDiagnostics", "params": { "uri": uri, "diagnostics": diagnostics_for(&text) } }),
                    )?;
                }
                documents.insert(uri, text);
            }
            "textDocument/hover" => {
                result = json!({ "contents": { "kind": "plaintext", "value": "fake hover" } });
            }
            "textDocument/diagnostic" => {
                let text = documents.get(&uri).map(String::as_str).unwrap_or_default();
                result = json!({ "kind": "full", "items": diagnostics_for(text) });
            }
            "shutdown" => {
                shut_down = true;
                if modes.hang_shutdown {
                    continue;
                }
            }
            "exit" => finish(if shut_down { 0 } else { 1 }),
            _ => {}
        }
        if let Some(id) = id {
            send(
                &output,
                &json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            )?;
        }
    }
    Ok(())
}

fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    if arguments.first().map(String::as_str) == Some("license")
        && arguments.get(1).map(String::as_str) == Some("status")
    {
        return license_status();
    }
    if arguments.iter().any(|argument| argument == "--show-eula") {
        println!("--- EULA START ---\n{EULA_TEXT}--- EULA END ---");
        return ExitCode::SUCCESS;
    }

    if let Some(path) = pid_file() {
        if let Err(error) = fs::write(&path, b"") {
            eprintln!("cannot write {}: {error}", path.display());
            return ExitCode::from(2);
        }
    }
    let result = serve(Modes::from_env());
    // Stdin closing ends the process, so a parent that dies never leaves a server behind.
    if let Err(error) = &result {
        eprintln!("fake server: {error}");
    }
    finish(if result.is_ok() { 0 } else { 1 })
}
