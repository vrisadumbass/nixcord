//! nixcord: a lightweight native Discord client.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod backend;
mod demo;
mod demo_assets;
mod media;
mod platform;
mod theme;
mod token;
mod ui;
mod video;

use eframe::egui;

fn main() -> eframe::Result {
    let mut filter = tracing_subscriber::EnvFilter::try_from_env("NIXCORD_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));
    // Transport crates can log raw frames/headers at debug/trace, which would include the
    // token (identify payload, Authorization header). Cap them no matter what NIXCORD_LOG
    // says; more specific directives win over a bare level like `debug`.
    for quiet in [
        "tungstenite=warn",
        "tokio_tungstenite=warn",
        "rustls=warn",
        "hyper=warn",
        "hyper_util=warn",
        "reqwest=warn",
        "h2=warn",
    ] {
        if let Ok(d) = quiet.parse() {
            filter = filter.add_directive(d);
        }
    }
    let log = tracing_subscriber::fmt().with_env_filter(filter);
    // Release builds are windows-subsystem binaries without a console: NIXCORD_LOG_FILE
    // appends the log to a file instead of stderr.
    let log_file = std::env::var_os("NIXCORD_LOG_FILE").and_then(|path| {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .inspect_err(|e| eprintln!("NIXCORD_LOG_FILE: cannot open {path:?}: {e}"))
            .ok()
    });
    match log_file {
        Some(file) => log
            .with_ansi(false)
            .with_writer(std::sync::Mutex::new(file))
            .init(),
        None => log.with_writer(std::io::stderr).init(),
    }

    let demo = std::env::args().any(|a| a == "--demo")
        || std::env::var("NIXCORD_DEMO").is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true"));

    let force_glow =
        std::env::var("NIXCORD_RENDERER").is_ok_and(|v| v.eq_ignore_ascii_case("glow"));
    let base = if force_glow {
        glow_options()
    } else {
        platform::renderer_options()
    };
    let primary_is_glow = base.renderer == eframe::Renderer::Glow;

    match run(base, demo) {
        // The preferred renderer (D3D12 on Windows) failed to start, e.g. a broken driver:
        // retry once with OpenGL before giving up.
        Err(e) if !primary_is_glow => {
            tracing::warn!("renderer failed to start ({e}); falling back to OpenGL");
            run(glow_options(), demo)
        }
        r => r,
    }
}

fn glow_options() -> eframe::NativeOptions {
    eframe::NativeOptions {
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    }
}

fn run(base: eframe::NativeOptions, demo: bool) -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("nixcord")
            .with_app_id("nixcord")
            .with_inner_size([1280.0, 800.0])
            .with_min_inner_size([720.0, 420.0]),
        // Keep the GPU footprint small: no MSAA, no depth/stencil buffers.
        multisampling: 0,
        depth_buffer: 0,
        stencil_buffer: 0,
        ..base
    };

    eframe::run_native(
        "nixcord",
        options,
        Box::new(move |cc| {
            // The renderer is up and the first frame follows right away; once startup has
            // settled, hand the pages only init touched back to the OS.
            platform::trim_after_startup();
            Ok(Box::new(app::NixApp::new(cc, demo)))
        }),
    )
}
