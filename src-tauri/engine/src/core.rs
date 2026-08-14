//! The shared block-based playback core.
//!
//! Both the offline renderer (`docs/SPEC.md` §12) and the real-time engine (§3)
//! drive **this exact code** — that is the mechanism behind the guarantee that an
//! offline render is sample-identical to what comes out of the audio device given
//! the same input. There is no "offline DSP" and "live DSP" to diverge; there is one
//! [`PlaybackCore::render_block`], and the two paths differ only in who supplies the
//! blocks' timing and the next [`Entry`] at each section boundary (the
//! [`Sequencer`]).
//!
//! Design constraints, in order:
//!
//! 1. **Real-time safe.** After construction, `render_block` never allocates, never
//!    locks, never does I/O, and never drops heap data (CLAUDE.md invariant 1). All
//!    buffers are preallocated in [`PlaybackCore::new`], which runs on a worker/UI
//!    thread, never in the callback. This is enforced by a counting-allocator test
//!    (`tests/rt_no_alloc.rs`), not by convention.
//! 2. **Block-size invariant.** Every per-sample value is computed from absolute
//!    positions (performance sample, hit-relative sample index, fade progress), so
//!    splitting the same timeline into different block sizes produces bit-identical
//!    output. Guarded by `tests/rt_equivalence.rs`.
//! 3. **Never accumulate beat positions** (CLAUDE.md invariant 2). The core holds a
//!    sample-counting transport position; everything musical (bar boundaries, click
//!    pulses, splice points) is computed through [`Grid`]'s
//!    round-from-absolute-pulse-index conversions.
//!
//! Processing order per block, matching the phase-1 offline renderer it replaced:
//! track audio (with the §7 equal-power splice crossfade inline), then click, then
//! master gain (1.0 except during stop/panic ramps — multiplying by 1.0 is a bitwise
//! no-op, preserving offline equality), then the per-bus soft-knee limiter.

use crate::click::{self, ClickSynthConfig};
use crate::crossfade;
use crate::limiter;
use crate::smoother::Smoother;
use crate::timeline::Grid;
use std::sync::Arc;

/// Upper bound on frames per `render_block` call. Callers with larger device
/// buffers split them (`process` in [`crate::rt`] does this); the offline renderer
/// simply renders in chunks of this size.
pub const MAX_BLOCK_FRAMES: usize = 8192;

/// One contiguous play-through span: `length_bars` bars of one section, placed at an
/// absolute performance-time position and mapped to absolute source frames. The
/// live-transport analogue of [`crate::sections::ResolvedEntry`], carrying the extra
/// `source_start_bar0` so a queued advance can truncate the span at a bar boundary
/// with exact grid arithmetic.
///
/// `source_end_sample` is the *nominal* grid-computed end
/// (`round(source_end_pulse * spp) + offset`), not `source_start_sample +
/// perf_length`: the two can differ by ±1 sample because the two coordinate systems
/// round independently. Contiguity checks and fade start positions use the nominal
/// value so that source-adjacent sections splice as a straight continuation with no
/// spurious crossfade.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    pub section_index: usize,
    /// 0-based bar index of the section's start inside the source audio.
    pub source_start_bar0: i64,
    /// 0-based bar index in the performance timeline where this span begins.
    pub perf_start_bar: i64,
    pub length_bars: u32,
    pub perf_start_pulse: i64,
    /// Exclusive.
    pub perf_end_pulse: i64,
    pub perf_start_sample: i64,
    /// Exclusive.
    pub perf_end_sample: i64,
    pub source_start_sample: i64,
    /// Exclusive, nominal (see type docs).
    pub source_end_sample: i64,
}

impl Entry {
    pub fn perf_length_samples(&self) -> i64 {
        self.perf_end_sample - self.perf_start_sample
    }
}

