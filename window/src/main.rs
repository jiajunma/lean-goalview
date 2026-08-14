//! lean-goalview-window — a native window that embeds a WebKit webview and
//! points it at the proxy's local goal-view server. Uses the OS webview
//! (WKWebView on macOS) via wry, so there is no bundled browser engine.
//!
//! Usage: lean-goalview-window [URL]   (default http://127.0.0.1:6237/)

use tao::{
    event::{ElementState, Event, KeyEvent, WindowEvent},
    event_loop::{ControlFlow, EventLoop},
    keyboard::{Key, ModifiersState},
    window::WindowBuilder,
};
use wry::WebViewBuilder;

fn main() -> wry::Result<()> {
    let url = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "http://127.0.0.1:6237/".to_string());

    let event_loop = EventLoop::new();
    let window = WindowBuilder::new()
        .with_title("Lean Goal View")
        .with_inner_size(tao::dpi::LogicalSize::new(460.0, 720.0))
        // Float above the editor so the goal stays visible while you type.
        .with_always_on_top(true)
        .build(&event_loop)
        .expect("failed to build window");

    let _webview = WebViewBuilder::new()
        .with_url(&url)
        .build(&window)?;

    let mut on_top = true;
    let mut mods = ModifiersState::empty();
    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        if let Event::WindowEvent { event, .. } = event {
            match event {
                WindowEvent::CloseRequested => *control_flow = ControlFlow::Exit,
                WindowEvent::ModifiersChanged(m) => mods = m,
                WindowEvent::KeyboardInput {
                    event: KeyEvent { logical_key, state: ElementState::Pressed, .. },
                    ..
                } => {
                    let cmd = mods.super_key() || mods.control_key();
                    match logical_key {
                        // ⌘T: toggle always-on-top (float / normal).
                        Key::Character(ref c) if cmd && c.eq_ignore_ascii_case("t") => {
                            on_top = !on_top;
                            window.set_always_on_top(on_top);
                        }
                        // ⌘W or Escape: hide the window (relaunches on next open).
                        Key::Character(ref c) if cmd && c.eq_ignore_ascii_case("w") => {
                            window.set_visible(false);
                        }
                        Key::Escape => window.set_visible(false),
                        _ => {}
                    }
                }
                _ => {}
            }
        }
    });
}
