//! Drives the fake server binary over stdio.
#![allow(clippy::disallowed_methods)]

use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Read, Write},
    path::Path,
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    sync::mpsc,
    thread,
    time::Duration,
};

const BINARY: &str = env!("CARGO_BIN_EXE_bongos-fake-kotlin-server");

struct Client {
    child: Child,
    stdin: Option<ChildStdin>,
    messages: mpsc::Receiver<Value>,
}

impl Client {
    fn start(mode: &str, pids: &Path) -> Self {
        let mut child = Command::new(BINARY)
            .env("BONGOS_FAKE_LSP_MODE", mode)
            .env("BONGOS_FAKE_LSP_PIDS", pids)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn fake server");
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().expect("piped stdout");
        let (sender, messages) = mpsc::channel();
        thread::spawn(move || read_messages(stdout, sender));
        Self {
            child,
            stdin,
            messages,
        }
    }

    fn send(&mut self, message: Value) {
        let body = serde_json::to_vec(&message).expect("serialize");
        let stdin = self.stdin.as_mut().expect("stdin open");
        write!(stdin, "Content-Length: {}\r\n\r\n", body.len()).expect("write header");
        stdin.write_all(&body).expect("write body");
        stdin.flush().expect("flush");
    }

    fn request(&mut self, id: u64, method: &str, params: Value) -> Value {
        self.send(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }));
        loop {
            let message = self.next();
            if message.get("id") == Some(&json!(id)) && message.get("method").is_none() {
                return message["result"].clone();
            }
        }
    }

    fn notify(&mut self, method: &str, params: Value) {
        self.send(json!({ "jsonrpc": "2.0", "method": method, "params": params }));
    }

    fn next(&mut self) -> Value {
        // Generous, because it only fires while the server is alive but silent, such as a
        // server still starting on a starved machine. A server that exits or crashes still
        // shows at once as a disconnect, and the test runner still ends a hung test.
        self.messages
            .recv_timeout(Duration::from_secs(60))
            .expect("a message within 60 s")
    }

    fn next_matching(&mut self, method: &str) -> Value {
        loop {
            let message = self.next();
            if message.get("method").and_then(Value::as_str) == Some(method) {
                return message;
            }
        }
    }

    fn close_stdin(&mut self) {
        self.stdin = None;
    }

    fn wait(&mut self) -> std::process::ExitStatus {
        self.child.wait().expect("wait")
    }

    fn initialize(&mut self) -> Value {
        let result = self.request(1, "initialize", json!({ "capabilities": {} }));
        self.notify("initialized", json!({}));
        result
    }

    fn open(&mut self, uri: &str, text: &str) {
        self.notify(
            "textDocument/didOpen",
            json!({ "textDocument": { "uri": uri, "languageId": "kotlin", "version": 1, "text": text } }),
        );
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        self.child.kill().ok();
        self.child.wait().ok();
    }
}

fn read_messages(stdout: ChildStdout, sender: mpsc::Sender<Value>) {
    let mut reader = BufReader::new(stdout);
    loop {
        let mut length = None;
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => return,
                Ok(_) => {}
            }
            let line = line.trim_end();
            if line.is_empty() {
                break;
            }
            if let Some(value) = line.strip_prefix("Content-Length:") {
                length = value.trim().parse::<usize>().ok();
            }
        }
        let Some(length) = length else { return };
        let mut body = vec![0; length];
        if reader.read_exact(&mut body).is_err() {
            return;
        }
        let Ok(message) = serde_json::from_slice(&body) else {
            return;
        };
        if sender.send(message).is_err() {
            return;
        }
    }
}

fn pid_files(pids: &Path) -> usize {
    std::fs::read_dir(pids).expect("read pids").count()
}

#[test]
fn initialize_hover_shutdown_and_pid_file() {
    let pids = tempfile::tempdir().expect("tempdir");
    let mut client = Client::start("", pids.path());
    let result = client.initialize();
    assert_eq!(result["capabilities"]["hoverProvider"], json!(true));
    assert!(result["capabilities"]["diagnosticProvider"].is_null());
    assert_eq!(pid_files(pids.path()), 1);

    let hover = client.request(2, "textDocument/hover", json!({}));
    assert_eq!(hover["contents"]["value"], "fake hover");

    assert_eq!(client.request(3, "shutdown", Value::Null), Value::Null);
    client.notify("exit", Value::Null);
    assert!(client.wait().success());
    assert_eq!(pid_files(pids.path()), 0);
}