/// Build an [`Entry`] with exactly the arithmetic `resolve_order` uses (integer bar
/// counts, one `round(pulse * samples_per_pulse)` per conversion, `offset_samples`
/// applied only on the source side). Live transport and offline expansion both come
/// through here, so their entries agree to the sample by construction.
pub fn make_entry(
    grid: &Grid,
    offset_samples: i64,
    section_index: usize,
    source_start_bar0: i64,
    perf_start_bar: i64,
    length_bars: u32,
) -> Entry {
    let perf_start_pulse = grid.bar_to_pulse(perf_start_bar);
    let perf_end_pulse = grid.bar_to_pulse(perf_start_bar + length_bars as i64);
    let source_start_pulse = grid.bar_to_pulse(source_start_bar0);
    let source_end_pulse = grid.bar_to_pulse(source_start_bar0 + length_bars as i64);
    Entry {
        section_index,
        source_start_bar0,
        perf_start_bar,
        length_bars,
        perf_start_pulse,
        perf_end_pulse,
        perf_start_sample: grid.pulse_to_sample(perf_start_pulse),
        perf_end_sample: grid.pulse_to_sample(perf_end_pulse),
        source_start_sample: grid.pulse_to_sample(source_start_pulse) + offset_samples,
        source_end_sample: grid.pulse_to_sample(source_end_pulse) + offset_samples,
    }
}

/// Supplies the next [`Entry`] when the playhead reaches the current one's end, or
/// `None` to let playback run out (silence plus the final click hits' decay tails).
///
/// Called from inside `render_block`, i.e. on the audio thread in the live path:
/// implementations must be allocation-free and non-blocking. Both implementations in
/// this crate (the offline slice iterator and the live transport) are pure
/// arithmetic.
pub trait Sequencer {
    fn next_entry(&mut self, ended: &Entry) -> Option<Entry>;
}

/// A [`Sequencer`] over a precomputed schedule — the offline renderer's driver, also
/// used by tests. `entries[0]` is expected to have been passed to
/// [`PlaybackCore::start`]; `next_entry` serves from index 1 onward.
pub struct SliceSequencer<'a> {
    entries: &'a [Entry],
    next: usize,
}

impl<'a> SliceSequencer<'a> {
    pub fn new(entries: &'a [Entry]) -> Self {
        SliceSequencer { entries, next: 1 }
    }
}

impl Sequencer for SliceSequencer<'_> {
    fn next_entry(&mut self, _ended: &Entry) -> Option<Entry> {
        let e = self.entries.get(self.next).copied();
        self.next += 1;
        e
    }
}

/// One playable track: preloaded mono audio at the engine rate, a bus assignment,
/// and smoothed gain and mute parameters. Mute is a smoothed gain factor (1.0
/// unmuted, 0.0 muted) so mute/unmute ramps per invariant 5.
pub struct CoreTrack {
    pub audio: Arc<[f32]>,
    pub bus: usize,
    pub gain: Smoother,
    pub mute: Smoother,
}

pub struct CoreBus {
    pub limiter_enabled: bool,
}

pub struct CoreClick {
    /// Resolved accent pattern (already run through
    /// [`click::effective_accent_pattern`], so never empty for a valid song).
    pub pattern: Vec<u8>,
    pub cfg: ClickSynthConfig,
    pub bus: usize,
    pub gain: Smoother,
}

/// Progress of the 15 ms equal-power splice crossfade (§7 / invariant 6). `len == 0`
/// or `pos >= len` means no fade is active. A single slot suffices: the fade length
/// is capped to the incoming entry's own length, so a fade always completes before
/// the next transition can start one.
#[derive(Debug, Clone, Copy)]
struct FadeState {
    /// Source frame the outgoing side reads at fade position 0 (the outgoing entry's
    /// nominal source end — "keep the outgoing read position alive", §7).
    src_start: i64,
    pos: i64,
    len: i64,
}

impl FadeState {
    const INACTIVE: FadeState = FadeState {
        src_start: 0,
        pos: 0,
        len: 0,
    };
}

