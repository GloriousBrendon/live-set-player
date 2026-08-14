//! The timeline grid: bar/pulse <-> sample conversion.
//!
//! This is the one piece of maths the whole application is built on top of. See
//! `docs/SPEC.md` §2 for the full rationale; the short version:
//!
//! - A **pulse** is one tick of the time-signature denominator (a quarter note in 4/4,
//!   an eighth note in 7/8). BPM counts quarter notes per minute (the DAW convention),
//!   so the pulse duration also depends on the denominator:
//!   `samples_per_pulse = sample_rate * 60 / bpm * 4 / denominator`.
//! - A **bar** is `numerator` pulses.
//! - Every sample position is computed from an **absolute pulse index** with a single
//!   multiply-and-round: `round(absolute_pulse * samples_per_pulse)`. Positions are
//!   never produced by repeatedly adding `samples_per_pulse` to a running total --
//!   `f64` addition of a non-integer step accumulates rounding error every step, and
//!   at 178 BPM / 48 kHz (16179.775 samples/pulse) that error is audible within
//!   minutes. See the `naive_*_accumulation_drifts` tests below for the measured size
//!   of that failure.
//!
//! There are two coordinate systems layered on top of the grid, and keeping them
//! separate is what makes reordering a song free:
//!
//! - **Performance time**: the output/transport timeline. Pulse 0 is the first beat of
//!   the first performance bar. Click, cues, and section boundaries are all scheduled
//!   in this space. It has no offset.
//! - **Source frames**: positions inside a backtrack file, via [`SourceMap`]:
//!   `source_frame = round(source_pulse * samples_per_pulse) + song.offset_samples`.
//!   `offset_samples` (where bar 1 beat 1 sits inside the backtrack, to compensate for
//!   leading silence in a Reaper render) applies *only* here. Section reordering is a
//!   mapping from performance pulses to source frames -- the click scheduler never
//!   needs to know a reorder happened.

use crate::error::TimelineError;
use serde::{Deserialize, Serialize};

/// A musical time signature, e.g. 4/4 or 7/8.
///
/// `denominator` must be a power of two (1, 2, 4, 8, 16, 32, ...). `numerator` is the
/// number of pulses per bar and must be at least 1 (odd meters like 7/8 are supported
/// and exercised by the test suite).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimeSignature {
    pub numerator: u32,
    pub denominator: u32,
}

impl TimeSignature {
    pub const FOUR_FOUR: TimeSignature = TimeSignature {
        numerator: 4,
        denominator: 4,
    };

    pub(crate) fn validate(self) -> Result<(), TimelineError> {
        if self.numerator == 0 {
            return Err(TimelineError::InvalidNumerator(self.numerator));
        }
        if self.denominator == 0 || !self.denominator.is_power_of_two() {
            return Err(TimelineError::InvalidDenominator(self.denominator));
        }
        Ok(())
    }
}

/// The sample-rate/tempo/time-signature grid for one song.
///
/// Immutable once constructed. Holds the precomputed `samples_per_pulse` so every
/// conversion is a single multiply, never a repeated one.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Grid {
    sample_rate: u32,
    bpm: f64,
    time_sig: TimeSignature,
    samples_per_pulse: f64,
}

