//! eframe App shell — M4 Shell.
//!
//! Rule of this file: the UI thread never blocks and never locks. Audio lives
//! behind `Cmd`/`Event`; the library scan lives behind `ScanCmd`/`ScanEvent`
//! (plan §contract). Everything the UI knows about playback is what the worker
//! reported, plus optimistic guesses that events correct within one frame.
//!
//! Every `egui` closure here *returns* its results (moved locals in, values
//! out) instead of capturing `&mut self`: the app is only ever borrowed
//! immutably inside a closure, and every mutation happens after the panel call
//! in `apply()`/`logic()`. That is what keeps a store-reading closure from
//! fighting the borrow checker.
//!
//! M3's library features (folder picker, drag-drop-to-scan, grouping, the
//! virtualized Songs list, search, sort, genre filter, hover row actions,
//! Shuffle-all) are all still here — what M4 adds is the shell around them.
//! M4 delivers the plan's Groove chrome: the 335 px nav rail (search box on
//! top, hamburger collapse, Settings pinned to the bottom, accent strip on the
//! active row), Segoe UI loaded at runtime plus the themed `Visuals` tokens
//! from `theme.rs`, the painted transport bar, the "Not finding everything?"
//! banner, per-page empty states, and the Artists/Albums circle-and-square
//! A-Z grids. Cover *textures* (lofty embedded art decoded in a worker feeding
//! a UI texture queue) still arrive in M5; until then every cover slot is the
//! painted placeholder glyph.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crossbeam_channel::{Receiver, Sender};
use eframe::egui;

use crate::audio::spawn_player;
use crate::glyphs;
use crate::model::{
    Cmd, Event, Group, Page, Query, RepeatMode, ScanCmd, ScanEvent, ShuffleMode, SortKey, Tab,
    TrackStore, tlog, fmt_mmss, is_audio, stem,
};
use crate::scan::spawn_scanner;
use crate::theme::{self, ACCENT, BG, HAIR, LINK, SIDEBAR};

/// Songs row pitch. `show_rows` virtualizes against one uniform height per row,
/// so each row is laid out to fill exactly this (see `song_row` cells).
const ROW_H: f32 = 34.0;
/// Column header height.
const HEAD_H: f32 = 26.0;
/// Nav rail widths (plan §UI: "nav rail fixed ~335px"). Collapsed keeps the
/// icons and Settings and drops the labels, like Groove's hamburger.
const NAV_W: f32 = 335.0;
const NAV_W_MIN: f32 = 68.0;
/// A–Z tile pitch: Artists are circles, Albums squares (plan §My music).
const TILE: f32 = 140.0;
const TILE_GAP: f32 = 12.0;
/// Height of an A–Z letter header row in the tile grid.
const LINE_H: f32 = 30.0;

/// Right-edge hover zone: play/pause, then the three-dot menu.
const W_GLYPH: f32 = 30.0;
const W_TIME: f32 = 58.0;
const W_YEAR: f32 = 44.0;
const W_GENRE: f32 = 130.0;
const W_ARTIST: f32 = 190.0;

/// One thing the UI wants to happen. Widgets never mutate the app inside an
/// egui closure — they return one of these and `apply()` does the work (and is
/// the only place a native file dialog is opened, so egui closures never block).
#[derive(Clone)]
enum Action {
    /// Play row `r`; the queue becomes exactly the view the user is looking at
    /// (Groove queues the visible list, so Next/Prev follow what is on screen).
    PlayRow { r: usize },
    /// Three-dot menu → "Play now": queue the whole library, start at this row.
    PlayTrack { lib: u32 },
    /// Three-dot menu → "Remove from library" (the file itself is untouched).
    Remove { lib: u32 },
    /// Artists/Albums row click: filter the Songs view to that artist/album.
    FilterTo { name: String },
    /// Drop the search + genre filters.
    ClearFilter,
    /// Search box contents changed (live filter).
    SetSearch(String),
    /// Genre dropdown changed (`None` = All music genres).
    SetGenre(Option<String>),
    /// Sort the view by a column header ("sort links" in the plan).
    SetSort(SortKey),
    /// "Open files…" quick-play (rfd here, then queue what it returned).
    PickFiles,
    AddFolder,
    Rescan,
    ShuffleAll,
    TogglePlay,
    NextTrack,
    PrevTrack,
    StopPlay,
    SeekRel(f32),
    SetVol(f32),
    ToggleShuffle,
    CycleRepeat,
    /// Nav rail / tab switch: which CentralPanel page to show.
    SetPage(Page),
    /// Hamburger: rail 335 px ↔ 68 px.
    ToggleNav,
    /// "Not finding everything?" banner ✕.
    DismissBanner,
    /// Banner link: show only the files lofty could not index.
    FilterUnindexed,
    /// A–Z rail in the tile grids: jump to a letter (`None` = show everything).
    SetLetter(Option<char>),
    /// Recent plays row: queue the library and start at this file (`None` when
    /// the file is no longer in the library — then it plays on its own).
    PlayAgain { path: PathBuf },
}

/// One played song, remembered for the Recent plays page. Rows keep the *path*
/// plus the unix second playback started ("5 minutes ago" is then plain
/// arithmetic) — paths, not library indices, because `TrackStore::remove`
/// shifts every index below what it deletes. Persisting this list is M8.
#[derive(Clone)]
struct Recent {
    path: PathBuf,
    at: i64,
}

pub struct GroveApp {
    cmd_tx: Sender<Cmd>,
    event_rx: Receiver<Event>,
    scan_tx: Sender<ScanCmd>,
    scan_rx: Receiver<ScanEvent>,

    store: TrackStore,
    query: Query,
    tab: Tab,

    // Derived views, cached on `TrackStore::revision()`: a 20 Hz repaint must
    // not re-filter, re-collect genres, or re-group on every frame.
    view: Vec<u32>,
    view_rev: u64,
    view_query: Query,
    genres: Vec<String>,
    genres_rev: u64,
    artist_groups: Vec<Group>,
    album_groups: Vec<Group>,
    groups_rev: u64,

    /// Folders we have been asked to watch (M7's re-scan uses these).
    roots: Vec<PathBuf>,

    // transport state (engine-reported; optimistic guesses corrected by events)
    playing: bool,
    elapsed: f32,
    duration: f32,
    volume: f32,
    shuffle: ShuffleMode,
    repeat: RepeatMode,
    /// Size of the queue we last sent, plus the engine's cursor into it.
    queue_len: usize,
    queue_paths: Vec<PathBuf>,
    queue_index: Option<usize>,
    /// File stem the engine last announced as playing.
    current_name: Option<String>,
    /// Library row of the playing track — resolved from the queue path when
    /// shuffle is off, from the announced stem when it is on.
    current_lib: Option<u32>,
    current_lib_key: Option<String>,

    status: String,
    scanning: bool,
    /// The drop we last acted on: `raw.dropped_files` survives frames, so this
    /// is what stops one drop from rescanning forever.
    last_drop: Vec<PathBuf>,
    /// Pointer position from last frame — a repaint is requested only when it
    /// actually moves, so hover-reveal stays live while idle CPU stays ~0.
    pointer_last: Option<egui::Pos2>,

    // ── M4 shell state ──────────────────────────────────────────────────────
    /// Which CentralPanel page the nav rail has selected.
    page: Page,
    nav_collapsed: bool,
    show_banner: bool,
    /// Letter jump inside the Artists/Albums grids. A *filter*, not a scroll:
    /// egui 0.37 made `ScrollArea::State` private and dropped `scroll_to_y`,
    /// so "jump to Q" means "show only Q" — deviation recorded in the build log.
    az_letter: Option<char>,
    /// 34 px page-title face (`Segoe UI Semibold` when that file probed as
    /// weight 540–650), handed back by `theme::apply`.
    title_font: egui::FontId,
    /// In-memory play history for the Recent page.
    recents: Vec<Recent>,
}

impl GroveApp {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded();
        let (ev_tx, ev_rx) = crossbeam_channel::unbounded();
        // cpal → worker internal fault lane (stream self-heal, see audio.rs)
        let (fault_tx, fault_rx) = crossbeam_channel::unbounded();
        spawn_player(cmd_rx, ev_tx, fault_rx, fault_tx);

        // Library scan lane: its own thread, parked on `recv` until asked.
        // Two channels, one per direction (the UI holds the command sender and
        // the event receiver, exactly like the audio lane).
        let (scan_cmd_tx, scan_cmd_rx) = crossbeam_channel::unbounded();
        let (scan_ev_tx, scan_ev_rx) = crossbeam_channel::unbounded();
        spawn_scanner(scan_cmd_rx, scan_ev_tx);

        // Automated verification hooks (same spirit as GROVE_IDLE_TEST):
        // GROVE_AUTOPLAY="a;b" queues + plays at startup, GROVE_SCAN="dir;dir"
        // scans those roots into the library — both before any click happens.
        let auto: Vec<PathBuf> = paths_from_env("GROVE_AUTOPLAY");
        if !auto.is_empty() {
            cmd_tx
                .send(Cmd::Load {
                    tracks: auto.clone(),
                    play_index: 0,
                })
                .ok();
        }
        let scan_roots = paths_from_env("GROVE_SCAN");
        // remembered because the struct literal below moves `scan_roots`
        let auto_scan_armed = !scan_roots.is_empty();
        if auto_scan_armed {
            scan_cmd_tx
                .send(ScanCmd::Scan {
                    roots: scan_roots.clone(),
                    replace: true,
                })
                .ok();
        }

        // Groove's dark look: palette + Segoe UI faces + text styles in one
        // call (plan §UI). Light/Dark/System come in M6, which is why M4 pins
        // Dark — and egui only applies visuals to the style of the *active*
        // theme, so `theme::apply` sets the theme and then edits that style.
        let title_font = theme::apply(&cc.egui_ctx);