pub struct PlaybackCore {
    grid: Grid,
    offset_samples: i64,
    tracks: Vec<CoreTrack>,
    click: CoreClick,
    buses: Vec<CoreBus>,
    /// Per-bus output for the most recent block, `bus_count x MAX_BLOCK_FRAMES`.
    bus_buf: Vec<Vec<f32>>,
    click_scratch: Vec<f32>,
    /// Whole-output gain, 1.0 in normal playback; ramped to 0 for stop/panic.
    master: Smoother,
    active: Option<Entry>,
    /// Set by [`Self::start`] when a count-in precedes `active`'s natural start;
    /// promoted to `active` once `perf_pos` reaches `perf_start_sample` (§6). While
    /// this is set and `active` is `None`, `render_block` renders click-only —
    /// `render_tracks_span` is never called, so the backtrack is silent by
    /// construction, not by a special case.
    pending: Option<Entry>,
    fade: FadeState,
    /// Absolute performance-time sample position; advances by exactly the number of
    /// frames rendered, and by nothing else.
    perf_pos: i64,
    /// Click pulses are generated strictly below this bound (the current schedule
    /// end); already-struck hits still decay past it.
    click_limit_pulse: i64,
    hit_len: i64,
}

impl PlaybackCore {
    /// Construct with everything preallocated. Runs on a worker/UI thread; the audio
    /// thread only ever receives a finished core (boxed, via the command queue).
    ///
    /// Callers are responsible for validating bus indices (`track.bus` and
    /// `click.bus` must be `< buses.len()`); [`crate::render::render_song`] and the
    /// prepare path both do.
    pub fn new(
        grid: Grid,
        offset_samples: i64,
        tracks: Vec<CoreTrack>,
        click: CoreClick,
        buses: Vec<CoreBus>,
    ) -> Self {
        let bus_count = buses.len().max(1);
        debug_assert!(tracks.iter().all(|t| t.bus < bus_count));
        debug_assert!(click.bus < bus_count);
        let hit_len = click::hit_length_samples(grid.sample_rate(), click.cfg.decay_ms);
        PlaybackCore {
            grid,
            offset_samples,
            tracks,
            click,
            buses,
            bus_buf: vec![vec![0.0; MAX_BLOCK_FRAMES]; bus_count],
            click_scratch: vec![0.0; MAX_BLOCK_FRAMES],
            master: Smoother::settled(1.0),
            active: None,
            pending: None,
            fade: FadeState::INACTIVE,
            perf_pos: 0,
            click_limit_pulse: 0,
            hit_len,
        }
    }

    pub fn grid(&self) -> &Grid {
        &self.grid
    }

    pub fn offset_samples(&self) -> i64 {
        self.offset_samples
    }

    pub fn perf_pos(&self) -> i64 {
        self.perf_pos
    }

    pub fn active(&self) -> Option<&Entry> {
        self.active.as_ref()
    }

    /// The entry a count-in is counting into, `None` once it has landed (or if
    /// playback wasn't started with a count-in at all). See [`Self::start`].
    pub fn pending(&self) -> Option<&Entry> {
        self.pending.as_ref()
    }

    pub fn hit_length(&self) -> i64 {
        self.hit_len
    }

    /// Exclusive end of the current schedule, in pulses: click pulses are generated
    /// strictly below this. After the final entry ends this marks where the song's
    /// content stops (its last click hit still decays for [`Self::hit_length`]
    /// samples past it).
    pub fn schedule_end_pulse(&self) -> i64 {
        self.click_limit_pulse
    }

    pub fn bus_count(&self) -> usize {
        self.bus_buf.len()
    }

    /// The per-bus output of the most recent `render_block` call; only the first
    /// `frames` samples of each buffer are meaningful.
    pub fn bus_buffer(&self, bus: usize) -> &[f32] {
        &self.bus_buf[bus]
    }

