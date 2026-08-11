//! Equal-power crossfade at playhead splices (`docs/SPEC.md` §7 / CLAUDE.md invariant 6).
//!
//! Looping back, or advancing to a non-adjacent section within a single mixed
//! backtrack, is an audio splice: the two sides of the join are unrelated audio, and
//! playing one after the other with a hard cut clicks. Per §7: "keep the outgoing
//! read position alive for the fade duration and mix both," with an equal-power pair
//! so the perceived loudness stays constant through the fade (a linear crossfade dips
//! in the middle for decorrelated material).
//!
//! This module only computes the fade curve and its length; applying it -- reading
//! the outgoing and incoming audio and mixing -- is [`crate::render`]'s job (and,
//! later, the real-time transport's).

pub const CROSSFADE_MS: f64 = 15.0;

/// Length of the crossfade in samples at `sample_rate`, optionally capped (e.g. to the
/// incoming entry's own length, so a fade never reads past a section shorter than
/// 15 ms). `cap_samples` of `None` or `<= 0` is treated as "no cap."
pub fn crossfade_length_samples(sample_rate: u32, cap_samples: Option<i64>) -> i64 {
    let full = ((CROSSFADE_MS / 1000.0) * sample_rate as f64).round() as i64;
    match cap_samples {
        Some(cap) if cap > 0 => full.min(cap),
        _ => full,
    }
}

/// Equal-power gain pair for fade position `t` in `[0, 1)`: `(gain_out, gain_in)`.
/// `gain_out` starts at 1 and falls to (near) 0; `gain_in` starts at (near) 0 and
/// rises to 1. `sin^2 + cos^2 == 1` exactly (to float precision) for every `t`, which
/// is the "equal power" property: the two gains' *power* sums to a constant, so a
/// crossfade between decorrelated signals of equal level doesn't dip or bump.
pub fn equal_power_gains(t: f64) -> (f32, f32) {
    let angle = t.clamp(0.0, 1.0) * std::f64::consts::FRAC_PI_2;
    (angle.cos() as f32, angle.sin() as f32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn length_matches_15ms_at_common_rates() {
        assert_eq!(crossfade_length_samples(48000, None), 720);
        assert_eq!(crossfade_length_samples(44100, None), 662); // round(661.5) = 662
    }

    #[test]
    fn length_is_capped_when_shorter_than_full_fade() {
        assert_eq!(crossfade_length_samples(48000, Some(100)), 100);
        assert_eq!(crossfade_length_samples(48000, Some(10_000)), 720);
        assert_eq!(crossfade_length_samples(48000, Some(0)), 720); // cap <= 0 means "no cap"
        assert_eq!(crossfade_length_samples(48000, None), 720);
    }

    #[test]
    fn gains_are_equal_power_across_the_fade() {
        for i in 0..=1000 {
            let t = i as f64 / 1000.0;
            let (g_out, g_in) = equal_power_gains(t);
            let power = (g_out as f64).powi(2) + (g_in as f64).powi(2);
            assert!(
                (power - 1.0).abs() < 1e-6,
                "power at t={t} was {power}, expected 1.0"
            );
        }
    }

    #[test]
    fn gains_at_endpoints() {
        let (g_out, g_in) = equal_power_gains(0.0);
        assert!((g_out - 1.0).abs() < 1e-6);
        assert!(g_in.abs() < 1e-6);

        let (g_out, g_in) = equal_power_gains(1.0);
        assert!(g_out.abs() < 1e-6);
        assert!((g_in - 1.0).abs() < 1e-6);
    }
}
