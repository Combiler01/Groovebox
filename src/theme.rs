//! Grove — M4 shell theme: Groove Music's dark palette, Segoe UI loaded at
//! runtime, and the pure layout math the A–Z grids need.
//!
//! Everything here is either data or pure functions: `cargo test` can verify
//! the plan's color tokens, the font-file classifier and the grid math without
//! a window, an audio device or a music library. The only IO is reading
//! `C:\Windows\Fonts` (skipped silently when absent → egui's bundled fonts).

use std::ops::Range;
use std::path::Path;
use std::sync::Arc;

use eframe::egui;

use crate::model::tlog;

// ── palette (plan §UI tokens, from the 8 Groove screenshots) ──────────────
pub const BG: egui::Color32 = egui::Color32::from_rgb(0x00, 0x00, 0x00);
pub const SIDEBAR: egui::Color32 = egui::Color32::from_rgb(0x2b, 0x2b, 0x2b);
pub const ALT_ROW: egui::Color32 = egui::Color32::from_rgb(0x19, 0x19, 0x19);
pub const HOVER: egui::Color32 = egui::Color32::from_rgb(0x30, 0x30, 0x30);
pub const ACCENT: egui::Color32 = egui::Color32::from_rgb(0x00, 0x78, 0xd4);
pub const LINK: egui::Color32 = egui::Color32::from_rgb(0x4c, 0xc2, 0xff);
pub const INK: egui::Color32 = egui::Color32::from_rgb(0xe5, 0xe5, 0xe5);
pub const INK_DIM: egui::Color32 = egui::Color32::from_rgb(0x9a, 0x9a, 0x9a);
/// Hairlines (column rules, field borders, separators).
pub const HAIR: egui::Color32 = egui::Color32::from_rgb(0x33, 0x33, 0x33);

/// Names `theme::apply` binds into `Style::text_styles`.
pub const TITLE_STYLE: &str = "PageTitle";

/// Groove's dark Visuals: the plan's tokens, mapped onto egui 0.37 fields so
/// widgets *and* the hand-painted rows agree on the same colors.
pub fn groove_visuals() -> egui::Visuals {
    let mut v = egui::Visuals::dark();
    v.dark_mode = true;

    v.panel_fill = BG;
    v.faint_bg_color = ALT_ROW; // alt-row stripes read this color directly
    v.extreme_bg_color = ALT_ROW; // text fields, scroll rails
    v.text_edit_bg_color = Some(ALT_ROW);
    v.code_bg_color = ALT_ROW;
    v.window_fill = SIDEBAR; // popups/menus = Groove's dark gray
    v.window_stroke = egui::Stroke::new(1.0, HAIR);

    // Groove is flat: square corners everywhere, thin slider rail.
    v.window_corner_radius = egui::CornerRadius::ZERO;
    v.menu_corner_radius = egui::CornerRadius::ZERO;
    for w in [
        &mut v.widgets.noninteractive,
        &mut v.widgets.inactive,
        &mut v.widgets.hovered,
        &mut v.widgets.active,
        &mut v.widgets.open,
    ] {
        w.corner_radius = egui::CornerRadius::ZERO;
        w.bg_stroke = egui::Stroke::new(1.0, HAIR);
        w.fg_stroke = egui::Stroke::new(1.0, INK);
    }
    v.widgets.noninteractive.fg_stroke = egui::Stroke::new(1.0, INK);
    v.widgets.inactive.fg_stroke = egui::Stroke::new(1.0, INK);
    v.widgets.hovered.fg_stroke = egui::Stroke::new(1.0, INK);
    v.widgets.hovered.weak_bg_fill = HOVER; // row hover + hover-lit buttons
    v.widgets.hovered.bg_fill = HOVER; // slider handle on its rail
    v.widgets.active.fg_stroke = egui::Stroke::new(1.0, INK);
    v.widgets.active.weak_bg_fill = HOVER;
    v.widgets.inactive.weak_bg_fill = egui::Color32::TRANSPARENT; // flat buttons
    v.widgets.open.weak_bg_fill = HOVER;

    // text color: primary #e5e5e5, secondary #9a9a9a (never dimmed by alpha)
    v.override_text_color = None;
    v.weak_text_color = Some(INK_DIM);
    v.weak_text_alpha = 1.0;
    v.hyperlink_color = LINK;

    // playing-row / selection fill = Groove accent
    v.selection.bg_fill = ACCENT;
    v.selection.stroke = egui::Stroke::new(1.0, LINK);

    v.slider_trailing_fill = true; // Groove's volume/progress show filled track
    v.interact_cursor = None; // native-app cursor, not the browser hand
    v.image_loading_spinners = false;
    v.window_shadow = egui::Shadow::NONE; // Win10 flyouts are flat with a border
    v.popup_shadow = egui::Shadow::NONE;
    v
}