    /// Begin playback at `first` (typically `perf_start_bar == 0`), preceded by
    /// `count_in_bars` bars of click-only count-in (§6). Resets the transport
    /// position and any leftover fade; does not touch parameter smoothers.
    ///
    /// The count-in's start pulse is `first.perf_start_pulse - count_in_bars *
    /// pulses_per_bar`, converted to a sample position the same
    /// `round(pulse * samples_per_pulse)` way as every other pulse (never by
    /// subtracting a sample count) — so it inherits the grid's exactness guarantee,
    /// and stays correct for a mid-song rehearsal start where `first` doesn't begin
    /// at pulse 0. With `count_in_bars == 0` this is exactly the old immediate start.
    pub fn start(&mut self, first: Entry, count_in_bars: u32) {
        debug_assert!(first.perf_end_sample > first.perf_start_sample);
        self.click_limit_pulse = first.perf_end_pulse;
        self.fade = FadeState::INACTIVE;
        if count_in_bars == 0 {
            self.perf_pos = first.perf_start_sample;
            self.active = Some(first);
            self.pending = None;
        } else {
            let count_in_pulses = count_in_bars as i64 * self.grid.pulses_per_bar();
            self.perf_pos = self
                .grid
                .pulse_to_sample(first.perf_start_pulse - count_in_pulses);
            self.active = None;
            self.pending = Some(first);
        }
    }

    /// Drop the active entry (panic stop / unload). No heap data is freed here —
    /// the `Entry` is `Copy` and the audio `Arc`s stay owned by the core.
    pub fn clear_active(&mut self) {
        self.active = None;
        self.pending = None;
        self.fade = FadeState::INACTIVE;
    }

    /// Truncate the active entry so it ends at performance bar `boundary_perf_bar`
    /// (exclusive) instead of its natural end — the mechanics of "manual advance
    /// quantises to the next bar boundary" (§7). Returns false (and changes nothing)
    /// if there is no active entry, the boundary isn't strictly inside it, or it
    /// would leave zero bars.
    pub fn truncate_active_to_bar(&mut self, boundary_perf_bar: i64) -> bool {
        let grid = self.grid;
        let offset = self.offset_samples;
        let Some(a) = &mut self.active else {
            return false;
        };
        let played_bars = boundary_perf_bar - a.perf_start_bar;
        if played_bars <= 0 {
            return false;
        }
        let boundary_pulse = grid.bar_to_pulse(boundary_perf_bar);
        let boundary_sample = grid.pulse_to_sample(boundary_pulse);
        if boundary_sample >= a.perf_end_sample {
            return false;
        }
        a.length_bars = played_bars as u32;
        a.perf_end_pulse = boundary_pulse;
        a.perf_end_sample = boundary_sample;
        let source_end_pulse = grid.bar_to_pulse(a.source_start_bar0 + played_bars);
        a.source_end_sample = grid.pulse_to_sample(source_end_pulse) + offset;
        self.click_limit_pulse = boundary_pulse;
        true
    }

    // ---- Live-adjustable parameters (all ramped; see smoother.rs) ----

    pub fn set_track_gain(&mut self, track: usize, linear: f32, ramp_samples: u32) {
        if let Some(t) = self.tracks.get_mut(track) {
            t.gain.set_target(linear, ramp_samples);
        }
    }

    pub fn set_track_muted(&mut self, track: usize, muted: bool, ramp_samples: u32) {
        if let Some(t) = self.tracks.get_mut(track) {
            t.mute
                .set_target(if muted { 0.0 } else { 1.0 }, ramp_samples);
        }
    }

    /// Bus assignment is a hard switch per §4 (routing, not panning).
    pub fn set_track_bus(&mut self, track: usize, bus: usize) {
        if bus >= self.bus_buf.len() {
            return;
        }
        if let Some(t) = self.tracks.get_mut(track) {
            t.bus = bus;
        }
    }

    pub fn set_click_gain(&mut self, linear: f32, ramp_samples: u32) {
        self.click.gain.set_target(linear, ramp_samples);
    }

    pub fn set_limiter_enabled(&mut self, bus: usize, enabled: bool) {
        if let Some(b) = self.buses.get_mut(bus) {
            b.limiter_enabled = enabled;
        }
    }

