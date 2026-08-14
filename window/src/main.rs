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
    dpi::LogicalSize,
    event::{ElementState, Event, KeyEvent, WindowEvent},
    event_loop::{ControlFlow, EventLoopBuilder},
    keyboard::{Key, ModifiersState},
    window::WindowBuilder,
};
use wry::WebViewBuilder;

#[derive(Debug)]
enum UserEvent {
    ToggleFloat,
    Minimize,
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

fn main() -> wry::Result<()> {
    let url = resolve_url();

    let event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();

    let window = WindowBuilder::new()
        .with_title("Lean Infoview")
        // Freely resizable (tao's default); start at a sensible size and keep a
        // floor so it can't be shrunk into uselessness.
        .with_inner_size(LogicalSize::new(460.0, 720.0))
        .with_min_inner_size(LogicalSize::new(240.0, 200.0))
        // Float above the editor so the goal stays visible while you type.
        .with_always_on_top(true)
        .build(&event_loop)
        .expect("failed to build window");

    let _webview = WebViewBuilder::new()
        .with_url(&url)
        // Right-click → Inspect Element opens the console (for diagnosing
        // infoview/widget issues).
        .with_devtools(true)
        .with_ipc_handler(move |req| match req.body().as_str() {
            "toggle-float" => {
                let _ = proxy.send_event(UserEvent::ToggleFloat);
            }
            "minimize" => {
                let _ = proxy.send_event(UserEvent::Minimize);
            }
            _ => {}
        })
        .build(&window)?;

    let mut on_top = true;
    let mut mods = ModifiersState::empty();
    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            Event::UserEvent(UserEvent::ToggleFloat) => {
                on_top = !on_top;
                window.set_always_on_top(on_top);
            }
            Event::UserEvent(UserEvent::Minimize) => window.set_minimized(true),
            Event::WindowEvent { event, .. } => match event {
                WindowEvent::CloseRequested => *control_flow = ControlFlow::Exit,
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
