//! Generated click synthesis (`docs/SPEC.md` §5) -- **provisional**.
//!
//! Phase 3 owns the real-time click synth that runs in the audio callback; this
//! module exists so the offline renderer (phase 1's deliverable) has something to
//! render. It is a plain, non-real-time function -- no callback, no scheduler state --
//! and is written so phase 3 can extend it (real-time click gain smoothing, live
//! accent editing) rather than replace it outright: the actual tone-generation maths
//! (`synthesize_hit`) is the part expected to survive unchanged.
//!
//! The click scheduler reads beat positions from [`crate::timeline::Grid`] only. It
//! never computes its own pulse-to-sample conversion, so it cannot drift from section
//! timing by construction (§5).
//!
//! Click waveform: a cosine burst under a fast exponential decay envelope. Cosine
//! (not sine) is deliberate -- the envelope is at its maximum exactly at the hit's
//! start sample, so the transient's peak sample is always exactly
//! `grid.pulse_to_sample(pulse)`, which is what the offline-render tests check against
//! a DAW grid.

use crate::timeline::Grid;

/// Any nonzero accent intensity gets the accent pitch/level; 0 is a normal click. See
/// [`effective_accent_pattern`] for how an unset (empty) pattern falls back to
/// "accent beat 1 only". Multi-level intensity (louder accents at higher values) is
/// left to phase 3; this provisional synth only distinguishes accented vs. not.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClickSynthConfig {
    pub decay_ms: f64,
    pub base_freq_hz: f64,
    pub accent_freq_hz: f64,
    pub base_gain: f32,
    pub accent_gain: f32,
}

impl Default for ClickSynthConfig {
    fn default() -> Self {
        ClickSynthConfig {
            decay_ms: 40.0,
            base_freq_hz: 1000.0,
            accent_freq_hz: 1500.0,
            base_gain: 0.6,
            accent_gain: 0.9,
        }
    }
}

/// If `pattern` is empty, fall back to "accent beat 1, everything else normal" for a
/// bar of `pulses_per_bar` pulses -- the default described in §5 ("distinct pitch and
/// level on beat 1"). If `pattern`'s length doesn't match `pulses_per_bar`, still use
/// it but treat any pulse index past the end of the pattern as unaccented (0), rather
/// than panicking on a malformed project -- `Project::validate` is the place that
/// rejects a mismatched pattern outright; this function just has to not crash.
pub fn effective_accent_pattern(pattern: &[u8], pulses_per_bar: i64) -> Vec<u8> {
    if !pattern.is_empty() {
        return pattern.to_vec();
    }
    let mut default = vec![0u8; pulses_per_bar.max(0) as usize];
    if let Some(first) = default.first_mut() {
        *first = 1;
    }
    default
}

/// Total length of one hit in samples: the envelope has decayed to exp(-5) (~0.7%)
/// by `decay_ms`, and the hit runs 1.5x that so the truncated tail is inaudible.
/// Shared by the offline and real-time paths so both agree on where a hit ends.
pub fn hit_length_samples(sample_rate: u32, decay_ms: f64) -> i64 {
    ((decay_ms / 1000.0) * 1.5 * sample_rate as f64).ceil() as i64
}

/// The value of one hit at sample `i` (0-based from the hit's start). This is *the*
/// per-sample click formula: both the offline renderer and the real-time core call
/// exactly this function, which is what makes their click output bit-identical no
/// matter how a hit is split across processing blocks (the value depends only on
/// `i`, never on any accumulated state).
#[inline]
pub fn hit_value(i: i64, sample_rate: u32, freq_hz: f64, gain: f32, decay_ms: f64) -> f32 {
    // Time constant chosen so the envelope has decayed to exp(-5) (~0.7%) by decay_ms.
    let tau_samples = decay_ms / 1000.0 * sample_rate as f64 / 5.0;
    let t = i as f64;
    let envelope = (-t / tau_samples).exp();
    let phase = 2.0 * std::f64::consts::PI * freq_hz * t / sample_rate as f64;
    (envelope * phase.cos()) as f32 * gain
}

/// Pick the (frequency, gain) for a pulse from the accent pattern. Any nonzero
/// intensity gets the accent voice; out-of-range indices are unaccented (see
/// [`effective_accent_pattern`] for why that must not panic).
#[inline]
pub fn pulse_voice(
    pattern: &[u8],
    pulses_per_bar: i64,
    pulse: i64,
    cfg: &ClickSynthConfig,
) -> (f64, f32) {
    let idx_in_bar = pulse.rem_euclid(pulses_per_bar) as usize;
    let intensity = pattern.get(idx_in_bar).copied().unwrap_or(0);
    if intensity > 0 {
        (cfg.accent_freq_hz, cfg.accent_gain)
    } else {
        (cfg.base_freq_hz, cfg.base_gain)
    }
}

