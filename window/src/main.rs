//! lean-goalview-window — a native window that embeds a WebKit webview and
//! points it at the proxy's local infoview server. Uses the OS webview
//! (WKWebView on macOS) via wry, so there is no bundled browser engine.
//!
//! The page's control bar talks back over wry IPC (`window.ipc.postMessage`):
//!   "toggle-float" → flip always-on-top   "minimize" → minimize the window
//! Keyboard equivalents: ⌘T toggle float, ⌘W / Esc hide.
//!
//! Usage: lean-goalview-window [URL]   (default http://127.0.0.1:6237/)

use tao::{
    dpi::{LogicalSize, PhysicalPosition, PhysicalSize},
    event::{ElementState, Event, KeyEvent, WindowEvent},
    event_loop::{ControlFlow, EventLoopBuilder},
    keyboard::{Key, ModifiersState},
    window::WindowBuilder,
};
use std::time::Duration;
use wry::WebViewBuilder;

#[derive(Debug)]
enum UserEvent {
    ToggleFloat,
    Minimize,
    SelfTest,
    /// Another launch asked us to come to the front (single-instance summon).
    Summon,
}

/// Saved window frame (physical px), so the window reopens exactly where the
/// user last put it.
fn frame_path() -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    format!("{home}/.local/share/lean-goalview/window-frame.json")
}

fn load_frame() -> Option<(i32, i32, u32, u32)> {
    let v: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(frame_path()).ok()?).ok()?;
    let (x, y) = (v["x"].as_i64()? as i32, v["y"].as_i64()? as i32);
    let (w, h) = (v["w"].as_u64()? as u32, v["h"].as_u64()? as u32);
    (w >= 200 && h >= 200).then_some((x, y, w, h))
}

fn wlog(msg: &str) {
    let tmp = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".into());
    let path = format!("{}/lean-goalview-window.log", tmp.trim_end_matches('/'));
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        use std::io::Write as _;
        let _ = writeln!(f, "{msg}");
    }
}

fn save_frame(x: i32, y: i32, w: u32, h: u32) {
    let p = frame_path();
    if let Some(dir) = std::path::Path::new(&p).parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(
        p,
        serde_json::json!({"x": x, "y": y, "w": w, "h": h}).to_string(),
    );
}

/// URL to load: an explicit arg wins, else the port the proxy recorded, else
/// the default. This lets a Zed task launch the window with no arguments.
fn resolve_url() -> String {
    if let Some(arg) = std::env::args().nth(1) {
        return arg;
    }
    let tmp = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".into());
    let port = std::fs::read_to_string(format!("{}/lean-goalview.port", tmp.trim_end_matches('/')))
        .ok()
        .and_then(|s| s.trim().parse::<u16>().ok())
        .unwrap_or(6237);
    format!("http://127.0.0.1:{port}/")
}

fn control_sock_path() -> String {
    let tmp = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".into());
    format!("{}/lean-goalview-window.sock", tmp.trim_end_matches('/'))
}

