#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod deploy;
mod disk;
mod elevate;
mod image;
mod vhdx;
mod vss;

#[cfg(test)]
mod tests;

use app::WinToUsbApp;
use eframe::egui::Vec2;
use std::io::Write;

fn main() -> eframe::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--version" || a == "-V") {
        let _ = writeln!(std::io::stdout(), "WinToUSB v{}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if args.iter().any(|a| a == "--help" || a == "-h") {
        let mut out = std::io::stdout();
        let _ = writeln!(
            out,
            "WinToUSB v{} - Native Rust Windows To Go, Live Clone & Bootable USB Suite",
            env!("CARGO_PKG_VERSION")
        );
        let _ = writeln!(out, "Usage: WinToUSB.exe [--version | --help]");
        return Ok(());
    }

    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size(Vec2::new(1180.0, 760.0))
            .with_min_inner_size(Vec2::new(860.0, 560.0))
            .with_title("WinToUSB v1.0.1 — Windows To Go, Live OS Cloner & Bootable USB Suite"),
        ..Default::default()
    };

    eframe::run_native(
        "WinToUSB",
        options,
        Box::new(|cc| Ok(Box::new(WinToUsbApp::new(cc)))),
    )
}
