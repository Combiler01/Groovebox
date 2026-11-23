//! Channel contract types (Cmd/Event + the M3 Scan lane), track model, the
//! library store, and shared constants.
//!
//! M1 fixed the Cmd/Event shape, M2 consumed it for queue semantics; M3 adds the
//! library data model that the lofty scan fills: `TrackInfo`, `TrackStore`
//! (dedupe + search/sort/genre/grouping) and the `ScanCmd`/`ScanEvent` contract
//! between the scan worker thread and the UI. Store logic is pure (no IO, no
//! audio) so it is unit-testable in-process.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

/// Relative timestamp base for app logs: initialized on the first log line.
pub(crate) static BOOT: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();

/// `println!` with `[grove]` prefix + seconds since app init, so playback
/// windows are unambiguous in soak logs (idle-CPU attribution).
macro_rules! tlog {
    ($($arg:tt)*) => {
        println!(
            "[grove]{:>8.2}s · {}",
            $crate::model::BOOT
                .get_or_init(std::time::Instant::now)
                .elapsed()
                .as_secs_f32(),
            format_args!($($arg)*)
        );
    };
}
pub(crate) use tlog;

/// File extensions the library accepts (M3 scan + M1 drag-drop filter).
pub const AUDIO_EXTS: [&str; 6] = ["mp3", "flac", "ogg", "m4a", "aac", "wav"];

/// Groove's buckets for files with no artist/album tag.
pub const UNKNOWN_ARTIST: &str = "Unknown artist";
pub const UNKNOWN_ALBUM: &str = "Unknown album";

/// Display name for a track (transport label, queue rows, NowPlaying events).
pub fn stem(p: &Path) -> String {
    p.file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "unknown".to_string())
}

/// `2:08`-style timestamp used by the transport bar.
pub fn fmt_mmss(t: f32) -> String {
    let t = t.max(0.0) as i64;
    format!("{:0}:{:02}", t / 60, t % 60)
}

/// True when the path's extension is one of `AUDIO_EXTS` (case-insensitive).
pub fn is_audio(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .map(|e| AUDIO_EXTS.iter().any(|x| e.eq_ignore_ascii_case(x)))
        .unwrap_or(false)
}

// ── UI → audio worker ────────────────────────────────────────────────────────
#[derive(Debug, Clone)]
pub enum Cmd {
    Load {
        tracks: Vec<PathBuf>,
        play_index: usize,
    },
    Play,
    Pause,
    Stop,
    Next,
    Prev,
    /// Absolute seek target in seconds (UI computes it from its optimistic
    /// position — the engine clamps against the real duration).
    Seek(f32),
    SetVolume(f32),
    #[allow(dead_code)] // UI EQ sliders send this from M7
    SetEq([f32; 3]),
    #[allow(dead_code)] // device switcher sends this from M7 (reuses rebuild path)
    SetDevice(usize),
    ShuffleMode(ShuffleMode),
    RepeatMode(RepeatMode),
}