#[test]
fn progress_reports_importing_then_indexing() {
    let pids = tempfile::tempdir().expect("tempdir");
    let mut client = Client::start("progress", pids.path());
    client.initialize();
    let mut titles = Vec::new();
    let mut reports = Vec::new();
    let mut ends = 0;
    while ends < 2 {
        let message = client.next_matching("$/progress");
        let value = &message["params"]["value"];
        match value["kind"].as_str() {
            Some("begin") => titles.push(value["title"].as_str().unwrap_or_default().to_string()),
            Some("report") => {
                reports.push(value["message"].as_str().unwrap_or_default().to_string())
            }
            Some("end") => ends += 1,
            other => panic!("unexpected progress kind {other:?}"),
        }
    }
    assert_eq!(titles, ["Importing", "Indexing"]);
    assert!(
        reports
            .iter()
            .any(|report| report == "indexing 3/5 modules")
    );
}

#[test]
fn push_diagnostics_flag_only_broken_lines() {
    let pids = tempfile::tempdir().expect("tempdir");
    let mut client = Client::start("push", pids.path());
    client.initialize();
    client.open("file:///a.kt", "fun ok() {}\nval BROKEN = 1\n");
    let message = client.next_matching("textDocument/publishDiagnostics");
    let diagnostics = message["params"]["diagnostics"]
        .as_array()
        .expect("diagnostics array");
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0]["range"]["start"]["line"], 1);
    assert_eq!(diagnostics[0]["severity"], 1);

    client.open("file:///b.kt", "fun fine() {}\n");
    let message = client.next_matching("textDocument/publishDiagnostics");
    assert_eq!(message["params"]["diagnostics"], json!([]));
}

#[test]
fn pull_diagnostics_are_advertised_and_answered() {
    let pids = tempfile::tempdir().expect("tempdir");
    let mut client = Client::start("pull", pids.path());
    let result = client.initialize();
    assert!(result["capabilities"]["diagnosticProvider"].is_object());
    client.open("file:///a.kt", "BROKEN\nfine\n");
    let report = client.request(
        2,
        "textDocument/diagnostic",
        json!({ "textDocument": { "uri": "file:///a.kt" } }),
    );
    assert_eq!(report["kind"], "full");
    assert_eq!(report["items"].as_array().map(Vec::len), Some(1));
}

#[test]
fn crash_after_exits_nonzero_and_leaves_its_pid_file() {
    let pids = tempfile::tempdir().expect("tempdir");
    let mut client = Client::start("crash-after=200", pids.path());
    client.initialize();
    let status = client.wait();
    assert_eq!(status.code(), Some(1));
    assert_eq!(pid_files(pids.path()), 1);
}

#[test]
fn closing_stdin_ends_the_server_and_removes_the_pid_file() {
    let pids = tempfile::tempdir().expect("tempdir");
    let mut client = Client::start("hang-shutdown", pids.path());
    client.initialize();
    client.send(json!({ "jsonrpc": "2.0", "id": 2, "method": "shutdown" }));
    client.close_stdin();
    assert!(client.wait().success());
    assert_eq!(pid_files(pids.path()), 0);
}

#[test]
fn license_lost_sends_the_show_message_signal() {
    let pids = tempfile::tempdir().expect("tempdir");
    let mut client = Client::start("license-lost-after=100", pids.path());
    client.initialize();
    let message = client.next_matching("window/showMessage");
    assert_eq!(message["params"]["message"], "No valid license found");
}

fn license_status(license: &str) -> (Option<i32>, String) {
    let output = Command::new(BINARY)
        .args(["license", "status"])
        .env("BONGOS_FAKE_LICENSE", license)
        .output()
        .expect("run license status");
    (
        output.status.code(),
        String::from_utf8_lossy(&output.stdout).into_owned(),
    )
}

#[test]
fn license_status_subcommand_follows_the_environment() {
    let (code, text) = license_status("licensed");
    assert_eq!(code, Some(0));
    assert!(text.contains("licensed"));
    let (code, text) = license_status("unlicensed");
    assert_eq!(code, Some(2));
    assert!(text.contains("No valid license found"));
    let (code, _) = license_status("offline");
    assert_eq!(code, Some(3));
}
