//! Grove — Groove Music clone in Rust. M1: eframe/wgpu window + Cmd/Event
//! channel contract + PlayerEngine worker (play/pause/volume). Window stays
//! OS-decorated until M6 (plan §chrome).

mod audio;
mod glyphs;
mod model;
mod scan;
mod theme;
mod ui;

use eframe::egui;

fn main() -> eframe::Result<()> {
    // Hidden-instance mode for automated idle soaks: no window → no hover,
    // no clicks, no user interference; heartbeat beats drive the measurement.
    let mut viewport = egui::ViewportBuilder::default()
        .with_inner_size([980.0, 640.0])
        .with_min_inner_size([640.0, 420.0]);
    if std::env::var_os("GROVE_IDLE_TEST").is_some() {
        viewport = viewport.with_visible(false);
    }
    let native_options = eframe::NativeOptions {
        viewport,
        // default decorated window (M1–M5), wgpu renderer, DX12 backend
        // compiled in via the wgpu feature flags in Cargo.toml.
        ..Default::default()
    };
    eframe::run_native(
        "Grove Music",
        native_options,
        Box::new(|cc| {
            if let Some(rs) = &cc.wgpu_render_state {
                let info = rs.adapter.get_info();
                model::tlog!(
                    "renderer init OK · adapter={} backend={:?} type={:?}",
                    info.name, info.backend, info.device_type
                );
            }
            Ok(Box::new(ui::GroveApp::new(cc)))
        }),
    )
}