/// Audio worker → UI, drained non-blocking every frame.
#[derive(Debug, Clone)]
pub enum Event {
    /// Playback position + track duration (duration = 0.0 when the container
    /// does not expose one) + live playing state from the engine.
    Position {
        elapsed: f32,
        duration: f32,
        playing: bool,
    },
    /// The engine has begun playing `index` (`name` = file stem) — load,
    /// auto-advance, user skip, prev, and post-recovery resume all announce here.
    NowPlaying {
        index: usize,
        name: String,
    },
    /// Idle keep-alive beat.
    Tick,
    /// A track finished (natural EOF advance, user skip, terminal stop, or
    /// Stop cmd) — the last announced index.
    TrackEnded {
        index: usize,
    },
    /// Output device unavailable / decode failure / other engine trouble.
    DeviceError {
        message: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShuffleMode {
    Off,
    On,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepeatMode {
    None,
    One,
    All,
}

// ── UI → scan worker (M3) ────────────────────────────────────────────────────
/// Library building runs on its own thread: a folder walk + lofty read of
/// thousands of files must never touch the UI frame loop or the audio worker.
#[derive(Debug, Clone)]
pub enum ScanCmd {
    /// Walk `roots` (folders expand recursively, loose audio files read
    /// directly) and stream `TrackInfo` batches back. `replace` clears the
    /// library on the first `Started` — M7's "Choose where we look" re-scan.
    Scan { roots: Vec<PathBuf>, replace: bool },
}

/// Scan worker → UI. Batches are streamed so a big library appears progressively
/// (Groove fills the list while it keeps scanning); the UI dedupes on merge.
#[derive(Debug, Clone)]
pub enum ScanEvent {
    Started {
        roots: Vec<PathBuf>,
        replace: bool,
    },
    Batch {
        tracks: Vec<TrackInfo>,
    },
    Finished {
        roots: Vec<PathBuf>,
        /// directory entries the walker looked at
        inspected: u32,
        /// audio entries emitted
        emitted: u32,
        /// audio entries whose tags/properties lofty could not read
        unreadable: u32,
        ms: u128,
    },
    Error {
        message: String,
    },
}

/// One library entry: `path` + everything lofty can read, with tag-free
/// fallbacks so unplayable/untagged files still show up (Groove lists them).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TrackInfo {
    pub path: PathBuf,
    pub title: String,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub genre: Option<String>,
    pub year: Option<i32>,
    pub duration: f32,
    /// "Date added" sort key = file mtime (unix secs). Windows creation time is
    /// still unstable in std (`file_create_time`), so mtime is the pinned choice.
    pub date_added: i64,
    /// Tag carries embedded cover art (decoded to an egui texture in M5).
    pub has_art: bool,
}

impl TrackInfo {
    /// Artist with Groove's fallback bucket.
    pub fn artist_or_unknown(&self) -> &str {
        self.artist.as_deref().unwrap_or(UNKNOWN_ARTIST)
    }

    pub fn album_or_unknown(&self) -> &str {
        self.album.as_deref().unwrap_or(UNKNOWN_ALBUM)
    }
}

/// Toolbar sort options ("Sort by: Date added" flyout).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortKey {
    DateAdded,
    Title,
    Artist,
    Album,
    Year,
}

impl SortKey {
    /// Order the flyout presents, and the value `next()` walks on repeated clicks
    /// (Groove's default is Date added).
    pub const CYCLE: [SortKey; 5] = [
        SortKey::DateAdded,
        SortKey::Title,
        SortKey::Artist,
        SortKey::Album,
        SortKey::Year,
    ];

    /// Sort-cycle step: clicking an already-active column header walks
    /// `CYCLE` (Groove's default is Date added).
    pub fn next(self) -> Self {
        let i = Self::CYCLE.iter().position(|k| *k == self).unwrap_or(0);
        Self::CYCLE[(i + 1) % Self::CYCLE.len()]
    }

    pub fn label(self) -> &'static str {
        match self {
            SortKey::DateAdded => "Date added",
            SortKey::Title => "Title",
            SortKey::Artist => "Artist",
            SortKey::Album => "Album",
            SortKey::Year => "Year",
        }
    }
}

/// The live Songs view: search text + sort + genre filter (`None` = All genres)
/// + the "Not finding everything?" filter (only files lofty could not index).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query {
    pub search: String,
    pub sort: SortKey,
    pub genre: Option<String>,
    pub unindexed_only: bool,
}

impl Query {
    pub fn new() -> Self {
        Self {
            search: String::new(),
            sort: SortKey::DateAdded,
            genre: None,
            unindexed_only: false,
        }
    }
}

/// Groove's default library order is Date added, so `Default` is explicit
/// (`SortKey` itself has no natural zero value worth deriving).
impl Default for Query {
    fn default() -> Self {
        Self::new()
    }
}

