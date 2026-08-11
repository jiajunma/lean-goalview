//! lean-goalview — a Lean 4 goal view for editors that cannot host the real
//! infoview (Zed, and any other plain-LSP editor).
//!
//! Sits between the editor and `lake serve` as a transparent LSP proxy:
//!
//!   editor ↔ lean-goalview ↔ lake serve
//!                 │
//!                 └→ renders the goal state at the cursor into a markdown
//!                    file (default: <workspace>/.goalview.md). Keep that
//!                    file open in a split; it reloads on every cursor move.
//!
//! Cursor tracking: the editor's own `textDocument/documentHighlight` and
//! `textDocument/hover` requests carry the cursor position — Zed sends
//! documentHighlight whenever the cursor rests on a symbol. On each observed
//! position the proxy asks the server for `$/lean/plainGoal` and
//! `$/lean/plainTermGoal` (the same data the official infoview renders,
//! minus the browser-only interactivity) using string request ids namespaced
//! "gv:" so they can never collide with the editor's numeric ids.
//!
//! Configuration (env vars):
//!   LEAN_GOALVIEW_FILE   output path (default <workspace>/.goalview.md)
//!   LEAN_GOALVIEW_LAKE   lake binary (default: `lake` on PATH, else ~/.elan/bin/lake)

use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------- framing --

fn read_frame(reader: &mut impl BufRead) -> Option<Vec<u8>> {
    let mut content_length: usize = 0;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some(v) = line.strip_prefix("Content-Length:") {
            content_length = v.trim().parse().ok()?;
        }
    }
    let mut body = vec![0u8; content_length];
    reader.read_exact(&mut body).ok()?;
    Some(body)
}

fn write_frame(writer: &mut impl Write, body: &[u8]) {
    let _ = write!(writer, "Content-Length: {}\r\n\r\n", body.len());
    let _ = writer.write_all(body);
    let _ = writer.flush();
}

// ------------------------------------------------------------------ state --

#[derive(Default)]
struct State {
    /// (uri, line, character) of the last observed cursor position.
    cursor: Option<(String, u64, u64)>,
    /// Diagnostics per uri, as last published by the server.
    diagnostics: HashMap<String, Value>,
    /// Files still being elaborated (uri -> true while fileProgress pending).
    processing: HashMap<String, bool>,
    /// Latest fetched goal state.
    plain_goal: Option<Value>,
    term_goal: Option<Value>,
    /// Which uri/position the fetched goals belong to.
    fetched_for: Option<(String, u64, u64)>,
    /// Output file path (set after `initialize`).
    out_path: Option<String>,
    /// In-flight request ids we injected: id -> kind ("goal" | "term").
    pending: HashMap<String, &'static str>,
    next_id: u64,
}

fn uri_to_display(uri: &str) -> String {
    let p = uri.strip_prefix("file://").unwrap_or(uri);
    p.rsplit('/').next().unwrap_or(p).to_string()
}

// ---------------------------------------------------------------- render ---

fn severity_icon(sev: u64) -> &'static str {
    match sev {
        1 => "❌",
        2 => "⚠️",
        _ => "ℹ️",
    }
}