        Self {
            cmd_tx,
            event_rx: ev_rx,
            scan_tx: scan_cmd_tx,
            scan_rx: scan_ev_rx,
            store: TrackStore::default(),
            query: Query::new(),
            tab: Tab::Songs,
            view: Vec::new(),
            view_rev: u64::MAX,
            view_query: Query::new(),
            genres: Vec::new(),
            genres_rev: u64::MAX,
            artist_groups: Vec::new(),
            album_groups: Vec::new(),
            groups_rev: u64::MAX,
            roots: scan_roots,
            playing: false,
            elapsed: 0.0,
            duration: 0.0,
            volume: 1.0,
            shuffle: ShuffleMode::Off,
            repeat: RepeatMode::None,
            queue_len: auto.len(),
            queue_paths: auto,
            queue_index: None,
            current_name: None,
            current_lib: None,
            current_lib_key: None,
            status: "Grove M4 · add a music folder or drop one on this window".to_string(),
            // the auto-scan hook just sent its command, so the UI starts
            // optimistic and lets `ScanEvent::Started` confirm it
            scanning: auto_scan_armed,
            last_drop: Vec::new(),
            pointer_last: None,
            page: Page::Library,
            nav_collapsed: false,
            show_banner: true,
            az_letter: None,
            title_font,
            recents: Vec::new(),
        }
    }

    fn send(&self, cmd: Cmd) {
        self.cmd_tx.send(cmd).ok();
    }

    fn scan(&mut self, roots: Vec<PathBuf>, replace: bool) {
        self.scan_tx.send(ScanCmd::Scan { roots, replace }).ok();
        // Mark it scanning on the spot: `logic()`'s repaint policy keeps asking
        // for ticks only while this flag is up, so the event stream gets
        // drained from the first batch to `Finished` instead of going to sleep
        // between two worker wake-ups.
        self.scanning = true;
    }

    /// Rebuild the derived views only when the library or the query moved.
    fn refresh_caches(&mut self) {
        let rev = self.store.revision();
        if self.view_rev != rev || self.view_query != self.query {
            let rows = self.store.rows_for(&self.query).to_vec();
            self.view_rev = self.store.revision();
            self.view_query = self.query.clone();
            self.view = rows;
        }
        if self.genres_rev != rev {
            self.genres = self.store.genres();
            self.genres_rev = rev;
        }
        if self.groups_rev != rev {
            self.artist_groups = self.store.artist_groups();
            self.album_groups = self.store.album_groups();
            self.groups_rev = rev;
        }
        self.resolve_current();
    }

    /// Which library row is playing. Shuffle off ⇒ the engine's cursor indexes
    /// the queue we sent, so the path is exact; shuffle on ⇒ match the announced
    /// stem, which is all the `NowPlaying` contract exposes.
    fn resolve_current(&mut self) {
        let key = if self.shuffle == ShuffleMode::Off {
            self.queue_index
                .and_then(|i| self.queue_paths.get(i))
                .map(|p| p.to_string_lossy().into_owned())
        } else {
            self.current_name.clone()
        };
        if key == self.current_lib_key {
            return;
        }
        let probe = key.clone();
        self.current_lib_key = key;
        self.current_lib = if self.shuffle == ShuffleMode::Off {
            probe.as_ref().map(PathBuf::from).and_then(|p| {
                self.store.index_of(&p).map(|i| i as u32)
            })
        } else {
            probe
                .as_ref()
                .and_then(|name| self.store.tracks.iter().position(|t| stem(&t.path) == *name))
                .map(|i| i as u32)
        };
    }

    /// Queue `paths` from `start` and guess the transport state; the engine's
    /// `NowPlaying` / `Position` events correct it within a frame.
    fn start_queue(&mut self, paths: Vec<PathBuf>, start: usize, status: impl Into<String>) {
        if paths.is_empty() {
            self.status = "nothing to play".to_string();
            return;
        }
        let start = start.min(paths.len() - 1);
        self.send(Cmd::Load {
            tracks: paths.clone(),
            play_index: start,
        });
        let name = paths.get(start).map(|p| stem(p));
        self.status = status.into();
        self.queue_len = paths.len();
        self.queue_paths = paths;
        self.queue_index = Some(start);
        self.current_name = name;
        self.elapsed = 0.0;
        self.duration = 0.0;
        self.playing = true;
        self.resolve_current();
        if let Some(i) = self.queue_index
            && let Some(p) = self.queue_paths.get(i)
        {
            let p = p.clone();
            self.note_recent(&p);
        }
    }

    /// Recent plays: remember what just started playing. The engine announces
    /// every track it begins (including gapless promotions and auto-advance),
    /// so this fires once per real change of song; replaying the same file
    /// within a session just refreshes its stamp instead of stacking rows.
    fn note_recent(&mut self, path: &Path) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        if let Some(last) = self.recents.last_mut()
            && last.path == path
        {
            last.at = now;
            return;
        }
        self.recents.push(Recent {
            path: path.to_path_buf(),
            at: now,
        });
        if self.recents.len() > 200 {
            self.recents.remove(0);
        }
    }

    fn apply(&mut self, act: Action) {
        match act {
            Action::PlayRow { r } => {
                let paths = self.store.paths_for(&self.view);
                self.start_queue(paths, r, "playing the current view");
            }
            Action::PlayTrack { lib } => {
                let i = lib.min(self.store.len().saturating_sub(1) as u32) as usize;
                self.start_queue(self.store.paths(), i, "playing from library");
            }
            Action::Remove { lib } => {
                let i = lib.min(self.store.len().saturating_sub(1) as u32) as usize;
                let path = self.store.tracks[i].path.clone();
                if self.store.remove(&path) {
                    self.status = format!("removed {} from library", stem(&path));
                }
            }
            Action::FilterTo { name } => {
                self.query.search = name;
                self.query.genre = None;
                self.tab = Tab::Songs;
                self.status = "filtered to selection".to_string();
            }
            Action::ClearFilter => {
                // every live filter the shell can set: search box, genre, the
                // banner's "could not index" view, and the A–Z letter filter —
                // so the banner link round-trips back to the whole library
                self.query.search.clear();
                self.query.genre = None;
                self.query.unindexed_only = false;
                self.az_letter = None;
                self.status = "filters cleared".to_string();
            }
            Action::SetSearch(s) => {
                self.query.search = s;
            }
            Action::SetGenre(g) => {
                self.query.genre = g;
            }
            Action::SetSort(k) => {
                self.query.sort = k;
                self.status = format!("sorted by {}", k.label());
            }
            Action::PickFiles => {
                let picked = rfd::FileDialog::new()
                    .set_title("Grove — pick music")
                    .add_filter("Audio", &crate::model::AUDIO_EXTS)
                    .pick_files();
                match picked {
                    Some(paths) => {
                        let audio: Vec<PathBuf> =
                            paths.into_iter().filter(|p| is_audio(p)).collect();
                        if audio.is_empty() {
                            self.status = "no audio files selected".to_string();
                        } else {
                            self.start_queue(audio, 0, "loaded from file picker");
                        }
                    }
                    None => self.status = "selection cancelled".to_string(),
                }
            }
            Action::AddFolder => {
                let picked = rfd::FileDialog::new()
                    .set_title("Grove — add a music folder")
                    .pick_folder();
                match picked {
                    Some(folder) => {
                        tlog!("add folder · {}", folder.display());
                        self.roots.push(folder.clone());
                        self.scan(vec![folder], false);
                    }
                    None => self.status = "folder pick cancelled".to_string(),
                }
            }
            Action::Rescan => {
                if self.roots.is_empty() {
                    self.status = "no folders added yet".to_string();
                } else {
                    let n = self.roots.len();
                    self.scan(self.roots.clone(), true);
                    self.status = format!("rescanning {n} folder(s)");
                }
            }
            Action::ShuffleAll => {
                let n = self.store.len();
                if n < 2 {
                    self.status = "not enough songs to shuffle".to_string();
                    return;
                }
                self.shuffle = ShuffleMode::On;
                self.send(Cmd::ShuffleMode(ShuffleMode::On));
                // Seeded random start (Groove's "Shuffle all"): no rand crate is
                // pinned, and a clock-seeded xorshift is plenty for a start index.
                let seed = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos() as u64)
                    .unwrap_or(0x9E37_79B9_85E6_A1A0)
                    | 1;
                let start = seeded_index(n, seed);
                self.start_queue(self.store.paths(), start, format!("shuffling all {n}"));
            }
            Action::TogglePlay => {
                let playing = self.playing;
                self.send(if playing { Cmd::Pause } else { Cmd::Play });
                self.playing = !playing; // optimistic; engine confirms via events
            }
            Action::NextTrack => {
                self.send(Cmd::Next);
                self.elapsed = 0.0;
                self.playing = true;
            }
            Action::PrevTrack => {
                self.send(Cmd::Prev);
                self.elapsed = 0.0;
                self.playing = true; // Prev always lands on a playing track
            }
            Action::StopPlay => {
                self.send(Cmd::Stop);
                self.playing = false;
            }
            Action::SeekRel(d) => {
                let t = (self.elapsed + d).max(0.0);
                self.send(Cmd::Seek(t));
                self.elapsed = t; // optimistic; the engine clamps against real duration
            }
            Action::SetVol(v) => {
                self.volume = v;
                self.send(Cmd::SetVolume(v));
            }
            Action::ToggleShuffle => {
                self.shuffle = if self.shuffle == ShuffleMode::On {
                    ShuffleMode::Off
                } else {
                    ShuffleMode::On
                };
                self.send(Cmd::ShuffleMode(self.shuffle));
                // shuffle On ⇒ the row highlight switches to stem matching
                self.current_lib_key = None;
                self.resolve_current();
            }
            Action::CycleRepeat => {
                self.repeat = match self.repeat {
                    RepeatMode::None => RepeatMode::All,
                    RepeatMode::All => RepeatMode::One,
                    RepeatMode::One => RepeatMode::None,
                };
                self.send(Cmd::RepeatMode(self.repeat));
            }
            Action::SetPage(p) => {
                self.page = p;
                // the letter-jump and the "cannot index" filter belong to My
                // music, so leaving the page clears them
                if p != Page::Library {
                    self.az_letter = None;
                }
            }
            Action::ToggleNav => {
                self.nav_collapsed = !self.nav_collapsed;
            }
            Action::DismissBanner => {
                self.show_banner = false;
                tlog!("banner dismissed · 'Not finding everything?' stays hidden");
            }
            Action::FilterUnindexed => {
                let on = !self.query.unindexed_only;
                self.query.unindexed_only = on;
                self.page = Page::Library;
                self.tab = Tab::Songs;
                self.status = if on {
                    "showing only the files the scan could not read".to_string()
                } else {
                    "showing the whole library".to_string()
                };
            }
            Action::SetLetter(c) => {
                self.az_letter = c;
                self.status = match c {
                    Some(ch) => format!("showing {ch} only"),
                    None => "showing every letter".to_string(),
                };
            }
            Action::PlayAgain { path } => {
                // Recent rows hold paths, so a rescan or a delete between
                // listens cannot make one of them point at the wrong song.
                match self.store.index_of(&path) {
                    Some(i) => self.start_queue(
                        self.store.paths(),
                        i,
                        "playing from recent plays",
                    ),
                    None => self.start_queue(
                        vec![path],
                        0,
                        "playing · that file is not in the library any more",
                    ),
                }
            }
        }
    }
}

impl eframe::App for GroveApp {
    /// Runs before each paint pass *and* on worker wake-ups: the one place both
    /// lanes are drained, so no Mutex and no blocking anywhere in the UI.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let mut busy = false;

        // ── audio lane ──────────────────────────────────────────────────────
        while let Ok(ev) = self.event_rx.try_recv() {
            busy = true;
            match ev {
                Event::Position {
                    elapsed,
                    duration,
                    playing,
                } => {
                    self.elapsed = elapsed;
                    self.playing = playing;
                    if duration > 0.0 {
                        self.duration = duration;
                    }
                }
                Event::NowPlaying { index, name } => {
                    self.queue_index = Some(index);
                    self.playing = true;
                    self.elapsed = 0.0;
                    self.current_name = Some(name);
                    self.resolve_current();
                    // engine-announced start (auto-advance, gapless promotion):
                    // the queue path at `index` is what actually began
                    if let Some(p) = self.queue_paths.get(index).cloned() {
                        self.note_recent(&p);
                    }
                    self.status = format!(
                        "now playing {}",
                        self.current_name.clone().unwrap_or_default()
                    );
                }
                Event::TrackEnded { index } => {
                    self.playing = false;
                    // optimistic; NowPlaying (next track) or Position (terminal
                    // stop) corrects within a frame — M5's queue column uses index
                    self.status = format!("track {index} ended");
                }
                Event::DeviceError { message } => {
                    self.status = message;
                }
                Event::Tick => {}
            }
        }

        // ── scan lane (M3): batches stream in, so the Songs list fills while
        // the walk is still running — Groove's indexing pass does the same ──
        while let Ok(ev) = self.scan_rx.try_recv() {
            busy = true;
            match ev {
                ScanEvent::Started { roots, replace } => {
                    self.scanning = true;
                    if replace {
                        self.store.clear();
                    }
                    self.status = format!(
                        "scanning {} folder(s) · {} tracks so far",
                        roots.len(),
                        self.store.len()
                    );
                }
                ScanEvent::Batch { tracks } => {
                    let stat = self.store.merge(tracks);
                    if stat.new > 0 || stat.updated > 0 {
                        self.status = format!(
                            "scanning · {} new · {} refreshed · {} rows",
                            stat.new,
                            stat.updated,
                            self.store.len()
                        );
                    }
                }
                ScanEvent::Finished {
                    roots,
                    inspected,
                    emitted,
                    unreadable,
                    ms,
                } => {
                    self.scanning = false;
                    self.status = format!(
                        "{} song(s) from {} folder(s) · {emitted} audio of {inspected} entries \
                         · {unreadable} unreadable · {ms} ms",
                        self.store.len(),
                        roots.len()
                    );
                    tlog!(
                        "library ready · {} rows · {} genres · {} artists · {} albums",
                        self.store.len(),
                        self.store.genres().len(),
                        self.store.artist_groups().len(),
                        self.store.album_groups().len()
                    );
                }
                ScanEvent::Error { message } => {
                    self.scanning = false;
                    self.status = message;
                }
            }
        }