/// One artist or album bucket, ready for the A–Z grid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    /// display name (most-common casing wins)
    pub name: String,
    /// A–Z section header letter
    pub letter: char,
    /// library indices, title-sorted inside the group
    pub members: Vec<u32>,
    /// any member carries embedded cover art
    pub has_art: bool,
}

impl Group {
    pub fn count(&self) -> usize {
        self.members.len()
    }
}

/// The scanned library: insertion-ordered tracks + a path index for dedupe + a
/// cached filtered/sorted row view. Pure logic: no IO, no audio, no channels.
#[derive(Debug, Default)]
pub struct TrackStore {
    pub tracks: Vec<TrackInfo>,
    /// dedupe key → track index (rebuilt after a removal, which shifts indices)
    index: HashMap<String, u32>,
    rev: u64,
    cached: Option<(u64, Query, Vec<u32>)>,
}

/// What a merge did, for the status line.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct MergeStat {
    pub new: usize,
    pub dup: usize,
    pub updated: usize,
}

impl TrackStore {
    pub fn len(&self) -> usize {
        self.tracks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tracks.is_empty()
    }

    /// Bumped by every mutation that changes a derived view (merge, remove,
    /// clear). The UI caches its row view, genre list and A–Z groups against
    /// this, so a 20 Hz repaint recomputes only when the library actually moved.
    pub fn revision(&self) -> u64 {
        self.rev
    }

    /// Stable key for dedupe: canonical path (resolves case + relative + 8.3
    /// variants on Windows) lowercased; falls back to the raw path when
    /// canonicalize fails (deleted while scanning, network ghosts, …).
    fn dedupe_key(path: &Path) -> String {
        path.canonicalize()
            .unwrap_or_else(|_| path.to_path_buf())
            .to_string_lossy()
            .to_lowercase()
    }

    /// Fold a scan batch in. A repeat sighting of a known path refreshes its
    /// metadata (the file may have been retagged between scans) instead of
    /// inserting a duplicate row.
    pub fn merge(&mut self, batch: Vec<TrackInfo>) -> MergeStat {
        let mut stat = MergeStat::default();
        for t in batch {
            let key = Self::dedupe_key(&t.path);
            match self.index.get(&key).copied() {
                Some(i) => {
                    stat.dup += 1;
                    let i = i as usize;
                    if self.tracks[i] != t {
                        self.tracks[i] = t;
                        stat.updated += 1;
                        self.rev += 1;
                    }
                }
                None => {
                    self.index.insert(key, self.tracks.len() as u32);
                    self.tracks.push(t);
                    stat.new += 1;
                }
            }
        }
        if stat.new > 0 {
            self.rev += 1;
        }
        stat
    }

    /// ⋮ row action: drop one entry from the library (file untouched).
    pub fn remove(&mut self, path: &Path) -> bool {
        let key = Self::dedupe_key(path);
        let i = match self.index.remove(&key) {
            Some(i) => i as usize,
            None => return false,
        };
        if i < self.tracks.len() {
            self.tracks.remove(i);
            self.rebuild_index();
            self.rev += 1;
        }
        true
    }

    fn rebuild_index(&mut self) {
        self.index.clear();
        for (i, t) in self.tracks.iter().enumerate() {
            self.index
                .insert(Self::dedupe_key(&t.path), i as u32);
        }
    }

    pub fn clear(&mut self) {
        self.tracks.clear();
        self.index.clear();
        self.cached = None;
        self.rev += 1;
    }

    /// Index of a library entry by path (row highlighting, ⋮ actions).
    pub fn index_of(&self, path: &Path) -> Option<usize> {
        self.index.get(&Self::dedupe_key(path)).map(|i| *i as usize)
    }

    /// Every path in insertion order (Shuffle-all queue).
    pub fn paths(&self) -> Vec<PathBuf> {
        self.tracks.iter().map(|t| t.path.clone()).collect()
    }

    /// Queue for a view: paths in exactly the order the user sees them, so
    /// Next/Prev follow the filtered+sorted list (Groove queues the current view).
    pub fn paths_for(&self, rows: &[u32]) -> Vec<PathBuf> {
        rows.iter()
            .map(|&i| self.tracks[i as usize].path.clone())
            .collect()
    }