/// Theme + palette + Segoe UI + text styles. Returns the [`egui::FontId`] for
/// page titles (32–36 px semibold, plan §UI) so the UI can paint its headers.
///
/// M4 pins Dark (plan §Settings Mode Light/Dark/System arrives in M6); the
/// theme call is what makes `set_visuals` land on the style egui actually uses.
pub fn apply(ctx: &egui::Context) -> egui::FontId {
    let title = load_segoe_ui(ctx);
    ctx.set_theme(egui::ThemePreference::Dark);
    ctx.style_mut_of(egui::Theme::Dark, |style| {
        style.visuals = groove_visuals();
        style.spacing.button_padding = egui::vec2(10.0, 4.0);
        style.spacing.slider_rail_height = 4.0;
        // Groove body/list text sits ~13–14 px; headings use the semibold face.
        style.text_styles.insert(egui::TextStyle::Small, egui::FontId::proportional(12.0));
        style.text_styles.insert(egui::TextStyle::Body, egui::FontId::proportional(13.5));
        style.text_styles.insert(egui::TextStyle::Button, egui::FontId::proportional(13.5));
        style.text_styles.insert(egui::TextStyle::Heading, title.clone());
        style
            .text_styles
            .insert(egui::TextStyle::Name(Arc::from(TITLE_STYLE)), title.clone());
    });
    title
}

// ── Segoe UI at runtime (plan §UI: "runtime-load C:\Windows\Fonts\segoeui*.ttf") ──

/// Windows font file names across versions, in preference order. Weight comes
/// from each file's `OS/2` table, so a differently-named build still lands in
/// the right slot (Regular / Semibold / Light) instead of trusting the name.
const SEGOE_DIR: &str = "C:\\Windows\\Fonts";
const SEGOE_CANDIDATES: [&str; 10] = [
    "segoeui.ttf",
    "seguisb.ttf",
    "segoeuib.ttf",
    "segoeuiz.ttf",
    "segoeuisl.ttf",
    "segoeuil.ttf",
    "Segoe UI.ttf",
    "Segoe UI Semibold.ttf",
    "Segoe UI Light.ttf",
    "Segoe UI Semilight.ttf",
];

/// Which candidate file fills each weight slot (index into the candidate list).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct FacePick {
    pub regular: Option<usize>,
    pub semibold: Option<usize>,
    pub light: Option<usize>,
}

impl FacePick {
    pub fn is_empty(&self) -> bool {
        self.regular.is_none() && self.semibold.is_none() && self.light.is_none()
    }
}

/// Slot assignment by `OS/2` weight class: nearest to the target inside the
/// band, ties break on candidate order, one file per slot.
pub fn pick_faces(faces: &[(String, u16)]) -> FacePick {
    let mut taken = vec![false; faces.len()];
    let mut pick = |target: u16, band: (u16, u16)| -> Option<usize> {
        let best = faces
            .iter()
            .enumerate()
            .filter(|(i, (_, w))| !taken[*i] && *w >= band.0 && *w <= band.1)
            .min_by_key(|(i, (_, w))| (w.abs_diff(target), *i))
            .map(|(i, _)| i);
        if let Some(i) = best {
            taken[i] = true;
        }
        best
    };
    FacePick {
        regular: pick(400, (350, 500)),
        // A true Bold (700) is a better 34 px page title than leaving Segoe
        // altogether, so the semibold band runs to 700; the nearest-to-600 rule
        // still prefers a real Semibold when the machine has one.
        semibold: pick(600, (540, 700)),
        light: pick(300, (250, 350)),
    }
}

