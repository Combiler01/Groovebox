// Grove — Groove Music, rebuilt in Rust. Audio worker thread (M2).
//
// The worker owns the sink AND the playback state machine:
//   - queue ordering: identity order or a fresh shuffle permutation (played
//     track stays first), Repeat None/All/One, successor differs for
//     auto-advance vs user-initiated moves (Repeat One re-loops only on auto)
//   - gapless: after every track starts, its scheduled successor is decoded
//     and appended as lookahead; natural EOF just promotes it (zero gap). If
//     the order/repeat changed since (or the lookahead never decoded), the
//     successor is decoded fresh instead — correctness over gaplessness, like
//     Groove does when you edit the queue.
//   - EOF detection: elapsed >= duration - tol, 3-tick debounce, where
//     duration = container total_duration() captured at decode time (known
//     before append, so lookahead durations come free). Duration-unknown files
//     fall back to 6 consecutive ticks of Player::empty(). rodio's 5 ms
//     periodic position callback makes get_pos() follow queue advances.
//   - Next = Player::skip_one (queued lookahead takes over mid-stream,
//     gapless) unless stale/Repeat-One (then clear + fresh decode);
//     Prev = restart current if elapsed > 3s else previous track (Groove).
//   - Stream faults (cpal ErrorKind StreamInvalidated/DeviceNotAvailable/
//     HostUnavailable): the worker rebuilds the sink itself, re-connects the
//     Player, resumes the SAME track at last_pos (cooldown coalesces storms).
//     DeviceChanged/Xrun self-heal per cpal docs · no rebuild. Load's
//     retry-open-forever replays through this same path — which is also what
//     the M7 device switcher will use (with_device + rebuild).
// The UI thread communicates ONLY through crossbeam channels (Cmd in, Event out).

use std::fs::File;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration as StdDur, Instant, SystemTime, UNIX_EPOCH};


use rodio::cpal::traits::{DeviceTrait, HostTrait};
use rodio::{
    CpalError, CpalErrorKind, Decoder, DeviceSinkBuilder, DeviceSinkError, Float, MixerDeviceSink,
    Player, Source,
};

use crate::model::{tlog, Cmd, Event, RepeatMode, ShuffleMode, stem};

/// Heartbeat cadence for the soak: one `beat` line + `Event::Tick` every 10 s.
const BEAT: StdDur = StdDur::from_secs(10);

pub fn spawn_player(
    cmd_rx: crossbeam_channel::Receiver<Cmd>,
    ev_tx: crossbeam_channel::Sender<Event>,
    fault_rx: crossbeam_channel::Receiver<CpalError>,
    fault_tx: crossbeam_channel::Sender<CpalError>,
) {
    thread::spawn(move || {
        // M1-proven boot rule: never fail, keep retrying the sink every 3 s;
        // the UI stays responsive and any Load sits queued until recovery.
        loop {
            match open_grove_sink(&fault_tx) {
                Ok(sink) => {
                    let mut engine = Engine::with_sink(sink, cmd_rx, ev_tx, fault_rx, fault_tx);
                    engine.run();
                    return;
                }
                Err(e) => {
                    tlog!("sink open failed · {e} · retry in 3s");
                    ev_tx
                        .send(Event::DeviceError {
                            message: format!("no audio output yet: {e} · retrying in 3s"),
                        })
                        .ok();
                    thread::sleep(StdDur::from_secs(3));
                }
            }
        }
    });
}

struct Engine {
    sink: MixerDeviceSink,
    player: Player,
    cmd_rx: crossbeam_channel::Receiver<Cmd>,
    ev_tx: crossbeam_channel::Sender<Event>,
    fault_rx: crossbeam_channel::Receiver<CpalError>,
    fault_tx: crossbeam_channel::Sender<CpalError>,
    last_rebuild: Instant,
    last_beat: Instant,
    last_advance: Instant, // any engine-side advance; natural splice within ~1s
                           // of a window promote is EXPECTED, not a desync
    vol: Float,

    // queue + state machine
    tracks: Vec<PathBuf>, // paths, as sent by Load
    order: Vec<usize>,   // permutation of 0..len(); identity when unshuffled
    order_pos: usize,    // pointer into `order` for the current track
    cur_dur: f64,        // audible track duration in s; 0 = container unknown
    lookahead_pos: Option<usize>, // queued successor order-position
    lookahead_idx: Option<usize>, // queued successor *track index* — survives
                                  // order rebuilds (position may remap after a
                                  // shuffle toggle; the queued audio is fixed)
    lookahead_ok: bool,  // false when order/repeat changed since it was queued
    next_dur: f64,       // successor duration captured at its decode
    shuffle: bool,
    repeat: RepeatMode,
    eof_hits: u8, // debounce: consecutive ticks meeting the EOF condition
    terminated: bool, // queue consumed (Repeat None end); suppresses re-chatter
    pos_guard: u8,  // ticks right after a start/promotion where get_pos() still
                    // carries the dead source's end position — EOF must not fire
    last_pos: StdDur, // live position, kept for seek / fault-resume
    pending: Option<Cmd>, // last Load, replayed after sink recovery
}

