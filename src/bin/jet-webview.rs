//! Login window for `jet`: shows a page in the OS web view (WKWebView on macOS, WebView2 on
//! Windows, WebKitGTK on Linux) and relays between the page and `jet` over stdin/stdout.
//!
//! It's a separate binary so that `jet` itself never links the GUI/web view libraries: on a
//! Linux machine without WebKitGTK only login falls back to another method.
//!
//! Protocol (JSON lines):
//! - stdin, first line: `{"url", "dataDir", "script", "intercept", "visible", "title"}`
//! - stdin, then: `{"cmd": "show" | "hide" | "close"}` or `{"cmd": "eval", "js": "..."}`
//! - stdout: `{"event": "ready"}`, `{"event": "ipc", "data": "<string from the page>"}`,
//!   `{"event": "redirect", "url": "..."}` (navigation to `intercept`, which is blocked),
//!   `{"event": "closed"}` (the user closed the window)

// No console window on Windows; stdin/stdout still work as pipes from `jet`.
#![cfg_attr(windows, windows_subsystem = "windows")]

use std::io::{BufRead, Write};
use std::path::PathBuf;

use serde::Deserialize;
use serde_json::json;
use tao::dpi::LogicalSize;
use tao::event::{Event, StartCause, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tao::window::WindowBuilder;
use wry::{WebContext, WebViewBuilder};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Config {
    url: String,
    data_dir: PathBuf,
    script: String,
    intercept: String,
    visible: bool,
    title: String,
}

#[derive(Deserialize)]
#[serde(tag = "cmd", rename_all = "lowercase")]
enum Command {
    Show,
    Hide,
    Close,
    Eval { js: String },
}

fn emit(value: serde_json::Value) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{value}");
    let _ = out.flush();
}

fn main() {
    // `jet-webview --check`: proves the binary and its libraries load (a missing WebKitGTK fails
    // in the dynamic loader, before main) without opening anything.
    if std::env::args().any(|a| a == "--check") {
        println!("ok");
        return;
    }
    let stdin = std::io::stdin();
    let mut first = String::new();
    if stdin.lock().read_line(&mut first).unwrap_or(0) == 0 {
        eprintln!(
            "jet-webview: expected a config line on stdin (this is a helper for `jet login`)"
        );
        std::process::exit(2);
    }
    let config: Config = match serde_json::from_str(&first) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("jet-webview: invalid config: {e}");
            std::process::exit(2);
        }
    };

    let event_loop = EventLoopBuilder::<Command>::with_user_event().build();
    #[cfg(target_os = "macos")]
    let event_loop = {
        // No Dock icon or menu bar takeover: this is a helper window for a terminal app.
        use tao::platform::macos::{ActivationPolicy, EventLoopExtMacOS};
        let mut event_loop = event_loop;
        event_loop.set_activation_policy(ActivationPolicy::Accessory);
        event_loop
    };

    // Commands from `jet` arrive on stdin; hand them to the event loop. EOF means `jet` is gone.
    let proxy = event_loop.create_proxy();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            if let Ok(cmd) = serde_json::from_str::<Command>(&line)
                && proxy.send_event(cmd).is_err()
            {
                return;
            }
        }
        let _ = proxy.send_event(Command::Close);
    });

    let window = WindowBuilder::new()
        .with_title(&config.title)
        .with_inner_size(LogicalSize::new(480.0, 760.0))
        .with_visible(config.visible)
        .build(&event_loop)
        .expect("failed to create window");

    std::fs::create_dir_all(&config.data_dir).ok();
    let mut context = WebContext::new(Some(config.data_dir.clone()));
    let intercept = config.intercept.clone();
    let builder = WebViewBuilder::new_with_web_context(&mut context)
        .with_url(&config.url)
        .with_initialization_script(config.script.as_str())
        .with_ipc_handler(|req| emit(json!({ "event": "ipc", "data": req.body() })))
        .with_navigation_handler(move |url| {
            if url.starts_with(&intercept) {
                emit(json!({ "event": "redirect", "url": url }));
                false
            } else {
                true
            }
        });

    #[cfg(any(target_os = "windows", target_os = "macos"))]
    let webview = builder.build(&window).expect("failed to create web view");
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let webview = {
        use tao::platform::unix::WindowExtUnix;
        use wry::WebViewBuilderExtUnix;
        builder
            .build_gtk(window.default_vbox().expect("no GTK container"))
            .expect("failed to create web view")
    };

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            Event::NewEvents(StartCause::Init) => emit(json!({ "event": "ready" })),
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => {
                emit(json!({ "event": "closed" }));
                *control_flow = ControlFlow::Exit;
            }
            Event::UserEvent(Command::Show) => {
                window.set_visible(true);
                window.set_focus();
            }
            Event::UserEvent(Command::Hide) => window.set_visible(false),
            Event::UserEvent(Command::Eval { js }) => {
                let _ = webview.evaluate_script(&js);
            }
            Event::UserEvent(Command::Close) => *control_flow = ControlFlow::Exit,
            _ => {}
        }
    });
}