    /// Master output gain, used by the transport for stop (10 ms) and panic (5 ms)
    /// ramps. At its steady-state 1.0 it multiplies bitwise-identically to not being
    /// there at all, which preserves offline/live equality.
    pub fn set_master(&mut self, target: f32, ramp_samples: u32) {
        self.master.set_target(target, ramp_samples);
    }

    pub fn master_settled_at(&self, value: f32) -> bool {
        !self.master.is_ramping() && self.master.target() == value
    }

    /// Render the next `frames` samples of performance time into the per-bus
    /// buffers. `seq` is consulted exactly when the playhead crosses an entry
    /// boundary. See module docs for the processing order and the invariants this
    /// function is bound by.
    pub fn render_block(&mut self, frames: usize, seq: &mut dyn Sequencer) {
        assert!(frames <= MAX_BLOCK_FRAMES);
        for bus in self.bus_buf.iter_mut() {
            bus[..frames].fill(0.0);
        }
        let block_start = self.perf_pos;

        let mut done = 0usize;
        while done < frames {
            // Promote a count-in's target entry once the transport reaches its start
            // — the mirror image of the entry-boundary loop below, but for an
            // entry's *start* rather than its end, so it belongs above that loop.
            if self.active.is_none() {
                if let Some(p) = self.pending {
                    if self.perf_pos >= p.perf_start_sample {
                        self.pending = None;
                        self.active = Some(p);
                    }
                }
            }

            // Resolve any entry boundary sitting exactly at the current position.
            while let Some(a) = self.active {
                if self.perf_pos < a.perf_end_sample {
                    break;
                }
                let next = seq.next_entry(&a);
                self.apply_transition(&a, next);
            }

            let n = match &self.active {
                Some(a) => ((a.perf_end_sample - self.perf_pos).min((frames - done) as i64)).max(0)
                    as usize,
                None => match &self.pending {
                    // Still counting in: advance up to (never past) the target
                    // entry's start, so the promotion above catches it exactly.
                    Some(p) => ((p.perf_start_sample - self.perf_pos).min((frames - done) as i64))
                        .max(0) as usize,
                    None => frames - done,
                },
            };
            if n == 0 {
                // Defensive: only reachable if a sequencer hands back a zero-length
                // entry, which `make_entry` cannot produce (length_bars >= 1), or a
                // count-in of zero bars, which `start` handles by activating `first`
                // immediately rather than ever setting `pending`.
                debug_assert!(false, "zero-length span in render_block");
                break;
            }

            if let Some(entry) = self.active {
                render_tracks_span(
                    &mut self.tracks,
                    &mut self.bus_buf,
                    &entry,
                    self.perf_pos,
                    done,
                    n,
                    &mut self.fade,
                );
            }
            // Silence spans (after the final entry) still advance the transport so
            // click decay tails land where the offline render puts them.
            self.perf_pos += n as i64;
            done += n;
        }

        self.render_click_block(block_start, frames);
        self.finish_block(frames);
    }

    fn apply_transition(&mut self, ended: &Entry, next: Option<Entry>) {
        match next {
            Some(n) => {
                debug_assert_eq!(
                    n.perf_start_sample, ended.perf_end_sample,
                    "entries must be performance-time contiguous"
                );
                debug_assert!(n.perf_end_sample > n.perf_start_sample);
                // A source-contiguous continuation is a straight read-through, not a
                // splice — no fade (matches the offline renderer's rule).
                if ended.source_end_sample != n.source_start_sample {
                    self.fade = FadeState {
                        src_start: ended.source_end_sample,
                        pos: 0,
                        len: crossfade::crossfade_length_samples(
                            self.grid.sample_rate(),
                            Some(n.perf_length_samples()),
                        ),
                    };
                }
                self.click_limit_pulse = n.perf_end_pulse;
                self.active = Some(n);
            }
            None => {
                self.active = None;
            }
        }
    }