impl Engine {
    fn with_sink(
        sink: MixerDeviceSink,
        cmd_rx: crossbeam_channel::Receiver<Cmd>,
        ev_tx: crossbeam_channel::Sender<Event>,
        fault_rx: crossbeam_channel::Receiver<CpalError>,
        fault_tx: crossbeam_channel::Sender<CpalError>,
    ) -> Self {
        Self {
            player: Player::connect_new(sink.mixer()),
            sink,
            cmd_rx,
            ev_tx,
            fault_rx,
            fault_tx,
            last_rebuild: Instant::now(),
            last_beat: Instant::now(),
            last_advance: Instant::now(),
            vol: 1.0,
            tracks: Vec::new(),
            order: Vec::new(),
            order_pos: 0,
            cur_dur: 0.0,
            lookahead_pos: None,
            lookahead_idx: None,
            lookahead_ok: false,
            next_dur: 0.0,
            shuffle: false,
            repeat: RepeatMode::None,
            eof_hits: 0,
            terminated: false,
            pos_guard: 0,
            last_pos: StdDur::ZERO,
            pending: None,
        }
    }

    #[inline]
    fn send(&self, ev: Event) {
        self.ev_tx.send(ev).ok();
    }

    fn queue_len(&self) -> usize {
        self.order.len()
    }

    fn cur(&self) -> usize {
        if self.queue_len() == 0 {
            return 0;
        }
        self.order[self.order_pos.min(self.order.len() - 1)]
    }

    /// Live playback state (rodio 0.23 has no `Player::playing()`): sounds are
    /// queued and not paused. Paused tracks must never count as playing/EOF.
    fn playing(&self) -> bool {
        !self.player.empty() && !self.player.is_paused()
    }

    /// Next order-position honoring repeat semantics. `user` = true for
    /// explicit Next/Prev moves: Repeat One does NOT re-loop the current (that
    /// is auto-advance only); wrap happens only under Repeat All.
    fn succ_pos(&self, user: bool) -> Option<usize> {
        let n = self.queue_len();
        if n == 0 {
            return None;
        }
        if !user && self.repeat == RepeatMode::One {
            return Some(self.order_pos); // auto-advance replays the same track
        }
        let nxt = self.order_pos + 1;
        if nxt < n {
            Some(nxt)
        } else if self.repeat == RepeatMode::All {
            Some(0) // wrap only under Repeat All (auto or user Next)
        } else {
            None // Repeat None at queue end: terminal stop (Groove behavior)
        }
    }