        // ── winit drag & drop: dropped folders scan into the library; dropped
        // loose files also play, so the M1/M2 "drop to play" gesture survives ─
        let dropped: Vec<PathBuf> = ctx
            .input(|i| {
                i.raw
                    .dropped_files
                    .iter()
                    .map(|d| d.path().to_path_buf())
                    .collect::<Vec<PathBuf>>()
            })
            .into_iter()
            .filter(|p| !p.as_os_str().is_empty())
            .collect();
        if !dropped.is_empty() && dropped != self.last_drop {
            self.last_drop = dropped.clone();
            let all_files = dropped.iter().all(|p| p.is_file());
            self.scan(dropped.clone(), false);
            // Groove sends a drop straight to the library view
            self.page = Page::Library;
            self.tab = Tab::Songs;
            if all_files {
                let audio: Vec<PathBuf> = dropped.into_iter().filter(|p| is_audio(p)).collect();
                if !audio.is_empty() {
                    self.start_queue(audio, 0, "loaded from drag & drop");
                }
            }
        }

        // Repaint policy: worker events and playback drive repaints; the pointer
        // only forces one when it actually moves, so hover-reveal stays live
        // while an untouched window costs nothing.
        let pointer = ctx.input(|i| i.pointer.hover_pos());
        if busy || self.playing {
            ctx.request_repaint();
        }
        if self.playing {
            ctx.request_repaint_after(Duration::from_millis(45)); // ~20 Hz transport
        }
        if self.scanning {
            // ~5 Hz while indexing: batches stream in and `Finished` has to be
            // seen by this thread, or the library stays half-built
            ctx.request_repaint_after(Duration::from_millis(200));
        }
        if pointer.is_some() && pointer != self.pointer_last {
            ctx.request_repaint_after(Duration::from_millis(45));
        }
        self.pointer_last = pointer;
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.refresh_caches();

        // ── transport: added *first* so it spans the whole window ───────────
        // egui allocates panels in the order they are added, and a bottom panel
        // added after the rail would stop at the rail's inner edge. Groove's
        // transport runs edge to edge, underneath the sidebar, so it goes first.
        let transport = egui::Panel::bottom("transport")
            .show_separator_line(false)
            .frame(
                egui::Frame::new()
                    .fill(BG)
                    .inner_margin(egui::Margin::symmetric(14, 8)),
            )
            .show(ui, |ui| {
                let mut acts: Vec<Action> = Vec::new();
                transport_body(
                    self.playing,
                    self.shuffle,
                    self.repeat,
                    self.elapsed,
                    self.duration,
                    &self.volume,
                    self.queue_index,
                    self.queue_len,
                    self.current_lib,
                    &self.store,
                    &mut acts,
                    ui,
                );
                acts
            });
        for a in transport.inner {
            self.apply(a);
        }

        // ── nav rail: search box on top, pages, Settings pinned at the bottom ─
        let w = if self.nav_collapsed {
            NAV_W_MIN
        } else {
            NAV_W
        };
        let nav = egui::Panel::left("nav")
            .exact_size(w)
            .resizable(false)
            .show_separator_line(false)
            .frame(
                egui::Frame::new()
                    .fill(SIDEBAR)
                    .inner_margin(egui::Margin::symmetric(10, 10)),
            )
            .show(ui, |ui| {
                let mut acts: Vec<Action> = Vec::new();
                let search = nav_body(
                    w,
                    &self.status,
                    self.scanning,
                    &self.query,
                    self.page,
                    &self.recents,
                    &self.title_font,
                    &mut acts,
                    ui,
                );
                (search, acts)
            });
        let (nav_search, acts) = nav.inner;
        if nav_search != self.query.search {
            // the same live-filter path M3 used, so one keystroke = one
            // `Action` and the store's cache still does the work
            self.apply(Action::SetSearch(nav_search));
        }
        for a in acts {
            self.apply(a);
        }

        // ── center: the page the rail selected ──────────────────────────────
        let q_before = self.query.clone();
        let tab_before = self.tab;
        let genres = self.genres.clone();
        let artists = self.artist_groups.clone();
        let albums = self.album_groups.clone();
        let rows = self.view.clone();
        let recents = self.recents.clone();
        let n_all = self.store.len();
        let roots = self.roots.clone();
        let status_mid = self.status.clone();
        let center = egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(BG)
                    .inner_margin(egui::Margin::symmetric(16, 10)),
            )
            .show(ui, |ui| {
                let mut acts: Vec<Action> = Vec::new();
                let mut tab = tab_before;
                let mut q = q_before;
                match self.page {
                    Page::Library => library_page(
                        &self.store,
                        &mut q,
                        &mut tab,
                        &rows,
                        &artists,
                        &albums,
                        &genres,
                        self.current_lib,
                        self.show_banner,
                        self.az_letter,
                        &self.title_font,
                        &status_mid,
                        self.scanning,
                        n_all,
                        &mut acts,
                        ui,
                    ),
                    Page::Recent => recent_page(
                        &self.store,
                        &recents,
                        self.current_lib,
                        &self.title_font,
                        &self.status,
                        &mut acts,
                        ui,
                    ),
                    Page::NowPlaying => nowplaying_page(
                        &self.store,
                        self.current_lib,
                        self.playing,
                        self.elapsed,
                        self.duration,
                        self.queue_index,
                        self.queue_len,
                        self.shuffle,
                        self.repeat,
                        &self.queue_paths,
                        &self.title_font,
                        &mut acts,
                        ui,
                    ),
                    Page::Playlists => {
                        empty_state(
                            "Playlists",
                            "Playlists are the next milestone: create, rename and \
                             fill them, then Grove keeps them in its config.",
                            &self.title_font,
                            &mut acts,
                            ui,
                        );
                    }
                    Page::Settings => settings_page(
                        &roots,
                        &status_mid,
                        &self.title_font,
                        &mut acts,
                        ui,
                    ),
                }
                (q, tab, acts)
            });
        let (q, tab, acts) = center.inner;
        if q != self.query {
            self.query = q;
        }
        if tab != self.tab {
            self.tab = tab;
        }
        for a in acts {
            self.apply(a);
        }
    }
}

// ── nav rail ──────────────────────────────────────────────────────────────────

/// Groove's left rail (plan §nav): hamburger + wordmark, the search box on top,
/// the four My-music destinations, the status line, and Settings pinned to the
/// bottom. The active item gets a hover-colored band plus a 3 px accent strip
/// on its left edge. Returns the search box's text; `ui()` commits it as the
/// live filter, exactly as the old toolbar did.
#[allow(clippy::too_many_arguments)]
fn nav_body(
    w: f32,
    status: &str,
    scanning: bool,
    q: &Query,
    page: Page,
    recents: &[Recent],
    title: &egui::FontId,
    acts: &mut Vec<Action>,
    ui: &mut egui::Ui,
) -> String {
    let avail = ui.available_width().max(40.0);
    let wide = w > NAV_W_MIN;
    let ink = ui.visuals().clone();
    let strong = ink.widgets.inactive.fg_stroke.color;
    let dim = ink.weak_text_color.unwrap_or(strong);

    // ── hamburger + wordmark ────────────────────────────────────────────────
    let (_, head) = ui.allocate_space(egui::vec2(avail, 34.0));
    let ham = glyphs::button(
        ui,
        egui::Rect::from_min_size(head.min, egui::vec2(34.0, 34.0)),
        glyphs::Icon::Hamburger,
        201,
        true,
    );
    if ham.clicked() {
        acts.push(Action::ToggleNav);
    }
    ham.on_hover_text(if wide {
        "Collapse the navigation rail"
    } else {
        "Expand the navigation rail"
    });
    if wide {
        ui.painter().text(
            egui::Pos2::new(head.min.x + 42.0, head.center().y),
            egui::Align2::LEFT_CENTER,
            "Grove Music",
            egui::FontId::new(20.0, title.family.clone()),
            strong,
        );
    }
    ui.add_space(8.0);

    // ── search box (plan §nav: "Search box top") ────────────────────────────
    let (_, srow) = ui.allocate_space(egui::vec2(avail, 34.0));
    let mag = glyphs::button(
        ui,
        egui::Rect::from_min_size(srow.min, egui::vec2(34.0, 34.0)),
        glyphs::Icon::Search,
        202,
        true,
    );
    // `on_hover_text` consumes the `Response`, so read the click first
    let mag_click = mag.clicked();
    mag.on_hover_text("Search my music · Esc clears the filter");
    let mut search = q.search.clone();
    let field = if wide {
        let f_rect = egui::Rect::from_min_size(
            egui::Pos2::new(srow.min.x + 36.0, srow.min.y + 4.0),
            egui::vec2((avail - 40.0).max(90.0), 26.0),
        );
        Some(
            ui.scope_builder(egui::UiBuilder::new().max_rect(f_rect), |ui| {
                egui::TextEdit::singleline(&mut search)
                    .hint_text("Search my music")
                    .margin(egui::Margin::symmetric(6, 4))
                    .show(ui)
            })
            .inner,
        )
    } else {
        None
    };
    if let Some(f) = &field {
        // `TextEditOutput` is not a `Response`; the interaction is its
        // `response` field (which derefs to one).
        let fr = &f.response;
        if fr.secondary_clicked() {
            search.clear();
        }
        if fr.clicked() && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            search.clear();
        }
        if mag_click {
            fr.request_focus();
        }
    } else if mag_click {
        // collapsed: tapping the lens brings the rail back out
        acts.push(Action::ToggleNav);
    }

    // ── destinations ────────────────────────────────────────────────────────
    ui.add_space(8.0);
    let items: [(Page, glyphs::Icon, String); 4] = [
        (Page::Library, glyphs::Icon::Note, "My music".to_string()),
        (
            Page::Recent,
            glyphs::Icon::Clock,
            if recents.is_empty() {
                "Recent plays".to_string()
            } else {
                format!("Recent plays ({})", recents.len())
            },
        ),
        (
            Page::NowPlaying,
            glyphs::Icon::Frame,
            "Now playing".to_string(),
        ),
        (Page::Playlists, glyphs::Icon::List, "Playlists".to_string()),
    ];
    for (i, (p, icon, label)) in items.into_iter().enumerate() {
        let (_, row) = ui.allocate_space(egui::vec2(avail, 34.0));
        nav_row(ui, row, icon, &label, page == p, wide, i as u64, acts, p);
    }

    // ── status line, then Settings pinned to the bottom ─────────────────────
    // egui flows top-down, so the pin is arithmetic: spend exactly the height
    // the footer needs and hand the rest of the rail to `add_space`.
    let foot = 34.0 + 34.0 + 22.0;
    let gap = (ui.available_height() - foot).max(0.0);
    ui.add_space(gap);
    let line_y = ui.cursor().min.y;
    ui.painter().line_segment(
        [
            egui::Pos2::new(ui.cursor().min.x, line_y),
            egui::Pos2::new(ui.cursor().min.x + avail, line_y),
        ],
        egui::Stroke::new(1.0, HAIR),
    );
    let (_, st) = ui.allocate_space(egui::vec2(avail, 22.0));
    ui.put(
        st,
        egui::Label::new(
            egui::RichText::new(if scanning {
                format!("{status} · indexing")
            } else {
                status.to_string()
            })
            .size(12.0)
            .color(dim),
        )
        .truncate()
        .selectable(false),
    );
    let (_, srow2) = ui.allocate_space(egui::vec2(avail, 34.0));
    nav_row(
        ui,
        srow2,
        glyphs::Icon::Gear,
        "Settings",
        page == Page::Settings,
        wide,
        900,
        acts,
        Page::Settings,
    );
    search
}

