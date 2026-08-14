//! lean-goalview-dap — presents the Lean goal state in the editor's native
//! debug panel, over the Debug Adapter Protocol.
//!
//! The model: a permanently-suspended single-thread "program". One stack frame
//! sits at the cursor; scopes are "Tactic State" and "Messages"; hypotheses and
//! the ⊢ target are variables (goals with structure expand as trees). On every
//! goal update from the proxy, a fresh `stopped` event (preserveFocusHint) asks
//! the client to re-fetch — Zed ignores DAP's `Invalidated`, so this is the
//! refresh path that works.
//!
//! Data comes from the lean-goalview proxy's unix socket
//! ($TMPDIR/lean-goalview.sock), which pushes a JSON snapshot on connect and on
//! every cursor/goal change. This adapter is a pure translator: no Lean logic.

use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

// ---------------------------------------------------------------- framing --
// DAP uses the same Content-Length framing as LSP.

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
    std::io::Read::read_exact(reader, &mut body).ok()?;
    Some(body)
}

fn send(msg: Value) {
    let body = msg.to_string();
    let stdout = std::io::stdout();
    let mut w = stdout.lock();
    let _ = write!(w, "Content-Length: {}\r\n\r\n{}", body.len(), body);
    let _ = w.flush();
}

/// Session log at $TMPDIR/lean-goalview-dap.log — the debug panel gives no
/// console, so this is how a dropped session gets diagnosed.
fn dlog(msg: &str) {
    let tmp = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".into());
    let path = format!("{}/lean-goalview-dap.log", tmp.trim_end_matches('/'));
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{msg}");
    }
}

static SEQ: AtomicI64 = AtomicI64::new(1);

fn respond(req: &Value, success: bool, body: Value) {
    send(json!({
        "type": "response",
        "seq": SEQ.fetch_add(1, Ordering::Relaxed),
        "request_seq": req["seq"],
        "command": req["command"],
        "success": success,
        "body": body,
    }));
}

fn event(name: &str, body: Value) {
    send(json!({
        "type": "event",
        "seq": SEQ.fetch_add(1, Ordering::Relaxed),
        "event": name,
        "body": body,
    }));
}

// ------------------------------------------------------------- goal model --

#[derive(Clone, Default)]
struct Var {
    name: String,
    value: String,
    reference: i64, // 0 = leaf
}

#[derive(Default)]
struct Model {
    /// Full path of the cursor's file (for the stack frame source).
    path: String,
    file: String,
    line: i64,
    /// variablesReference -> children.
    vars: HashMap<i64, Vec<Var>>,
}

const REF_TACTIC: i64 = 1;
const REF_MESSAGES: i64 = 2;

fn single_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Parse one pretty-printed goal block ("case …\nh : T\n⊢ G") into variables.
fn parse_goal(block: &str) -> Vec<Var> {
    let mut out: Vec<Var> = Vec::new();
    for line in block.lines() {
        let trimmed = line.trim_end();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(case) = trimmed.strip_prefix("case ") {
            out.push(Var { name: "case".into(), value: case.to_string(), reference: 0 });
        } else if let Some(goal) = trimmed.strip_prefix("⊢") {
            out.push(Var { name: "⊢".into(), value: goal.trim().to_string(), reference: 0 });
        } else if !line.starts_with(' ') && trimmed.contains(" : ") {
            let (names, ty) = trimmed.split_once(" : ").unwrap();
            out.push(Var { name: names.to_string(), value: ty.to_string(), reference: 0 });
        } else if let Some(last) = out.last_mut() {
            // Wrapped continuation of the previous hypothesis/goal line.
            last.value.push(' ');
            last.value.push_str(trimmed.trim_start());
        }
    }
    out
}

fn rebuild(model: &mut Model, snap: &Value) {
    model.vars.clear();
    model.file = snap["file"].as_str().unwrap_or("").to_string();
    model.path = snap["path"].as_str().unwrap_or("").to_string();
    model.line = snap["line"].as_i64().unwrap_or(1).max(1);

    let goals: Vec<&str> = snap["goals"]
        .as_array()
        .map(|a| a.iter().filter_map(|g| g.as_str()).collect())
        .unwrap_or_default();

    let mut root: Vec<Var> = Vec::new();
    if goals.is_empty() {
        let label = if snap["busy"].as_bool().unwrap_or(false) {
            "⏳ elaborating…"
        } else {
            "no goals at this position"
        };
        root.push(Var { name: "state".into(), value: label.into(), reference: 0 });
    }
    for (i, g) in goals.iter().enumerate() {
        let goal_ref = 100 + i as i64;
        let children = parse_goal(g);
        let target = children
            .iter()
            .find(|v| v.name == "⊢")
            .map(|v| v.value.clone())
            .unwrap_or_default();
        root.push(Var {
            name: if goals.len() == 1 { "goal".into() } else { format!("goal {}", i + 1) },
            value: format!("⊢ {}", single_line(&target)),
            reference: goal_ref,
        });
        model.vars.insert(goal_ref, children);
    }
    if let Some(t) = snap["termGoal"].as_str() {
        root.push(Var {
            name: "expected type".into(),
            value: single_line(t.trim_start_matches('⊢').trim()),
            reference: 0,
        });
    }
    model.vars.insert(REF_TACTIC, root);

    let mut msgs: Vec<Var> = Vec::new();
    if let Some(list) = snap["messages"].as_array() {
        for m in list {
            let sev = match m["severity"].as_u64().unwrap_or(3) {
                1 => "❌",
                2 => "⚠️",
                _ => "ℹ️",
            };
            msgs.push(Var {
                name: format!("{sev} line {}", m["line"].as_u64().unwrap_or(0)),
                value: single_line(m["text"].as_str().unwrap_or("")),
                reference: 0,
            });
        }
    }
    model.vars.insert(REF_MESSAGES, msgs);
}