    /// Identity order, or a fresh permutation with the current track pinned
    /// first (Groove behavior: toggling shuffle keeps the song playing).
    /// splitmix64-style LCG — no external rng dependency (stack stays pinned).
    fn build_order(&mut self) {
        let n = self.tracks.len();
        self.order = (0..n).collect();
        self.order_pos = 0;
        if self.shuffle && n > 1 {
            let mut seed = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64 ^ std::process::id() as u64)
                .unwrap_or(0x9E37_79B9_7F4A_7C15);
            let mut i = 1usize;
            while i < n {
                seed = seed
                    .wrapping_mul(0xFF51_AF7D_D2A7_088E)
                    .wrapping_add(0x2D71_71F7_B7D8_A743);
                let j = (seed >> 33) as usize % (n - i) + i; // high bits = good bits
                self.order.swap(i, j);
                i += 1;
            }
        }
    }

    /// Decode `order[order_pos]`'s file and append it. Returns the track's
    /// container duration in seconds (0 = unknown); `total_duration()` is read
    /// from the decoder BEFORE appending, so durations are known upfront —
    /// including for lookahead tracks that have not started yet.
    fn append_decode(
        &mut self,
        idx: usize,
        at_secs: f64,
        log_format: bool,
    ) -> Result<f64, String> {
        let path = self.tracks[idx].clone();
        let mut d = match Decoder::try_from(File::open(&path).map_err(|e| describe(&path, e))?) {
            Ok(d) => d,
            Err(e) => return Err(format!("{path:?}: {e}")),
        };
        let dur = d.total_duration().map(|x| x.as_secs_f64()).unwrap_or(0.0);
        if log_format {
            tlog!(
                "audio: {} Hz, {} ch, dur {dur:.1}s",
                d.sample_rate().get(),
                d.channels().get()
            );
        }
        if at_secs > 0.01 {
            if let Err(e) = d.try_seek(StdDur::from_secs_f64(at_secs)) {
                tlog!("resume seek failed for {path:?}: {e} · starting at 0");
            }
        }
        self.player.append(d);
        Ok(dur)
    }

    /// Clear the queue and start the track at `order_pos`, skipping unplayable
    /// files forward through the order until one decodes (or the queue ends).
    /// On success, announces NowPlaying and maintains the lookahead invariant.
    fn play_pos(&mut self, at_secs: f64) {
        self.last_advance = Instant::now();
        self.player.clear(); // pauses; worker re-plays below (never the UI thread)
        self.eof_hits = 0;
        self.terminated = false;
        self.lookahead_pos = None; self.lookahead_idx = None;
        self.lookahead_ok = false;
        self.next_dur = 0.0;
        self.last_pos = StdDur::ZERO;
        let n = self.queue_len();
        if n == 0 {
            self.cur_dur = 0.0;
            self.send(Event::Position {
                elapsed: 0.0,
                duration: 0.0,
                playing: false,
            });
            return;
        }
        let mut tries = 0usize;
        let mut started = false;
        while tries <= n {
            let idx = self.order[self.order_pos];
            match self.append_decode(idx, at_secs, true) {
                Ok(d) => {
                    self.cur_dur = d;
                    started = true;
                    break;
                }
                Err(e) => {
                    tlog!("skipping unplayable {idx}: {e}");
                    match self.succ_pos(false) {
                        Some(p) => {
                            self.order_pos = p;
                            tries += 1;
                        }
                        None => break, // dead end (Repeat None at end, or retried out)
                    }
                }
            }
        }
        if !started {
            tlog!("nothing playable in queue");
            self.send(Event::DeviceError {
                message: "none of the selected tracks could be decoded".to_string(),
            });
            self.terminated = true;
            self.cur_dur = 0.0;
            self.send(Event::TrackEnded { index: self.cur() });
            return;
        }
        let idx = self.cur();
        self.player.set_volume(self.vol);
        self.pos_guard = 1; // fresh decode: get_pos is stale for one tick
        self.player.play();
        tlog!("playing {idx} · {}", stem(&self.tracks[idx]));
        self.sync_lookahead();
        self.send(Event::NowPlaying {
            index: idx,
            name: stem(&self.tracks[idx]),
        });
    }

    /// Invariant: after the current track starts, its scheduled successor
    /// (per auto-advance semantics) is decoded and appended as lookahead.
    fn sync_lookahead(&mut self) {
        self.lookahead_pos = None; self.lookahead_idx = None;
        self.lookahead_ok = false;
        self.next_dur = 0.0;
        match self.succ_pos(false) {
            Some(p) => {
                let idx = self.order[p];
                match self.append_decode(idx, 0.0, false) {
                    Ok(d) => {
                        self.lookahead_pos = Some(p);
                        self.lookahead_idx = Some(idx);
                        self.lookahead_ok = true;
                        self.next_dur = d;
                        tlog!("lookahead appended: {idx} ({d:.1}s)");
                    }
                    Err(e) => {
                        tlog!("lookahead unavailable {idx}: {e}");
                    }
                }
            }
            None => {}
        }
    }

    /// The current source ended naturally; the already-queued lookahead has
    /// taken over audibly (rodio advances the queue on its own), so this is
    /// pure bookkeeping + announcement — zero gap.
    fn promote_lookahead(&mut self, why: &str) {
        let old = self.cur();
        match self.lookahead_pos.take() {
            Some(p) => {
                // Follow the QUEUED track by index, not the old position:
                // after a shuffle toggle, `order` was rebuilt and position p
                // may now name a different track than the audio in the queue.
                let np = match self.lookahead_idx {
                    Some(ix) => self
                        .order
                        .iter()
                        .position(|x| *x == ix)
                        .unwrap_or(p),
                    None => p,
                };
                self.order_pos = np;
                self.cur_dur = self.next_dur;
                self.next_dur = 0.0;
                self.lookahead_ok = false; // re-armed by sync_lookahead below
                self.last_advance = Instant::now();
                self.eof_hits = 0;
                self.last_pos = StdDur::ZERO;
                self.pos_guard = 1; // lookahead head start: ignore stale position
                let new = self.cur();
                tlog!("{why}: {old} -> {new}");
                self.send(Event::TrackEnded { index: old });
                self.send(Event::NowPlaying {
                    index: new,
                    name: stem(&self.tracks[new]),
                });
                self.sync_lookahead();
            }
            None => {
                self.terminated = true; // defensive: no queued successor
                self.send(Event::TrackEnded { index: old });
            }
        }
    }

    /// Successor not queued (stale after an order/repeat change, or its decode
    /// failed earlier): stop the current source and start the successor fresh.
    fn advance_fresh(&mut self, user: bool, why: &str) {
        let old = self.cur();
        match self.succ_pos(user) {
            Some(p) => {
                tlog!("{why} · audible gap risk · {old} -> {}", self.order[p]);
                self.send(Event::TrackEnded { index: old });
                self.order_pos = p;
                self.play_pos(0.0);
            }
            None => {
                tlog!("end of queue");
                self.terminated = true;
                self.eof_hits = 0;
                self.send(Event::TrackEnded { index: old });
                self.send(Event::Position {
                    elapsed: self.cur_dur as f32,
                    duration: self.cur_dur as f32,
                    playing: false,
                });
            }
        }
    }

    /// Per-tick work: publish position (20 Hz drives the UI slider), then
    /// debounced EOF detection and the advance decision.
    fn on_tick(&mut self, cmd: Option<Cmd>) {
        if let Some(c) = cmd {
            self.dispatch(c);
        }

        // A source that just ended leaves its final position in `get_pos()`
        // until rodio's 5 ms periodic callback re-anchors on the new head
        // source. On the tick right after a start/promotion report and
        // evaluate from `last_pos` (ZERO) instead, so the dead track's tail
        // can never fake an EOF (matters for the 1-tick debounce path).
        let fresh = self.pos_guard > 0;
        if fresh {
            self.pos_guard -= 1;
        }
        let pos = if fresh {
            self.last_pos
        } else {
            self.player.get_pos()
        };
        // If the audible head swapped without the engine deciding it, `pos`
        // drops sharply versus the previous tick — rodio advanced its queue
        // outside our promote/advance logic. Silent bookkeeping desyncs are
        // otherwise invisible; this line is the missed-splice signature.
        if !fresh && self.queue_len() > 0 && !self.terminated {
            let drop = self.last_pos.as_secs_f64() - pos.as_secs_f64();
            // Splice within ~1 s of an engine-side advance is EXPECTED: the
            // window promote announces early, rodio swaps heads at the true
            // end. A desync only matters when nothing advanced recently.
            if drop > 0.5 && self.last_advance.elapsed() > StdDur::from_secs(1) {
                tlog!(
                    "WARN head swapped outside engine · pos {:.2} was {:.2}",
                    pos.as_secs_f64(),
                    self.last_pos.as_secs_f64()
                );
            }
        }
        self.last_pos = pos;
        let e = pos.as_secs_f64();
        let playing = self.playing();
        self.send(Event::Position {
            elapsed: e as f32,
            duration: self.cur_dur as f32,
            playing,
        });

        // EOF: duration-based when known (tol = min(0.4s, 25% of duration) so
        // a 0.2s track is never "ended" at t=0; f64 tuple .min() doesn't
        // compile — NaN breaks Ord — so use f64::min), empty()-based when
        // unknown. Paused never counts a hit; a user seek resets the window
        // naturally. Debounce count scales with window/tick so short test
        // tracks are never worse protected than long library ones: window
        // ≥150 ms (every real song, ≥3 ticks), narrower windows take 1 tick
        // at 50 ms cadence — rodio's position callback itself lands every
        // 5 ms, so a single 50 ms sample of `elapsed ≥ dur − tol` is already
        // strong evidence the tail of the track has been consumed.
        //
        // Position evidence = `last_pos`, the engine's own record (it equals
        // the latest rodio position while playing, and is ZERO right after
        // every start/promote/advance so a fresh track can never "reached the
        // end" from a stale value). Using `last_pos` instead of `playing() &&
        // get_pos()` fixes terminal announce on the last track: once its
        // source ends, `Player::sound_count` hits 0 → `playing()` false →
        // `get_pos()` reports 0 again, so a `playing`-gated duration check
        // can never fire — the queue ends in silence with `terminated` false
        // (observed live in the GROVE_AUTOPLAY probe: no terminal line, Play
        // at queue end fell through to `player.play()` on a drained player).
        let tol = (0.4f64).min(self.cur_dur * 0.25);
        let reached_end =
            self.last_pos.as_secs_f64() >= self.cur_dur - tol;
        let hit = if self.cur_dur > 0.0 {
            !fresh && !self.player.is_paused() && !self.tracks.is_empty() && reached_end
        } else {
            !fresh && self.player.empty() && !self.player.is_paused() && !self.tracks.is_empty()
        };
        self.eof_hits = if hit { self.eof_hits + 1 } else { 0 };
        let needed: u8 = if self.cur_dur > 0.0 {
            if tol >= 0.15 {
                3
            } else {
                1
            }
        } else {
            6
        };

        if self.eof_hits >= needed && !self.terminated && !self.player.is_paused() {
            tlog!(
                "EOF hit · pos {e:.2}s of {dur:.2}s",
                dur = self.cur_dur
            );
            if self.lookahead_ok {
                self.promote_lookahead("auto-advance GAPLESS-PROMOTE");
            } else if self.lookahead_pos.is_some() {
                // Stale after a shuffle/repeat toggle, but a source IS
                // physically queued (rodio's queue still holds the old
                // lookahead). rodio cannot pop a non-head source without
                // cutting the playing one, so: let it play as-is (zero gap),
                // slip the order by one, and sync re-queues the correct
                // successor from the new cursor right after.
                self.promote_lookahead("auto-advance GAPLESS-QUEUED · order slip");
            } else if let Some(p) = self.succ_pos(false) {
                // Nothing queued (successor decode failed earlier): the
                // playing source still has the whole tol window of audio
                // left — decode+append the successor NOW so it splices at
                // the true end, then promote the bookkeeping.
                let idx = self.order[p];
                match self.append_decode(idx, 0.0, false) {
                    Ok(d) => {
                        self.lookahead_pos = Some(p);
                        self.lookahead_idx = Some(idx);
                        self.lookahead_ok = true;
                        self.next_dur = d;
                        self.promote_lookahead("auto-advance GAPLESS-LATE-APPEND");
                    }
                    Err(err) => {
                        tlog!("successor {idx} undecodable at boundary: {err}");
                        self.advance_fresh(false, "auto-advance FRESH");
                    }
                }
            } else {
                // terminal (Repeat None, queue end, nothing queued): audio
                // ends on its own; this announces the stop.
                self.advance_fresh(false, "auto-advance TERMINAL");
            }
        }
    }

    fn dispatch(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Load { tracks, play_index } => {
                tlog!("load: {} tracks · start at {play_index}", tracks.len());
                self.pending = Some(Cmd::Load {
                    tracks: tracks.clone(),
                    play_index,
                });
                self.tracks = tracks;
                self.build_order();
                if self.queue_len() > 0 {
                    self.order_pos = self
                        .order
                        .iter()
                        .position(|x| *x == play_index.min(self.tracks.len() - 1))
                        .unwrap_or(0);
                }
                self.play_pos(0.0);
            }
            Cmd::Play => {
                if self.terminated && self.player.empty() && !self.tracks.is_empty() {
                    // play pressed after end-of-queue: Groove restarts only when
                    // repeat would actually play something again
                    if self.succ_pos(true).is_some() {
                        self.play_pos(0.0);
                    } else {
                        self.player.clear(); // stay stopped, slider back to 0
                        self.send(Event::Position {
                            elapsed: 0.0,
                            duration: self.cur_dur as f32,
                            playing: false,
                        });
                    }
                } else {
                    self.player.play();
                }
            }
            Cmd::Pause => self.player.pause(),
            Cmd::Stop => {
                tlog!("stop");
                self.last_advance = Instant::now();
                self.player.clear(); // pauses, flushes queue (incl. lookahead)
                self.terminated = true;
                self.eof_hits = 0;
                self.lookahead_pos = None; self.lookahead_idx = None;
                self.lookahead_ok = false;
                self.last_pos = StdDur::ZERO;
                self.send(Event::TrackEnded { index: self.cur() });
                self.send(Event::Position {
                    elapsed: 0.0,
                    duration: self.cur_dur as f32,
                    playing: false,
                });
            }
            Cmd::Next => {
                if self.tracks.is_empty() {
                    return;
                }
                if self.terminated {
                    match self.succ_pos(true) {
                        Some(p) => {
                            self.order_pos = p;
                            self.play_pos(0.0);
                        }
                        None => {
                            self.terminated = true; // Repeat None at the end
                            tlog!("next at end (repeat none): staying");
                        }
                    }
                    return;
                }
                let user_next = self.succ_pos(true);
                // gapless skip only when the queued lookahead IS the track the
                // user is asking for (stale order/Repeat-One differ → fresh)
                let promotable = self.lookahead_ok
                    && self.lookahead_pos == Some(user_next.unwrap_or(usize::MAX));
                self.last_advance = Instant::now();
                self.player.skip_one();
                if promotable {
                    self.promote_lookahead("user skip");
                } else if user_next.is_some() {
                    self.advance_fresh(true, "user next");
                } else {
                    self.terminated = true;
                    self.eof_hits = 0;
                    self.send(Event::TrackEnded { index: self.cur() });
                }
            }
            Cmd::Prev => {
                if self.tracks.is_empty() {
                    return;
                }
                let e = self.last_pos.as_secs_f64();
                if e > 3.0 {
                    tlog!("prev: restart current ({e:.1}s in)");
                    self.eof_hits = 0;
                    match self.player.try_seek(StdDur::ZERO) {
                        Ok(()) => {
                            self.last_pos = StdDur::ZERO;
                            self.pos_guard = 1; // seek-to-0: stale tail tick again
                        }
                        Err(_) => self.play_pos(0.0),
                    }
                    self.send(Event::Position {
                        elapsed: 0.0,
                        duration: self.cur_dur as f32,
                        playing: self.playing(),
                    });
                    return;
                }
                let n = self.queue_len();
                let pred = if self.order_pos > 0 {
                    self.order_pos - 1
                } else if self.repeat == RepeatMode::All {
                    n - 1
                } else {
                    self.order_pos // start of queue under Repeat None: restart it
                };
                tlog!("prev: to {}", self.order[pred]);
                self.order_pos = pred;
                self.play_pos(0.0);
            }
            Cmd::Seek(t) => {
                let mut t = f64::from(t.max(0.0));
                if self.cur_dur > 0.0 && t > self.cur_dur {
                    t = self.cur_dur - 0.01;
                }
                let target = StdDur::from_secs_f64(t);
                match self.player.try_seek(target) {
                    Ok(()) => {
                        self.last_pos = target;
                        self.eof_hits = 0;
                        tlog!("seek ok {t:.1}s");
                    }
                    Err(err) => {
                        tlog!("try_seek failed ({err}) · re-decoding at {t:.1}s");
                        self.play_pos(t); // re-arms the lookahead too
                    }
                }
                self.send(Event::Position {
                    elapsed: t as f32,
                    duration: self.cur_dur as f32,
                    playing: self.playing(),
                });
            }
            Cmd::SetVolume(v) => {
                self.vol = v;
                self.player.set_volume(v);
            }
            Cmd::SetEq(_) | Cmd::SetDevice(_) => {} // M7 stubs (EQ + device switcher)
            Cmd::ShuffleMode(mode) => {
                let on = mode == ShuffleMode::On;
                if on != self.shuffle {
                    tlog!("shuffle {} · order rebuilt", if on { "on" } else { "off" });
                    self.shuffle = on;
                    let cur_idx = self.cur();
                    self.build_order();
                    if self.queue_len() > 0 {
                        self.order_pos = self.order.iter().position(|x| *x == cur_idx).unwrap_or(0);
                    }
                    self.lookahead_ok = false; // stale: re-decoded at advance
                }
            }
            Cmd::RepeatMode(mode) => {
                if mode != self.repeat {
                    tlog!("repeat → {mode:?}");
                    self.repeat = mode;
                    self.lookahead_ok = false; // stale: boundary re-decided at advance
                    if mode != RepeatMode::None {
                        self.terminated = false; // wrap becomes possible again
                    }
                }
            }
        }
    }

    /// Fault lane + heartbeat live in the worker: stream invalidation or
    /// device loss is rebuilt here immediately, without waiting for the UI.
    fn on_fault(&mut self, err: CpalError) {
        let severe = matches!(
            err.kind(),
            CpalErrorKind::StreamInvalidated | CpalErrorKind::DeviceNotAvailable | CpalErrorKind::HostUnavailable
        );
        tlog!("stream error: {err}");
        if !severe {
            return; // DeviceChanged self-reroutes, Xrun is a one-sample glitch
        }
        if self.last_rebuild.elapsed() < StdDur::from_secs(2) {
            tlog!("rebuild skipped · cooldown");
            return; // storm coalescing: one rebuild covers repeated reports
        }
        tlog!("rebuilding output device");
        let sink = match open_grove_sink(&self.fault_tx) {
            Ok(s) => s,
            Err(e) => {
                tlog!("rebuild failed: {e} · retry on next fault");
                self.send(Event::DeviceError {
                    message: format!("output device gone: {e}"),
                });
                return;
            }
        };
        self.sink = sink;
        self.player = Player::connect_new(self.sink.mixer());
        self.last_rebuild = Instant::now();
        self.player.set_volume(self.vol);
        // clone out of `pending` first so its borrow ends before the mutable work
        let replay = match &self.pending {
            Some(Cmd::Load { tracks, .. }) => Some(tracks.clone()),
            _ => None,
        };
        match replay {
            Some(tracks) => {
                let resume_at = self.last_pos.as_secs_f64();
                tlog!("sink recovered · resuming at {resume_at:.1}s");
                if self.tracks != tracks {
                    self.tracks = tracks; // queue changed since the fault
                    self.build_order();
                    self.order_pos = 0;
                }
                // order_pos untouched: resume the SAME track where it left off
                self.play_pos(resume_at);
            }
            None => {
                tlog!("sink recovered · nothing loaded");
            }
        }
    }

    fn run(&mut self) {
        loop {
            let next_deadline = Instant::now() + StdDur::from_millis(50);
            let cmd = match self.cmd_rx.recv_timeout(StdDur::from_millis(50)) {
                Ok(c) => {
                    self.dispatch(c);
                    while let Ok(c2) = self.cmd_rx.try_recv() {
                        self.dispatch(c2);
                    }
                    while let Ok(e) = self.fault_rx.try_recv() {
                        self.on_fault(e);
                    }
                    continue;
                }
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => None,
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                    tlog!("cmd channel closed · worker exiting");
                    break;
                }
            };
            while let Ok(e) = self.fault_rx.try_recv() {
                self.on_fault(e);
            }
            self.on_tick(cmd);
            if self.last_beat.elapsed() >= BEAT {
                self.last_beat = Instant::now();
                self.send(Event::Tick);
                tlog!(
                    "beat · playing={} pos={:.1}s vol={:.2}",
                    self.playing(),
                    self.player.get_pos().as_secs_f64(),
                    self.vol
                );
            }
            let overshoot = Instant::now().saturating_duration_since(next_deadline);
            if overshoot > StdDur::from_millis(70) {
                tlog!("tick overrun {:.1} ms", overshoot.as_secs_f64() * 1000.0);
            }
        }
        tlog!("worker exiting");
        // MixerDeviceSink drops last: flushes queued audio before the device closes
    }
}