fn main() -> wry::Result<()> {
    let url = resolve_url();

    // Single instance: if a window is already running, summon it instead of
    // opening a second one — this is how a window sent behind the editor (or
    // hidden with ⌘W) is recovered: just hit the open-infoview key again.
    {
        use std::os::unix::net::UnixStream as CtlStream;
        let sock = control_sock_path();
        if let Ok(mut c) = CtlStream::connect(&sock) {
            use std::io::Write as _;
            if c.write_all(b"show\n").is_ok() {
                return Ok(());
            }
        }
        let _ = std::fs::remove_file(&sock);
    }

    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();

    // Layout on open: last saved frame if there is one, else dock to the
    // right edge of the primary screen at full working height.
    let saved = load_frame();
    let default_frame = event_loop.primary_monitor().map(|m| {
        let scale = m.scale_factor();
        let pos = m.position();
        let size = m.size();
        let width = (420.0 * scale) as u32;
        let menubar = (25.0 * scale) as i32;
        (
            pos.x + size.width as i32 - width as i32,
            pos.y + menubar,
            width,
            size.height.saturating_sub(menubar as u32),
        )
    });
    let (x, y, w, h) = saved.or(default_frame).unwrap_or((100, 100, 460, 720));

    let window = WindowBuilder::new()
        // The key is in the title on purpose: a hidden window still shows its
        // title in ⌘-Tab and Mission Control, which is exactly when you need
        // to be told how to get it back.
        .with_title("Lean Infoview — ⌘⌥I to summon")
        // Freely resizable (tao's default); floor so it can't be shrunk away.
        .with_inner_size(PhysicalSize::new(w, h))
        .with_position(PhysicalPosition::new(x, y))
        .with_min_inner_size(LogicalSize::new(240.0, 200.0))
        // Float above the editor so the goal stays visible while you type.
        .with_always_on_top(true)
        .build(&event_loop)
        .expect("failed to build window");

    let webview = WebViewBuilder::new()
        .with_url(&url)
        // Without this, macOS swallows the first click on a non-key window
        // (which an always-on-top helper usually is) just to focus it, so
        // buttons and infoview links appear "unclickable". Let clicks through.
        .with_accept_first_mouse(true)
        // Right-click → Inspect Element opens the console (for diagnosing
        // infoview/widget issues).
        .with_devtools(true)
        .with_ipc_handler(move |req| {
            wlog(&format!("ipc: {}", req.body()));
            match req.body().as_str() {
                "toggle-float" => {
                    let _ = proxy.send_event(UserEvent::ToggleFloat);
                }
                "minimize" => {
                    let _ = proxy.send_event(UserEvent::Minimize);
                }
                _ => {}
            }
        })
        .build(&window)?;
    let _webview = webview;

    {
        use std::os::unix::net::UnixListener as CtlListener;
        let p3 = event_loop.create_proxy();
        if let Ok(listener) = CtlListener::bind(control_sock_path()) {
            std::thread::spawn(move || {
                for stream in listener.incoming().flatten() {
                    let mut reader = std::io::BufReader::new(stream);
                    let mut line = String::new();
                    use std::io::BufRead as _;
                    if reader.read_line(&mut line).is_ok() && line.trim() == "show" {
                        wlog("summon requested");
                        let _ = p3.send_event(UserEvent::Summon);
                    }
                }
            });
        }
    }

    if std::env::var("LEAN_GOALVIEW_WINDOW_SELFTEST").is_ok() {
        // Fire the simulated click from inside the running event loop, after
        // the page has had time to load; exit shortly after.
        wlog("selftest: armed");
        let p2 = event_loop.create_proxy();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(3000));
            let _ = p2.send_event(UserEvent::SelfTest);
            std::thread::sleep(Duration::from_millis(3000));
            wlog("selftest: exiting");
            std::process::exit(0);
        });
    }

    let persist = |window: &tao::window::Window| {
        if let (Ok(pos), size) = (window.outer_position(), window.inner_size()) {
            save_frame(pos.x, pos.y, size.width, size.height);
        }
    };
    let mut last_persist = std::time::Instant::now();
    let mut on_top = true;
    let mut mods = ModifiersState::empty();
    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            Event::UserEvent(UserEvent::ToggleFloat) => {
                on_top = !on_top;
                wlog(&format!("native: set_always_on_top({on_top})"));
                window.set_always_on_top(on_top);
            }
            Event::UserEvent(UserEvent::Minimize) => window.set_minimized(true),
            Event::UserEvent(UserEvent::Summon) => {
                window.set_visible(true);
                window.set_minimized(false);
                window.set_focus(); // raises above the editor without changing float state
            }
            Event::UserEvent(UserEvent::SelfTest) => {
                wlog("selftest: evaluating click");
                let _ = _webview.evaluate_script(
                    "document.getElementById('pin').click();\
                     window.ipc.postMessage('log:clicked');",
                );
            }
            Event::WindowEvent { event, .. } => match event {
                WindowEvent::CloseRequested => {
                    persist(&window);
                    *control_flow = ControlFlow::Exit;
                }
                // Remember where the user puts the window (throttled: drags
                // fire these continuously).
                WindowEvent::Moved(_) | WindowEvent::Resized(_) => {
                    if last_persist.elapsed() > std::time::Duration::from_millis(500) {
                        persist(&window);
                        last_persist = std::time::Instant::now();
                    }
                }
                WindowEvent::ModifiersChanged(m) => mods = m,
                WindowEvent::KeyboardInput {
                    event: KeyEvent { logical_key, state: ElementState::Pressed, .. },
                    ..
                } => {
                    let cmd = mods.super_key() || mods.control_key();
                    match logical_key {
                        Key::Character(ref c) if cmd && c.eq_ignore_ascii_case("t") => {
                            on_top = !on_top;
                            window.set_always_on_top(on_top);
                        }
                        Key::Character(ref c) if cmd && c.eq_ignore_ascii_case("w") => {
                            window.set_visible(false);
                        }
                        Key::Escape => window.set_visible(false),
                        _ => {}
                    }
                }
                _ => {}
            },
            _ => {}
        }
    });
}