/// One rail entry: painted hover/active band, accent strip, painted glyph and
/// label. A plain `interact` covers the whole row, so the label never needs
/// its own hit-testing.
fn nav_row(
    ui: &mut egui::Ui,
    row: egui::Rect,
    icon: glyphs::Icon,
    label: &str,
    active: bool,
    wide: bool,
    salt: u64,
    acts: &mut Vec<Action>,
    page: Page,
) {
    let ink = ui.visuals().clone();
    let strong = ink.widgets.inactive.fg_stroke.color;
    let dim = ink.weak_text_color.unwrap_or(strong);
    let resp = ui.interact(
        row,
        ui.scope_id().with(salt).with("nav-item"),
        egui::Sense::click(),
    );
    if active {
        ui.painter()
            .rect_filled(row, 0.0, ink.widgets.hovered.weak_bg_fill);
        // Groove marks the selected destination with a strip at the left edge
        ui.painter().rect_filled(
            egui::Rect::from_min_size(row.min, egui::vec2(3.0, row.height())),
            0.0,
            ACCENT,
        );
    } else if resp.hovered() {
        ui.painter()
            .rect_filled(row, 0.0, ink.widgets.hovered.bg_fill.gamma_multiply(0.45));
    }
    if resp.clicked() && !active {
        acts.push(Action::SetPage(page));
    }
    let icon_rect = if wide {
        egui::Rect::from_min_size(row.min, egui::vec2(34.0, row.height()))
    } else {
        row
    };
    let lit = active || resp.hovered();
    glyphs::paint(
        ui,
        icon_rect,
        icon,
        if lit { strong } else { dim },
    );
    if wide {
        ui.painter().text(
            egui::Pos2::new(row.min.x + 42.0, row.center().y),
            egui::Align2::LEFT_CENTER,
            label,
            egui::FontId::proportional(14.0),
            if lit { strong } else { dim },
        );
    } else {
        resp.on_hover_text(label);
    }
}

// ── My music page ─────────────────────────────────────────────────────────────

/// Everything the Songs/Artists/Albums tabs share: the page title, the tabs
/// (active one underlined), the blue-link toolbar and the
/// "Not finding everything?" banner, then the tab's own body.
#[allow(clippy::too_many_arguments)]
fn library_page(
    store: &TrackStore,
    q: &mut Query,
    tab: &mut Tab,
    view: &[u32],
    artists: &[Group],
    albums: &[Group],
    genres: &[String],
    current: Option<u32>,
    banner: bool,
    letter: Option<char>,
    title: &egui::FontId,
    status: &str,
    scanning: bool,
    n_all: usize,
    acts: &mut Vec<Action>,
    ui: &mut egui::Ui,
) {
    my_music_header(
        genres,
        n_all,
        view.len(),
        scanning,
        status,
        q,
        tab,
        banner,
        title,
        n_all > 0,
        acts,
        ui,
    );
    match *tab {
        Tab::Songs => songs_page(store, q, view, current, status, scanning, acts, ui),
        Tab::Artists => group_page("Artists", false, artists, letter, title, acts, ui),
        Tab::Albums => group_page("Albums", true, albums, letter, title, acts, ui),
    }
}

/// A Groove-blue toolbar link: text only, clickable, no button chrome.
fn link(text: impl Into<egui::RichText>) -> egui::Label {
    egui::Label::new(text.into().color(LINK).size(13.5))
        .selectable(false)
        .sense(egui::Sense::click())
}

#[allow(clippy::too_many_arguments)]
fn my_music_header(
    genres: &[String],
    n_all: usize,
    n_view: usize,
    scanning: bool,
    status: &str,
    q: &mut Query,
    tab: &mut Tab,
    banner: bool,
    title: &egui::FontId,
    has_library: bool,
    acts: &mut Vec<Action>,
    ui: &mut egui::Ui,
) {
    let ink = ui.visuals().clone();
    let strong = ink.widgets.inactive.fg_stroke.color;
    let dim = ink.weak_text_color.unwrap_or(strong);

    // ── page title (32-36 px semibold, plan §UI) ────────────────────────────
    let full = ui.available_width();
    let (_, trow) = ui.allocate_space(egui::vec2(full, 44.0));
    ui.painter().text(
        egui::Pos2::new(trow.min.x, trow.center().y),
        egui::Align2::LEFT_CENTER,
        "My music",
        title.clone(),
        strong,
    );
    if scanning {
        ui.put(
            egui::Rect::from_min_size(
                egui::Pos2::new(trow.max.x - 320.0, trow.min.y + 10.0),
                egui::vec2(320.0, 24.0),
            ),
            egui::Label::new(egui::RichText::new(format!("{status} · {n_all} so far")).size(12.0).color(dim))
                .truncate()
                .selectable(false)
                .halign(egui::Align::Max),
        );
    }

    // ── tabs: Songs / Artists / Albums, active one underlined ───────────────
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 18.0;
        for (t, label) in [
            (Tab::Songs, "Songs"),
            (Tab::Artists, "Artists"),
            (Tab::Albums, "Albums"),
        ] {
            let rich = egui::RichText::new(label)
                .size(16.0)
                .color(if *tab == t { strong } else { dim });
            let resp = ui.add(
                egui::Label::new(rich)
                    .selectable(false)
                    .sense(egui::Sense::click()),
            );
            if *tab == t {
                ui.painter().rect_filled(
                    egui::Rect::from_min_size(
                        egui::Pos2::new(resp.rect.min.x, resp.rect.max.y - 2.0),
                        egui::vec2(resp.rect.width(), 2.0),
                    ),
                    0.0,
                    ACCENT,
                );
            }
            if resp.clicked() && *tab != t {
                *tab = t;
            }
        }
    });
    ui.add_space(2.0);

    // ── toolbar (`Shuffle all (N) · Sort by · Genre:`, plan §My music) ──────
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 16.0;
        if !scanning && n_all > 1 {
            let r = ui.add(link(format!("Shuffle all ({n_all})")));
            if r.clicked() {
                acts.push(Action::ShuffleAll);
            }
            r.on_hover_text("Shuffle the whole library");
        }
        let cur = q.sort;
        let r = ui.add(link(format!("Sort by: {}", cur.label())));
        if let Some(p) = egui::Popup::menu(&r).show(|ui| {
            let mut pick = None;
            for k in SortKey::CYCLE {
                if ui.selectable_label(k == cur, k.label()).clicked() {
                    pick = Some(k);
                    ui.close();
                }
            }
            pick
        }) {
            if let Some(k) = p.inner {
                acts.push(Action::SetSort(k));
            }
        }
        let shown = q
            .genre
            .clone()
            .unwrap_or_else(|| "All music genres".to_string());
        let cur_genre = q.genre.clone();
        let r = ui.add(link(format!("Genre: {shown}")));
        if let Some(p) = egui::Popup::menu(&r).show(|ui| {
            let mut pick: Option<Option<String>> = None;
            if ui
                .selectable_label(cur_genre.is_none(), "All music genres")
                .clicked()
            {
                pick = Some(None);
            }
            for g in genres {
                let on = cur_genre.as_deref() == Some(g.as_str());
                if ui.selectable_label(on, g).clicked() {
                    pick = Some(Some(g.clone()));
                }
            }
            pick
        }) {
            if let Some(g) = p.inner {
                acts.push(Action::SetGenre(g));
            }
        }
        if !q.search.is_empty()
            || q.genre.is_some()
            || q.unindexed_only
        {
            let r = ui.add(link(if q.unindexed_only {
                "show the whole library"
            } else {
                "Clear filter"
            }));
            if r.clicked() {
                acts.push(Action::ClearFilter);
            }
        }
        ui.label(format!("{n_view} of {n_all} songs"));
    });

    // ── "Not finding everything?" banner (+ ✕ dismiss) ──────────────────────
    if banner && has_library {
        let (_, row) = ui.allocate_space(egui::vec2(ui.available_width(), 44.0));
        ui.painter()
            .rect_filled(row, 0.0, ink.faint_bg_color.gamma_multiply(0.6));
        ui.painter().rect_stroke(
            row,
            0.0,
            egui::Stroke::new(1.0, HAIR),
            egui::StrokeKind::Middle,
        );
        ui.painter().text(
            egui::Pos2::new(row.min.x + 10.0, row.center().y - 8.0),
            egui::Align2::LEFT_CENTER,
            "Not finding everything?",
            egui::FontId::proportional(14.0),
            strong,
        );
        ui.painter().text(
            egui::Pos2::new(row.min.x + 10.0, row.center().y + 9.0),
            egui::Align2::LEFT_CENTER,
            "Grove only lists the files it can read · click here to see the ones it could not index",
            egui::FontId::proportional(12.5),
            dim,
        );
        let body = egui::Rect::from_min_max(
            row.min,
            egui::Pos2::new(row.max.x - 34.0, row.max.y),
        );
        let resp = ui.interact(
            body,
            ui.scope_id().with("not-finding-everything"),
            egui::Sense::click(),
        );
        if resp.clicked() {
            acts.push(Action::FilterUnindexed);
        }
        resp.on_hover_text("show only the files the scan could not read");
        let close = glyphs::button(
            ui,
            egui::Rect::from_min_size(
                egui::Pos2::new(row.max.x - 30.0, row.center().y - 14.0),
                egui::vec2(28.0, 28.0),
            ),
            glyphs::Icon::Cross,
            77,
            true,
        );
        let close_click = close.clicked();
        close.on_hover_text("dismiss this banner");
        if close_click {
            acts.push(Action::DismissBanner);
        }
    }
    ui.add_space(6.0);
}

