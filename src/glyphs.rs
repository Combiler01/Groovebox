//! Grove — M4 painted icons. Every transport, row and page glyph is drawn from
//! shapes, never from font characters, so `▶ ⏸ ⏭ ⋮` can never go tofu no
//! matter which font happens to be loaded (plan §UI keeps Segoe UI for *text*,
//! but Groove's own buttons are flat white symbols).
//!
//! [`button`] is the one interaction+paint primitive the shell uses: the hover
//! state comes from the *same* frame (interact first, then paint), which is
//! what keeps hover-lit icons honest without a second repaint.

use eframe::egui;

use crate::theme::{INK, INK_DIM};

/// Only the glyphs the shell actually paints (an unconstructed variant would
/// trip the crate's dead-code policy).
#[derive(Clone, Copy)]
pub enum Icon {
    Play,
    Pause,
    Prev,
    Next,
    Shuffle,
    Repeat,
    RepeatOne,
    Volume,
    /// transport "⋯"
    DotsH,
    /// row "⋮" menu
    DotsV,
    Hamburger,
    Search,
    Note,
    Gear,
    List,
    /// picture placeholder (cover slots until real art decodes in M5)
    Frame,
    Cross,
    /// Recent plays nav entry: a clock face
    Clock,
}

/// Points along a circular arc, degrees, `n` samples (≥ 2). y grows down, so
/// 0° is right and 90° is straight below the center.
pub fn arc_points(
    center: egui::Pos2,
    r: f32,
    from_deg: f32,
    to_deg: f32,
    n: usize,
) -> Vec<egui::Pos2> {
    let n = n.max(2);
    (0..n)
        .map(|k| {
            let a =
                (from_deg + (to_deg - from_deg) * (k as f32 / (n - 1) as f32)).to_radians();
            egui::pos2(center.x + a.cos() * r, center.y + a.sin() * r)
        })
        .collect()
}

/// Stroke a polyline, `w` pixels wide.
fn line(p: &egui::Painter, pts: Vec<egui::Pos2>, w: f32, ink: egui::Color32) {
    p.add(egui::epaint::PathShape::line(
        pts,
        egui::epaint::PathStroke::new(w, ink),
    ));
}

/// Fill a convex polygon (epaint fills only convex shapes).
fn fill(p: &egui::Painter, pts: Vec<egui::Pos2>, ink: egui::Color32) {
    p.add(egui::epaint::PathShape::convex_polygon(
        pts,
        ink,
        egui::epaint::PathStroke::NONE,
    ));
}

/// Groove-style icon button: hit-test the rect *first*, then paint the hover
/// wash and the glyph from that same-frame state. `salt` keeps each button's
/// `Id` unique inside the enclosing `Ui`; `enabled` gates hover paint and
/// hit-testing (disabled buttons still report hover=false and paint dim).
pub fn button(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    icon: Icon,
    salt: u64,
    enabled: bool,
) -> egui::Response {
    let resp = ui.interact(
        rect,
        ui.scope_id().with(salt).with("groove-icon"),
        if enabled {
            egui::Sense::click()
        } else {
            egui::Sense::hover()
        },
    );
    let v = ui.visuals();
    if enabled && resp.hovered() {
        // Groove lights a soft rounded square behind transport/row icons
        ui.painter().rect_filled(
            rect.shrink(1.0),
            rect.height() * 0.5,
            v.widgets.hovered.weak_bg_fill,
        );
    }
    let ink = if !enabled {
        INK_DIM
    } else if resp.hovered() {
        v.widgets.hovered.fg_stroke.color
    } else {
        INK
    };
    paint(ui, rect, icon, ink);
    resp
}

