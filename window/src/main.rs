//! lean-goalview-window — a native window that embeds a WebKit webview and
//! points it at the proxy's local goal-view server. Uses the OS webview
//! (WKWebView on macOS) via wry, so there is no bundled browser engine.
//!
//! Usage: lean-goalview-window [URL]   (default http://127.0.0.1:6237/)

use tao::{
    event::{Event, WindowEvent},
    event_loop::{ControlFlow, EventLoop},
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
        .build(&event_loop)
        .expect("failed to build window");

    let _webview = WebViewBuilder::new()
        .with_url(&url)
        .build(&window)?;

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        if let Event::WindowEvent { event: WindowEvent::CloseRequested, .. } = event {
            *control_flow = ControlFlow::Exit;
        }
    });
}