// ── transport bar ────────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn transport_body(
    playing: bool,
    shuffle: ShuffleMode,
    repeat: RepeatMode,
    elapsed: f32,
    duration: f32,
    volume: &f32,
    queue_index: Option<usize>,
    queue_len: usize,
    current_lib: Option<u32>,
    store: &TrackStore,
    acts: &mut Vec<Action>,
    ui: &mut egui::Ui,
) {
    // Groove's transport: `shuffle · prev · play/pause · next · repeat` dead
    // centre, `now playing` text on the left, volume + `⋯` on the right, and a
    // thin progress line under the controls with `mm:ss / mm:ss` on each end.
    let now = current_lib
        .and_then(|i| store.tracks.get(i as usize))
        .map(|t| (t.title.clone(), t.artist_or_unknown().to_string()))
        .unwrap_or_else(|| ("nothing playing".to_string(), String::new()));
    let full = ui.available_width();
    let ctl_h = 44.0;
    let bar_h = 20.0;

    // ── row 1: controls ─────────────────────────────────────────────────────
    // `allocate_space` reserves the band and hands back `(id, rect)`; every
    // widget inside it is placed at a computed rect, which keeps the row
    // symmetric without guessing egui's item spacing.
    let (_, r1) = ui.allocate_space(egui::vec2(full, ctl_h));
    let cx = r1.center().x;
    // Every centre button is the same width and the two side clusters are the
    // same width, so the transport is symmetric no matter how wide the window
    // gets (`side` is what the text/volume clusters share).
    let btn = 44.0;
    let side = (full * 0.36).min(380.0).max(120.0);
    let order: [(glyphs::Icon, u64, bool, Action); 5] = [
        (glyphs::Icon::Shuffle, 1, true, Action::ToggleShuffle),
        (glyphs::Icon::Prev, 2, true, Action::PrevTrack),
        (
            if playing {
                glyphs::Icon::Pause
            } else {
                glyphs::Icon::Play
            },
            3,
            true,
            Action::TogglePlay,
        ),
        (glyphs::Icon::Next, 4, queue_len > 0, Action::NextTrack),
        (
            match repeat {
                RepeatMode::One => glyphs::Icon::RepeatOne,
                _ => glyphs::Icon::Repeat,
            },
            5,
            true,
            Action::CycleRepeat,
        ),
    ];
    // shuffle / playing / repeat get the accent underline when they are on
    let lit = [
        shuffle == ShuffleMode::On,
        false,
        playing,
        false,
        repeat != RepeatMode::None,
    ];
    let cluster = order.len() as f32 * btn;
    for (i, (icon, salt, enabled, act)) in order.into_iter().enumerate() {
        let rect = egui::Rect::from_min_size(
            egui::Pos2::new(cx - cluster * 0.5 + i as f32 * btn, r1.min.y),
            egui::vec2(btn, ctl_h),
        );
        let resp = glyphs::button(ui, rect, icon, salt, enabled);
        if resp.clicked() {
            acts.push(act);
        }
        // Groove marks shuffle/repeat/playing with the accent, not just a hover
        // wash: a thin accent underline under the lit button.
        if lit[i] {
            ui.painter().rect_filled(
                egui::Rect::from_min_size(
                    egui::Pos2::new(rect.min.x + 6.0, rect.max.y - 3.0),
                    egui::vec2(rect.width() - 12.0, 2.0),
                ),
                0.0,
                ACCENT,
            );
        }
        match i {
            0 => {
                resp.on_hover_text(if shuffle == ShuffleMode::On {
                    "Shuffle on · the queue plays in a random order"
                } else {
                    "Shuffle off · play the queue in order"
                });
            }
            4 => {
                resp.on_hover_text(match repeat {
                    RepeatMode::None => "Repeat off",
                    RepeatMode::All => "Repeat all · loop the queue",
                    RepeatMode::One => "Repeat one · loop this song",
                });
            }
            _ => {}
        }
    }
    // left cluster: what is playing (same width as the right cluster, so the
    // centre buttons stay dead centre at any window width)
    let ink = ui.visuals().clone();
    let glyph_ink = ink.widgets.inactive.fg_stroke.color;
    ui.painter().text(
        egui::Pos2::new(r1.min.x + 2.0, r1.center().y - 7.0),
        egui::Align2::LEFT_CENTER,
        now.0.as_str(),
        egui::FontId::proportional(13.5),
        glyph_ink,
    );
    ui.painter().text(
        egui::Pos2::new(r1.min.x + 2.0, r1.center().y + 9.0),
        egui::Align2::LEFT_CENTER,
        format!(
            "{}{}",
            now.1,
            match queue_index {
                Some(i) if queue_len > 0 => format!(" · {} of {queue_len}", i + 1),
                _ => String::new(),
            }
        ),
        egui::FontId::proportional(12.0),
        ink.weak_text_color.unwrap_or(glyph_ink),
    );
    // right cluster: volume icon + slider + `⋯`
    let vr = egui::Rect::from_min_size(
        egui::Pos2::new(r1.max.x - side, r1.min.y),
        egui::vec2(side, ctl_h),
    );
    let vol_btn_w = 34.0;
    let menu_btn_w = 34.0;
    let slider_w = (side - vol_btn_w - menu_btn_w - 8.0).max(60.0);
    let mute = glyphs::button(
        ui,
        egui::Rect::from_min_size(
            egui::Pos2::new(vr.min.x, vr.min.y),
            egui::vec2(vol_btn_w, ctl_h),
        ),
        glyphs::Icon::Volume,
        91,
        true,
    );
    if mute.clicked() {
        // clicking the speaker mutes / unmutes (Groove's speaker icon does the
        // same); the slider underneath stays the precise control
        acts.push(Action::SetVol(if *volume > 0.0 { 0.0 } else { 1.0 }));
    }
    let mut vol = *volume;
    let s_rect = egui::Rect::from_min_size(
        egui::Pos2::new(vr.min.x + vol_btn_w + 4.0, vr.min.y + 16.0),
        egui::vec2(slider_w, 12.0),
    );
    let s_resp = ui
        .scope_builder(egui::UiBuilder::new().max_rect(s_rect), |ui| {
            // A `Slider` sizes itself from `spacing.slider_width` (250 pt by
            // default), so pinning that inside the scope is what makes the
            // volume rail fill exactly this cluster.
            ui.spacing_mut().slider_width = slider_w;
            ui.add(egui::Slider::new(&mut vol, 0.0..=1.0).show_value(false))
        })
        .inner;
    if s_resp.changed() {
        acts.push(Action::SetVol(vol));
    }
    let dots = glyphs::button(
        ui,
        egui::Rect::from_min_size(
            egui::Pos2::new(vr.max.x - menu_btn_w, vr.min.y),
            egui::vec2(menu_btn_w, ctl_h),
        ),
        glyphs::Icon::DotsH,
        92,
        true,
    );
    // M5's mini/expanded toggle lives in this frame's cover icon; until the
    // cover textures decode the `⋯` menu carries the transport extras that
    // M1/M2 verified against (stop, ±10 s).
    if let Some(p) = egui::Popup::menu(&dots).show(|ui| {
        let mut act = None;
        if ui.button("Stop").clicked() {
            act = Some(Action::StopPlay);
            ui.close();
        }
        if ui.button("Back 10 seconds").clicked() {
            act = Some(Action::SeekRel(-10.0));
            ui.close();
        }
        if ui.button("Forward 10 seconds").clicked() {
            act = Some(Action::SeekRel(10.0));
            ui.close();
        }
        if ui.button("Now playing").clicked() {
            act = Some(Action::SetPage(Page::NowPlaying));
            ui.close();
        }
        act
    }) {
        if let Some(a) = p.inner {
            acts.push(a);
        }
    }

    // ── row 2: thin progress line ───────────────────────────────────────────
    let (_, r2) = ui.allocate_space(egui::vec2(full, bar_h));
    let txt = 46.0;
    let rail = egui::Rect::from_min_max(
        egui::Pos2::new(r2.min.x + txt + 6.0, r2.center().y - 2.0),
        egui::Pos2::new(r2.max.x - txt - 6.0, r2.center().y + 2.0),
    );
    let frac = if duration > 0.0 {
        (elapsed / duration).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let played = egui::Rect::from_min_max(
        rail.min,
        egui::Pos2::new(rail.min.x + rail.width() * frac, rail.max.y),
    );
    let dot = egui::Pos2::new(rail.min.x + rail.width() * frac, rail.center().y);
    ui.painter().rect_filled(rail, 2.0, ink.faint_bg_color);
    ui.painter().rect_filled(played, 2.0, if playing { ACCENT } else { LINK });
    ui.painter().circle_filled(
        dot,
        5.0,
        if playing {
            ACCENT
        } else {
            ink.widgets.inactive.fg_stroke.color
        },
    );
    ui.painter().text(
        egui::Pos2::new(r2.min.x, r2.center().y),
        egui::Align2::LEFT_CENTER,
        fmt_mmss(elapsed),
        egui::FontId::proportional(12.0),
        ink.widgets.inactive.fg_stroke.color,
    );
    ui.painter().text(
        egui::Pos2::new(r2.max.x, r2.center().y),
        egui::Align2::RIGHT_CENTER,
        if duration > 0.0 {
            fmt_mmss(duration)
        } else {
            "--:--".to_string()
        },
        egui::FontId::proportional(12.0),
        ink.weak_text_color.unwrap_or(glyph_ink),
    );
    // Click or drag the thin line to seek: `drag_stopped` gives one honest
    // target instead of one `Cmd::Seek` per pixel.
    let seek = ui.interact(
        egui::Rect::from_min_size(
            egui::Pos2::new(rail.min.x, r2.min.y),
            egui::vec2(rail.width(), bar_h),
        ),
        ui.scope_id().with("transport-seek"),
        egui::Sense::drag(),
    );
    if (seek.drag_stopped() || seek.clicked())
        && let Some(x) = seek.hover_pos()
    {
        let f = ((x.x - rail.min.x) / rail.width()).clamp(0.0, 1.0);
        if duration > 0.0 {
            acts.push(Action::SeekRel(f * duration - elapsed));
        }
    }
    seek.on_hover_text("Click or drag to move the position");
}

// ── Recent / Now playing / Playlists / Settings ──────────────────────────────

/// Groove's "nothing here yet" page: big semibold heading, two lines of plain
/// explanation, and whatever entry point actually works on that page.
fn empty_state(
    head: &str,
    body: &str,
    title: &egui::FontId,
    _acts: &mut Vec<Action>,
    ui: &mut egui::Ui,
) {
    let ink = ui.visuals().clone();
    let (_, row) = ui.allocate_space(egui::vec2(ui.available_width(), 44.0));
    ui.painter().text(
        egui::Pos2::new(row.min.x, row.center().y),
        egui::Align2::LEFT_CENTER,
        head,
        title.clone(),
        ink.widgets.inactive.fg_stroke.color,
    );
    ui.add_space(4.0);
    ui.label(
        egui::RichText::new(body)
            .size(13.5)
            .color(ink.weak_text_color.unwrap_or(egui::Color32::GRAY)),
    );
}

fn recent_page(
    store: &TrackStore,
    recents: &[Recent],
    current: Option<u32>,
    title: &egui::FontId,
    _status: &str,
    acts: &mut Vec<Action>,
    ui: &mut egui::Ui,
) {
    let ink = ui.visuals().clone();
    let strong = ink.widgets.inactive.fg_stroke.color;
    let dim = ink.weak_text_color.unwrap_or(strong);
    let (_, row) = ui.allocate_space(egui::vec2(ui.available_width(), 44.0));
    ui.painter().text(
        egui::Pos2::new(row.min.x, row.center().y),
        egui::Align2::LEFT_CENTER,
        "Recent plays",
        title.clone(),
        strong,
    );
    if recents.is_empty() {
        ui.add_space(8.0);
        ui.label(
            egui::RichText::new(
                "Grove remembers the last 200 songs it starts, newest first. \
                 Nothing has played yet.",
            )
            .size(13.5)
            .color(dim),
        );
        let r = ui.add(link("go to My music"));
        if r.clicked() {
            acts.push(Action::SetPage(Page::Library));
        }
        return;
    }
    ui.label(format!(
        "{} recent · clicking a row queues the library and starts there",
        recents.len()
    ));
    ui.add_space(2.0);

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    ui.scope(|ui| {
        ui.spacing_mut().item_spacing.y = 0.0;
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show_rows(ui, ROW_H, recents.len(), |ui, range| {
                let top = ui.cursor().min.y;
                let left = ui.cursor().min.x;
                let full = ui.available_width();
                let here = current_path(store, current);
                for (k, r) in range.into_iter().enumerate() {
                    // newest first: row 0 is the end of the log
                    let item = &recents[recents.len() - 1 - r];
                    let found = store.find_by_path(&item.path);
                    let name = found
                        .map(|t| t.title.clone())
                        .unwrap_or_else(|| stem(&item.path));
                    let artist = found
                        .map(|t| t.artist_or_unknown().to_string())
                        .unwrap_or_default();
                    let y = top + k as f32 * ROW_H;
                    let row = egui::Rect::from_min_size(
                        egui::Pos2::new(left, y),
                        egui::vec2(full, ROW_H),
                    );
                    // the hit-zone first, so the background can paint from the
                    // same-frame hover state (the Songs list does the same)
                    let resp = ui.interact(
                        row,
                        ui.scope_id().with(r).with("recent-row"),
                        egui::Sense::click(),
                    );
                    let playing_now = found.map(|t| t.path == here).unwrap_or(false);
                    row_background(row, playing_now, resp.hovered(), ui);
                    if resp.clicked() {
                        acts.push(Action::PlayAgain {
                            path: item.path.clone(),
                        });
                    }
                    ui.painter().text(
                        egui::Pos2::new(row.min.x + 8.0, row.center().y),
                        egui::Align2::LEFT_CENTER,
                        name,
                        egui::FontId::proportional(13.5),
                        strong,
                    );
                    ui.painter().text(
                        egui::Pos2::new(row.min.x + W_ARTIST + 8.0, row.center().y),
                        egui::Align2::LEFT_CENTER,
                        artist,
                        egui::FontId::proportional(13.5),
                        dim,
                    );
                    ui.painter().text(
                        egui::Pos2::new(row.max.x - 8.0, row.center().y),
                        egui::Align2::RIGHT_CENTER,
                        ago_text(now - item.at),
                        egui::FontId::proportional(12.5),
                        dim,
                    );
                    resp.on_hover_text(format!("play {} again", item.path.display()));
                }
            });
    });
}

/// The path of the currently playing library row, for matching recent rows
/// against it without comparing indices that a rescan could have shifted.
fn current_path(store: &TrackStore, current: Option<u32>) -> PathBuf {
    current
        .and_then(|i| store.tracks.get(i as usize))
        .map(|t| t.path.clone())
        .unwrap_or_default()
}

/// `12s ago`, `7m ago`, `3h ago` … the Recent page's right-hand column.
fn ago_text(secs: i64) -> String {
    let s = secs.max(0);
    if s < 60 {
        format!("{s}s ago")
    } else if s < 3600 {
        format!("{}m ago", s / 60)
    } else if s < 86_400 {
        format!("{}h ago", s / 3600)
    } else {
        format!("{}d ago", s / 86_400)
    }
}

#[allow(clippy::too_many_arguments)]
fn nowplaying_page(
    store: &TrackStore,
    current: Option<u32>,
    playing: bool,
    elapsed: f32,
    duration: f32,
    queue_index: Option<usize>,
    queue_len: usize,
    shuffle: ShuffleMode,
    repeat: RepeatMode,
    queue_paths: &[PathBuf],
    title: &egui::FontId,
    acts: &mut Vec<Action>,
    ui: &mut egui::Ui,
) {
    let ink = ui.visuals().clone();
    let strong = ink.widgets.inactive.fg_stroke.color;
    let dim = ink.weak_text_color.unwrap_or(strong);
    let mut head = ui.available_width();
    let (_, row) = ui.allocate_space(egui::vec2(head, 44.0));
    head = row.width();
    ui.painter().text(
        egui::Pos2::new(row.min.x, row.center().y),
        egui::Align2::LEFT_CENTER,
        "Now playing",
        title.clone(),
        strong,
    );

    // ── cover + title block ─────────────────────────────────────────────────
    // Real cover pixels arrive in M5 (lofty embedded art → image → texture
    // queue); until then the placeholder glyph is the honest answer.
    let cover = 180.0;
    let (_, band) = ui.allocate_space(egui::vec2(head, cover + 12.0));
    let sq = egui::Rect::from_min_size(band.min, egui::vec2(cover, cover));
    ui.painter().rect_filled(sq, 0.0, ink.faint_bg_color.gamma_multiply(0.7));
    ui.painter().rect_stroke(
        sq,
        0.0,
        egui::Stroke::new(1.0, HAIR),
        egui::StrokeKind::Middle,
    );
    glyphs::paint(ui, sq.shrink(28.0), glyphs::Icon::Frame, dim);
    let track = current
        .and_then(|i| store.tracks.get(i as usize))
        .map(|t| (t.title.clone(), t.artist_or_unknown().to_string()))
        .unwrap_or_else(|| ("nothing playing".to_string(), String::new()));
    let tx = band.min.x + cover + 18.0;
    ui.painter().text(
        egui::Pos2::new(tx, band.min.y + 34.0),
        egui::Align2::LEFT_CENTER,
        track.0.as_str(),
        egui::FontId::new(24.0, title.family.clone()),
        strong,
    );
    ui.painter().text(
        egui::Pos2::new(tx, band.min.y + 62.0),
        egui::Align2::LEFT_CENTER,
        track.1.as_str(),
        egui::FontId::proportional(16.0),
        dim,
    );
    ui.painter().text(
        egui::Pos2::new(tx, band.min.y + 84.0),
        egui::Align2::LEFT_CENTER,
        format!(
            "{}{}{} · {}",
            match queue_index {
                Some(i) if queue_len > 0 => format!("track {} of {queue_len}", i + 1),
                _ => "no queue".to_string(),
            },
            if shuffle == ShuffleMode::On {
                " · shuffle on"
            } else {
                ""
            },
            match repeat {
                RepeatMode::None => "",
                RepeatMode::All => " · repeat all",
                RepeatMode::One => " · repeat one",
            },
            if playing { "playing" } else { "paused" }
        ),
        egui::FontId::proportional(13.0),
        if playing { LINK } else { dim },
    );

    // ── wide progress `0:00 ──●── 2:08` ─────────────────────────────────────
    let (_, prow) = ui.allocate_space(egui::vec2(head, 34.0));
    let frac = if duration > 0.0 {
        (elapsed / duration).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let rail = egui::Rect::from_min_max(
        egui::Pos2::new(prow.min.x + 54.0, prow.center().y - 2.0),
        egui::Pos2::new(prow.max.x - 54.0, prow.center().y + 2.0),
    );
    ui.painter().rect_filled(rail, 2.0, ink.faint_bg_color);
    ui.painter().rect_filled(
        egui::Rect::from_min_max(
            rail.min,
            egui::Pos2::new(rail.min.x + rail.width() * frac, rail.max.y),
        ),
        2.0,
        if playing { ACCENT } else { LINK },
    );
    ui.painter().circle_filled(
        egui::Pos2::new(rail.min.x + rail.width() * frac, rail.center().y),
        6.0,
        if playing { ACCENT } else { strong },
    );
    ui.painter().text(
        egui::Pos2::new(prow.min.x, prow.center().y),
        egui::Align2::LEFT_CENTER,
        fmt_mmss(elapsed),
        egui::FontId::proportional(13.0),
        strong,
    );
    ui.painter().text(
        egui::Pos2::new(prow.max.x, prow.center().y),
        egui::Align2::RIGHT_CENTER,
        if duration > 0.0 {
            fmt_mmss(duration)
        } else {
            "--:--".to_string()
        },
        egui::FontId::proportional(13.0),
        strong,
    );
    let seek = ui.interact(
        egui::Rect::from_min_size(
            egui::Pos2::new(rail.min.x, prow.min.y),
            egui::vec2(rail.width(), prow.height()),
        ),
        ui.scope_id().with("np-seek"),
        egui::Sense::drag(),
    );
    if (seek.drag_stopped() || seek.clicked())
        && let Some(x) = seek.hover_pos()
    {
        let f = ((x.x - rail.min.x) / rail.width()).clamp(0.0, 1.0);
        if duration > 0.0 {
            acts.push(Action::SeekRel(f * duration - elapsed));
        }
    }
    seek.on_hover_text("click or drag to move the position");

    // ── controls ────────────────────────────────────────────────────────────
    let (_, crow) = ui.allocate_space(egui::vec2(head, 44.0));
    let btn = 44.0;
    let ctl: [(glyphs::Icon, u64, Action); 5] = [
        (glyphs::Icon::Shuffle, 11, Action::ToggleShuffle),
        (glyphs::Icon::Prev, 12, Action::PrevTrack),
        (
            if playing {
                glyphs::Icon::Pause
            } else {
                glyphs::Icon::Play
            },
            13,
            Action::TogglePlay,
        ),
        (glyphs::Icon::Next, 14, Action::NextTrack),
        (glyphs::Icon::Repeat, 15, Action::CycleRepeat),
    ];
    // `into_iter` moves `ctl`, so measure the cluster before consuming it
    let n_ctl = ctl.len() as f32;
    for (i, (icon, salt, act)) in ctl.into_iter().enumerate() {
        let rect = egui::Rect::from_min_size(
            egui::Pos2::new(
                crow.center().x - n_ctl * btn * 0.5 + i as f32 * btn,
                crow.min.y,
            ),
            egui::vec2(btn, crow.height()),
        );
        let resp = glyphs::button(ui, rect, icon, salt, queue_len > 0 || i == 2);
        if resp.clicked() {
            acts.push(act);
        }
    }
    ui.add_space(4.0);
    ui.separator();

    // ── queue list ──────────────────────────────────────────────────────────
    // The plan's full queue columns (Title · Artist · Album · Year · Time) and
    // the collapsible chevron are M5; M4 lists the queue with what the
    // transport already knows, so the page is honest rather than decorative.
    ui.label(format!(
        "Queue · {} song(s){}",
        queue_len,
        if queue_len == 0 {
            " · nothing loaded".to_string()
        } else {
            String::new()
        }
    ));
    ui.scope(|ui| {
        ui.spacing_mut().item_spacing.y = 0.0;
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show_rows(ui, ROW_H, queue_len, |ui, range| {
                let top = ui.cursor().min.y;
                let left = ui.cursor().min.x;
                let full = ui.available_width();
                for (k, q) in range.into_iter().enumerate() {
                    let y = top + k as f32 * ROW_H;
                    let row = egui::Rect::from_min_size(
                        egui::Pos2::new(left, y),
                        egui::vec2(full, ROW_H),
                    );
                    let is_current = queue_index == Some(q);
                    if is_current {
                        ui.painter()
                            .rect_filled(row, 0.0, ink.selection.bg_fill);
                    }
                    let path = match queue_paths.get(q) {
                        Some(p) => p,
                        None => continue,
                    };
                    let t = store.find_by_path(path);
                    let name = t
                        .map(|t| t.title.clone())
                        .unwrap_or_else(|| stem(path));
                    let artist = t
                        .map(|t| t.artist_or_unknown().to_string())
                        .unwrap_or_default();
                    let time = t.map(|t| fmt_mmss(t.duration)).unwrap_or_default();
                    ui.painter().text(
                        egui::Pos2::new(row.min.x + 8.0, row.center().y),
                        egui::Align2::LEFT_CENTER,
                        format!("{}  {}", q + 1, name),
                        egui::FontId::proportional(13.5),
                        if is_current {
                            egui::Color32::WHITE
                        } else {
                            strong
                        },
                    );
                    ui.painter().text(
                        egui::Pos2::new(row.min.x + W_ARTIST + 34.0, row.center().y),
                        egui::Align2::LEFT_CENTER,
                        artist,
                        egui::FontId::proportional(13.5),
                        if is_current { strong } else { dim },
                    );
                    ui.painter().text(
                        egui::Pos2::new(row.max.x - 8.0, row.center().y),
                        egui::Align2::RIGHT_CENTER,
                        time,
                        egui::FontId::proportional(12.5),
                        if is_current { strong } else { dim },
                    );
                }
            });
    });
}

fn settings_page(
    roots: &[PathBuf],
    status: &str,
    title: &egui::FontId,
    acts: &mut Vec<Action>,
    ui: &mut egui::Ui,
) {
    let ink = ui.visuals().clone();
    let strong = ink.widgets.inactive.fg_stroke.color;
    let dim = ink.weak_text_color.unwrap_or(strong);
    let (_, row) = ui.allocate_space(egui::vec2(ui.available_width(), 44.0));
    ui.painter().text(
        egui::Pos2::new(row.min.x, row.center().y),
        egui::Align2::LEFT_CENTER,
        "Settings",
        title.clone(),
        strong,
    );

    // ── "Choose where we look" (plan §Settings: working) ────────────────────
    settings_head(ui, "Music folders", ink.widgets.inactive.fg_stroke.color);
    if roots.is_empty() {
        ui.label(
            egui::RichText::new(
                "No folders yet. Grove only indexes the folders you add here or \
                 drop on the window.",
            )
            .size(13.5)
            .color(dim),
        );
    }
    for (i, r) in roots.iter().enumerate() {
        ui.label(
            egui::RichText::new(format!("{} · {}", i + 1, r.display()))
                .size(13.5)
                .color(strong),
        );
    }
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        let r = ui.add(link("add music folder"));
        if r.clicked() {
            acts.push(Action::AddFolder);
        }
        let r = ui.add(link(if roots.is_empty() {
            "rescan (needs a folder)"
        } else {
            "rescan now"
        }));
        if r.clicked() {
            acts.push(Action::Rescan);
        }
        r.on_hover_text("Re-walk every folder Grove has been asked to watch");
        let r = ui.add(link("open files"));
        if r.clicked() {
            acts.push(Action::PickFiles);
        }
    });

    // ── the rest of Groove's Settings pages, in plan order ──────────────────
    ui.add_space(10.0);
    settings_head(ui, "Playback", strong);
    settings_stub(ui, "Equalizer · 3 sliders", "arrives in M7");
    settings_stub(ui, "Output device switcher", "arrives in M7");
    ui.add_space(10.0);
    settings_head(ui, "Display", strong);
    settings_stub(ui, "Mode: Dark", "Light and System follow in M6");
    settings_stub(ui, "Show artist art on tiles", "cover art decodes in M5");
    ui.add_space(10.0);
    settings_head(ui, "Media information", strong);
    settings_stub(ui, "Get media information", "a stub link per plan §Settings");
    ui.add_space(10.0);
    settings_head(ui, "App", strong);
    settings_stub(ui, "Help · Feedback · About", "dead links until M8");
    ui.add_space(10.0);
    ui.label(
        egui::RichText::new(format!("status · {status}"))
            .size(12.5)
            .color(dim),
    );
}