/// Open the default output sink with Grove's fault callback wired in; mirrors
/// rodio's own fallback strategy when the default device fails (hosts → devices,
/// skipping null-driver ghosts). The same call powers fault rebuild + M7 switch.
fn open_grove_sink(
    fault_tx: &crossbeam_channel::Sender<CpalError>,
) -> Result<MixerDeviceSink, DeviceSinkError> {
    let first = match DeviceSinkBuilder::from_default_device() {
        Ok(b) => {
            let tx = fault_tx.clone();
            let cb = move |e: CpalError| {
                tlog!("cpal stream error · {e}");
                tx.send(e).ok();
            };
            match b.with_error_callback(cb).open_sink_or_fallback() {
                Ok(s) => return Ok(s),
                Err(e) => e,
            }
        }
        Err(e) => e,
    };
    tlog!("default sink path failed, walking hosts: {first}");
    fallback_sinks(fault_tx, &first.to_string())
}

fn fallback_sinks(
    fault_tx: &crossbeam_channel::Sender<CpalError>,
    why: &str,
) -> Result<MixerDeviceSink, DeviceSinkError> {
    tlog!("default sink failed · {why} · walking hosts for a usable device");
    let mut last = why.to_string();
    for host_id in rodio::cpal::available_hosts() {
        let Ok(host) = rodio::cpal::host_from_id(host_id) else {
            continue;
        };
        let Ok(devs) = host.output_devices() else {
            continue;
        };
        for dev in devs.filter(|d| {
            d.description()
                .map(|desc| desc.driver().is_some_and(|drv| drv != "null"))
                .unwrap_or(false)
        }) {
            let tx = fault_tx.clone();
            let cb = move |e: CpalError| {
                tlog!("cpal stream error · {e}");
                tx.send(e).ok();
            };
            let result = DeviceSinkBuilder::from_device(dev.clone())
                .and_then(|b| b.with_error_callback(cb).open_sink_or_fallback());
            match result {
                Ok(s) => {
                    tlog!("fallback sink opened · {dev:?}");
                    return Ok(s);
                }
                Err(e) => last = format!("{dev:?}: {e}"),
            }
        }
    }
    tlog!("no usable output device found · {last}");
    Err(DeviceSinkError::NoDevice)
}