fn render(state: &State) -> String {
    let mut out = String::new();
    let (uri, line) = match &state.cursor {
        Some((u, l, _)) => (u.clone(), *l),
        None => (String::new(), 0),
    };

    if uri.is_empty() {
        out.push_str("# Lean Goal View\n\n*Move the cursor into a Lean file…*\n");
        return out;
    }

    let busy = state.processing.get(&uri).copied().unwrap_or(false);
    out.push_str(&format!(
        "# {}:{}{}\n\n",
        uri_to_display(&uri),
        line + 1,
        if busy { "  ⏳" } else { "" }
    ));

    // Tactic goals ($/lean/plainGoal): { rendered: String, goals: [String] }.
    match &state.plain_goal {
        Some(Value::Null) | None => {
            out.push_str("*No tactic goals at this position.*\n\n");
        }
        Some(pg) => {
            let goals: Vec<String> = pg["goals"]
                .as_array()
                .map(|a| a.iter().filter_map(|g| g.as_str().map(String::from)).collect())
                .unwrap_or_default();
            if goals.is_empty() {
                // An empty goals array inside a proof means: all goals closed.
                out.push_str("## Goals accomplished 🎉\n\n");
            } else {
                out.push_str(&format!(
                    "## Tactic state ({} goal{})\n\n",
                    goals.len(),
                    if goals.len() == 1 { "" } else { "s" }
                ));
                for g in &goals {
                    out.push_str("```lean\n");
                    out.push_str(g);
                    out.push_str("\n```\n\n");
                }
            }
        }
    }

    // Expected type ($/lean/plainTermGoal): { goal: String, range }.
    if let Some(tg) = &state.term_goal {
        if let Some(goal) = tg["goal"].as_str() {
            out.push_str("## Expected type\n\n```lean\n");
            out.push_str(goal);
            out.push_str("\n```\n\n");
        }
    }

    // Messages for this file.
    if let Some(diags) = state.diagnostics.get(&uri).and_then(|d| d.as_array()) {
        if !diags.is_empty() {
            out.push_str("## Messages\n\n");
            for d in diags {
                let l = d["range"]["start"]["line"].as_u64().unwrap_or(0) + 1;
                let sev = severity_icon(d["severity"].as_u64().unwrap_or(3));
                let msg = d["message"].as_str().unwrap_or("");
                out.push_str(&format!("**{sev} line {l}**\n\n```\n{msg}\n```\n\n"));
            }
        }
    }

    out
}

fn write_out(state: &State, last_written: &mut String) {
    let Some(path) = &state.out_path else { return };
    let content = render(state);
    if content == *last_written {
        return;
    }
    let tmp = format!("{path}.tmp");
    if std::fs::write(&tmp, &content).is_ok() && std::fs::rename(&tmp, path).is_ok() {
        *last_written = content;
    }
}

// ------------------------------------------------------------------ main ---

fn lake_command() -> Command {
    if let Ok(lake) = std::env::var("LEAN_GOALVIEW_LAKE") {
        let mut c = Command::new(lake);
        c.arg("serve").arg("--");
        return c;
    }
    let home = std::env::var("HOME").unwrap_or_default();
    let elan_lake = format!("{home}/.elan/bin/lake");
    let lake = if std::path::Path::new(&elan_lake).is_file() { elan_lake } else { "lake".into() };
    let mut c = Command::new(lake);
    c.arg("serve").arg("--");
    c
}