/// Big-endian u16 (TTF tables are all big-endian); `None` when out of range.
fn be_u16(b: &[u8], at: usize) -> Option<u16> {
    b.get(at..at + 2).map(|p| u16::from_be_bytes([p[0], p[1]]))
}

fn be_u32(b: &[u8], at: usize) -> Option<u32> {
    b.get(at..at + 4)
        .map(|p| u32::from_be_bytes([p[0], p[1], p[2], p[3]]))
}

/// `OS/2::usWeightClass`: 300 Light · 350 Semilight · 400 Regular ·
/// 600 Semibold · 700 Bold. Works on a partial read (returns `None` if the
/// table sits past the buffer), so a 128 KB head probe is enough.
pub fn os2_weight_class(b: &[u8]) -> Option<u16> {
    let base = match b.get(0..4)? {
        // TrueType collection: the first font's table directory lives at offsets[0].
        b"ttcf" => be_u32(b, 12)? as usize,
        // sfnt version is big-endian: 0x00010000 (TTF) → `00 01 00 00`,
        // 0x01000000 (CFF/OTF) → `01 00 00 00`, plus the Apple `true`/`typ1`.
        [0, 1, 0, 0] | [1, 0, 0, 0] | b"true" | b"typ1" => 0,
        _ => return None,
    };
    // sfnt header: version(4) numTables(2) searchArray/range/entry/rangeShift(8)
    // → `numTables` at base+4 and the 16-byte table records at base+12.
    let n_tables = be_u16(b, base + 4)? as usize;
    for i in 0..n_tables {
        let rec = base + 12 + i * 16;
        if b.len() < rec + 16 {
            return None; // truncated head probe
        }
        if &b[rec..rec + 4] == b"OS/2" {
            let raw = be_u32(b, rec + 8)? as usize;
            // A plain sfnt measures tables from the start of the file; inside a
            // collection the same record can be header-relative, so accept
            // whichever candidate actually sits on an `OS/2` (version 0..=5
            // then usWeightClass 1..=1000) instead of trusting one reading.
            for cand in [raw, base.saturating_add(raw)] {
                if matches!(be_u16(b, cand), Some(0..=5))
                    && let Some(w) = be_u16(b, cand + 4)
                    && (1..=1000).contains(&w)
                {
                    return Some(w);
                }
            }
            return None;
        }
    }
    None
}

fn read_head(path: &Path, n: usize) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).ok()?;
    let mut buf = vec![0u8; n];
    let got = f.read(&mut buf).ok()?;
    buf.truncate(got);
    if buf.is_empty() {
        None
    } else {
        Some(buf)
    }
}

