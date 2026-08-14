//! Generated click synthesis (`docs/SPEC.md` §5).
//!
//! Click waveform: a cosine burst under a fast exponential decay envelope. Cosine
//! (not sine) is deliberate -- the envelope is at its maximum exactly at the hit's
//! start sample, so the transient's peak sample is always exactly
//! `grid.pulse_to_sample(pulse)`, which is what the offline-render tests check against
//! a DAW grid.
//!
//! [`hit_value`], [`pulse_voice`], [`hit_length_samples`], and
//! [`effective_accent_pattern`] are the real-time click synth: the offline renderer
//! (`render.rs`) and the live audio callback (`core::PlaybackCore::render_click_block`)
//! call exactly these functions, which is what makes their click output bit-identical
//! no matter how a hit is split across processing blocks.
//!
//! The click scheduler reads beat positions from [`crate::timeline::Grid`] only. It
//! never computes its own pulse-to-sample conversion, so it cannot drift from section
//! timing by construction (§5). Count-in (§6) is this same synth firing at negative
//! pulses before performance-time 0 -- `Grid::pulse_to_sample` is symmetric for
//! negative pulses by construction, so count-in needed no separate click code path,
//! only a scheduler willing to ask for negative pulses (see
//! `core::PlaybackCore::start`).

/// Any nonzero accent intensity gets the accent pitch/level; 0 is a normal click. See
/// [`effective_accent_pattern`] for how an unset (empty) pattern falls back to
/// "accent beat 1 only". Multi-level intensity (louder accents at higher values) is a
/// known limitation: this synth only distinguishes accented vs. not.
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
