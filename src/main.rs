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
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::net::{UnixListener, UnixStream};
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
    /// Editor hover request ids awaiting a server response we'll enrich.
    pending_hovers: HashSet<u64>,
    next_id: u64,
}

/// The cached goal state as hover markdown, or None if there's nothing to add.
fn goal_hover_markdown(state: &State) -> Option<String> {
    let pg = state.plain_goal.as_ref()?;
    let goals: Vec<String> = pg["goals"]
        .as_array()?
        .iter()
        .filter_map(|g| g.as_str().map(String::from))
        .collect();
    let mut s = String::new();
    if goals.is_empty() {
        s.push_str("**Goals accomplished** 🎉");
    } else {
        for (i, g) in goals.iter().enumerate() {
            if goals.len() > 1 {
                s.push_str(&format!("*goal {} / {}*\n", i + 1, goals.len()));
            }
            s.push_str("```lean\n");
            s.push_str(g);
            s.push_str("\n```\n");
        }
    }
    Some(s)
}

/// Append the goal markdown to a hover response, coping with the several
/// shapes `result.contents` can take (MarkupContent / string / absent).
fn inject_goal_into_hover(msg: &mut Value, goal_md: &str) {
    const SEP: &str = "\n\n---\n\n";
    let header = format!("**⊢ Goal**\n\n{goal_md}");

    if msg["result"].is_null() {
        msg["result"] = json!({"contents": {"kind": "markdown", "value": header}});
        return;
    }
    let contents = &msg["result"]["contents"];
    let existing = if let Some(v) = contents["value"].as_str() {
        v.to_string() // MarkupContent { kind, value }
    } else if let Some(v) = contents.as_str() {
        v.to_string() // bare string
    } else {
        String::new() // MarkedString[] or unknown: replace rather than nest
    };
    let combined = if existing.is_empty() {
        header
    } else {
        format!("{existing}{SEP}{header}")
    };
    msg["result"]["contents"] = json!({"kind": "markdown", "value": combined});
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

/// A 4-space-indented markdown code block: renders as a code box in preview,
/// but in raw text it's just clean indentation — no ``` fences to read past.
fn code_block(out: &mut String, body: &str) {
    for line in body.lines() {
        out.push_str("    ");
        out.push_str(line);
        out.push('\n');
    }
    out.push('\n');
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
                    "## Tactic state · {} goal{}\n\n",
                    goals.len(),
                    if goals.len() == 1 { "" } else { "s" }
                ));
                for (i, g) in goals.iter().enumerate() {
                    if goals.len() > 1 {
                        out.push_str(&format!("**goal {} / {}**\n\n", i + 1, goals.len()));
                    }
                    code_block(&mut out, g);
                }
            }
        }
    }

    // Expected type ($/lean/plainTermGoal): { goal: String, range }.
    if let Some(tg) = &state.term_goal {
        if let Some(goal) = tg["goal"].as_str() {
            out.push_str("## Expected type\n\n");
            code_block(&mut out, goal);
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
                out.push_str(&format!("**{sev} line {l}**\n\n"));
                code_block(&mut out, msg);
            }
        }
    }

    out
}