fn describe(path: &PathBuf, e: std::io::Error) -> String {
    let kind = match e.kind() {
        std::io::ErrorKind::NotFound => "file not found",
        std::io::ErrorKind::PermissionDenied => "access denied",
        _ => "open failed",
    };
    format!("{path:?} ({kind}): {e}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossbeam_channel::unbounded; // cmd/ev/fault channels for the harness
    use std::collections::HashSet;
    use std::fs;

    /// 16-bit mono PCM WAV of silence, `secs` long: deterministic header-based
    /// durations that `total_duration()` reports exactly — cheap fixtures for
    /// state-machine tests (silent = safe on a shared audio device).
    fn write_silent_wavs(tag: &str, n: usize, secs: f64) -> Vec<PathBuf> {
        let sr: u32 = 44100;
        let samples = (sr as f64 * secs) as u32;
        let data_len = samples * 2;
        let dir = std::env::temp_dir().join("grove_m2_tests").join(tag);
        fs::create_dir_all(&dir).expect("temp dir");
        let mut out = Vec::new();
        for i in 0..n {
            let p = dir.join(format!("t{i}.wav"));
            let mut bytes: Vec<u8> = Vec::new();
            bytes.extend_from_slice(b"RIFF");
            bytes.extend((36 + data_len).to_le_bytes().to_vec());
            bytes.extend_from_slice(b"WAVEfmt ");
            bytes.extend(16u32.to_le_bytes().to_vec());
            bytes.extend(1u16.to_le_bytes().to_vec()); // PCM
            bytes.extend(1u16.to_le_bytes().to_vec()); // mono
            bytes.extend(sr.to_le_bytes().to_vec());
            bytes.extend((sr * 2).to_le_bytes().to_vec());
            bytes.extend(2u16.to_le_bytes().to_vec());
            bytes.extend(16u16.to_le_bytes().to_vec());
            bytes.extend_from_slice(b"data");
            bytes.extend(data_len.to_le_bytes().to_vec());
            bytes.resize(44 + data_len as usize, 0);
            fs::write(&p, &bytes).expect("write wav");
            out.push(p);
        }
        out
    }

    /// Headless harness: engine on the test thread, drive with cmds, drain
    /// events — asserts state-machine sequences, no UI, no wall-clock babysit.
    struct Harness {
        e: Engine,
        cmd_tx: crossbeam_channel::Sender<Cmd>,
        ev_rx: crossbeam_channel::Receiver<Event>,
    }

    fn harness() -> Harness {
        let (cmd_tx, cmd_rx) = unbounded();
        let (ev_tx, ev_rx) = unbounded();
        let (fault_tx, fault_rx) = unbounded(); // crossbeam hands back (Sender, Receiver)
        let sink = open_grove_sink(&fault_tx).expect("sink");
        let e = Engine::with_sink(sink, cmd_rx, ev_tx, fault_rx, fault_tx);
        Harness {
            e,
            cmd_tx,
            ev_rx,
        }
    }

    /// Drive the engine at production cadence (50 ms ticks → 150 ms EOF
    /// debounce), so wall-clock windows assert the shipping behavior exactly.
    fn run_for(h: &mut Harness, ms: u64) -> Vec<Event> {
        let deadline = Instant::now() + StdDur::from_millis(ms);
        let mut evs = Vec::new();
        while Instant::now() < deadline {
            let cmd = h.e.cmd_rx.try_recv().ok(); // engine owns the receiver (field access OK in this module's tests)
            if let Some(c) = cmd {
                h.e.dispatch(c);
            }
            h.e.on_tick(None);
            while let Ok(ev) = h.ev_rx.try_recv() {
                evs.push(ev);
            }
            thread::sleep(StdDur::from_millis(50)); // harness real-time pacing (test-only)
        }
        while let Ok(ev) = h.ev_rx.try_recv() {
            evs.push(ev);
        }
        evs
    }

    fn starts(evs: &[Event]) -> Vec<usize> {
        evs
            .iter()
            .filter_map(|e| match e {
                Event::NowPlaying { index, .. } => Some(*index),
                _ => None,
            })
            .collect()
    }

    fn ends(evs: &[Event]) -> Vec<usize> {
        evs
            .iter()
            .filter_map(|e| match e {
                Event::TrackEnded { index } => Some(*index),
                _ => None,
            })
            .collect()
    }

    fn last_playing(evs: &[Event]) -> Option<bool> {
        evs.iter().rev().find_map(|e| match e {
            Event::Position { playing, .. } => Some(*playing),
            _ => None,
        })
    }

    #[test]
    fn auto_advance_then_terminal_stop() {
        let files = write_silent_wavs("auto_advance", 3, 0.20);
        let mut h = harness();
        h.cmd_tx
            .send(Cmd::Load {
                tracks: files,
                play_index: 0,
            })
            .expect("load cmd");
        let evs = run_for(&mut h, 2500);
        assert_eq!(starts(&evs), vec![0, 1, 2], "must auto-advance 0→1→2");
        assert_eq!(
            ends(&evs),
            vec![0, 1, 2],
            "each advance announces TrackEnded"
        );
        assert_eq!(last_playing(&evs), Some(false), "must stop at end of queue");
        assert!(
            evs.iter().any(|e| match e {
                Event::Position {
                    duration,
                    playing,
                    ..
                } => *playing && *duration > 0.15,
                _ => false,
            }),
            "Position must carry the real container duration"
        );
    }

    #[test]
    fn repeat_one_loops_current_without_gap() {
        let files = write_silent_wavs("repeat_one", 1, 0.20);
        let mut h = harness();
        h.cmd_tx
            .send(Cmd::RepeatMode(RepeatMode::One))
            .expect("repeat cmd");
        h.cmd_tx
            .send(Cmd::Load {
                tracks: files,
                play_index: 0,
            })
            .expect("load cmd");
        let evs = run_for(&mut h, 1500);
        assert!(
            starts(&evs).iter().all(|x| *x == 0),
            "Repeat One must only ever play track 0: {:?}",
            starts(&evs)
        );
        assert!(
            starts(&evs).len() >= 3,
            "must loop itself repeatedly: {:?}",
            starts(&evs)
        );
        assert!(
            ends(&evs).iter().all(|x| *x == 0),
            "its loops end only track 0, never the queue"
        );
        let max_e = evs.iter().fold(0.0f64, |m, e| match e {
            Event::Position { elapsed, .. } => m.max(*elapsed as f64),
            _ => m,
        });
        assert!(
            max_e > 0.14,
            "position must run past the EOF threshold ({max_e:.2}s)"
        );
        assert_eq!(last_playing(&evs), Some(true), "still playing at the end");
    }

    #[test]
    fn repeat_all_wraps_the_queue() {
        let files = write_silent_wavs("repeat_all", 2, 0.20);
        let mut h = harness();
        h.cmd_tx
            .send(Cmd::RepeatMode(RepeatMode::All))
            .expect("repeat cmd");
        h.cmd_tx
            .send(Cmd::Load {
                tracks: files,
                play_index: 0,
            })
            .expect("load cmd");
        let evs = run_for(&mut h, 1800);
        let s = starts(&evs);
        assert!(s.len() >= 4, "must loop at least twice: {s:?}");
        assert!(s.contains(&0) && s.contains(&1), "both tracks played");
        assert!(!ends(&evs).is_empty(), "wraps announce TrackEnded");
    }

    #[test]
    fn shuffle_gives_a_unique_permutation_via_gapless_skips() {
        let files = write_silent_wavs("shuffle", 6, 0.20);
        let mut h = harness();
        h.cmd_tx
            .send(Cmd::ShuffleMode(ShuffleMode::On))
            .expect("shuffle cmd");
        h.cmd_tx
            .send(Cmd::RepeatMode(RepeatMode::All))
            .expect("repeat cmd");
        h.cmd_tx
            .send(Cmd::Load {
                tracks: files,
                play_index: 0,
            })
            .expect("load cmd");
        let mut evs = Vec::new();
        let mut seen: HashSet<usize> = HashSet::new();
        let mut rounds = 0;
        let began = Instant::now();
        while seen.len() < 6 && rounds < 60 && began.elapsed() < StdDur::from_secs(15) {
            h.cmd_tx.send(Cmd::Next).expect("next cmd");
            let evs_now = run_for(&mut h, 180);
            evs.extend(evs_now);
            seen.extend(starts(&evs));
            rounds += 1;
        }
        assert_eq!(
            seen.len(),
            6,
            "shuffle must visit all 6 exactly once: {seen:?}"
        );
        let s = starts(&evs);
        assert!(
            !s.windows(2).any(|w| w[0] == w[1]),
            "never plays the same track twice in a row: {s:?}"
        );
    }

    #[test]
    fn prev_at_start_rewinds_to_previous_track() {
        let files = write_silent_wavs("prev", 2, 0.40);
        let mut h = harness();
        h.cmd_tx
            .send(Cmd::Load {
                tracks: files,
                play_index: 0,
            })
            .expect("load cmd");
        let evs0 = run_for(&mut h, 1200); // 0.4s tracks with 1-tick debounce:
        // cycles ~350 ms → several wraps; what matters is it DID advance
        assert!(
            starts(&evs0).contains(&1),
            "must auto-advance by 1200 ms: {:?}",
            starts(&evs0)
        );
        h.cmd_tx.send(Cmd::Prev).expect("prev cmd");
        let evs1 = run_for(&mut h, 250); // short enough that track 0 cannot
        // hit its own EOF (≈300 ms) within the window
        let all: Vec<Event> = evs0.iter().chain(evs1.iter()).cloned().collect();
        let s = starts(&all);
        assert_eq!(
            s.last(),
            Some(&0),
            "Prev at <3s must rewind to track 0: {s:?}"
        );
        assert!(
            evs1.iter().any(|e| matches!(e, Event::Position { elapsed, .. } if *elapsed < 0.15)),
            "after Prev the position must restart near 0"
        );
    }
}