/// Reads only the heads of the candidate files (128 KB each), classifies by
/// weight, then reads the full bytes of the three chosen faces. Adds them to
/// `ctx` as the primary proportional font plus two named families, and returns
/// the page-title [`egui::FontId`].
pub fn load_segoe_ui(ctx: &egui::Context) -> egui::FontId {
    let fallback = egui::FontId::proportional(34.0);
    if !Path::new(SEGOE_DIR).is_dir() {
        tlog!("note: no {SEGOE_DIR} · using egui's bundled fonts");
        return fallback;
    }
    let mut faces: Vec<(String, u16)> = Vec::new();
    for name in SEGOE_CANDIDATES {
        let path = std::path::PathBuf::from(SEGOE_DIR).join(name);
        if let Some(head) = read_head(&path, 128 * 1024)
            && let Some(w) = os2_weight_class(&head)
        {
            faces.push((name.to_string(), w));
        }
    }
    if faces.is_empty() {
        tlog!("note: Segoe UI not found · using egui's bundled fonts");
        return fallback;
    }
    let pick = pick_faces(&faces);
    let names: Vec<String> = faces.iter().map(|(n, _)| n.clone()).collect();
    tlog!(
        "Segoe UI probe · {} candidate file(s) · regular={} semibold={} light={}",
        faces.len(),
        pick.regular.map_or("none", |i| &names[i]),
        pick.semibold.map_or("none", |i| &names[i]),
        pick.light.map_or("none", |i| &names[i]),
    );

    // Every candidate could sit outside all three bands (a machine with only
    // e.g. `segoeuiblack.ttf`): then nothing is inserted and the bundled fonts
    // stay in charge, which is the honest outcome to log.
    if pick.is_empty() {
        tlog!("note: no candidate matched the weight bands · bundled fonts stay");
        return fallback;
    }

    let insert = |slot: Option<usize>, key: &str, family: egui::FontFamily| {
        let i = match slot {
            Some(i) => i,
            None => return,
        };
        let path = std::path::PathBuf::from(SEGOE_DIR).join(faces[i].0.as_str());
        let Ok(bytes) = std::fs::read(&path) else {
            tlog!("note: {} unreadable · skipped", path.display());
            return;
        };
        ctx.add_font(egui::FontInsert::new(
            key,
            egui::FontData::from_owned(bytes),
            vec![egui::InsertFontFamily {
                family,
                priority: egui::FontPriority::Highest,
            }],
        ));
    };

    // Regular leads the proportional fallback chain, so anything Segoe lacks
    // still falls through to egui's bundled Ubuntu-Light + emoji font.
    insert(pick.regular, "segoe-ui", egui::FontFamily::Proportional);
    insert(
        pick.semibold,
        "segoe-ui-semibold",
        egui::FontFamily::Name(Arc::from("Segoe UI Semibold")),
    );
    insert(
        pick.light,
        "segoe-ui-light",
        egui::FontFamily::Name(Arc::from("Segoe UI Light")),
    );

    let title = if pick.semibold.is_some() {
        egui::FontId::new(
            34.0,
            egui::FontFamily::Name(Arc::from("Segoe UI Semibold")),
        )
    } else if pick.regular.is_some() {
        egui::FontId::new(34.0, egui::FontFamily::Proportional)
    } else {
        fallback
    };
    tlog!("theme ready · page titles use {} @34px", title.family);
    title
}

// ── grid math (Artists/Albums A–Z tiles virtualized as flat lines) ───────

/// How many `tile`-wide columns of `gap`-spaced tiles fit in `width` (≥ 1).
pub fn tile_cols(width: f32, tile: f32, gap: f32) -> usize {
    if width <= 0.0 || tile <= 0.0 {
        return 1;
    }
    (((width + gap) / (tile + gap)).floor().max(1.0)) as usize
}

/// Top offset of every line + the total height, for variable-height
/// virtualization (`ScrollArea::show_viewport` does not know our row heights).
pub fn line_tops(heights: &[f32]) -> (Vec<f32>, f32) {
    let mut tops = Vec::with_capacity(heights.len());
    let mut y = 0.0f32;
    for h in heights {
        tops.push(y);
        y += h;
    }
    (tops, y)
}