impl Grid {
    pub fn new(sample_rate: u32, bpm: f64, time_sig: TimeSignature) -> Result<Self, TimelineError> {
        if sample_rate == 0 {
            return Err(TimelineError::InvalidSampleRate);
        }
        if !bpm.is_finite() || bpm <= 0.0 {
            return Err(TimelineError::InvalidBpm(bpm.to_string()));
        }
        time_sig.validate()?;

        let samples_per_pulse = sample_rate as f64 * 60.0 / bpm * 4.0 / time_sig.denominator as f64;

        Ok(Grid {
            sample_rate,
            bpm,
            time_sig,
            samples_per_pulse,
        })
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn bpm(&self) -> f64 {
        self.bpm
    }

    pub fn time_signature(&self) -> TimeSignature {
        self.time_sig
    }

    /// Samples per pulse (one tick of the time-signature denominator). Almost never an
    /// integer -- see module docs.
    pub fn samples_per_pulse(&self) -> f64 {
        self.samples_per_pulse
    }

    /// Pulses per bar. Equal to the time signature's numerator by definition.
    pub fn pulses_per_bar(&self) -> i64 {
        self.time_sig.numerator as i64
    }

    /// Convert an absolute pulse index (performance-time, no offset) to a sample
    /// position. This is the *only* place a pulse becomes a sample, and it is always a
    /// single `round(pulse * samples_per_pulse)` -- never an accumulation.
    pub fn pulse_to_sample(&self, pulse: i64) -> i64 {
        round_half_away_from_zero(pulse as f64 * self.samples_per_pulse)
    }

    /// The absolute pulse index of the first beat of `bar_index` (0-based). Exact
    /// integer arithmetic -- bar counts never touch floating point.
    pub fn bar_to_pulse(&self, bar_index: i64) -> i64 {
        bar_index * self.pulses_per_bar()
    }

    /// Sample position of a given (0-based bar, pulse-within-bar) pair.
    pub fn bar_beat_to_sample(&self, bar_index: i64, pulse_in_bar: i64) -> i64 {
        self.pulse_to_sample(self.bar_to_pulse(bar_index) + pulse_in_bar)
    }

    /// The pulse whose sample position is the largest one `<= sample` -- "which pulse
    /// is the transport currently in / just past," the question a live position
    /// display or a quantize-to-next-bar-boundary needs answered.
    ///
    /// A plain `floor(sample / samples_per_pulse)` is *not* correct here: because
    /// [`Grid::pulse_to_sample`] rounds to the nearest sample, `pulse_to_sample(n)` can
    /// land up to 0.5 samples *below* pulse `n`'s exact continuous position. Dividing
    /// that rounded-down sample back by `samples_per_pulse` then yields a ratio a hair
    /// under the integer `n`, and a plain floor reads that as pulse `n - 1` -- not a
    /// rare edge case but roughly half of all pulses (whenever `pulse_to_sample`
    /// happened to round down). We compensate by nudging the ratio up by slightly more
    /// than that worst-case 0.5-sample gap (in pulse units, `0.5 / samples_per_pulse`)
    /// before flooring, which is small enough to never misattribute a sample that's
    /// genuinely mid-pulse, and exactly cancels the rounding-direction bias for
    /// samples that came from `pulse_to_sample` in the first place.
    pub fn sample_to_pulse(&self, sample: i64) -> i64 {
        let ratio = sample as f64 / self.samples_per_pulse;
        let epsilon = 0.5 / self.samples_per_pulse + 1e-9;
        (ratio + epsilon).floor() as i64
    }

    /// Decompose a sample position into (0-based bar index, pulse within bar, sample
    /// remainder within that pulse). `remainder` is the offset from the pulse's own
    /// sample position and is small (roughly `[0, samples_per_pulse)`), useful for
    /// sub-pulse diagnostics.
    pub fn sample_to_bar_beat(&self, sample: i64) -> (i64, i64, i64) {
        let pulse = self.sample_to_pulse(sample);
        let pulses_per_bar = self.pulses_per_bar();
        let bar = pulse.div_euclid(pulses_per_bar);
        let pulse_in_bar = pulse.rem_euclid(pulses_per_bar);
        let remainder = sample - self.pulse_to_sample(pulse);
        (bar, pulse_in_bar, remainder)
    }
}

/// Maps performance-time pulses in one song's *source* material (the backtrack file's
/// own bar numbering) to sample frames inside that file, accounting for the song's
/// `offset_samples` (leading silence before bar 1 beat 1).
#[derive(Debug, Clone, Copy)]
pub struct SourceMap {
    grid: Grid,
    offset_samples: i64,
}

impl SourceMap {
    pub fn new(grid: Grid, offset_samples: i64) -> Self {
        SourceMap {
            grid,
            offset_samples,
        }
    }

    pub fn grid(&self) -> &Grid {
        &self.grid
    }

    pub fn offset_samples(&self) -> i64 {
        self.offset_samples
    }