/// Structured goal state for the GPUI window (which does its own layout),
/// as opposed to `render()`'s pre-formatted markdown for the file/preview.
fn goal_json(state: &State) -> String {
    let (file, line) = match &state.cursor {
        Some((u, l, _)) => (u.rsplit('/').next().unwrap_or(u).to_string(), *l + 1),
        None => (String::new(), 0),
    };
    let busy = state
        .cursor
        .as_ref()
        .map(|(u, _, _)| state.processing.get(u).copied().unwrap_or(false))
        .unwrap_or(false);
    let goals: Vec<Value> = state
        .plain_goal
        .as_ref()
        .and_then(|pg| pg["goals"].as_array())
        .map(|a| a.to_vec())
        .unwrap_or_default();
    let term = state
        .term_goal
        .as_ref()
        .and_then(|t| t["goal"].as_str())
        .map(String::from);
    let messages: Vec<Value> = state
        .cursor
        .as_ref()
        .and_then(|(u, _, _)| state.diagnostics.get(u))
        .and_then(|d| d.as_array())
        .map(|a| {
            a.iter()
                .map(|d| {
                    json!({
                        "line": d["range"]["start"]["line"].as_u64().unwrap_or(0) + 1,
                        "severity": d["severity"].as_u64().unwrap_or(3),
                        "text": d["message"].as_str().unwrap_or(""),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    json!({
        "file": file, "line": line, "busy": busy,
        "goals": goals, "termGoal": term, "messages": messages,
    })
    .to_string()
}

/// Push a JSON line to every connected socket client, dropping the dead ones.
fn broadcast(clients: &Mutex<Vec<UnixStream>>, line: &str) {
    let mut cs = clients.lock().unwrap();
    cs.retain_mut(|c| c.write_all(line.as_bytes()).and_then(|_| c.write_all(b"\n")).is_ok());
}

/// Push a goal-state update to every connected browser as an SSE event.
fn broadcast_sse(clients: &Mutex<Vec<TcpStream>>, json: &str) {
    let frame = format!("data: {json}\n\n");
    let mut cs = clients.lock().unwrap();
    cs.retain_mut(|c| c.write_all(frame.as_bytes()).and_then(|_| c.flush()).is_ok());
}

/// The goal-view web page: connects to /events (SSE) and renders each update.
/// Self-contained, theme-aware, no external requests.
const PAGE: &str = r####"<!doctype html><html><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Lean Goal View</title><style>
:root{--bg:#fbfbfd;--fg:#1c1c22;--dim:#6b6b78;--acc:#1f6feb;--card:#fff;--bd:#e3e3ea;--goal:#0b7285;--hyp:#364fc7}
@media(prefers-color-scheme:dark){:root{--bg:#181820;--fg:#e6e6ee;--dim:#9a9aa8;--acc:#58a6ff;--card:#21212b;--bd:#33333f;--goal:#66d9e8;--hyp:#a5b4fc}}
*{box-sizing:border-box}body{margin:0;font:14px/1.5 -apple-system,system-ui,sans-serif;background:var(--bg);color:var(--fg)}
header{position:sticky;top:0;background:var(--bg);border-bottom:1px solid var(--bd);padding:8px 14px;display:flex;gap:8px;align-items:baseline}
header .f{font-weight:600}header .l{color:var(--dim)}header .b{margin-left:auto;color:var(--acc);font-size:12px}
main{padding:12px 14px}.sec{font-size:11px;text-transform:uppercase;letter-spacing:.06em;color:var(--dim);margin:14px 0 6px}
.goal{background:var(--card);border:1px solid var(--bd);border-radius:8px;padding:10px 12px;margin:6px 0;
font:13px/1.55 ui-monospace,SFMono-Regular,Menlo,monospace;white-space:pre-wrap;overflow-x:auto}
.goal .t{color:var(--goal)}.msg{border-left:3px solid var(--bd);padding:4px 10px;margin:6px 0;white-space:pre-wrap;
font:12px/1.5 ui-monospace,monospace}.msg.e{border-color:#e5484d}.msg.w{border-color:#f5a623}
.empty{color:var(--dim);font-style:italic;padding:20px 0}.gn{color:var(--dim);font-size:12px;margin:8px 0 2px}
.ok{color:#2f9e44;font-weight:600;padding:8px 0}
</style></head><body>
<header><span class="f" id="file">Lean Goal View</span><span class="l" id="line"></span><span class="b" id="busy"></span></header>
<main id="main"><div class="empty">Waiting for the cursor to enter a proof…</div></main>
<script>
const M=document.getElementById('main'),F=document.getElementById('file'),L=document.getElementById('line'),B=document.getElementById('busy');
// mark the turnstile so it can be tinted
function esc(s){return s.replace(/[&<>]/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;'}[c]))}
function goalHtml(g){return esc(g).replace(/⊢/g,'<span class="t">⊢</span>')}
function render(d){
  F.textContent=d.file||'Lean Goal View';
  L.textContent=d.line?(':'+d.line):'';
  B.textContent=d.busy?'⏳ elaborating…':'';
  let h='';
  const goals=d.goals||[];
  if(goals.length===0 && !d.termGoal && (!d.messages||!d.messages.length)){
    h='<div class="empty">No goals at this position.</div>';
  }else{
    if(goals.length){
      h+='<div class="sec">Tactic state · '+goals.length+' goal'+(goals.length>1?'s':'')+'</div>';
      goals.forEach((g,i)=>{if(goals.length>1)h+='<div class="gn">goal '+(i+1)+' / '+goals.length+'</div>';
        h+='<div class="goal">'+goalHtml(g)+'</div>';});
    }else if(d.termGoal===null||d.termGoal===undefined){
      // inside a proof with an empty goals array => closed
    }
    if(d.termGoal){h+='<div class="sec">Expected type</div><div class="goal">'+goalHtml(d.termGoal)+'</div>';}
    if(d.messages&&d.messages.length){
      h+='<div class="sec">Messages</div>';
      d.messages.forEach(m=>{const c=m.severity===1?'e':m.severity===2?'w':'';
        h+='<div class="msg '+c+'"><b>line '+m.line+'</b>\n'+esc(m.text)+'</div>';});
    }
  }
  M.innerHTML=h||'<div class="empty">No goals at this position.</div>';
}
const ev=new EventSource('/events');
ev.onmessage=e=>{try{render(JSON.parse(e.data))}catch(x){}};
ev.onerror=()=>{B.textContent='⚠ disconnected';};
</script></body></html>"####;

/// Minimal HTTP server: `GET /` serves the page, `GET /events` is an SSE
/// stream registered for goal broadcasts. Returns the bound port.
fn start_http(state: Arc<Mutex<State>>, sse: Arc<Mutex<Vec<TcpStream>>>) -> Option<u16> {
    // Try a small range so two projects don't fight over one port.
    let listener = (6237u16..6247)
        .find_map(|p| TcpListener::bind(("127.0.0.1", p)).ok())?;
    let port = listener.local_addr().ok()?.port();
    std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            let mut stream = conn;
            let mut reader = BufReader::new(match stream.try_clone() {
                Ok(c) => c,
                Err(_) => continue,
            });
            let mut req = String::new();
            if reader.read_line(&mut req).is_err() {
                continue;
            }
            // Drain the rest of the request headers.
            let mut h = String::new();
            while reader.read_line(&mut h).map(|n| n > 0).unwrap_or(false) {
                if h == "\r\n" {
                    break;
                }
                h.clear();
            }
            if req.starts_with("GET /events") {
                let hdr = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\
                           Cache-Control: no-cache\r\nConnection: keep-alive\r\n\r\n";
                if stream.write_all(hdr.as_bytes()).is_err() {
                    continue;
                }
                let snap = goal_json(&state.lock().unwrap());
                let _ = stream.write_all(format!("data: {snap}\n\n").as_bytes());
                let _ = stream.flush();
                sse.lock().unwrap().push(stream); // main loop pushes updates
            } else if req.starts_with("GET / ") || req.starts_with("GET /index") {
                let body = PAGE.as_bytes();
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(resp.as_bytes());
                let _ = stream.write_all(body);
            } else {
                let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n");
            }
        }
    });
    Some(port)
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
                        // Cursor position leaks out of several editor-initiated
                        // requests. documentHighlight/hover carry `.position`;
                        // codeAction carries `.range` (a zero-width range at the
                        // cursor when there's no selection). Zed fires codeAction
                        // whenever the cursor settles — even off a symbol, where
                        // documentHighlight stays silent — so together they track
                        // the cursor far more tightly than either alone.
                        "textDocument/documentHighlight"
                        | "textDocument/hover"
                        | "textDocument/codeAction" => {
                            let uri = msg["params"]["textDocument"]["uri"]
                                .as_str()
                                .unwrap_or("")
                                .to_string();
                            let pos = if msg["params"]["position"].is_object() {
                                &msg["params"]["position"]
                            } else {
                                &msg["params"]["range"]["start"]
                            };
                            let line = pos["line"].as_u64().unwrap_or(0);
                            let ch = pos["character"].as_u64().unwrap_or(0);
                            if uri.ends_with(".lean") {
                                let mut st = state.lock().unwrap();
                                st.cursor = Some((uri, line, ch));
                                // Remember hover requests so their responses can
                                // be enriched with the goal on the way back.
                                if method == "textDocument/hover" {
                                    if let Some(id) = msg["id"].as_u64() {
                                        st.pending_hovers.insert(id);
                                    }
                                }
                                drop(st);
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

                // Response to an editor hover we flagged? Enrich it with the
                // goal, then forward under the editor's original id.
                if let Some(id) = msg["id"].as_u64() {
                    let goal = {
                        let mut st = state.lock().unwrap();
                        if st.pending_hovers.remove(&id) {
                            Some(goal_hover_markdown(&st))
                        } else {
                            None
                        }
                    };
                    if let Some(goal_md) = goal {
                        match goal_md {
                            Some(g) => {
                                let mut m = msg;
                                inject_goal_into_hover(&mut m, &g);
                                write_frame(&mut stdout.lock(), m.to_string().as_bytes());
                            }
                            None => write_frame(&mut stdout.lock(), &body),
                        }
                        continue;
                    }
                }

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
    // Socket for the GPUI goal-view window. Harmless when nothing connects.
    // Single fixed path per user: one Zed/proxy at a time is the common case;
    // a later revision can namespace it per workspace.
    let clients: Arc<Mutex<Vec<UnixStream>>> = Arc::new(Mutex::new(Vec::new()));
    {
        let clients = Arc::clone(&clients);
        let state = Arc::clone(&state);
        let sock_path = format!(
            "{}/lean-goalview.sock",
            std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".into()).trim_end_matches('/')
        );
        std::thread::spawn(move || {
            let _ = std::fs::remove_file(&sock_path);
            if let Ok(listener) = UnixListener::bind(&sock_path) {
                for stream in listener.incoming().flatten() {
                    // Push the current snapshot immediately, so a window that
                    // connects after the goal has settled still gets it.
                    let mut s = stream;
                    let snap = goal_json(&state.lock().unwrap());
                    let _ = s.write_all(snap.as_bytes()).and_then(|_| s.write_all(b"\n"));
                    clients.lock().unwrap().push(s);
                }
            }
        });
    }

    // Web goal view: tiny HTTP+SSE server, browser auto-opened on startup.
    let sse: Arc<Mutex<Vec<TcpStream>>> = Arc::new(Mutex::new(Vec::new()));
    if let Some(port) = start_http(Arc::clone(&state), Arc::clone(&sse)) {
        if std::env::var("LEAN_GOALVIEW_NO_OPEN").is_err() {
            let url = format!("http://127.0.0.1:{port}/");
            let opener = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
            let _ = Command::new(opener).arg(&url).spawn();
        }
    }

    let mut last_written = String::new();
    let mut last_broadcast = String::new();
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

        {
            let st = state.lock().unwrap();
            write_out(&st, &mut last_written);
            let json = goal_json(&st);
            drop(st);
            if json != last_broadcast {
                last_broadcast = json.clone();
                broadcast(&clients, &json);
                broadcast_sse(&sse, &json);
            }
        }
    }
}