/// Lines that intersect `y0..y1` inside content of height `total` (`tops`
/// ascending, so the search is binary — a 260-group page costs log n per frame;
/// the result is padded by one line at each end for honest partials).
pub fn visible_lines(tops: &[f32], y0: f32, y1: f32, total: f32) -> Range<usize> {
    let n = tops.len();
    if n == 0 {
        return 0..0;
    }
    let lo = tops.partition_point(|t| *t < y0.max(0.0));
    let start = lo.saturating_sub(1).min(n - 1);
    let end = tops
        .partition_point(|t| *t <= y1.min(total))
        .max(start + 1)
        .min(n);
    start..end
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal single-table sfnt with `OS/2` at `table_off`, weight at `+4`.
    fn fake_ttf(weight: u16, table_off: u32) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend([0u8, 1u8, 0u8, 0u8]); // sfnt version 0x00010000, big-endian
        b.extend(1u16.to_be_bytes()); // numTables = 1
        b.extend([0u8; 4]); // searchL/1/2/3 filler continues below
        // header totals 12 bytes: version(4) + numTables(2) + searchL(2) + searchR(2) + searchU(2)
        // rewrite: pad to exactly 12
        b.truncate(6);
        b.extend([0u8; 6]);
        // table record: tag, checksum, offset, length
        b.extend(b"OS/2");
        b.extend(0u32.to_be_bytes());
        b.extend(table_off.to_be_bytes());
        b.extend(0x1000u32.to_be_bytes());
        while b.len() < table_off as usize {
            b.push(0);
        }
        b.extend(0u16.to_be_bytes()); // OS/2 version
        b.extend(0u16.to_be_bytes()); // xAvgCharWidth
        b.extend(weight.to_be_bytes()); // usWeightClass
        b
    }

    #[test]
    fn weight_class_reads_os2_at_any_offset() {
        assert_eq!(os2_weight_class(&fake_ttf(600, 64)), Some(600));
        assert_eq!(os2_weight_class(&fake_ttf(300, 200)), Some(300));
        // A table past a truncated probe stays unread: `fake_ttf` writes the
        // bytes at that offset, so cut the buffer short to model a head probe
        // that really did not reach it.
        let mut clipped = fake_ttf(400, 1_000_000);
        clipped.truncate(4096);
        assert_eq!(os2_weight_class(&clipped), None);
        assert_eq!(
            os2_weight_class(&fake_ttf(400, 1_000_000)),
            Some(400),
            "a full read reaches a far table"
        );
        assert_eq!(os2_weight_class(b"not a font at all"), None);
        assert_eq!(os2_weight_class(&[]), None);
    }

    #[test]
    fn collection_header_resolves_to_the_first_font() {
        let mut inner = fake_ttf(350, 64);
        let dir_off = 24u32; // where the inner sfnt sits
        let mut ttc = Vec::new();
        ttc.extend(b"ttcf");
        ttc.extend([0u8; 4]); // major/minor version
        ttc.extend(1u32.to_be_bytes()); // numFonts
        ttc.extend(dir_off.to_be_bytes()); // offsets[0]
        while ttc.len() < dir_off as usize {
            ttc.push(0);
        }
        ttc.append(&mut inner);
        assert_eq!(os2_weight_class(&ttc), Some(350));
    }

    #[test]
    fn slots_fill_by_nearest_weight_and_stay_distinct() {
        // Win10-era naming, deliberately out of weight order
        let faces = [
            ("segoeuil.ttf".to_string(), 300u16),
            ("segoeui.ttf".to_string(), 400),
            ("segoeuisl.ttf".to_string(), 350),
            ("segoeuib.ttf".to_string(), 700),
            ("segoeuiz.ttf".to_string(), 600),
        ];
        let p = pick_faces(&faces);
        assert_eq!(p.regular, Some(1)); // 400 wins over 350 for Regular
        assert_eq!(p.semibold, Some(4)); // 600 wins over 700 for Semibold
        assert_eq!(p.light, Some(0));
        assert_ne!(p.regular, p.semibold);
    }

    #[test]
    fn bold_only_leaves_regular_empty_instead_of_guessing() {
        let faces = [("segoeuib.ttf".to_string(), 700u16), ("x.ttf".to_string(), 900)];
        let p = pick_faces(&faces);
        assert_eq!(p.regular, None); // 700 is outside the 350..=500 band
        assert_eq!(p.semibold, Some(0));
        assert_eq!(p.light, None);
    }

    #[test]
    fn palette_matches_the_plans_tokens() {
        let v = groove_visuals();
        assert_eq!(BG, egui::Color32::from_rgb(0, 0, 0));
        assert_eq!(SIDEBAR, egui::Color32::from_rgb(0x2b, 0x2b, 0x2b));
        assert_eq!(ALT_ROW, egui::Color32::from_rgb(0x19, 0x19, 0x19));
        assert_eq!(HOVER, egui::Color32::from_rgb(0x30, 0x30, 0x30));
        assert_eq!(ACCENT, egui::Color32::from_rgb(0x00, 0x78, 0xd4));
        assert_eq!(LINK, egui::Color32::from_rgb(0x4c, 0xc2, 0xff));
        assert!(v.dark_mode);
        assert_eq!(v.panel_fill, BG);
        assert_eq!(v.faint_bg_color, ALT_ROW);
        assert_eq!(v.widgets.hovered.weak_bg_fill, HOVER);
        assert_eq!(v.selection.bg_fill, ACCENT);
        assert_eq!(v.hyperlink_color, LINK);
        assert_eq!(v.window_fill, SIDEBAR);
        assert_eq!(
            v.weak_text_color,
            Some(egui::Color32::from_rgb(0x9a, 0x9a, 0x9a))
        );
    }

    #[test]
    fn tile_grid_fits_whole_tiles_and_never_zero_columns() {
        assert_eq!(tile_cols(1200.0, 140.0, 12.0), 7); // 7×140 + 6×12 = 1052 ≤ 1200
        assert_eq!(tile_cols(300.0, 140.0, 12.0), 2);
        assert_eq!(tile_cols(10.0, 140.0, 12.0), 1); // one overflowing column
        assert_eq!(tile_cols(0.0, 140.0, 12.0), 1);
    }

    #[test]
    fn line_offsets_and_viewport_range_agree() {
        // letter header (30) then two tile rows (170) then one more header
        let h = [30.0f32, 170.0, 170.0, 30.0];
        let (tops, total) = line_tops(&h);
        assert_eq!(tops, vec![0.0, 30.0, 200.0, 370.0]);
        assert_eq!(total, 400.0);
        // viewport shows content y 190..340 → lines 1 (30..200) and 2 (200..370)
        assert_eq!(visible_lines(&tops, 190.0, 340.0, total), 1..3);
        assert_eq!(visible_lines(&tops, 0.0, 340.0, total), 0..3);
        assert_eq!(visible_lines(&tops, 380.0, 400.0, total), 3..4);
        assert_eq!(visible_lines(&[], 0.0, 10.0, 0.0), 0..0);
    }

    /// Real-machine check (this build runs on Windows): the candidate probe
    /// finds a true 400-weight Regular and a ≥540 Semibold/Bold face.
    #[test]
    fn windows_segoe_probe_classifies_the_files_that_exist() {
        let dir = Path::new(SEGOE_DIR);
        if !dir.is_dir() {
            return;
        }
        let mut faces: Vec<(String, u16)> = Vec::new();
        let mut why: Vec<String> = Vec::new();
        for name in SEGOE_CANDIDATES {
            let path = dir.join(name);
            match read_head(&path, 128 * 1024) {
                None => why.push(format!("{name}: no bytes")),
                Some(head) => match os2_weight_class(&head) {
                    Some(w) => faces.push((name.to_string(), w)),
                    None => {
                        // what the parser actually saw, so a real-machine
                        // failure reads as data instead of a bare assertion
                        why.push(format!(
                            "{name}: {} probe bytes · sig {:?} · nt={} · OS/2 unreadable",
                            head.len(),
                            &head[..head.len().min(4)],
                            be_u16(&head, 4).unwrap_or(0),
                        ));
                    }
                },
            }
        }
        assert!(
            !faces.is_empty(),
            "{SEGOE_DIR} exists but no candidate parsed · {why:?}"
        );
        let p = pick_faces(&faces);
        assert!(p.regular.is_some(), "no 350..=500 weight face among {faces:?}");
        assert!(
            p.semibold.is_some(),
            "no 540..=700 weight face among {faces:?}"
        );
        assert!(p.light.is_some(), "no 250..=350 weight face among {faces:?}");
    }
}
