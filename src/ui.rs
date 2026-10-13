//! The desktop window: a WebView2 page (ui/index.html) wired to
//! the device service. The page sends JSON through `window.ipc.postMessage`; the service answers
//! by running small scripts in the page.

use anyhow::Result;
use serde_json::{Value, json};
use tao::dpi::LogicalSize;
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tao::platform::windows::IconExtWindows;
use tao::window::{Icon, WindowBuilder};
use wry::WebViewBuilder;

const PAGE: &str = include_str!("../ui/index.html");
const FONTS_MARK: &str = "<!-- FONTS: the app inserts @font-face rules for ui/fonts here (bundled, no network) -->";
/// Bundled UI fonts (SIL Open Font License, see ui/fonts): family, weight range, file.
const FONTS: [(&str, &str, &[u8]); 3] = [
    ("Michroma", "400", include_bytes!("../ui/fonts/Michroma.woff2")),
    ("Hanken Grotesk", "100 900", include_bytes!("../ui/fonts/HankenGrotesk.woff2")),
    ("JetBrains Mono", "100 800", include_bytes!("../ui/fonts/JetBrainsMono.woff2")),
];

/// The page with the fonts inlined, so the window never fetches anything from the internet.
fn page() -> String {
    let faces: String = FONTS
        .iter()
        .map(|(family, weight, data)| {
            format!(
                "@font-face{{font-family:'{family}';font-style:normal;font-weight:{weight};font-display:block;src:url(data:font/woff2;base64,{}) format('woff2')}}\n",
                crate::service::base64(data)
            )
        })
        .collect();
    PAGE.replacen(FONTS_MARK, &format!("<style>\n{faces}</style>"), 1)
}

/// Opens the window maximized. `minimized` is for starting with Windows: open on the taskbar, not
/// on screen; restoring it from there gives the maximized window.
pub fn run(minimized: bool) -> Result<()> {
    let event_loop = EventLoopBuilder::<String>::with_user_event().build();
    let proxy = event_loop.create_proxy();
    let window = WindowBuilder::new()
        .with_title(crate::desktop::WINDOW_TITLE)
        .with_window_icon(Icon::from_resource(1, None).ok())
        .with_inner_size(LogicalSize::new(1380.0, 900.0))
        .with_min_inner_size(LogicalSize::new(760.0, 560.0))
        .with_maximized(true)
        .build(&event_loop)?;
    if minimized {
        window.set_minimized(true);
    }

    let (tx, rx) = std::sync::mpsc::channel::<Value>();
    let crash_proxy = proxy.clone();
    let mut service = Some(std::thread::spawn(move || {
        let run = std::panic::AssertUnwindSafe(|| {
            crate::service::run(rx, move |js| {
                let _ = proxy.send_event(js);
            })
        });
        // A bug in the service must not leave a window that looks alive but controls nothing.
        if std::panic::catch_unwind(run).is_err() {
            let status = json!({ "kind": "error", "message": "Mini Console hit an internal error and stopped controlling the Mini. Restart the app." });
            let _ = crash_proxy.send_event(format!("window.goxlr && goxlr.status({status})"));
        }
    }));

    let ipc_tx = tx.clone();
    let webview = WebViewBuilder::new()
        .with_html(page())
        .with_background_color((6, 8, 14, 255))
        .with_ipc_handler(move |req| {
            if let Ok(v) = serde_json::from_str::<Value>(req.body()) {
                let _ = ipc_tx.send(v);
            }
        })
        .build(&window)?;

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            Event::UserEvent(js) => {
                let _ = webview.evaluate_script(&js);
            }
            Event::WindowEvent { event: WindowEvent::CloseRequested, .. } => {
                // Let the service save the profile before the process ends.
                let _ = tx.send(json!({ "cmd": "quit" }));
                if let Some(handle) = service.take() {
                    let _ = handle.join();
                }
                *control_flow = ControlFlow::Exit;
            }
            _ => {}
        }
    });
}