    /// Convert a pulse index expressed in the *source* file's own bar numbering (i.e.
    /// `grid.bar_to_pulse(source_bar_index)`, or an arbitrary pulse offset from it)
    /// into an absolute sample frame inside that file.
    pub fn source_frame(&self, source_pulse: i64) -> i64 {
        self.grid.pulse_to_sample(source_pulse) + self.offset_samples
    }
}

/// `f64::round` already rounds half away from zero (not banker's rounding), which is
/// what we want here. This wrapper exists so call sites read as a deliberate choice
/// and so the drift tests can name the exact rounding rule they're checking against.
fn round_half_away_from_zero(x: f64) -> i64 {
    x.round() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- Reference implementation: exact rational arithmetic in i128, independent of
    // the f64 path under test. bpm is given as an exact fraction `p/q` (e.g. 143.5 =
    // 287/2), so:
    //   samples_per_pulse = sample_rate * 60 / bpm * 4 / denominator
    //                     = sample_rate * 240 * q / (p * denominator)
    // and pulse_to_sample(n) = round(n * sample_rate * 240 * q / (p * denominator)),
    // with `round` implemented as exact half-away-from-zero rounding on the rational
    // number, matching `f64::round`'s rounding rule (but not its floating-point path).
    fn rational_pulse_to_sample(
        pulse: i64,
        sample_rate: u32,
        bpm_numerator: i64,
        bpm_denominator: i64,
        time_sig_denominator: u32,
    ) -> i64 {
        let num = (pulse as i128) * (sample_rate as i128) * 240 * (bpm_denominator as i128);
        let den = (bpm_numerator as i128) * (time_sig_denominator as i128);
        rational_round_half_away_from_zero(num, den)
    }

    /// round(num/den) to the nearest integer, half away from zero. `den` must be > 0;
    /// `num` may be negative.
    fn rational_round_half_away_from_zero(num: i128, den: i128) -> i64 {
        debug_assert!(den > 0);
        let (sign, num_abs) = if num < 0 {
            (-1i128, -num)
        } else {
            (1i128, num)
        };
        let rounded_abs = (2 * num_abs + den) / (2 * den);
        (sign * rounded_abs) as i64
    }

    /// Distance of `num/den` from the nearest `.5` rounding-tie boundary (i.e. how far
    /// `samples_per_pulse * n` sits from a value ending in exactly `.5`). Landing
    /// exactly on an *integer* is not a tie -- it rounds to itself with no ambiguity --
    /// so only the `0.5` boundary matters here. Used to assert the drift tests below
    /// aren't accidentally relying on a round-half tie-break rule.
    fn distance_from_half_boundary(num: i128, den: i128) -> f64 {
        let frac = ((num.rem_euclid(den)) as f64) / (den as f64); // in [0, 1)
        (frac - 0.5).abs()
    }

    /// (bpm_numerator, bpm_denominator, display bpm) for the three BPMs under test,
    /// as exact fractions.
    const TEST_BPMS: [(i64, i64, f64); 3] = [(178, 1, 178.0), (287, 2, 143.5), (91, 1, 91.0)];
    const TEST_SAMPLE_RATES: [u32; 2] = [44100, 48000];

    #[test]
    fn rational_round_matches_f64_round_on_known_values() {
        assert_eq!(rational_round_half_away_from_zero(5, 2), 3); // 2.5 -> 3
        assert_eq!(rational_round_half_away_from_zero(-5, 2), -3); // -2.5 -> -3
        assert_eq!(rational_round_half_away_from_zero(7, 2), 4); // 3.5 -> 4
        assert_eq!(rational_round_half_away_from_zero(4, 3), 1); // 1.333 -> 1
        assert_eq!(rational_round_half_away_from_zero(0, 5), 0);
    }

    /// The core drift guard: every pulse over 15 minutes, at three non-integer BPMs
    /// and both project sample rates, must match the exact rational reference with
    /// zero error. This is what "never accumulate beat positions" buys you.
    #[test]
    fn exact_matches_rational_reference_15_minutes() {
        for &sample_rate in &TEST_SAMPLE_RATES {
            for &(p, q, bpm) in &TEST_BPMS {
                let grid = Grid::new(sample_rate, bpm, TimeSignature::FOUR_FOUR).unwrap();
                // 15 minutes of quarter-note pulses at this bpm, generously rounded up.
                let pulses_in_15_min = (bpm * 15.0).ceil() as i64 + 8;
                for n in 0..pulses_in_15_min {
                    let got = grid.pulse_to_sample(n);
                    let want = rational_pulse_to_sample(n, sample_rate, p, q, 4);
                    assert_eq!(
                        got, want,
                        "pulse {n} at {bpm} bpm / {sample_rate} Hz: got {got}, want {want}"
                    );
                }
            }
        }
    }

    /// Precondition check for the test above: none of the (pulse, bpm, rate) cases
    /// exercised land within 1e-3 of a `.5` rounding boundary, so the exact-match
    /// assertion isn't silently passing courtesy of a shared tie-break rule between
    /// the f64 path and the rational reference. States the test's own precondition
    /// instead of relying on it by luck.
    #[test]
    fn drift_test_cases_avoid_rounding_ties() {
        const MIN_DISTANCE: f64 = 1e-3;
        for &sample_rate in &TEST_SAMPLE_RATES {
            for &(p, q, bpm) in &TEST_BPMS {
                let pulses_in_15_min = (bpm * 15.0).ceil() as i64 + 8;
                for n in 1..pulses_in_15_min {
                    let num = (n as i128) * (sample_rate as i128) * 240 * (q as i128);
                    let den = (p as i128) * 4;
                    let dist = distance_from_half_boundary(num, den);
                    assert!(
                        dist >= MIN_DISTANCE,
                        "pulse {n} at {bpm} bpm / {sample_rate} Hz sits within {dist} of a rounding tie"
                    );
                }
            }
        }
    }

    /// Spot check at 60 minutes so the guard isn't only proven over a short window.
    #[test]
    fn exact_matches_rational_reference_60_minutes_spot_check() {
        let sample_rate = 48000u32;
        let (p, q, bpm) = (178, 1, 178.0);
        let grid = Grid::new(sample_rate, bpm, TimeSignature::FOUR_FOUR).unwrap();
        let pulses_in_60_min = (bpm * 60.0) as i64;
        // Check every pulse near the start, middle, and end rather than all ~10,680,
        // to keep the test fast; the per-pulse formula has no state, so there is
        // nothing for a spot check to miss that a full sweep would catch.
        let checkpoints = [0, 1, 2, 3]
            .into_iter()
            .chain(pulses_in_60_min / 2 - 2..=pulses_in_60_min / 2 + 2)
            .chain(pulses_in_60_min - 4..pulses_in_60_min);
        for n in checkpoints {
            let got = grid.pulse_to_sample(n);
            let want = rational_pulse_to_sample(n, sample_rate, p, q, 4);
            assert_eq!(got, want, "pulse {n} at 60-minute mark");
        }
    }

    /// The regression guard: this is the test that fails under naive incremental
    /// accumulation and passes only with the correct round-from-absolute-index
    /// implementation. Truncating accumulation (`pos += spp as i64` every pulse) is
    /// the crudest wrong approach and drifts the most.
    #[test]
    fn naive_truncating_accumulation_drifts_but_correct_path_does_not() {
        let sample_rate = 48000u32;
        let bpm = 178.0;
        let grid = Grid::new(sample_rate, bpm, TimeSignature::FOUR_FOUR).unwrap();
        let spp = grid.samples_per_pulse();

        let ten_minutes_pulses = (bpm * 10.0) as i64; // 1780 pulses

        // Naive: truncate the step every time and accumulate.
        let mut naive_pos: i64 = 0;
        for _ in 0..ten_minutes_pulses {
            naive_pos += spp as i64; // truncates towards zero each step
        }
        let correct_pos = grid.pulse_to_sample(ten_minutes_pulses);

        let naive_error_samples = (correct_pos - naive_pos).abs();
        let naive_error_ms = naive_error_samples as f64 * 1000.0 / sample_rate as f64;

        assert!(
            naive_error_ms > 1.0,
            "expected the naive truncating accumulator to drift by more than 1ms over \
             10 minutes at 178bpm/48kHz, but it only drifted {naive_error_ms}ms -- this \
             test is supposed to demonstrate the failure mode invariant 2 warns about"
        );

        // The correct, non-accumulating path has zero error by construction: it *is*
        // the reference other assertions in this file are compared against. Restated
        // here so this one test is a self-contained demonstration of "accumulation
        // drifts, round-from-absolute-index does not".
        let want = rational_pulse_to_sample(ten_minutes_pulses, sample_rate, 178, 1, 4);
        assert_eq!(correct_pos, want);
    }

    /// Same demonstration with a less crude naive implementation (rounding the step
    /// instead of truncating it). Still drifts, just more slowly.
    #[test]
    fn naive_rounded_accumulation_drifts() {
        let sample_rate = 48000u32;
        let bpm = 178.0;
        let grid = Grid::new(sample_rate, bpm, TimeSignature::FOUR_FOUR).unwrap();
        let spp = grid.samples_per_pulse();

        let ten_minutes_pulses = (bpm * 10.0) as i64;

        let mut naive_pos: i64 = 0;
        for _ in 0..ten_minutes_pulses {
            naive_pos += spp.round() as i64;
        }
        let correct_pos = grid.pulse_to_sample(ten_minutes_pulses);

        let naive_error_samples = (correct_pos - naive_pos).abs();
        let naive_error_ms = naive_error_samples as f64 * 1000.0 / sample_rate as f64;

        assert!(
            naive_error_ms > 1.0,
            "expected rounded accumulation to drift by more than 1ms over 10 minutes \
             at 178bpm/48kHz, but it only drifted {naive_error_ms}ms"
        );
    }

    /// BPM is quarter-notes-per-minute (DAW convention): the pulse duration also
    /// depends on the time-signature denominator. In 7/8 at 178 BPM, the pulse (an
    /// eighth note) is half the duration of a quarter note, so pulses fire at
    /// 356/minute, not 178/minute. Guards the `* 4 / denominator` term against a
    /// silent regression back to a 4/4-only formula.
    #[test]
    fn seven_eight_pulse_rate_is_356_per_minute_at_178_bpm() {
        let sample_rate = 48000u32;
        let ts_7_8 = TimeSignature {
            numerator: 7,
            denominator: 8,
        };
        let grid = Grid::new(sample_rate, 178.0, ts_7_8).unwrap();

        // 356 eighth-note pulses in exactly 60 seconds.
        assert_eq!(grid.pulse_to_sample(356), sample_rate as i64 * 60);
        // A 7/8 bar is 7 pulses.
        assert_eq!(grid.pulses_per_bar(), 7);
        // The pulse duration itself: sample_rate*60/178*4/8.
        let expected_spp = sample_rate as f64 * 60.0 / 178.0 * 4.0 / 8.0;
        assert!((grid.samples_per_pulse() - expected_spp).abs() < 1e-9);
        assert!((grid.samples_per_pulse() - 8089.887640449438).abs() < 1e-6);

        // For comparison, 4/4 at the same BPM ticks at 178 pulses/minute, not 356.
        let ts_4_4 = TimeSignature::FOUR_FOUR;
        let grid_4_4 = Grid::new(sample_rate, 178.0, ts_4_4).unwrap();
        assert_eq!(grid_4_4.pulse_to_sample(178), sample_rate as i64 * 60);
        assert!((grid_4_4.samples_per_pulse() - 16179.775280898877).abs() < 1e-6);
    }

    #[test]
    fn negative_pulses_are_symmetric_for_count_in() {
        let grid = Grid::new(48000, 178.0, TimeSignature::FOUR_FOUR).unwrap();
        for n in 1..2000i64 {
            assert_eq!(grid.pulse_to_sample(-n), -grid.pulse_to_sample(n));
        }
    }

    #[test]
    fn bar_to_pulse_is_exact_integer_arithmetic() {
        let grid = Grid::new(48000, 143.5, TimeSignature::FOUR_FOUR).unwrap();
        assert_eq!(grid.bar_to_pulse(0), 0);
        assert_eq!(grid.bar_to_pulse(1), 4);
        assert_eq!(grid.bar_to_pulse(100), 400);
        assert_eq!(grid.bar_to_pulse(-1), -4);
    }

    /// `sample_to_bar_beat(bar_beat_to_sample(bar, pulse))` must recover exactly
    /// `(bar, pulse, 0)` -- this is the regression guard for the `sample_to_pulse`
    /// rounding-direction fix (see its doc comment): a plain `floor(sample /
    /// samples_per_pulse)` fails this for roughly half of all pulses, wherever
    /// `pulse_to_sample` happened to round down, because the rounded-down sample
    /// divided back out sits a hair under the target integer and floors to one pulse
    /// short. Swept across the same BPM/rate combinations as the drift tests, since
    /// the failure rate depends on the fractional part of `samples_per_pulse`.
    #[test]
    fn sample_to_bar_beat_round_trips_exactly() {
        for &sample_rate in &TEST_SAMPLE_RATES {
            for &(_, _, bpm) in &TEST_BPMS {
                let grid = Grid::new(sample_rate, bpm, TimeSignature::FOUR_FOUR).unwrap();
                for bar in 0..50i64 {
                    for pulse_in_bar in 0..4i64 {
                        let sample = grid.bar_beat_to_sample(bar, pulse_in_bar);
                        let got = grid.sample_to_bar_beat(sample);
                        assert_eq!(
                            got,
                            (bar, pulse_in_bar, 0),
                            "bpm {bpm} / {sample_rate} Hz: bar {bar} pulse {pulse_in_bar} \
                             (sample {sample}) did not round-trip"
                        );
                    }
                }
            }
        }
    }

    /// Same guarantee as [`sample_to_bar_beat_round_trips_exactly`], extended to
    /// negative pulses -- the range count-in (`docs/SPEC.md` §6) schedules into.
    /// `sample_to_bar_beat` decomposes into a non-negative `pulse_in_bar` via
    /// `rem_euclid`, so this checks the round trip through `sample_to_pulse` directly
    /// instead, which is the primitive count-in status reporting depends on.
    #[test]
    fn sample_to_pulse_round_trips_exactly_for_negative_pulses() {
        for &sample_rate in &TEST_SAMPLE_RATES {
            for &(_, _, bpm) in &TEST_BPMS {
                let grid = Grid::new(sample_rate, bpm, TimeSignature::FOUR_FOUR).unwrap();
                for pulse in -32i64..0 {
                    let sample = grid.pulse_to_sample(pulse);
                    let got = grid.sample_to_pulse(sample);
                    assert_eq!(
                        got, pulse,
                        "bpm {bpm} / {sample_rate} Hz: pulse {pulse} (sample {sample}) did \
                         not round-trip"
                    );
                }
            }
        }
    }

    #[test]
    fn source_map_applies_offset_only_at_source_mapping() {
        let grid = Grid::new(48000, 178.0, TimeSignature::FOUR_FOUR).unwrap();
        let offset = 2400i64; // 50ms of leading silence at 48kHz
        let map = SourceMap::new(grid, offset);
        assert_eq!(map.source_frame(0), offset);
        assert_eq!(map.source_frame(4), grid.pulse_to_sample(4) + offset);
        // Performance-time pulse_to_sample is untouched by the source offset.
        assert_eq!(grid.pulse_to_sample(4), grid.pulse_to_sample(4));
    }

    #[test]
    fn invalid_grid_construction_is_rejected() {
        assert!(Grid::new(0, 178.0, TimeSignature::FOUR_FOUR).is_err());
        assert!(Grid::new(48000, 0.0, TimeSignature::FOUR_FOUR).is_err());
        assert!(Grid::new(48000, -178.0, TimeSignature::FOUR_FOUR).is_err());
        assert!(Grid::new(48000, f64::NAN, TimeSignature::FOUR_FOUR).is_err());
        assert!(Grid::new(
            48000,
            178.0,
            TimeSignature {
                numerator: 4,
                denominator: 3
            }
        )
        .is_err());
        assert!(Grid::new(
            48000,
            178.0,
            TimeSignature {
                numerator: 0,
                denominator: 4
            }
        )
        .is_err());
    }
}