    /// Genre filter dropdown contents: unique (case-insensitive), display casing
    /// of the first sighting, A–Z.
    pub fn genres(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for t in &self.tracks {
            if let Some(g) = &t.genre {
                if g.trim().is_empty() {
                    continue;
                }
                let k = g.trim().to_lowercase();
                if seen.insert(k) {
                    out.push(g.trim().to_string());
                }
            }
        }
        out.sort_by(|a, b| a.to_lowercase().cmp(&b.to_lowercase()));
        out
    }

    /// Artist pages (A–Z). Album/artist names group case-insensitively.
    pub fn artist_groups(&self) -> Vec<Group> {
        self.group_by(|t| t.artist_or_unknown().to_string())
    }

    pub fn album_groups(&self) -> Vec<Group> {
        self.group_by(|t| t.album_or_unknown().to_string())
    }

    fn group_by(&self, key_of: impl Fn(&TrackInfo) -> String) -> Vec<Group> {
        let mut order: Vec<(String, Group)> = Vec::new();
        let mut slot: HashMap<String, usize> = HashMap::new();
        for (i, t) in self.tracks.iter().enumerate() {
            let key = key_of(t).to_lowercase();
            let s = match slot.get(&key).copied() {
                Some(s) => s,
                None => {
                    let display = key_of(t); // first sighting's casing
                    order.push((
                        key.clone(),
                        Group {
                            name: display.clone(),
                            letter: section_letter(&display.to_lowercase()),
                            members: Vec::new(),
                            has_art: false,
                        },
                    ));
                    slot.insert(key, order.len() - 1);
                    order.len() - 1
                }
            };
            let g = &mut order[s].1;
            g.members.push(i as u32);
            g.has_art |= t.has_art;
        }
        for (_, g) in order.iter_mut() {
            g.members.sort_by_key(|i| {
                let t = &self.tracks[*i as usize];
                (t.title.to_lowercase(), t.artist_or_unknown().to_lowercase())
            });
        }
        let mut groups: Vec<Group> = order.into_iter().map(|(_, g)| g).collect();
        // Groove's A–Z runs A…Z then `#`; `'{'` ranks the non-letter bucket last
        // without changing the letter the section header shows.
        let rank = |c: char| if c == '#' { '{' } else { c };
        groups.sort_by_key(|g| (rank(g.letter), g.name.to_lowercase()));
        groups
    }

    /// Row indices for the current query, cached until the library or the
    /// query changes (a 20 Hz repaint must not re-sort on every frame).
    pub fn rows_for(&mut self, q: &Query) -> &[u32] {
        let fresh = self
            .cached
            .as_ref()
            .is_some_and(|(rev, cq, _)| *rev == self.rev && cq == q);
        if !fresh {
            let rows = self.build_rows(q);
            self.cached = Some((self.rev, q.clone(), rows));
        }
        &self.cached.as_ref().expect("filled above").2
    }

    /// The library row for a file path, if the library has it. Recent rows and
    /// the NowPlaying page store *paths* rather than indices, because
    /// [`TrackStore::remove`] shifts every index below the removed track.
    pub fn find_by_path(&self, path: &Path) -> Option<&TrackInfo> {
        self.tracks
            .iter()
            .find(|t| t.path == path)
    }

