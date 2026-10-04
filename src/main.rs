//! Claude Agent Graph: a live view of your Claude Code sessions and subagents, plus a workflow builder and runner.
//!
//! Usage:  agent-graph [--browser] [--port N]
//!         agent-graph list | run <workflow or file.yaml> | validate [files] | runs
//! Opens a native window when built with the `window` feature; otherwise, or with --browser, serves on
//! localhost and opens your default browser.

mod cli;
mod compat;
mod providers;
mod quality;
mod runner;
mod server;
mod util;
mod watcher;
mod wfstore;
mod workflows;
mod yamldoc;

use clap::Parser;

#[derive(Parser)]
#[command(
    name = "agent-graph",
    version,
    about = "Claude Agent Graph: a live view of your Claude Code sessions and subagents.",
    after_help = "Terminal commands: agent-graph list | run <workflow or file.yaml> | validate [files] | runs  (add --help to each)"
)]
struct Args {
    /// open in the default browser instead of a window
    #[arg(long)]
    browser: bool,
    /// port to serve on (default: random free port)
    #[arg(long, default_value_t = 0)]
    port: u16,
}

fn main() {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.first().is_some_and(|a| ["run", "list", "runs", "validate"].contains(&a.as_str())) {
        // terminal commands, no window
        std::process::exit(cli::main(argv));
    }
    let args = Args::parse();
    let app = server::App::new();
    app.initial_scan();
    let poller = app.clone();
    std::thread::spawn(move || poller.poll_forever());
    let port = if args.port != 0 { args.port } else if args.browser { 8765 } else { 0 };
    let port = app.serve(port).unwrap_or_else(|e| {
        eprintln!("Couldn't start the server on port {port}: {e}");
        std::process::exit(1)
    });
    let url = format!("http://127.0.0.1:{port}/");

    if !args.browser {
        match open_window(&url) {
            Ok(()) => return,
            // no GUI toolkit: the browser works everywhere
            Err(e) => eprintln!("No native window available ({e}); opening your browser instead."),
        }
    }
    println!("Claude Agent Graph running at {url}");
    let _ = webbrowser::open(&url);
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}

#[cfg(not(feature = "window"))]
fn open_window(_url: &str) -> Result<(), String> {
    Err("this build has no window support; build with `--features window` for one".into())
}

#[cfg(feature = "window")]
fn open_window(url: &str) -> Result<(), String> {
    use tao::event::{Event, WindowEvent};
    use tao::event_loop::{ControlFlow, EventLoop};
    use tao::window::{Icon, WindowBuilder};

    let event_loop = EventLoop::new();
    let mut builder = WindowBuilder::new().with_title("Claude Agent Graph").with_inner_size(tao::dpi::LogicalSize::new(1400.0, 900.0));
    if let Some(icon) = decode_icon(server::ICON_PNG).and_then(|(rgba, w, h)| Icon::from_rgba(rgba, w, h).ok()) {
        builder = builder.with_window_icon(Some(icon));
    }
    let window = builder.build(&event_loop).map_err(|e| e.to_string())?;
    let wb = wry::WebViewBuilder::new().with_url(url);
    #[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "ios", target_os = "android")))]
    let webview = {
        use tao::platform::unix::WindowExtUnix;
        use wry::WebViewBuilderExtUnix;
        wb.build_gtk(window.default_vbox().ok_or("no GTK container")?)
    };
    #[cfg(any(target_os = "windows", target_os = "macos", target_os = "ios", target_os = "android"))]
    let webview = wb.build(&window);
    let webview = webview.map_err(|e| e.to_string())?;
    event_loop.run(move |event, _, control_flow| {
        let _ = &webview;
        *control_flow = ControlFlow::Wait;
        if let Event::WindowEvent { event: WindowEvent::CloseRequested, .. } = event {
            *control_flow = ControlFlow::Exit;
        }
    })
}

#[cfg(feature = "window")]
fn decode_icon(bytes: &[u8]) -> Option<(Vec<u8>, u32, u32)> {
    let mut decoder = png::Decoder::new(bytes);
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::ALPHA);
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).ok()?;
    buf.truncate(info.buffer_size());
    if info.color_type != png::ColorType::Rgba || info.bit_depth != png::BitDepth::Eight {
        return None;
    }
    Some((buf, info.width, info.height))
}
