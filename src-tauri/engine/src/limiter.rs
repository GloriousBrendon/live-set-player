//! Per-bus safety limiter (`docs/SPEC.md` §4): soft knee, ceiling −1 dBFS.
//!
//! This is a **memoryless soft-knee saturator**, not an envelope/lookahead limiter:
//! linear (bit-exact passthrough) below the knee, then a smooth exponential approach
//! to the ceiling. Chosen deliberately:
//!
//! - It is stateless, so it is trivially block-size invariant -- a hard requirement
//!   for the offline-render == real-time sample-identity guarantee.
//! - It is deterministic and allocation-free in the audio callback.
//! - As a *safety* net (§4 calls it a "safety limiter") the goal is "never slam the
//!   FOH desk", not transparent program-dependent limiting.
//!
//! The curve: for `|x| <= KNEE_START` output is exactly `x`. Above it,
//! `y = sign(x) * (C - (C - K) * exp(-(|x| - K) / (C - K)))` where `K = KNEE_START`
//! and `C = CEILING`. This is C1-continuous at the knee (slope 1), strictly
//! monotonic, and asymptotically approaches -- never reaches -- the −1 dBFS ceiling.

/// Where the knee begins: −3 dBFS. Everything below this passes through bit-exact,
/// which is also what keeps the offline renderer's below-knee test material intact.
pub const KNEE_START: f32 = 0.707_945_78; // 10^(-3/20)

/// The ceiling the curve approaches asymptotically: −1 dBFS.
pub const CEILING: f32 = 0.891_250_94; // 10^(-1/20)

/// Apply the soft-knee curve to one sample.
#[inline]
pub fn soft_knee(x: f32) -> f32 {
    let a = x.abs();
    if a <= KNEE_START {
        return x;
    }
    let span = CEILING - KNEE_START;
    let shaped = CEILING - span * (-(a - KNEE_START) / span).exp();
    if x < 0.0 {
        -shaped
    } else {
        shaped
    }
}

/// Apply [`soft_knee`] in place over a buffer.
#[inline]
pub fn soft_knee_buffer(buf: &mut [f32]) {
    for s in buf {
        *s = soft_knee(*s);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn below_knee_is_bit_exact_passthrough() {
        for &x in &[0.0f32, 0.1, -0.5, 0.7, -0.707, KNEE_START, -KNEE_START] {
            assert_eq!(soft_knee(x), x, "below-knee value {x} must pass unchanged");
        }
    }

    #[test]
    fn output_never_exceeds_ceiling() {
        // Strictly below in exact arithmetic; at f32 precision the asymptote can
        // round to the ceiling itself, which is still within the −1 dBFS contract.
        for i in 0..10_000 {
            let x = i as f32 * 0.01; // 0 .. 100.0
            let y = soft_knee(x);
            assert!(y <= CEILING, "input {x} produced {y} > ceiling");
            assert!(soft_knee(-x) >= -CEILING);
        }
    }

    #[test]
    fn curve_is_monotonic() {
        let mut last = 0.0f32;
        for i in 1..100_000 {
            let x = i as f32 * 0.001;
            let y = soft_knee(x);
            assert!(y >= last, "non-monotonic at {x}: {y} < {last}");
            last = y;
        }
    }

    #[test]
    fn curve_is_continuous_at_the_knee() {
        let below = soft_knee(KNEE_START - 1e-5);
        let above = soft_knee(KNEE_START + 1e-5);
        assert!(
            (above - below).abs() < 1e-3,
            "discontinuity at knee: {below} vs {above}"
        );
        // Slope just above the knee is ~1 (C1 continuity).
        let slope = (soft_knee(KNEE_START + 2e-4) - soft_knee(KNEE_START + 1e-4)) / 1e-4;
        assert!(
            (slope - 1.0).abs() < 0.05,
            "knee entry slope should be ~1, got {slope}"
        );
    }

    #[test]
    fn symmetric_for_negative_inputs() {
        for i in 0..1000 {
            let x = i as f32 * 0.005;
            assert_eq!(soft_knee(-x), -soft_knee(x));
        }
    }
}