    fn build_rows(&self, q: &Query) -> Vec<u32> {
        let needle = q.search.trim().to_lowercase();
        let mut keyed: Vec<(i64, String, String, String, usize)> = Vec::with_capacity(self.len());
        for (i, t) in self.tracks.iter().enumerate() {
            // "Not finding everything?" filter: only the rows lofty could not
            // read (zero duration = the tag/duration probe failed)
            if q.unindexed_only && t.duration > 0.0 {
                continue;
            }
            if let Some(g) = &q.genre {
                let same = t
                    .genre
                    .as_deref()
                    .map(|x| x.trim().eq_ignore_ascii_case(g.as_str()))
                    .unwrap_or(false);
                if !same {
                    continue;
                }
            }
            if !needle.is_empty() {
                let hay = format!(
                    "{}\u{1f}{}\u{1f}{}\u{1f}{}",
                    t.title,
                    t.artist_or_unknown(),
                    t.album_or_unknown(),
                    t.genre.clone().unwrap_or_default()
                )
                .to_lowercase();
                if !hay.contains(&needle) {
                    continue;
                }
            }
            keyed.push(self.sort_terms(q, i));
        }
        keyed.sort_by(
            |a, b| (a.0, &a.1, &a.2, &a.3, a.4).cmp(&(b.0, &b.1, &b.2, &b.3, b.4)),
        );
        keyed.into_iter().map(|(_, _, _, _, i)| i as u32).collect()
    }

    /// Precomputed sort key for one row: `(num, k1, k2, k3)` — lowercase so the
    /// comparator allocates nothing, `num` first for newest-first orders.
    fn sort_terms(&self, q: &Query, i: usize) -> (i64, String, String, String, usize) {
        let t = &self.tracks[i];
        let ci = |s: &str| s.to_lowercase();
        match q.sort {
            SortKey::DateAdded => (
                -t.date_added,
                t.title.to_lowercase(),
                t.artist_or_unknown().to_lowercase(),
                String::new(),
                i,
            ),
            SortKey::Title => (
                0,
                t.title.to_lowercase(),
                t.artist_or_unknown().to_lowercase(),
                t.album_or_unknown().to_lowercase(),
                i,
            ),
            SortKey::Artist => (
                0,
                ci(t.artist_or_unknown()),
                ci(t.album_or_unknown()),
                ci(&t.title),
                i,
            ),
            SortKey::Album => (
                0,
                ci(t.album_or_unknown()),
                ci(t.artist_or_unknown()),
                ci(&t.title),
                i,
            ),
            SortKey::Year => (
                -i64::from(t.year.unwrap_or(0)),
                ci(t.artist_or_unknown()),
                ci(&t.title),
                String::new(),
                i,
            ),
        }
    }
}

/// A–Z letter for a *normalized* (lowercase) name; `#` sorts last in Groove.
pub fn section_letter(name_lower: &str) -> char {
    match name_lower
        .chars()
        .next()
        .map(|c| c.to_ascii_uppercase())
        .filter(|c| c.is_ascii_alphabetic())
    {
        Some(c) => c,
        None => '#',
    }
}

/// CentralPanel pages (M4 shell): Library is the real music library, Recent
/// keeps played history, NowPlaying shows the current queue head,
/// Playlists is the empty state until M8, Settings holds folder management +
/// the stubs M6/M7/M8 fill in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Page {
    Library,
    Recent,
    NowPlaying,
    Playlists,
    Settings,
}