fn main() {
    let mut child: Child = lake_command()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap_or_else(|e| {
            eprintln!("lean-goalview: cannot start lake serve: {e}");
            std::process::exit(1);
        });

    let state = Arc::new(Mutex::new(State::default()));
    let child_stdin = Arc::new(Mutex::new(child.stdin.take().unwrap()));
    let child_stdout = child.stdout.take().unwrap();

    // Debounced fetch scheduling: worker fetches goals ~120ms after the last
    // cursor event, so scrolling through a file doesn't spam the server.
    let (tick_tx, tick_rx): (Sender<()>, Receiver<()>) = channel();

    // -- thread A: editor -> server ----------------------------------------
    {
        let state = Arc::clone(&state);
        let child_stdin = Arc::clone(&child_stdin);
        let tick_tx = tick_tx.clone();
        std::thread::spawn(move || {
            let stdin = std::io::stdin();
            let mut reader = BufReader::new(stdin.lock());
            while let Some(body) = read_frame(&mut reader) {
                if let Ok(msg) = serde_json::from_slice::<Value>(&body) {
                    let method = msg["method"].as_str().unwrap_or("");
                    match method {
                        "initialize" => {
                            let root = msg["params"]["rootUri"]
                                .as_str()
                                .or_else(|| {
                                    msg["params"]["workspaceFolders"][0]["uri"].as_str()
                                })
                                .unwrap_or("")
                                .to_string();
                            let mut st = state.lock().unwrap();
                            st.out_path = Some(
                                std::env::var("LEAN_GOALVIEW_FILE").unwrap_or_else(|_| {
                                    format!(
                                        "{}/.goalview.md",
                                        root.strip_prefix("file://").unwrap_or(".")
                                    )
                                }),
                            );
                        }
                        "textDocument/documentHighlight" | "textDocument/hover" => {
                            let uri = msg["params"]["textDocument"]["uri"]
                                .as_str()
                                .unwrap_or("")
                                .to_string();
                            let line = msg["params"]["position"]["line"].as_u64().unwrap_or(0);
                            let ch =
                                msg["params"]["position"]["character"].as_u64().unwrap_or(0);
                            if uri.ends_with(".lean") {
                                state.lock().unwrap().cursor = Some((uri, line, ch));
                                let _ = tick_tx.send(());
                            }
                        }
                        _ => {}
                    }
                }
                write_frame(&mut *child_stdin.lock().unwrap(), &body);
            }
            // Editor hung up: nothing more to proxy.
            std::process::exit(0);
        });
    }

    // -- thread B: server -> editor (filtering our injected requests) ------
    {
        let state = Arc::clone(&state);
        let tick_tx = tick_tx.clone();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(child_stdout);
            let stdout = std::io::stdout();
            while let Some(body) = read_frame(&mut reader) {
                let msg: Value = match serde_json::from_slice(&body) {
                    Ok(m) => m,
                    Err(_) => {
                        write_frame(&mut stdout.lock(), &body);
                        continue;
                    }
                };

                // Response to one of OUR injected requests? Consume it.
                if let Some(id) = msg["id"].as_str() {
                    if id.starts_with("gv:") && msg["method"].is_null() {
                        let mut st = state.lock().unwrap();
                        if let Some(kind) = st.pending.remove(id) {
                            let result = msg["result"].clone();
                            match kind {
                                "goal" => st.plain_goal = Some(result),
                                _ => {
                                    st.term_goal =
                                        if result.is_null() { None } else { Some(result) }
                                }
                            }
                            let _ = tick_tx.send(());
                        }
                        continue;
                    }
                }

                let method = msg["method"].as_str().unwrap_or("");
                match method {
                    "textDocument/publishDiagnostics" => {
                        let uri =
                            msg["params"]["uri"].as_str().unwrap_or("").to_string();
                        let mut st = state.lock().unwrap();
                        st.diagnostics
                            .insert(uri.clone(), msg["params"]["diagnostics"].clone());
                        // Elaboration finished for the cursor file → refetch.
                        if st.cursor.as_ref().is_some_and(|(u, _, _)| *u == uri) {
                            let _ = tick_tx.send(());
                        }
                    }
                    "$/lean/fileProgress" => {
                        let uri = msg["params"]["textDocument"]["uri"]
                            .as_str()
                            .unwrap_or("")
                            .to_string();
                        let busy = msg["params"]["processing"]
                            .as_array()
                            .is_some_and(|a| !a.is_empty());
                        state.lock().unwrap().processing.insert(uri, busy);
                        let _ = tick_tx.send(());
                    }
                    _ => {}
                }

                write_frame(&mut stdout.lock(), &body);
            }
            std::process::exit(0);
        });
    }

    // -- main thread: debounce, fetch, render ------------------------------
    let mut last_written = String::new();
    let mut last_fetch_pos: Option<(String, u64, u64)> = None;
    loop {
        // Wait for activity, then let events settle.
        if tick_rx.recv().is_err() {
            break;
        }
        let deadline = Instant::now() + Duration::from_millis(120);
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            if tick_rx.recv_timeout(left).is_err() {
                break;
            }
        }

        let (cursor, needs_fetch) = {
            let st = state.lock().unwrap();
            let c = st.cursor.clone();
            (c.clone(), c.is_some() && (c != last_fetch_pos || c != st.fetched_for))
        };

        if let (Some((uri, line, ch)), true) = (cursor.clone(), needs_fetch) {
            let mut st = state.lock().unwrap();
            let params = json!({
                "textDocument": {"uri": uri},
                "position": {"line": line, "character": ch}
            });
            for (method, kind) in [
                ("$/lean/plainGoal", "goal"),
                ("$/lean/plainTermGoal", "term"),
            ] {
                st.next_id += 1;
                let id = format!("gv:{}", st.next_id);
                st.pending.insert(id.clone(), kind);
                let req = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
                write_frame(&mut *child_stdin.lock().unwrap(), req.to_string().as_bytes());
            }
            st.fetched_for = Some((uri.clone(), line, ch));
            last_fetch_pos = cursor;
        }

        write_out(&state.lock().unwrap(), &mut last_written);
    }
}