fn settings_head(ui: &mut egui::Ui, text: &str, ink: egui::Color32) {
    ui.label(egui::RichText::new(text).strong().size(15.0).color(ink));
    ui.add_space(2.0);
}

/// A Settings row that names what it will do and says which milestone brings
/// it: the plan calls these "others dead links", so they never pretend to work.
fn settings_stub(ui: &mut egui::Ui, what: &str, when: &str) {
    ui.label(egui::RichText::new(what).size(13.5));
    ui.label(
        egui::RichText::new(format!("   {when}"))
            .size(12.5)
            .color(ui.visuals().weak_text_color.unwrap_or(egui::Color32::GRAY)),
    );
}

// ── Songs page ───────────────────────────────────────────────────────────────

fn songs_page(
    store: &TrackStore,
    query: &Query,
    view: &[u32],
    current: Option<u32>,
    status: &str,
    scanning: bool,
    acts: &mut Vec<Action>,
    ui: &mut egui::Ui,
) {
    if store.is_empty() && !scanning {
        // Groove's empty library ("Play your own music").
        ui.vertical_centered(|ui| {
            ui.add_space(40.0);
            ui.label(egui::RichText::new("Play your own music").heading());
            ui.label(
                "Add a music folder or drag one onto this window — Grove lists every \
                 song it finds, tagged or not.",
            );
            if ui.button("Add music folder…").clicked() {
                acts.push(Action::AddFolder);
            }
        });
        return;
    }

    // Filter line: what the search/genre currently restricts the view to.
    ui.horizontal(|ui| {
        if !query.search.is_empty() {
            ui.label(
                egui::RichText::new(format!("Search: “{}”", query.search)).strong(),
            );
        }
        if let Some(g) = &query.genre {
            ui.label(egui::RichText::new(format!("Genre: {g}")).strong());
        }
        if !query.search.is_empty() || query.genre.is_some() {
            if ui.button("show all").clicked() {
                acts.push(Action::ClearFilter);
            }
        } else {
            ui.label(format!(
                "{} songs · sorted by {}",
                view.len(),
                query.sort.label()
            ));
        }
        if scanning {
            ui.label(format!("{status} · filling the list as it goes"));
        }
    });
    ui.add_space(2.0);

    let full = ui.available_width();
    let cols = Cols::layout(full, ui.spacing().item_spacing.x);

    // Column headers double as sort links. Clicking a link that is already
    // active walks `SortKey::CYCLE` (Date added → Title → Artist → Album →
    // Year → Date added), which is what `SortKey::next` exists for.
    let mut sort_pick: Option<SortKey> = None;
    ui.horizontal(|ui| {
        ui.add_sized([cols.note, HEAD_H], egui::Label::new("").selectable(false));
        for (label, key, width) in [
            ("Title", SortKey::Title, cols.title),
            ("Artist", SortKey::Artist, cols.artist),
            ("Year", SortKey::Year, cols.year),
            ("Genre", SortKey::Album, cols.genre),
            ("Time", SortKey::DateAdded, cols.time),
        ] {
            if width <= 0.0 {
                continue;
            }
            let was = query.sort;
            if header_cell(label, width, key, was, ui).clicked() {
                sort_pick = Some(if was == key { key.next() } else { key });
            }
        }
        ui.add_sized([cols.play, HEAD_H], egui::Label::new("").selectable(false));
        ui.add_sized([cols.menu, HEAD_H], egui::Label::new("").selectable(false));
    });
    ui.separator();
    if let Some(k) = sort_pick {
        acts.push(Action::SetSort(k));
    }

    // Virtualized list. egui 0.37 dropped `VirtualList` (and `egui_extras` has
    // no TableBuilder either), so `ScrollArea::show_rows` is the virtualizer:
    // it lays out only the rows inside the viewport, which is what a 20k-song
    // library needs. `item_spacing.y = 0` inside this scope makes the row pitch
    // exactly ROW_H — `show_rows` captures the spacing when it is entered, so
    // the change has to happen before the call, not inside the row callback.
    ui.scope(|ui| {
        ui.spacing_mut().item_spacing.y = 0.0;
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show_rows(ui, ROW_H, view.len(), |ui, range| {
                let top = ui.cursor().min.y;
                let left = ui.cursor().min.x;
                // Hover row straight from the pointer position: no per-row
                // response to chase, and the background decision stays same-frame.
                let hover_row = ui
                    .input(|i| i.pointer.hover_pos())
                    .filter(|p| p.x >= left && p.x <= left + full)
                    .filter(|p| p.y >= top && p.y < top + (range.len() as f32) * ROW_H)
                    .map(|p| range.start + ((p.y - top) / ROW_H).floor().max(0.0) as usize);

                for (k, r) in range.into_iter().enumerate() {
                    let li = view[r] as usize;
                    let t = &store.tracks[li];
                    let lib = li as u32;
                    let is_current = current == Some(lib);
                    let hovered = hover_row == Some(r);
                    let y = top + k as f32 * ROW_H;
                    let row = egui::Rect::from_min_size(
                        egui::Pos2::new(left, y),
                        egui::vec2(full, ROW_H),
                    );

                    // Background first (painted under the cells), then the row
                    // click zone, then cells + glyphs.
                    row_background(row, is_current, hovered, ui);
                    let body = egui::Rect::from_min_max(
                        row.min,
                        egui::Pos2::new(row.max.x - cols.play - cols.menu - cols.sp, row.max.y),
                    );
                    let body_resp = ui.interact(
                        body,
                        ui.scope_id().with(lib).with("body"),
                        egui::Sense::click(),
                    );
                    if body_resp.clicked() {
                        acts.push(Action::PlayRow { r });
                    }

                    // Cells, plus the two hover-revealed buttons parked on the
                    // right edge. `glyphs::button` hit-tests and paints from
                    // the *same* frame, so the revealed glyph never trails the
                    // pointer by a repaint.
                    let (play, menu) = ui.horizontal(|ui| {
                        // left prefix cell, always 24 px: the playing row shows
                        // Groove's note icon there (`Cols::note`)
                        if is_current {
                            let note_rect = egui::Rect::from_min_size(
                                ui.cursor().min,
                                egui::vec2(cols.note, ROW_H),
                            );
                            glyphs::paint(ui, note_rect, glyphs::Icon::Note, ACCENT);
                        }
                        ui.add_sized(
                            [cols.note, ROW_H],
                            egui::Label::new("").selectable(false),
                        );
                        cell(&t.title, cols.title, egui::Align::Min, ui);
                        if cols.artist > 0.0 {
                            cell(t.artist_or_unknown(), cols.artist, egui::Align::Min, ui);
                        }
                        if cols.year > 0.0 {
                            cell(
                                &t.year.map(|y| y.to_string()).unwrap_or_default(),
                                cols.year,
                                egui::Align::Min,
                                ui,
                            );
                        }
                        if cols.genre > 0.0 {
                            cell(
                                t.genre.as_deref().unwrap_or(""),
                                cols.genre,
                                egui::Align::Min,
                                ui,
                            );
                        }
                        cell(&fmt_mmss(t.duration), cols.time, egui::Align::Max, ui);
                        let play_rect = egui::Rect::from_min_size(
                            ui.cursor().min,
                            egui::vec2(cols.play, ROW_H),
                        );
                        let play = glyphs::button(
                            ui,
                            play_rect,
                            if is_current {
                                glyphs::Icon::Pause
                            } else {
                                glyphs::Icon::Play
                            },
                            lib as u64 * 2,
                            hovered || is_current,
                        );
                        // `interact` does not move the cursor, so the row has
                        // to step past the button it just hit-tested
                        ui.add_space(cols.play);
                        let menu_rect = egui::Rect::from_min_size(
                            ui.cursor().min,
                            egui::vec2(cols.menu, ROW_H),
                        );
                        let menu = glyphs::button(
                            ui,
                            menu_rect,
                            glyphs::Icon::DotsV,
                            lib as u64 * 2 + 1,
                            hovered,
                        );
                        ui.add_space(cols.menu);
                        (play, menu)
                    })
                    .inner;
                    if play.clicked() {
                        acts.push(if is_current {
                            Action::TogglePlay
                        } else {
                            Action::PlayRow { r }
                        });
                    }

                    // Groove's row menu, two items in M3; `Popup` handles the
                    // open/closed state and the click-outside dismissal.
                    if let Some(p) = egui::Popup::menu(&menu).show(|ui| {
                        ui.label(
                            egui::RichText::new(format!(
                                "{} — {}",
                                t.title,
                                t.artist_or_unknown()
                            ))
                            .strong()
                            .size(12.5),
                        );
                        ui.separator();
                        if ui.button("Play now").clicked() {
                            ui.close();
                            Some(Action::PlayTrack { lib })
                        } else if ui.button("Remove from library").clicked() {
                            ui.close();
                            Some(Action::Remove { lib })
                        } else {
                            None
                        }
                    }) {
                        if let Some(a) = p.inner {
                            acts.push(a);
                        }
                    }
                }
            });
    });
}

