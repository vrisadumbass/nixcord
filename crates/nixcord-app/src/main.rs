//! nixcord: a lightweight native Discord client.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod backend;
mod demo;
mod media;
mod theme;
mod token;
mod ui;

use eframe::egui;

fn main() -> eframe::Result {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("NIXCORD_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();

    let demo = std::env::args().any(|a| a == "--demo")
        || std::env::var("NIXCORD_DEMO").is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true"));

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("nixcord")
            .with_app_id("nixcord")
            .with_inner_size([1280.0, 800.0])
            .with_min_inner_size([720.0, 420.0]),
        renderer: eframe::Renderer::Glow,
        // Keep the GPU footprint small: no MSAA, no depth/stencil buffers.
        multisampling: 0,
        depth_buffer: 0,
        stencil_buffer: 0,
        ..Default::default()
    };

    eframe::run_native(
        "nixcord",
        options,
        Box::new(move |cc| Ok(Box::new(app::NixApp::new(cc, demo)))),
    )
}