    /// Generate every click hit intersecting `[block_start, block_start + frames)`.
    /// Hits are identified by absolute pulse index and rendered with
    /// [`click::hit_value`] at hit-relative sample indices, so a hit split across
    /// blocks is bit-identical to the same hit rendered whole.
    fn render_click_block(&mut self, block_start: i64, frames: usize) {
        let scratch = &mut self.click_scratch[..frames];
        scratch.fill(0.0);

        let grid = &self.grid;
        let rate = grid.sample_rate();
        let cfg = &self.click.cfg;
        let ppb = grid.pulses_per_bar();
        let block_end = block_start + frames as i64;

        // Candidate pulses: any whose hit window [pulse_sample, pulse_sample +
        // hit_len) can intersect the block. sample_to_pulse under-shoots by at most
        // one pulse either side; the per-pulse intersection test below is exact.
        // No lower clamp at 0: count-in (§6) schedules pulses before performance-time
        // 0, and Grid::pulse_to_sample/sample_to_pulse are exact for negative pulses
        // by construction (see timeline.rs's negative-pulse tests).
        let lo = grid.sample_to_pulse(block_start - self.hit_len);
        let hi = (grid.sample_to_pulse(block_end) + 1).min(self.click_limit_pulse);
        for pulse in lo..hi {
            let s = grid.pulse_to_sample(pulse);
            let rel = s - block_start;
            if rel >= frames as i64 || rel + self.hit_len <= 0 {
                continue;
            }
            let (freq, gain) = click::pulse_voice(&self.click.pattern, ppb, pulse, cfg);
            if gain == 0.0 || !freq.is_finite() || freq <= 0.0 {
                continue;
            }
            for i in 0..self.hit_len {
                let idx = rel + i;
                if idx < 0 {
                    continue;
                }
                if idx >= frames as i64 {
                    break;
                }
                scratch[idx as usize] += click::hit_value(i, rate, freq, gain, cfg.decay_ms);
            }
        }

        let bus = &mut self.bus_buf[self.click.bus];
        for (i, s) in scratch.iter().enumerate() {
            let g = self.click.gain.tick();
            bus[i] += *s * g;
        }
    }

    /// Master gain (per-sample, smoothed) then the per-bus soft-knee limiter.
    fn finish_block(&mut self, frames: usize) {
        for i in 0..frames {
            let m = self.master.tick();
            for bus in self.bus_buf.iter_mut() {
                bus[i] *= m;
            }
        }
        for (idx, bus_cfg) in self.buses.iter().enumerate() {
            if bus_cfg.limiter_enabled {
                limiter::soft_knee_buffer(&mut self.bus_buf[idx][..frames]);
            }
        }
    }
}

/// Render `n` samples of track audio starting at performance position `perf_pos`
/// into `bus_buf[..][out_offset..out_offset + n]`, applying the splice crossfade
/// while one is active. Free function so the borrows of `tracks`, `bus_buf`, and the
/// fade state stay disjoint.
///
/// Per-sample, per-track accumulation order is fixed (track list order; outgoing
/// fade term before incoming) — float addition is not associative, so a fixed order
/// is part of the bit-identity contract.
fn render_tracks_span(
    tracks: &mut [CoreTrack],
    bus_buf: &mut [Vec<f32>],
    entry: &Entry,
    perf_pos: i64,
    out_offset: usize,
    n: usize,
    fade: &mut FadeState,
) {
    for i in 0..n {
        let p = perf_pos + i as i64;
        let src = entry.source_start_sample + (p - entry.perf_start_sample);
        let fading = fade.pos < fade.len;
        let (gain_out, gain_in) = if fading {
            crossfade::equal_power_gains(fade.pos as f64 / fade.len as f64)
        } else {
            (0.0, 0.0)
        };
        let out_idx = out_offset + i;
        for t in tracks.iter_mut() {
            let g = t.gain.tick() * t.mute.tick();
            let buf = &mut bus_buf[t.bus];
            if fading {
                let out_frame = fade.src_start + fade.pos;
                buf[out_idx] += gain_out * g * read_sample(&t.audio, out_frame);
                buf[out_idx] += gain_in * g * read_sample(&t.audio, src);
            } else {
                buf[out_idx] += g * read_sample(&t.audio, src);
            }
        }
        if fading {
            fade.pos += 1;
        }
    }
}