// ------------------------------------------------------------------ main ---

fn main() {
    dlog("=== adapter started ===");
    let model = Arc::new(Mutex::new(Model::default()));
    let configured = Arc::new(AtomicBool::new(false));

    // Socket reader: keep trying to (re)connect to the proxy; every received
    // snapshot rebuilds the model and, once the session is configured, emits a
    // `stopped` so the client re-fetches the tree.
    {
        let model = Arc::clone(&model);
        let configured = Arc::clone(&configured);
        std::thread::spawn(move || {
            let sock_path = format!(
                "{}/lean-goalview.sock",
                std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".into()).trim_end_matches('/')
            );
            loop {
                if let Ok(stream) = UnixStream::connect(&sock_path) {
                    dlog("socket: connected to proxy");
                    let mut reader = BufReader::new(stream);
                    let mut line = String::new();
                    loop {
                        line.clear();
                        match reader.read_line(&mut line) {
                            Ok(0) | Err(_) => break,
                            Ok(_) => {
                                if let Ok(snap) = serde_json::from_str::<Value>(&line) {
                                    rebuild(&mut model.lock().unwrap(), &snap);
                                    if configured.load(Ordering::Relaxed) {
                                        event(
                                            "stopped",
                                            json!({
                                                "reason": "pause",
                                                "description": "Lean goal state",
                                                "threadId": 1,
                                                "allThreadsStopped": true,
                                                "preserveFocusHint": true,
                                            }),
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
                std::thread::sleep(Duration::from_secs(1));
            }
        });
    }

    let stdin = std::io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    while let Some(body) = read_frame(&mut reader) {
        let Ok(msg) = serde_json::from_slice::<Value>(&body) else { continue };
        if msg["type"].as_str() != Some("request") {
            dlog(&format!("<- non-request: {}", msg["type"]));
            continue;
        }
        dlog(&format!("<- {}", msg["command"].as_str().unwrap_or("?")));
        match msg["command"].as_str().unwrap_or("") {
            "initialize" => {
                respond(
                    &msg,
                    true,
                    json!({
                        "supportsConfigurationDoneRequest": true,
                        "supportsTerminateRequest": true,
                    }),
                );
                event("initialized", json!({}));
            }
            "launch" | "attach" => respond(&msg, true, json!({})),
            "setBreakpoints" => respond(&msg, true, json!({"breakpoints": []})),
            "setExceptionBreakpoints" => respond(&msg, true, json!({"breakpoints": []})),
            "configurationDone" => {
                respond(&msg, true, json!({}));
                configured.store(true, Ordering::Relaxed);
                event(
                    "stopped",
                    json!({
                        "reason": "entry",
                        "description": "Lean goal state",
                        "threadId": 1,
                        "allThreadsStopped": true,
                        "preserveFocusHint": true,
                    }),
                );
            }
            "threads" => {
                respond(&msg, true, json!({"threads": [{"id": 1, "name": "Lean"}]}));
            }
            "stackTrace" => {
                let m = model.lock().unwrap();
                let mut frame = json!({
                    "id": 1,
                    "name": if m.file.is_empty() { "Lean".to_string() }
                            else { format!("{}:{}", m.file, m.line) },
                    "line": m.line,
                    "column": 1,
                });
                if !m.path.is_empty() {
                    frame["source"] = json!({"name": m.file, "path": m.path});
                }
                respond(&msg, true, json!({"stackFrames": [frame], "totalFrames": 1}));
            }
            "scopes" => {
                respond(
                    &msg,
                    true,
                    json!({"scopes": [
                        {"name": "Tactic State", "variablesReference": REF_TACTIC, "expensive": false},
                        {"name": "Messages", "variablesReference": REF_MESSAGES, "expensive": false},
                    ]}),
                );
            }
            "variables" => {
                let reference = msg["arguments"]["variablesReference"].as_i64().unwrap_or(0);
                let m = model.lock().unwrap();
                let vars: Vec<Value> = m
                    .vars
                    .get(&reference)
                    .map(|vs| {
                        vs.iter()
                            .map(|v| {
                                json!({
                                    "name": v.name,
                                    "value": v.value,
                                    "variablesReference": v.reference,
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                respond(&msg, true, json!({"variables": vars}));
            }
            "continue" => {
                // Nothing runs; acknowledge and immediately re-stop so the
                // panel stays in the paused (inspectable) state.
                respond(&msg, true, json!({"allThreadsContinued": true}));
                event(
                    "stopped",
                    json!({
                        "reason": "pause",
                        "description": "Lean goal state",
                        "threadId": 1,
                        "allThreadsStopped": true,
                        "preserveFocusHint": true,
                    }),
                );
            }
            "disconnect" | "terminate" => {
                dlog("-> exiting on disconnect/terminate");
                respond(&msg, true, json!({}));
                event("terminated", json!({}));
                std::process::exit(0);
            }
            _ => respond(&msg, true, json!({})),
        }
    }
    dlog("stdin EOF — client hung up");
}