/// Draw `icon` centered in `rect` in `ink`, sized to ~60 % of the small side.
pub fn paint(ui: &egui::Ui, rect: egui::Rect, icon: Icon, ink: egui::Color32) {
    let p = ui.painter();
    let c = rect.center();
    let s = rect.width().min(rect.height()) * 0.30; // "unit": half-extent
    let w = (s * 0.16).max(1.5); // stroke weight that still reads at 24 px
    let x = |u: f32| c.x + u * s;
    let y = |v: f32| c.y + v * s;
    let at = |u: f32, v: f32| egui::pos2(x(u), y(v));

    match icon {
        Icon::Play => fill(&p, vec![at(-0.35, -0.5), at(0.5, 0.0), at(-0.35, 0.5)], ink),
        Icon::Pause => {
            for u in [-0.42, 0.12] {
                p.rect_filled(
                    egui::Rect::from_min_max(egui::pos2(x(u), y(-0.5)), egui::pos2(x(u + 0.3), y(0.5))),
                    0.0,
                    ink,
                );
            }
        }
        Icon::Prev => {
            p.rect_filled(
                egui::Rect::from_min_max(at(-0.5, -0.5), at(-0.36, 0.5)),
                0.0,
                ink,
            );
            fill(&p, vec![at(0.45, -0.5), at(-0.25, 0.0), at(0.45, 0.5)], ink);
        }
        Icon::Next => {
            fill(&p, vec![at(-0.45, -0.5), at(0.25, 0.0), at(-0.45, 0.5)], ink);
            p.rect_filled(
                egui::Rect::from_min_max(at(0.36, -0.5), at(0.5, 0.5)),
                0.0,
                ink,
            );
        }
        Icon::Shuffle => {
            // two crossing strokes with an arrowhead at each exit
            line(
                &p,
                vec![at(-0.5, -0.32), at(-0.2, -0.32), at(0.5, 0.32)],
                w,
                ink,
            );
            line(
                &p,
                vec![at(-0.5, 0.32), at(-0.2, 0.32), at(0.5, -0.32)],
                w,
                ink,
            );
            // arrowheads point along each line's exit direction
            line(
                &p,
                vec![at(0.28, 0.11), at(0.5, 0.32), at(0.28, 0.32)],
                w,
                ink,
            );
            line(
                &p,
                vec![at(0.28, -0.32), at(0.5, -0.32), at(0.28, -0.11)],
                w,
                ink,
            );
        }
        Icon::Repeat | Icon::RepeatOne => {
            // loop, drawn as a stroked rectangle; the two opposite-facing
            // chevrons on top and bottom carry the rotation direction, which
            // is exactly how Groove's repeat glyph reads at 24 px
            p.add(egui::epaint::PathShape::closed_line(
                vec![
                    at(-0.45, -0.42),
                    at(0.45, -0.42),
                    at(0.45, 0.42),
                    at(-0.45, 0.42),
                ],
                egui::epaint::PathStroke::new(w, ink),
            ));
            // top edge runs right …
            line(
                &p,
                vec![at(0.0, -0.54), at(0.16, -0.42), at(0.0, -0.3)],
                w,
                ink,
            );
            // … bottom edge runs left
            line(
                &p,
                vec![at(0.0, 0.3), at(-0.16, 0.42), at(0.0, 0.54)],
                w,
                ink,
            );
            if matches!(icon, Icon::RepeatOne) {
                // a small "1" in the loop's eye
                line(&p, vec![at(0.04, -0.14), at(0.04, 0.14)], w, ink);
                line(&p, vec![at(-0.06, -0.06), at(0.04, -0.14)], w * 0.8, ink);
            }
        }
        Icon::Volume => {
            // speaker: narrow slab + flaring cone, then two sound arcs
            fill(
                &p,
                vec![at(-0.5, -0.17), at(-0.27, -0.17), at(-0.27, 0.17), at(-0.5, 0.17)],
                ink,
            );
            fill(
                &p,
                vec![at(-0.27, -0.17), at(0.0, -0.42), at(0.0, 0.42), at(-0.27, 0.17)],
                ink,
            );
            for r in [0.34, 0.55] {
                line(
                    &p,
                    arc_points(at(0.02, 0.0), s * r, -55.0, 55.0, 10),
                    w,
                    ink,
                );
            }
        }
        Icon::DotsH => {
            for u in [-0.5, 0.0, 0.5] {
                p.circle_filled(at(u, 0.0), s * 0.15, ink);
            }
        }
        Icon::DotsV => {
            for v in [-0.5, 0.0, 0.5] {
                p.circle_filled(at(0.0, v), s * 0.15, ink);
            }
        }
        Icon::Hamburger => {
            for v in [-0.35, 0.0, 0.35] {
                line(&p, vec![at(-0.5, v), at(0.5, v)], w, ink);
            }
        }
        Icon::Search => {
            p.circle_stroke(at(-0.08, -0.08), s * 0.42, egui::Stroke::new(w, ink));
            line(&p, vec![at(0.22, 0.22), at(0.5, 0.5)], w, ink);
        }
        Icon::Note => {
            // eighth note: filled head, stem, flag
            p.circle_filled(at(-0.32, 0.34), s * 0.28, ink);
            line(&p, vec![at(-0.06, 0.24), at(-0.06, -0.45)], w, ink);
            line(
                &p,
                vec![at(-0.06, -0.45), at(0.42, -0.3), at(0.42, -0.12)],
                w,
                ink,
            );
        }
        Icon::Gear => {
            p.circle_stroke(c, s * 0.34, egui::Stroke::new(w, ink));
            p.circle_stroke(c, s * 0.13, egui::Stroke::new(w, ink));
            for k in 0..8 {
                let a = k as f32 * 45.0_f32.to_radians();
                line(
                    &p,
                    vec![
                        egui::pos2(c.x + a.cos() * s * 0.34, c.y + a.sin() * s * 0.34),
                        egui::pos2(c.x + a.cos() * s * 0.55, c.y + a.sin() * s * 0.55),
                    ],
                    w * 1.4,
                    ink,
                );
            }
        }
        Icon::List => {
            for k in 0..4u32 {
                let v = -0.45 + k as f32 * 0.3;
                line(&p, vec![at(-0.5, v), at(-0.34, v)], w, ink);
                line(
                    &p,
                    vec![at(-0.14, v), at(0.5 - (k % 2) as f32 * 0.18, v)],
                    w,
                    ink,
                );
            }
        }
        Icon::Frame => {
            // picture placeholder: frame + sun + ridge (Groove's empty-cover look)
            p.rect_stroke(
                egui::Rect::from_min_max(at(-0.5, -0.5), at(0.5, 0.5)),
                0.0,
                egui::Stroke::new(w, ink),
                egui::StrokeKind::Middle,
            );
            p.circle_stroke(at(-0.24, -0.24), s * 0.14, egui::Stroke::new(w, ink));
            line(
                &p,
                vec![at(-0.5, 0.38), at(-0.12, -0.02), at(0.12, 0.22), at(0.5, -0.08)],
                w,
                ink,
            );
        }
        Icon::Clock => {
            p.circle_stroke(c, s * 0.45, egui::Stroke::new(w, ink));
            // hour + minute hand from the centre
            line(&p, vec![c, at(0.0, -0.3)], w, ink);
            line(&p, vec![c, at(0.26, 0.06)], w, ink);
        }
        Icon::Cross => {
            line(&p, vec![at(-0.42, -0.42), at(0.42, 0.42)], w, ink);
            line(&p, vec![at(0.42, -0.42), at(-0.42, 0.42)], w, ink);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // `HOVER` only appears in the headless paint check, so it lives here and
    // keeps the non-test build free of an unused-import warning.
    use crate::theme::HOVER;

    #[test]
    fn arc_points_sample_the_requested_span() {
        let a = arc_points(egui::Pos2::ZERO, 10.0, 0.0, 90.0, 4);
        assert_eq!(a.len(), 4);
        assert!((a[0].x - 10.0).abs() < 0.01 && a[0].y.abs() < 0.01);
        assert!(a[3].x.abs() < 0.01 && (a[3].y - 10.0).abs() < 0.01);
    }

    /// Headless: every glyph paints through a real `egui::Ui` without
    /// panicking, [`button`] registers its rect for input, and a pass with no
    /// pointer must not report hover or click.
    #[test]
    fn every_glyph_paints_and_buttons_hit_test() {
        let all = [
            Icon::Play,
            Icon::Pause,
            Icon::Prev,
            Icon::Next,
            Icon::Shuffle,
            Icon::Repeat,
            Icon::RepeatOne,
            Icon::Volume,
            Icon::DotsH,
            Icon::DotsV,
            Icon::Hamburger,
            Icon::Search,
            Icon::Note,
            Icon::Gear,
            Icon::List,
            Icon::Frame,
            Icon::Cross,
            Icon::Clock,
        ];
        let ctx = egui::Context::default();
        let mut stray = false;
        let out = ctx.run_ui(egui::RawInput::default(), |ui| {
            for (i, ic) in all.iter().enumerate() {
                let r = egui::Rect::from_min_size(
                    egui::pos2(i as f32 * 34.0, 0.0),
                    egui::vec2(28.0, 28.0),
                );
                let resp = button(ui, r, *ic, i as u64, true);
                stray |= resp.hovered() || resp.clicked();
                paint(ui, r, *ic, INK);
                paint(ui, r.shrink(8.0), *ic, HOVER.gamma_multiply(0.5));
                paint(ui, r.shrink(4.0), *ic, INK_DIM);
            }
        });
        assert!(!stray, "no pointer means no hover and no click");
        // 17 glyphs × 3 sizes must have produced real geometry
        let shapes = out.shapes.len();
        // A headless test never uploads the font atlas, so hand the texture
        // deltas back exactly the way `FullOutput` says to — a real backend
        // applies them to the painter instead.
        out.drop_without_applying_deltas();
        assert!(
            shapes >= all.len() * 3,
            "expected {}+ shapes, got {shapes}",
            all.len() * 3
        );
    }
}
