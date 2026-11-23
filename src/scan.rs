// Grove — Groove Music, rebuilt in Rust. Library scan worker (M3).
//
// One dedicated thread, blocking on its command channel: a folder walk plus a
// lofty read of every audio file must never touch the UI frame loop nor the
// audio worker. Idle CPU is therefore exactly zero until a scan is requested
// (`recv` parks the thread; the 20 Hz audio tick and the UI repaint cadence are
// untouched by this lane).
//
// Contract: UI sends `ScanCmd::Scan { roots, replace }` → worker streams
// `ScanEvent::Started` → a series of `Batch`es (so a big library fills the
// Songs list while the walk continues, like Groove does) → `Finished` with
// inspected/emitted/unreadable counts. The UI merges (dedupe by canonical
// path) and owns the library; the worker owns nothing else.
//
// Per-file rule: anything lofty can parse contributes tags + container duration;
// anything it cannot still becomes a row with its filename stem as the title
// and duration 0 (Groove lists unplayable files rather than hiding them).

use std::fs;
use std::mem;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration as StdDur, Instant, UNIX_EPOCH};

use crossbeam_channel::{Receiver, Sender};
use lofty::prelude::*; // AudioFile, TaggedFileExt, Accessor, ItemKey, TagExt

use crate::model::{ScanCmd, ScanEvent, TrackInfo, is_audio, stem, tlog};

/// Emit a batch this often: large enough to keep channel traffic cheap, small
/// enough that a 20k-file library appears in the UI within ~1 s per batch.
const BATCH: usize = 128;
/// …and flush a partial batch this often so the list is never blank mid-scan.
const FLUSH: StdDur = StdDur::from_millis(150);
/// Guard against a pathological tree (network share loops): stop after this
/// many directory entries and say so.
const MAX_INSPECT: u32 = 200_000;

pub fn spawn_scanner(cmd_rx: Receiver<ScanCmd>, ev_tx: Sender<ScanEvent>) {
    let builder = thread::Builder::new().name("grove-scanner".to_string());
    match builder.spawn(move || scanner(&cmd_rx, &ev_tx)) {
        Ok(_) => {}
        Err(e) => {
            tlog!("scanner thread failed to start: {e}");
        }
    }
}

/// Blocking command loop: park on `recv` when idle (no polling, no CPU).
fn scanner(cmd_rx: &Receiver<ScanCmd>, ev_tx: &Sender<ScanEvent>) {
    tlog!("scanner online");
    for cmd in cmd_rx.iter() {
        match cmd {
            ScanCmd::Scan { roots, replace } => {
                tlog!(
                    "scan start · {} root(s) · replace={replace}",
                    roots.len()
                );
                scan_roots(&roots, replace, ev_tx);
            }
        }
    }
    tlog!("scanner exiting (UI dropped the scan lane)");
}

fn scan_roots(roots: &[PathBuf], replace: bool, ev_tx: &Sender<ScanEvent>) {
    let began = Instant::now();

    // A root that does not exist is a user-visible problem, not an empty
    // success: report it through `Error` (the UI clears "scanning" there),
    // rather than silently finishing with zero rows.
    let missing: Vec<PathBuf> = roots.iter().filter(|r| !r.exists()).cloned().collect();
    if missing.len() == roots.len() {
        let message = format!(
            "nothing to scan · {} does not exist",
            missing.first().map(|p| p.display().to_string()).unwrap_or_default()
        );
        tlog!("scan aborted · {message}");
        ev_tx.send(ScanEvent::Error { message }).ok();
        return;
    }
    if !missing.is_empty() {
        tlog!(
            "note: {} of {} root(s) gone ({} skipped)",
            missing.len(),
            roots.len(),
            missing.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ")
        );
    }

    ev_tx
        .send(ScanEvent::Started {
            roots: roots.to_vec(),
            replace,
        })
        .ok();

    let mut batch: Vec<TrackInfo> = Vec::with_capacity(BATCH);
    let mut flush_at = Instant::now() + FLUSH;
    let mut emitted: u32 = 0;
    let mut unreadable: u32 = 0;

    let mut walk = Walk::new(roots.iter().cloned());
    while let Some(path) = walk.next() {
        let outcome = read_track(&path);
        emitted += 1;
        if !outcome.meta_ok {
            unreadable += 1;
        }
        batch.push(outcome.track);
        if batch.len() >= BATCH || Instant::now() >= flush_at {
            ev_tx
                .send(ScanEvent::Batch {
                    tracks: mem::take(&mut batch),
                })
                .ok();
            flush_at = Instant::now() + FLUSH;
        }
    }
    if !batch.is_empty() {
        ev_tx
            .send(ScanEvent::Batch {
                tracks: mem::take(&mut batch),
            })
            .ok();
    }
    let ms = began.elapsed().as_millis();
    tlog!(
        "scan done · inspected {} entries · {emitted} audio · {unreadable} unreadable · {ms} ms{}",
        walk.inspected(),
        if walk.capped() { " (capped)" } else { "" }
    );
    ev_tx
        .send(ScanEvent::Finished {
            roots: roots.to_vec(),
            inspected: walk.inspected(),
            emitted,
            unreadable,
            ms,
        })
        .ok();
    if unreadable > 0 {
        tlog!("note: {unreadable} file(s) had no readable metadata (listed by filename)");
    }
}