fn row_background(row: egui::Rect, is_current: bool, is_hover: bool, ui: &mut egui::Ui) {
    let v = ui.visuals();
    if is_current {
        ui.painter().rect_filled(row, 0.0, v.selection.bg_fill);
        // Groove marks the playing row with a thin accent bar at the left edge.
        ui.painter().rect_filled(
            egui::Rect::from_min_size(row.min, egui::vec2(3.0, row.height())),
            0.0,
            ACCENT,
        );
    } else if is_hover {
        ui.painter()
            .rect_filled(row, 0.0, v.widgets.hovered.weak_bg_fill);
    } else {
        // Alt-row banding (Groove's list stripes) — faint, so text stays dominant.
        ui.painter()
            .rect_filled(row, 0.0, v.faint_bg_color.gamma_multiply(0.35));
    }
}

/// One column cell: exact width, vertically centered, truncated with an
/// ellipsis. Labels are hover-only, so clicks fall through to the row click zone.
fn cell(text: &str, width: f32, halign: egui::Align, ui: &mut egui::Ui) {
    ui.add_sized(
        [width, ROW_H],
        egui::Label::new(egui::RichText::new(text).size(13.5))
            .truncate()
            .selectable(false)
            .halign(halign),
    );
}

/// Header cell that sorts by `key` when clicked (the plan's "sort links").
fn header_cell(
    text: &str,
    width: f32,
    key: SortKey,
    current: SortKey,
    ui: &mut egui::Ui,
) -> egui::Response {
    let rich = if key == current {
        egui::RichText::new(format!("{text} ↓"))
            .strong()
            .color(ACCENT)
    } else {
        egui::RichText::new(text).strong().size(12.0)
    };
    ui.add_sized(
        [width, HEAD_H],
        egui::Label::new(rich)
            .truncate()
            .selectable(false)
            .halign(egui::Align::Min)
            .sense(egui::Sense::click()),
    )
}