/// Write one decaying cosine burst into `out`, added to whatever is already there
/// (so multiple hits / bus contributions can be mixed by repeated calls). `start_sample`
/// may be negative or beyond `out.len()`; samples outside `[0, out.len())` are simply
/// not written.
pub fn synthesize_hit(
    out: &mut [f32],
    start_sample: i64,
    sample_rate: u32,
    freq_hz: f64,
    gain: f32,
    decay_ms: f64,
) {
    if gain == 0.0 || !freq_hz.is_finite() || freq_hz <= 0.0 {
        return;
    }
    let hit_len = hit_length_samples(sample_rate, decay_ms);
    for i in 0..hit_len {
        let idx = start_sample + i;
        if idx < 0 {
            continue;
        }
        let idx = idx as usize;
        if idx >= out.len() {
            break;
        }
        out[idx] += hit_value(i, sample_rate, freq_hz, gain, decay_ms);
    }
}

/// Render every click pulse in the performance-time pulse range `[start_pulse,
/// end_pulse)` into `out`, additively. `sample_offset` (typically the render's
/// `lead_in_samples`) is added to every pulse's sample position before writing, so
/// this function never needs to know about render-level lead-in itself.
///
/// `accent_pattern` is indexed by `pulse.rem_euclid(pulses_per_bar)` -- valid because
/// performance-time bar 0 always starts at pulse 0 by construction (see
/// `crate::sections::resolve_order`), so every pulse's phase within its bar lines up
/// with the pattern regardless of which section it came from.
pub fn render_click_pulses(
    grid: &Grid,
    accent_pattern: &[u8],
    start_pulse: i64,
    end_pulse: i64,
    cfg: &ClickSynthConfig,
    sample_offset: i64,
    out: &mut [f32],
) {
    let pulses_per_bar = grid.pulses_per_bar();
    let pattern = effective_accent_pattern(accent_pattern, pulses_per_bar);
    if pattern.is_empty() {
        return;
    }

    for pulse in start_pulse..end_pulse {
        let (freq, gain) = pulse_voice(&pattern, pulses_per_bar, pulse, cfg);
        let sample_pos = grid.pulse_to_sample(pulse) + sample_offset;
        synthesize_hit(
            out,
            sample_pos,
            grid.sample_rate(),
            freq,
            gain,
            cfg.decay_ms,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::timeline::TimeSignature;

    #[test]
    fn default_pattern_accents_only_beat_one() {
        let p = effective_accent_pattern(&[], 4);
        assert_eq!(p, vec![1, 0, 0, 0]);
    }

    #[test]
    fn explicit_pattern_passes_through_unchanged() {
        let p = effective_accent_pattern(&[2, 0, 0, 1, 0, 0, 0], 7);
        assert_eq!(p, vec![2, 0, 0, 1, 0, 0, 0]);
    }

    #[test]
    fn synthesize_hit_peaks_exactly_at_start_sample() {
        let sample_rate = 48000u32;
        let mut buf = vec![0.0f32; sample_rate as usize / 2];
        let start = 1000i64;
        synthesize_hit(&mut buf, start, sample_rate, 1000.0, 0.8, 40.0);

        // Peak magnitude across the whole hit occurs at the start sample: envelope is
        // monotonically decreasing from 1.0, and |cos| <= 1, so envelope(t)*|cos(t)|
        // <= envelope(0)*|cos(0)| = gain for all t >= 0.
        let (peak_idx, peak_val) = buf
            .iter()
            .enumerate()
            .max_by(|a, b| a.1.abs().partial_cmp(&b.1.abs()).unwrap())
            .unwrap();
        assert_eq!(peak_idx, start as usize);
        assert!((peak_val.abs() - 0.8).abs() < 1e-6);
    }

    #[test]
    fn synthesize_hit_out_of_bounds_start_does_not_panic() {
        let mut buf = vec![0.0f32; 100];
        synthesize_hit(&mut buf, -50, 48000, 1000.0, 0.8, 40.0);
        synthesize_hit(&mut buf, 1_000_000, 48000, 1000.0, 0.8, 40.0);
        // No panic is the assertion; also confirm nothing spurious got written from
        // the deeply-negative-start call (its hit fully precedes sample 0... unless
        // it partially overlaps, in which case tail samples near 0 may be nonzero,
        // which is fine and expected).
        let _ = buf;
    }

    #[test]
    fn render_click_pulses_places_transients_on_exact_grid_samples() {
        let grid = Grid::new(48000, 178.0, TimeSignature::FOUR_FOUR).unwrap();
        let cfg = ClickSynthConfig::default();
        let mut out = vec![0.0f32; grid.pulse_to_sample(20) as usize + 10_000];
        render_click_pulses(&grid, &[], 0, 20, &cfg, 0, &mut out);

        for pulse in 0..20i64 {
            let expected = grid.pulse_to_sample(pulse) as usize;
            assert!(
                out[expected].abs() > 0.0,
                "expected a transient at pulse {pulse}, sample {expected}"
            );
        }
    }

    #[test]
    fn render_click_pulses_respects_sample_offset() {
        let grid = Grid::new(48000, 178.0, TimeSignature::FOUR_FOUR).unwrap();
        let cfg = ClickSynthConfig::default();
        let offset = 5000i64;
        let mut out = vec![0.0f32; grid.pulse_to_sample(4) as usize + offset as usize + 10_000];
        render_click_pulses(&grid, &[], 0, 4, &cfg, offset, &mut out);
        for pulse in 0..4i64 {
            let expected = (grid.pulse_to_sample(pulse) + offset) as usize;
            assert!(out[expected].abs() > 0.0);
        }
    }
}