/// My music tabs (M3: Songs list + Artists/Albums grids).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Songs,
    Artists,
    Albums,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A library row built in code (no IO): `title` doubles as the path stem,
    /// so every fixture row has a distinct path.
    fn song(
        title: &str,
        artist: Option<&str>,
        album: Option<&str>,
        genre: Option<&str>,
        year: Option<i32>,
        added: i64,
    ) -> TrackInfo {
        TrackInfo {
            path: PathBuf::from(format!("lib/{title}.flac")),
            title: title.to_string(),
            artist: artist.map(String::from),
            album: album.map(String::from),
            genre: genre.map(String::from),
            year,
            duration: 200.0,
            date_added: added,
            has_art: false,
        }
    }

    fn store_with(rows: Vec<TrackInfo>) -> TrackStore {
        let mut s = TrackStore::default();
        s.merge(rows);
        s
    }

    fn titles(store: &mut TrackStore, q: &Query) -> Vec<String> {
        let rows: Vec<u32> = store.rows_for(q).to_vec();
        rows.into_iter().map(|i| store.tracks[i as usize].title.clone()).collect()
    }

    #[test]
    fn default_view_is_newest_first_and_every_sort_is_case_insensitive() {
        let mut s = store_with(vec![
            song("Older", Some("Alpha Artist"), None, None, Some(2001), 10),
            song("Newest", Some("beta artist"), None, None, Some(2010), 30),
            song("Middle", Some("Gamma"), None, None, Some(2005), 20),
            song("aaa", Some("Zeta"), None, None, Some(1999), 40),
        ]);
        let q = Query::new();
        assert_eq!(q.sort, SortKey::DateAdded, "Groove default sort");
        assert_eq!(
            titles(&mut s, &q),
            vec!["aaa", "Newest", "Middle", "Older"],
            "Date added = newest first (aaa is the newest by mtime)"
        );
        let by_title = Query {
            sort: SortKey::Title,
            ..Query::new()
        };
        assert_eq!(
            titles(&mut s, &by_title),
            vec!["aaa", "Middle", "Newest", "Older"],
            "title sort must not put uppercase before lowercase"
        );
        let by_artist = Query {
            sort: SortKey::Artist,
            ..Query::new()
        };
        assert_eq!(
            titles(&mut s, &by_artist),
            vec!["Older", "Newest", "Middle", "aaa"],
            "artist sort: alpha < beta < gamma < zeta, casing irrelevant"
        );
        let by_year = Query {
            sort: SortKey::Year,
            ..Query::new()
        };
        assert_eq!(
            titles(&mut s, &by_year),
            vec!["Newest", "Middle", "Older", "aaa"],
            "year sort = newest release first"
        );
        // sort cycle walks the flyout order and wraps
        assert_eq!(SortKey::DateAdded.next(), SortKey::Title);
        assert_eq!(SortKey::Year.next(), SortKey::DateAdded);
        assert_eq!(SortKey::DateAdded.label(), "Date added");
    }

    #[test]
    fn search_and_genre_filter_the_view() {
        let mut s = store_with(vec![
            song("One", Some("Alpha Artist"), Some("Alpha Album"), Some("Test Genre"), None, 1),
            song("Two", Some("Beta Artist"), None, Some("other genre"), None, 2),
            song("Three", Some("Alpha Artist"), None, None, None, 3),
        ]);
        let mut find = |text: &str| {
            let q = Query {
                search: text.to_string(),
                ..Query::new()
            };
            titles(&mut s, &q)
        };
        assert_eq!(find("alpha"), vec!["Three", "One"], "artist match, newest first");
        assert_eq!(find("ALPHA ALBUM"), vec!["One"], "album match, case-insensitive");
        assert_eq!(find("two"), vec!["Two"], "title match");
        assert_eq!(find("nope"), Vec::<String>::new(), "no match = empty view");
        assert_eq!(
            find("  th  "),
            vec!["Three", "Two"],
            "query whitespace is trimmed; Two also matches its 'other genre' tag"
        );
        let genre = Query {
            genre: Some("Test Genre".to_string()),
            ..Query::new()
        };
        assert_eq!(
            titles(&mut s, &genre),
            vec!["One"],
            "genre filter keeps only the tagged row"
        );
    }

    #[test]
    fn genre_filter_matches_case_insensitively_and_genre_list_is_unique_sorted() {
        let mut s = store_with(vec![
            song(
                "One",
                Some("Alpha"),
                None,
                Some("Test Genre"),
                None,
                1,
            ),
            song(
                "Two",
                Some("Beta"),
                None,
                Some("other genre"),
                None,
                2,
            ),
            song(
                "Three",
                Some("Gamma"),
                None,
                Some("test genre "), // duplicate of the first modulo casing + padding
                None,
                3,
            ),
            song("Four", Some("Delta"), None, Some("   "), None, 4), // blank = no genre
        ]);
        let g = Query {
            genre: Some("TEST GENRE".to_string()),
            ..Query::new()
        };
        assert_eq!(
            titles(&mut s, &g),
            vec!["Three", "One"],
            "genre filter ignores casing + trailing padding"
        );
        let genres = s.genres();
        assert_eq!(genres.len(), 2, "blank genres never appear: {genres:?}");
        assert_eq!(genres[0], "other genre", "A–Z ignoring case: {genres:?}");
        assert_eq!(genres[1], "Test Genre", "first-seen display casing kept");
    }

    #[test]
    fn groups_merge_casing_and_split_into_az_letters() {
        let s = store_with(vec![
            song("Zebra", Some("Alpha Artist"), Some("Alpha Album"), None, None, 1),
            song("apple", Some("alpha artist"), Some("alpha album"), None, None, 2),
            song("Mid", Some("Zeta"), Some("Zeta Album"), None, None, 3),
            song("Num", Some("1234 Band"), None, None, None, 4),
            song("NoTag", None, None, None, None, 5),
        ]);
        let artists = s.artist_groups();
        let view: Vec<(char, String, usize)> = artists
            .iter()
            .map(|g| (g.letter, g.name.clone(), g.count()))
            .collect();
        assert_eq!(
            view,
            vec![
                ('A', "Alpha Artist".to_string(), 2), // "Alpha Artist" + "alpha artist"
                ('U', UNKNOWN_ARTIST.to_string(), 1),
                ('Z', "Zeta".to_string(), 1),
                ('#', "1234 Band".to_string(), 1), // non-letter starts land in the last bucket
            ],
            "A–Z order, casing-merged, # last"
        );
        let alpha = artists.iter().find(|g| g.name == "Alpha Artist").expect("merged");
        assert_eq!(
            alpha
                .members
                .iter()
                .map(|i| s.tracks[*i as usize].title.clone())
                .collect::<Vec<String>>(),
            vec!["apple", "Zebra"],
            "group members sort by title, case-insensitive"
        );
        let albums = s.album_groups();
        assert_eq!(
            albums
                .iter()
                .map(|g| (g.letter, g.name.clone(), g.count()))
                .collect::<Vec<_>>(),
            vec![
                ('A', "Alpha Album".to_string(), 2),
                ('U', UNKNOWN_ALBUM.to_string(), 2), // untagged: Num + NoTag
                ('Z', "Zeta Album".to_string(), 1),
            ],
            "albums group the same way, first-seen casing wins"
        );
    }

    #[test]
    fn merge_dedupes_the_same_file_and_refreshes_stale_tags() {
        let dir = std::env::temp_dir().join("grove_m3_store").join("dedupe");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(dir.join("Song.mp3"), b"dummy").expect("write");
        let exact = dir.join("Song.mp3");
        let other_case = dir.join("SONG.mp3");
        let relative = dir.join(".").join("Song.mp3");
        let mut s = TrackStore::default();
        s.merge(vec![
            song("Song", Some("First"), None, None, None, 1), // path overwritten below
        ]);
        // three spellings of the same existing file → one row
        let mut a = song("Song", Some("First"), None, None, None, 1);
        a.path = exact.clone();
        let mut b = a.clone();
        b.path = other_case.clone();
        let mut c = a.clone();
        c.path = relative.clone();
        let stat = s.merge(vec![a, b, c]);
        assert_eq!(stat.new, 1, "first sighting inserts");
        assert_eq!(stat.dup, 2, "case + relative spellings dedupe");
        assert_eq!(s.len(), 2, "the seeded row + Song");
        // retagged between scans: same path, better metadata → refreshed, not doubled
        let mut retagged = song("Song (Remix)", Some("First"), Some("Album"), Some("Genre"), Some(2020), 1);
        retagged.path = other_case;
        let stat2 = s.merge(vec![retagged]);
        assert_eq!(stat2.new, 0);
        assert_eq!(stat2.updated, 1, "known path refreshes in place");
        assert_eq!(s.index_of(&exact).map(|i| s.tracks[i].title.clone()), Some("Song (Remix)".to_string()));
        assert_eq!(s.len(), 2, "still no duplicate row");
    }

    #[test]
    fn remove_takes_one_row_and_the_cache_follows_every_mutation() {
        let mut s = store_with(vec![
            song("One", Some("Alpha"), None, None, None, 1),
            song("Two", Some("Beta"), None, None, None, 2),
            song("Three", Some("Gamma"), None, None, None, 3),
        ]);
        let q = Query::new();
        let first = s.rows_for(&q).to_vec();
        let again = s.rows_for(&q).to_vec();
        assert_eq!(first, again, "unchanged query hits the cache");
        assert_eq!(first.len(), 3);
        assert!(
            s.remove(&PathBuf::from("lib/Two.flac")),
            "remove must find the row"
        );
        assert!(!s.remove(&PathBuf::from("lib/Ghost.flac")), "unknown path = false");
        let after = s.rows_for(&q).to_vec();
        assert_eq!(after.len(), 2, "cache must invalidate on mutation");
        assert_eq!(
            after.into_iter().map(|i| s.tracks[i as usize].title.clone()).collect::<Vec<_>>(),
            vec!["Three", "One"],
            "newest first, removed row gone, indices consistent"
        );
        // the surviving rows' indices must still resolve to the right files
        assert_eq!(s.index_of(&PathBuf::from("lib/One.flac")), Some(0));
        assert_eq!(s.index_of(&PathBuf::from("lib/Three.flac")), Some(1));
    }

    /// The "Not finding everything?" banner's link: show only the files lofty
    /// could not read (`duration <= 0` is exactly what the scan worker leaves
    /// behind when probing fails), so the list never invents rows.
    #[test]
    fn unindexed_only_shows_the_rows_tagging_failed_on() {
        let mut unreadable = song("Broken", None, None, None, None, 9);
        unreadable.duration = 0.0; // lofty could not probe this one
        let mut s = store_with(vec![
            song("Fine", Some("Alpha"), None, None, None, 1),
            unreadable,
            song("Also", Some("Beta"), None, None, None, 2),
        ]);
        let q = Query::new();
        assert_eq!(titles(&mut s, &q).len(), 3, "the plain view keeps every row");
        let flag = Query {
            unindexed_only: true,
            ..Query::new()
        };
        assert_eq!(titles(&mut s, &flag), vec!["Broken"], "only the probe failure");
        // combined with search, the filters must AND together like Groove's
        let both = Query {
            search: "b".to_string(),
            unindexed_only: true,
            ..Query::new()
        };
        assert_eq!(titles(&mut s, &both), vec!["Broken"]);
        let none_left = Query {
            search: "fine".to_string(),
            unindexed_only: true,
            ..Query::new()
        };
        assert_eq!(
            titles(&mut s, &none_left),
            Vec::<String>::new(),
            "a searchable row is by definition indexed"
        );
    }

    /// Recent + NowPlaying store paths, not row indices, so a rescan or a
    /// deleted file can never make them point at the wrong song.
    #[test]
    fn find_by_path_tracks_rows_across_index_shifts() {
        let mut s = store_with(vec![
            song("One", Some("Alpha"), None, None, None, 1),
            song("Two", Some("Beta"), None, None, None, 2),
            song("Three", Some("Gamma"), None, None, None, 3),
        ]);
        let p = PathBuf::from("lib/Three.flac");
        assert_eq!(s.find_by_path(&p).map(|t| t.title.clone()), Some("Three".to_string()));
        assert_eq!(s.find_by_path(&PathBuf::from("lib/Ghost.flac")), None);
        // removing the first row shifts everything: the path lookup must follow
        assert!(s.remove(&PathBuf::from("lib/One.flac")));
        assert_eq!(s.find_by_path(&p).map(|t| t.artist_or_unknown()), Some("Gamma"));
        assert_eq!(
            s.index_of(&p),
            Some(1),
            "index moved from 2 to 1; path lookup is the stable handle"
        );
    }
}