/// Read one source frame, treating everything outside the file as silence.
#[inline]
pub fn read_sample(audio: &Arc<[f32]>, frame: i64) -> f32 {
    if frame < 0 {
        return 0.0;
    }
    audio.get(frame as usize).copied().unwrap_or(0.0)
}

pub fn db_to_linear(db: f64) -> f64 {
    10f64.powf(db / 20.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::timeline::TimeSignature;

    fn grid() -> Grid {
        Grid::new(48000, 178.0, TimeSignature::FOUR_FOUR).unwrap()
    }

    #[test]
    fn make_entry_matches_resolve_order_arithmetic() {
        use crate::project::{Section, Song};
        use crate::sections::{resolve_order, PerformanceEntry};
        let song = Song {
            id: "s".into(),
            title: "t".into(),
            bpm: 178.0,
            time_signature: TimeSignature::FOUR_FOUR,
            offset_samples: 2400,
            count_in_bars: 1,
            accent_pattern: vec![],
            sections: vec![
                Section {
                    name: "A".into(),
                    start_bar: 5,
                    length_bars: 8,
                    loopable: false,
                    cue_text: None,
                    cue_lead_beats: 4,
                },
                Section {
                    name: "B".into(),
                    start_bar: 13,
                    length_bars: 4,
                    loopable: false,
                    cue_text: None,
                    cue_lead_beats: 4,
                },
            ],
            tracks: vec![],
            disabled: false,
        };
        let order = [PerformanceEntry::once(1), PerformanceEntry::once(0)];
        let resolved = resolve_order(&song, 48000, &order).unwrap();
        let g = grid();
        let mut perf_bar = 0i64;
        for r in &resolved {
            let section = &song.sections[r.section_index];
            let e = make_entry(
                &g,
                song.offset_samples,
                r.section_index,
                (section.start_bar - 1) as i64,
                perf_bar,
                section.length_bars,
            );
            assert_eq!(e.perf_start_sample, r.perf_start_sample);
            assert_eq!(e.perf_end_sample, r.perf_end_sample);
            assert_eq!(e.source_start_sample, r.source_start_sample);
            assert_eq!(e.source_end_sample, r.source_end_sample);
            assert_eq!(e.perf_start_pulse, r.perf_start_pulse);
            assert_eq!(e.perf_end_pulse, r.perf_end_pulse);
            perf_bar += section.length_bars as i64;
        }
    }

    #[test]
    fn truncate_active_recomputes_grid_exact_bounds() {
        let g = grid();
        let mut core = PlaybackCore::new(
            g,
            2400,
            vec![],
            CoreClick {
                pattern: vec![1, 0, 0, 0],
                cfg: ClickSynthConfig::default(),
                bus: 1,
                gain: Smoother::settled(1.0),
            },
            vec![
                CoreBus {
                    limiter_enabled: true,
                },
                CoreBus {
                    limiter_enabled: false,
                },
            ],
        );
        let e = make_entry(&g, 2400, 0, 4, 0, 8);
        core.start(e, 0);
        assert!(core.truncate_active_to_bar(3));
        let a = *core.active().unwrap();
        assert_eq!(a.length_bars, 3);
        assert_eq!(a.perf_end_pulse, g.bar_to_pulse(3));
        assert_eq!(a.perf_end_sample, g.pulse_to_sample(g.bar_to_pulse(3)));
        assert_eq!(
            a.source_end_sample,
            g.pulse_to_sample(g.bar_to_pulse(4 + 3)) + 2400
        );
        // Boundary at or past the natural end: refused.
        assert!(!core.truncate_active_to_bar(3));
        assert!(!core.truncate_active_to_bar(0));
    }
}