// ── Artists / Albums pages ───────────────────────────────────────────────────

/// One A–Z section: the letter header plus its tiles in a uniform grid.
struct Section {
    letter: char,
    /// index into the (possibly letter-filtered) group list where this
    /// section starts
    first: usize,
    count: usize,
    /// rows of tiles this section needs at the measured column count
    rows: usize,
}

/// Groove's Artists (circles) and Albums (squares) grids: flat A–Z sections,
/// one tile per group, virtualized as *variable*-height lines because
/// `show_rows` only knows one uniform row pitch (plan §My music). Tiles carry
/// the placeholder glyph until embedded cover art decodes in M5.
fn group_page(
    head: &str,
    square: bool,
    groups: &[Group],
    letter: Option<char>,
    title_font: &egui::FontId,
    acts: &mut Vec<Action>,
    ui: &mut egui::Ui,
) {
    let ink = ui.visuals().clone();
    let strong = ink.widgets.inactive.fg_stroke.color;
    let dim = ink.weak_text_color.unwrap_or(strong);
    let full = ui.available_width();
    let (_, trow) = ui.allocate_space(egui::vec2(full, 44.0));
    ui.painter().text(
        egui::Pos2::new(trow.min.x, trow.center().y),
        egui::Align2::LEFT_CENTER,
        head,
        title_font.clone(),
        strong,
    );
    if groups.is_empty() {
        ui.add_space(8.0);
        ui.label(
            egui::RichText::new(format!(
                "no {} yet — add a music folder",
                head.to_lowercase()
            ))
                .size(13.5)
                .color(dim),
        );
        return;
    }

    // ── letter jump ─────────────────────────────────────────────────────────
    // egui 0.37 dropped `ScrollArea::scroll_to_y` and made its `State`
    // private, so "jump to Q" cannot scroll: it filters. The rail therefore
    // shows one letter at a time (recorded as a deviation in the build log).
    let present = |ch: char| groups.iter().any(|g| g.letter == ch);
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        let mk = |text: String, hot: bool| {
            egui::Label::new(egui::RichText::new(text).size(12.5).color(if hot {
                LINK
            } else {
                dim
            }))
            .selectable(false)
            .sense(egui::Sense::click())
        };
        let r = ui.add(mk("all".to_string(), letter.is_some()));
        if r.clicked() && letter.is_some() {
            acts.push(Action::SetLetter(None));
        }
        for ch in 'A'..='Z' {
            let on = present(ch);
            let r = ui.add(mk(ch.to_string(), on && letter == Some(ch)));
            if on && r.clicked() {
                acts.push(Action::SetLetter(if letter == Some(ch) {
                    None
                } else {
                    Some(ch)
                }));
            }
        }
        let on = present('#');
        let r = ui.add(mk("#".to_string(), on && letter == Some('#')));
        if on && r.clicked() {
            acts.push(Action::SetLetter(if letter == Some('#') {
                None
            } else {
                Some('#')
            }));
        }
    });
    ui.add_space(4.0);

    // ── sections + virtualization math (all of it before the scroll area, so
    // `set_height` knows the content size on the very first frame) ──────────
    let shown: Vec<usize> = groups
        .iter()
        .enumerate()
        .filter(|(_, g)| letter.is_none_or(|c| g.letter == c))
        .map(|(i, _)| i)
        .collect();
    if shown.is_empty() {
        ui.label(
            egui::RichText::new(format!("nothing under {letter:?}"))
                .size(13.5)
                .color(dim),
        );
        return;
    }
    let cols = theme::tile_cols(full - 4.0, TILE, TILE_GAP).max(1);
    let cap_h = 30.0;
    let pitch = TILE + cap_h + TILE_GAP;
    let mut sections: Vec<Section> = Vec::new();
    let mut heights: Vec<f32> = Vec::new();
    let mut i = 0usize;
    while i < shown.len() {
        let l = groups[shown[i]].letter;
        let mut n = 0usize;
        while i + n < shown.len() && groups[shown[i + n]].letter == l {
            n += 1;
        }
        sections.push(Section {
            letter: l,
            first: i,
            count: n,
            rows: (n + cols - 1) / cols,
        });
        heights.push(LINE_H + sections[sections.len() - 1].rows as f32 * pitch + 10.0);
        i += n;
    }
    let (tops, total) = theme::line_tops(&heights);

    ui.scope(|ui| {
        ui.spacing_mut().item_spacing.y = 0.0;
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show_viewport(ui, |ui, viewport| {
                // `show_viewport` gives the viewport in *content* coordinates:
                // `viewport.min.y` is how far the user has scrolled
                ui.set_height(total);
                let base = ui.max_rect().top();
                let left = ui.cursor().min.x;
                let visible =
                    theme::visible_lines(&tops, viewport.min.y, viewport.max.y, total);
                for s in visible {
                    let sec = &sections[s];
                    let sy = base + tops[s];
                    // A–Z section header + hairline, exactly like the M3 list
                    ui.painter().text(
                        egui::Pos2::new(left + 4.0, sy + LINE_H * 0.5),
                        egui::Align2::LEFT_CENTER,
                        sec.letter.to_string(),
                        egui::FontId::proportional(18.0),
                        ACCENT,
                    );
                    ui.painter().line_segment(
                        [
                            egui::Pos2::new(left + 28.0, sy + LINE_H - 1.0),
                            egui::Pos2::new(left + full, sy + LINE_H - 1.0),
                        ],
                        egui::Stroke::new(1.0, HAIR),
                    );
                    for k in 0..sec.count {
                        let gi = shown[sec.first + k];
                        let g = &groups[gi];
                        let tx =
                            left + (k % cols) as f32 * (TILE + TILE_GAP) + 2.0;
                        let ty = sy + LINE_H + (k / cols) as f32 * pitch;
                        let tile = egui::Rect::from_min_size(
                            egui::Pos2::new(tx, ty),
                            egui::vec2(TILE, TILE),
                        );
                        let resp = ui.interact(
                            tile,
                            ui.scope_id().with(gi).with("tile"),
                            egui::Sense::click(),
                        );
                        paint_tile(ui, tile, g, square, resp.hovered(), ink.faint_bg_color);
                        // caption: name, then the song count
                        ui.put(
                            egui::Rect::from_min_size(
                                egui::Pos2::new(tx, ty + TILE + 2.0),
                                egui::vec2(TILE, cap_h * 0.5),
                            ),
                            egui::Label::new(
                                egui::RichText::new(g.name.as_str())
                                    .strong()
                                    .size(12.5)
                                    .color(strong),
                            )
                            .truncate()
                            .selectable(false),
                        );
                        ui.put(
                            egui::Rect::from_min_size(
                                egui::Pos2::new(tx, ty + TILE + 16.0),
                                egui::vec2(TILE, cap_h * 0.5),
                            ),
                            egui::Label::new(
                                egui::RichText::new(format!("{} song(s)", g.count()))
                                    .size(11.5)
                                    .color(dim),
                            )
                            .truncate()
                            .selectable(false),
                        );
                        if resp.clicked() {
                            acts.push(Action::FilterTo {
                                name: g.name.clone(),
                            });
                        }
                        resp.on_hover_text(format!(
                            "{} · {} songs · click to filter My music to it",
                            g.name,
                            g.count()
                        ));
                    }
                }
            });
    });
}

/// One tile: Artists are circles (letter monogram, like Groove), Albums are
/// squares (the picture-placeholder glyph unless a member carries art).
fn paint_tile(
    ui: &mut egui::Ui,
    tile: egui::Rect,
    g: &Group,
    square: bool,
    hovered: bool,
    faint: egui::Color32,
) {
    let strong = ui.visuals().widgets.inactive.fg_stroke.color;
    if square {
        if g.has_art {
            ui.painter()
                .rect_filled(tile, 0.0, strong.gamma_multiply(0.35));
        } else {
            ui.painter()
                .rect_filled(tile, 0.0, faint.gamma_multiply(0.55));
            glyphs::paint(ui, tile.shrink(TILE * 0.22), glyphs::Icon::Frame, strong);
        }
        if hovered {
            ui.painter().rect_stroke(
                tile.shrink(1.0),
                0.0,
                egui::Stroke::new(2.0, ACCENT),
                egui::StrokeKind::Inside,
            );
        }
    } else {
        let r = tile.width() * 0.5;
        ui.painter()
            .circle_filled(tile.center(), r, faint.gamma_multiply(0.55));
        if g.has_art {
            ui.painter()
                .circle_filled(tile.center(), r * 0.98, strong.gamma_multiply(0.35));
        } else {
            ui.painter().text(
                tile.center(),
                egui::Align2::CENTER_CENTER,
                g.letter.to_uppercase().to_string(),
                egui::FontId::new(42.0, egui::FontFamily::Proportional),
                strong,
            );
        }
        if hovered {
            ui.painter().circle_stroke(
                tile.center(),
                r + 1.0,
                egui::Stroke::new(2.0, ACCENT),
            );
        }
    }
}

// ── column math ──────────────────────────────────────────────────────────────

/// Column widths for one row. Genre drops out first, then Artist — Groove's
/// narrow list drops columns the same way.
struct Cols {
    sp: f32,
    note: f32,
    title: f32,
    artist: f32,
    year: f32,
    genre: f32,
    time: f32,
    play: f32,
    menu: f32,
}

impl Cols {
    fn layout(total: f32, spacing: f32) -> Cols {
        let mut c = Cols {
            sp: spacing,
            note: NOTE_W,
            title: 0.0,
            artist: W_ARTIST,
            year: W_YEAR,
            genre: W_GENRE,
            time: W_TIME,
            play: W_GLYPH,
            menu: W_GLYPH,
        };
        // Cells + the gaps between them have to fit inside `total`; the title
        // column absorbs whatever is left, and drops columns when it cannot.
        loop {
            let fixed = c.note + c.artist + c.year + c.genre + c.time + c.play + c.menu;
            let gaps = c.gaps();
            if total - fixed - gaps - TITLE_MIN >= 0.0 {
                c.title = (total - fixed - gaps).max(TITLE_MIN);
                break;
            }
            if c.genre > 0.0 {
                c.genre = 0.0;
                continue;
            }
            if c.artist > 0.0 {
                c.artist = 0.0;
                continue;
            }
            c.title = (total - fixed - gaps).max(90.0);
            break;
        }
        c
    }

    fn gaps(&self) -> f32 {
        // items: note, title, [artist], year, [genre], time, play, menu
        // (Rust 2024 forbids `bool as f32`, so count with ifs)
        let mut n: f32 = 6.0;
        if self.artist > 0.0 {
            n += 1.0;
        }
        if self.genre > 0.0 {
            n += 1.0;
        }
        self.sp * (n - 1.0).max(0.0)
    }
}

/// Left-edge prefix cell: the playing row's note icon (plan §My music), sized
/// the same on every row so the columns never shift.
const NOTE_W: f32 = 24.0;

/// Narrowest the title column ever gets before a column drops out.
const TITLE_MIN: f32 = 140.0;

// ── helpers ──────────────────────────────────────────────────────────────────

/// `PATH1;PATH2` env var → paths (trimmed, empties ignored).
fn paths_from_env(key: &str) -> Vec<PathBuf> {
    std::env::var(key)
        .unwrap_or_default()
        .split(';')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .collect()
}

/// Clock-seeded xorshift: enough randomness for a "Shuffle all" start index
/// without pulling in a rand crate (the crate stack is pinned).
fn seeded_index(n: usize, seed0: u64) -> usize {
    let mut x = seed0 ^ 0x5A5A_5A5A_5A5A_5A5A;
    if x == 0 {
        x = 1;
    }
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    (x % n as u64) as usize
}