/// One file → one library row, plus `meta_ok` for the scan summary.
pub struct ReadOutcome {
    pub track: TrackInfo,
    pub meta_ok: bool,
}

pub fn read_track(path: &Path) -> ReadOutcome {
    let mut track = TrackInfo {
        path: path.to_path_buf(),
        title: stem(path), // overwritten by the tag title when one exists
        date_added: mtime_secs(path),
        ..TrackInfo::default()
    };
    match lofty::read_from_path(path) {
        Ok(tf) => {
            track.duration = tf.properties().duration().as_secs_f32();
            let tag = tf.primary_tag().or_else(|| tf.first_tag());
            if let Some(tag) = tag {
                if let Some(title) = text(tag.title()) {
                    track.title = title;
                }
                track.artist = text(tag.artist());
                track.album = text(tag.album());
                track.genre = text(tag.genre());
                track.year = year_of(tag);
                track.has_art = !tag.pictures().is_empty();
            }
            ReadOutcome {
                track,
                meta_ok: true,
            }
        }
        // the parse error text stays out of the hot path: only the count is
        // reported (Finished.unreadable) and one summary line is logged
        Err(_) => ReadOutcome {
            track,
            meta_ok: false,
        },
    }
}

/// `Some(trimmed string)` only when the tag field carries real text, so an
/// empty tag does not beat the filename stem.
fn text<S: AsRef<str>>(v: Option<S>) -> Option<String> {
    v.map(|s| s.as_ref().trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Year, taken in the order tags really carry it: the numeric `Year` key (the
/// ecosystem agreed on it even for full date strings), then the date fields'
/// raw text, and finally lofty's own timestamp parser as a fallback.
fn year_of(tag: &lofty::tag::Tag) -> Option<i32> {
    [ItemKey::Year, ItemKey::RecordingDate, ItemKey::ReleaseDate]
        .into_iter()
        .filter_map(|k| tag.get_string(k))
        .filter_map(|s| parse_year(s))
        .next()
        .or_else(|| {
            tag.date()
                .and_then(|d| match d.year { 0 => None, y => Some(i32::from(y)) })
        })
}

/// First four-digit run in a tag string: `"1998"`, `"2002-05-14"`, `"(c) 2021"`.
fn parse_year(s: &str) -> Option<i32> {
    let b = s.as_bytes();
    let mut i = 0;
    while i + 4 <= b.len() {
        if b[i].is_ascii_digit()
            && b[i + 1].is_ascii_digit()
            && b[i + 2].is_ascii_digit()
            && b[i + 3].is_ascii_digit()
        {
            return s[i..i + 4].parse().ok();
        }
        i += 1;
    }
    None
}

fn mtime_secs(path: &Path) -> i64 {
    match fs::metadata(path).and_then(|m| m.modified()) {
        Ok(t) => t
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0),
        Err(_) => 0,
    }
}

/// Depth-first walker: directories expand recursively, loose files pass through,
/// everything is filtered by `AUDIO_EXTS` (case-insensitive). Deterministic
/// A→Z order per directory so repeated scans insert in the same order.
pub struct Walk {
    dirs: Vec<PathBuf>,
    files: Vec<PathBuf>,
    inspected: u32,
    capped: bool,
}

impl Walk {
    pub fn new(roots: impl Iterator<Item = PathBuf>) -> Self {
        let mut w = Walk {
            dirs: Vec::new(),
            files: Vec::new(),
            inspected: 0,
            capped: false,
        };
        for r in roots {
            match fs::metadata(&r) {
                Ok(m) if m.is_dir() => w.dirs.push(r),
                Ok(_) => w.files.push(r), // dropped loose audio files
                Err(_) => {}              // vanished between dialog and scan
            }
        }
        w.sort_pending();
        w
    }

    pub fn inspected(&self) -> u32 {
        self.inspected
    }

    pub fn capped(&self) -> bool {
        self.capped
    }

    /// Reverse-sort the pending queues so `pop()` yields A→Z.
    fn sort_pending(&mut self) {
        self.files.sort_by(|a, b| key(a).cmp(&key(b)));
        self.files.reverse();
        self.dirs.sort_by(|a, b| key(a).cmp(&key(b)));
        self.dirs.reverse();
    }

    fn take_dir(&mut self) -> Option<PathBuf> {
        if self.inspected >= MAX_INSPECT {
            self.capped = true;
            return None;
        }
        self.dirs.pop()
    }
}

fn key(p: &Path) -> String {
    p.to_string_lossy().to_lowercase()
}

impl Iterator for Walk {
    type Item = PathBuf;

    fn next(&mut self) -> Option<PathBuf> {
        loop {
            if let Some(p) = self.files.pop() {
                if is_audio(&p) {
                    return Some(p);
                }
                continue;
            }
            let dir = self.take_dir()?;
            match fs::read_dir(&dir) {
                Ok(rd) => {
                    for e in rd.flatten() {
                        self.inspected += 1;
                        let p = e.path();
                        match e.file_type().ok() {
                            Some(ft) if ft.is_dir() => self.dirs.push(p),
                            Some(ft) if ft.is_file() && is_audio(&p) => self.files.push(p),
                            // symlinks/sockets/fifos and unreadable entries:
                            // never followed (loop protection)
                            _ => {}
                        }
                    }
                    self.sort_pending();
                }
                Err(e) => {
                    tlog!("scan: skipping {} ({e})", dir.display());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{TrackStore, UNKNOWN_ALBUM, UNKNOWN_ARTIST};
    use lofty::tag::{Tag, TagType};
    use std::fs::File;
    use std::io::Write;
    use std::process::Command;

    /// Deterministic temp dir per test (`grove_m3_tests/<tag>`), wiped first so
    /// reruns never inherit stale files.
    fn workdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("grove_m3_tests").join(tag);
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    /// Silent 16-bit mono PCM WAV with an exact header duration (no external
    /// tool needed; `total_duration()` reports it precisely).
    fn silent_wav(path: &Path, secs: f64) {
        let sr: u32 = 44100;
        let samples = (sr as f64 * secs) as u32;
        let data_len = samples * 2;
        let mut b: Vec<u8> = Vec::new();
        b.extend_from_slice(b"RIFF");
        b.extend((36 + data_len).to_le_bytes().to_vec());
        b.extend_from_slice(b"WAVEfmt ");
        b.extend(16u32.to_le_bytes().to_vec());
        b.extend(1u16.to_le_bytes().to_vec()); // PCM
        b.extend(1u16.to_le_bytes().to_vec()); // mono
        b.extend(sr.to_le_bytes().to_vec());
        b.extend((sr * 2).to_le_bytes().to_vec());
        b.extend(2u16.to_le_bytes().to_vec());
        b.extend(16u16.to_le_bytes().to_vec());
        b.extend_from_slice(b"data");
        b.extend(data_len.to_le_bytes().to_vec());
        b.resize(44 + data_len as usize, 0);
        fs::write(path, &b).expect("write wav");
    }

    fn ffmpeg_available() -> bool {
        Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-version"])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// Tagged fixture in `fmt` via ffmpeg (lavfi tone, container-native tags);
    /// `false` when ffmpeg is absent so tests fall back to the WAV writer.
    fn ffmpeg_tagged(dir: &Path, name: &str, fmt: &str, secs: f64, meta: &[(&str, &str)]) -> bool {
        if !ffmpeg_available() {
            return false;
        }
        let path = dir.join(name);
        let mut cmd = Command::new("ffmpeg");
        cmd.args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            &format!("sine=frequency=440:duration={secs}"),
            "-c:a",
            match fmt {
                "flac" => "flac",
                "mp3" => "libmp3lame",
                "ogg" => "libvorbis",
                "m4a" => "aac",
                _ => "aac",
            },
        ]);
        for (k, v) in meta {
            cmd.args(["-metadata", &format!("{k}={v}")]);
        }
        cmd.arg(&path);
        let out = cmd.output().expect("spawn ffmpeg");
        if !out.status.success() {
            tlog!(
                "ffmpeg fixture failed for {}: {}",
                name,
                String::from_utf8_lossy(&out.stderr)
            );
            return false;
        }
        // ffmpeg writes duration tags fine but some containers need a nudge:
        // nothing to do here, lofty reads the container header.
        true
    }

    /// Library used by the scan tests:
    /// ```text
    /// Alpha/One.flac        Alpha Artist · Alpha Album · Test Genre · 2021
    /// Alpha/Beta/Two.mp3    alpha artist (other casing) · Beta Album · other genre
    /// Gamma/Three.ogg       Alpha Artist · (no album) · Test Genre
    /// Gamma/notes.txt       must be ignored
    /// Loose.WAV             uppercase extension, no tags
    /// Broken.flac           junk bytes, still listed by stem
    /// ```
    /// Returns `(root, has_real_tags)`.
    fn make_library(tag: &str) -> (PathBuf, bool) {
        let root = workdir(tag);
        fs::create_dir_all(root.join("Alpha/Beta")).expect("nested dir");
        fs::create_dir_all(root.join("Gamma")).expect("nested dir");
        let real = ffmpeg_tagged(
            &root,
            "Alpha/One.flac",
            "flac",
            1.2,
            &[
                ("title", "One Song"),
                ("artist", "Alpha Artist"),
                ("album", "Alpha Album"),
                ("genre", "Test Genre"),
                ("date", "2021"),
            ],
        ) && ffmpeg_tagged(
            &root,
            "Alpha/Beta/Two.mp3",
            "mp3",
            1.4,
            &[
                ("title", "Two Song"),
                ("artist", "alpha artist"), // different casing on purpose
                ("album", "Beta Album"),
                ("genre", "other genre"),
                ("date", "2002"),
            ],
        ) && ffmpeg_tagged(
            &root,
            "Gamma/Three.ogg",
            "ogg",
            1.6,
            &[
                ("title", "Three Song"),
                ("artist", "Alpha Artist"),
                ("genre", "Test Genre"),
            ],
        );
        if !real {
            // no ffmpeg on this machine → tagged assertions are skipped, the
            // structural assertions (walk/filter/grouping) still run
            silent_wav(&root.join("Alpha/One.flac"), 1.2);
            silent_wav(&root.join("Alpha/Beta/Two.mp3"), 1.4);
            silent_wav(&root.join("Gamma/Three.ogg"), 1.6);
        }
        let mut notes = File::create(root.join("Gamma/notes.txt")).expect("txt");
        notes.write_all(b"not music").ok();
        silent_wav(&root.join("Loose.WAV"), 1.0);
        fs::write(root.join("Broken.flac"), b"not really flac at all").expect("junk");
        (root, real)
    }

    fn collect_scan(root: &Path) -> Vec<TrackInfo> {
        Walk::new([root.to_path_buf()].into_iter())
            .collect::<Vec<PathBuf>>()
            .into_iter()
            .map(|p| read_track(&p).track)
            .collect()
    }

    #[test]
    fn walk_finds_nested_and_uppercase_audio_and_ignores_other_files() {
        let (root, _) = make_library("walk_filter");
        let found = Walk::new([root.clone()].into_iter()).collect::<Vec<PathBuf>>();
        let names: Vec<String> = found
            .iter()
            .map(|p| {
                p.strip_prefix(&root)
                    .unwrap_or_else(|_| p.as_path())
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();
        assert_eq!(names.len(), 5, "5 audio entries expected, got {names:?}");
        assert!(
            names
                .iter()
                .any(|n| n.eq_ignore_ascii_case("Loose.WAV")),
            "uppercase extension must be found: {names:?}"
        );
        assert!(
            names.iter().any(|n| n.ends_with("Alpha/Beta/Two.mp3")),
            "nested subdirectory must be walked: {names:?}"
        );
        assert!(
            !names.iter().any(|n| n.ends_with("notes.txt")),
            "non-audio must be filtered: {names:?}"
        );
    }

    #[test]
    fn scan_reads_tags_durations_and_year() {
        let (root, real) = make_library("tags");
        let tracks = collect_scan(&root);
        let store = {
            let mut s = TrackStore::default();
            s.merge(tracks.clone());
            s
        };
        assert_eq!(store.len(), 5, "every audio file must become a row");
        assert!(
            tracks.iter().any(|t| t.duration > 0.9),
            "container durations must come from lofty"
        );
        if !real {
            eprintln!("ffmpeg absent: tag assertions skipped");
            return;
        }
        let one = tracks
            .iter()
            .find(|t| t.path.to_string_lossy().ends_with("One.flac"))
            .expect("One.flac");
        assert_eq!(one.title, "One Song", "tag title must beat the stem");
        assert_eq!(one.artist.as_deref(), Some("Alpha Artist"));
        assert_eq!(one.album.as_deref(), Some("Alpha Album"));
        assert_eq!(one.genre.as_deref(), Some("Test Genre"));
        assert_eq!(one.year, Some(2021), "year from date/date-added tags");
        let loose = tracks
            .iter()
            .find(|t| t.path.to_string_lossy().eq_ignore_ascii_case(
                &root.join("Loose.WAV").to_string_lossy(),
            ))
            .expect("Loose.WAV");
        assert_eq!(loose.title, "Loose", "untagged file keeps its stem");
        assert_eq!(loose.artist, None);
        let broken = tracks
            .iter()
            .find(|t| t.path.to_string_lossy().ends_with("Broken.flac"))
            .expect("Broken.flac");
        assert_eq!(broken.title, "Broken", "unreadable file still listed");
        assert_eq!(broken.duration, 0.0);

        // case-insensitive artist merge: One (Alpha Artist) + Two (alpha
        // artist) + Three (Alpha Artist) = 3, plus the two bucket rows
        let artists = store.artist_groups();
        let alpha = artists
            .iter()
            .find(|g| g.name.eq_ignore_ascii_case("Alpha Artist"))
            .expect("merged artist");
        assert_eq!(alpha.count(), 3, "casing must not split an artist: {artists:?}");
        let unknown_a = artists
            .iter()
            .find(|g| g.name == UNKNOWN_ARTIST)
            .expect("unknown bucket");
        assert_eq!(unknown_a.count(), 2, "Loose + Broken");
        let albums = store.album_groups();
        let unknown_b = albums
            .iter()
            .find(|g| g.name == UNKNOWN_ALBUM)
            .expect("unknown album bucket");
        assert_eq!(unknown_b.count(), 3, "Three + Loose + Broken: {albums:?}");
        assert!(
            albums.iter().any(|g| g.name == "Alpha Album"),
            "album groups present: {albums:?}"
        );
        let genres = store.genres();
        assert_eq!(genres.len(), 2, "{genres:?}");
        assert_eq!(
            genres[0].to_lowercase(),
            "other genre",
            "A–Z case-insensitive: {genres:?}"
        );
    }

    #[test]
    fn scan_is_incremental_and_survives_junk_files() {
        let (root, _) = make_library("incremental");
        let mut store = TrackStore::default();
        let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded::<ScanCmd>();
        let (ev_tx, ev_rx) = crossbeam_channel::unbounded::<ScanEvent>();
        spawn_scanner(cmd_rx, ev_tx);
        cmd_tx
            .send(ScanCmd::Scan {
                roots: vec![root.clone()],
                replace: false,
            })
            .expect("scan cmd");
        let began = Instant::now();
        let mut batches = 0usize;
        let mut finished = false;
        let mut unreadable_seen = false;
        while Instant::now() - began < StdDur::from_secs(15) {
            while let Ok(ev) = ev_rx.try_recv() {
                match ev {
                    ScanEvent::Started { .. } => {}
                    ScanEvent::Batch { tracks } => {
                        batches += 1;
                        store.merge(tracks);
                    }
                    ScanEvent::Finished { unreadable, .. } => {
                        unreadable_seen = unreadable > 0;
                        finished = true;
                    }
                    ScanEvent::Error { message } => panic!("scan error: {message}"),
                }
            }
            if finished {
                break;
            }
            thread::sleep(StdDur::from_millis(20));
        }
        assert!(finished, "scanner must report Finished");
        assert_eq!(store.len(), 5, "streamed batches must cover the library");
        assert!(unreadable_seen, "Broken.flac must be counted unreadable");
        // second scan of the same tree adds nothing (dedupe by canonical path)
        cmd_tx
            .send(ScanCmd::Scan {
                roots: vec![root.clone()],
                replace: false,
            })
            .expect("rescan cmd");
        let mut again = false;
        while Instant::now() - began < StdDur::from_secs(15) {
            while let Ok(ev) = ev_rx.try_recv() {
                if let ScanEvent::Batch { tracks } = ev {
                    store.merge(tracks);
                } else if let ScanEvent::Finished { .. } = ev {
                    again = true;
                }
            }
            if again {
                break;
            }
            thread::sleep(StdDur::from_millis(20));
        }
        assert!(again, "rescan must complete");
        assert_eq!(store.len(), 5, "rescan must dedupe, not double rows");
        assert!(batches >= 1, "at least one streamed batch");
    }

    /// Empirical API probe kept as a regression: proves the lofty 0.25.5 read
    /// path this crate depends on (`read_from_path` → `properties().duration()`
    /// → `primary_tag()/first_tag()` → Accessor getters → `pictures()`), and
    /// whether lofty can *write* tags onto a hand-made WAV (only then are the
    /// tag-free fallback rows ever exercised by real fixtures).
    #[test]
    fn lofty_read_path_probe_and_wav_tag_writing() {
        let dir = workdir("lofty_probe");
        let p = dir.join("probe.wav");
        silent_wav(&p, 0.5);
        let out = read_track(&p);
        assert!(out.meta_ok, "lofty must parse a bare PCM WAV");
        assert!((out.track.duration - 0.5).abs() < 0.05, "duration probe");
        let tf = lofty::read_from_path(&p).expect("read");
        let ft = tf.file_type();
        let ty = [TagType::Id3v2, TagType::RiffInfo]
            .into_iter()
            .find(|t| ft.tag_support(*t).is_writable());
        match ty {
            Some(ty) => {
                tlog!("probe: WAV accepts writable tags via {ty:?}");
                let mut tag = Tag::new(ty);
                tag.set_artist("Probe Artist".to_string());
                tag.set_album("Probe Album".to_string());
                tag.set_genre("Probe Genre".to_string());
                tag.set_title("Probe Title".to_string());
                // discovered behavior (locked in below): lofty's ID3v2 writer
                // keeps the Accessor fields and the date fields, but silently
                // drops a text `ItemKey::Year`, so Grove reads years through
                // RecordingDate/ReleaseDate first and only then falls back.
                tag.insert_text(ItemKey::RecordingDate, "2001-07-08".to_string());
                tag.insert_text(ItemKey::Year, "1998".to_string());
                tag.save_to_path(&p, lofty::config::WriteOptions::default())
                    .expect("save tags to WAV");
                let after = read_track(&p);
                assert_eq!(after.track.artist.as_deref(), Some("Probe Artist"));
                assert_eq!(after.track.title, "Probe Title", "tag beats stem");
                assert_eq!(after.track.genre.as_deref(), Some("Probe Genre"));
                assert_eq!(
                    after.track.year,
                    Some(2001),
                    "year must come from the field lofty actually writes"
                );
                let back = lofty::read_from_path(&p).expect("re-read");
                let t = back.primary_tag().or_else(|| back.first_tag());
                tlog!(
                    "probe round-trip · year_key={:?} recording_date={:?} date={:?}",
                    t.and_then(|x| x.get_string(ItemKey::Year).map(|s| s.to_string())),
                    t.and_then(|x| x.get_string(ItemKey::RecordingDate).map(|s| s.to_string())),
                    t.and_then(|x| x.date().map(|d| d.year))
                );
            }
            None => {
                tlog!(
                    "probe: WAV has no writable tag type in lofty 0.25.5 ({:?} · id3={:?} info={:?})",
                    ft,
                    ft.tag_support(TagType::Id3v2),
                    ft.tag_support(TagType::RiffInfo)
                );
            }
        }
    }
}
