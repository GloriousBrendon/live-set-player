//! Linear parameter smoothing (`docs/SPEC.md` §3 / CLAUDE.md invariant 5).
//!
//! Every gain and mute change ramps over 5–10 ms; un-ramped changes click. This
//! smoother is the one primitive that enforces that: a target change never takes
//! effect instantaneously (unless explicitly requested with a zero-length ramp, which
//! is reserved for transport (re)starts from silence, where there is nothing to
//! click against).
//!
//! Real-time discipline: [`Smoother::tick`] is branch-cheap, allocation-free, and --
//! critically for the offline/live sample-identity guarantee -- returns the target
//! value *exactly* once the ramp has completed, so a smoother sitting at a constant
//! gain multiplies identically to the offline renderer's plain constant.

/// Default ramp length in milliseconds, inside the 5–10 ms window invariant 5 demands.
pub const DEFAULT_RAMP_MS: f64 = 8.0;

/// Ramp length in samples for [`DEFAULT_RAMP_MS`] at `sample_rate`.
pub fn default_ramp_samples(sample_rate: u32) -> u32 {
    (DEFAULT_RAMP_MS / 1000.0 * sample_rate as f64).round() as u32
}

/// A linearly-ramped parameter. `tick()` is called exactly once per rendered sample.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Smoother {
    current: f32,
    target: f32,
    step: f32,
    remaining: u32,
}

impl Smoother {
    /// A smoother already settled at `value` (no ramp in progress).
    pub fn settled(value: f32) -> Self {
        Smoother {
            current: value,
            target: value,
            step: 0.0,
            remaining: 0,
        }
    }

    /// Begin a linear ramp from the current value to `target` over `ramp_samples`.
    /// `ramp_samples == 0` jumps immediately -- callers must reserve that for starts
    /// from silence, never for live gain changes (invariant 5).
    pub fn set_target(&mut self, target: f32, ramp_samples: u32) {
        if ramp_samples == 0 || target == self.current {
            self.current = target;
            self.target = target;
            self.remaining = 0;
            return;
        }
        self.target = target;
        self.step = (target - self.current) / ramp_samples as f32;
        self.remaining = ramp_samples;
    }

    /// Advance one sample and return the value to apply for that sample. Once the
    /// ramp completes this returns `target` exactly (not an accumulated
    /// approximation), so steady state is bit-identical to a constant.
    #[inline]
    pub fn tick(&mut self) -> f32 {
        if self.remaining == 0 {
            return self.target;
        }
        self.remaining -= 1;
        if self.remaining == 0 {
            self.current = self.target;
        } else {
            self.current += self.step;
        }
        self.current
    }

    /// The value the smoother is heading towards (or sitting at).
    pub fn target(&self) -> f32 {
        self.target
    }

    /// True if a ramp is still in progress.
    pub fn is_ramping(&self) -> bool {
        self.remaining > 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_ramp_is_within_5_to_10_ms() {
        for rate in [44100u32, 48000] {
            let samples = default_ramp_samples(rate);
            let ms = samples as f64 * 1000.0 / rate as f64;
            assert!((5.0..=10.0).contains(&ms), "{rate} Hz ramp is {ms} ms");
        }
    }

    #[test]
    fn settled_smoother_returns_exact_constant() {
        let mut s = Smoother::settled(0.75);
        for _ in 0..1000 {
            assert_eq!(s.tick(), 0.75);
        }
    }

    #[test]
    fn ramp_reaches_target_exactly_in_ramp_samples() {
        let mut s = Smoother::settled(1.0);
        s.set_target(0.25, 384); // 8ms at 48k
        let mut last = 1.0f32;
        for i in 0..384 {
            let v = s.tick();
            assert!(
                v <= last + 1e-6,
                "ramp down must be monotonic (sample {i}: {v} > {last})"
            );
            last = v;
        }
        // Ramp complete: exact target from here on, forever.
        for _ in 0..100 {
            assert_eq!(s.tick(), 0.25);
        }
    }

    #[test]
    fn zero_ramp_jumps_immediately() {
        let mut s = Smoother::settled(0.0);
        s.set_target(1.0, 0);
        assert_eq!(s.tick(), 1.0);
    }

    #[test]
    fn retarget_mid_ramp_continues_from_current_value() {
        let mut s = Smoother::settled(0.0);
        s.set_target(1.0, 100);
        for _ in 0..50 {
            s.tick();
        }
        let mid = s.tick();
        assert!(mid > 0.4 && mid < 0.6);
        s.set_target(0.0, 100);
        let first_after = s.tick();
        assert!(
            (first_after - mid).abs() < 0.02,
            "retarget must not jump: {mid} -> {first_after}"
        );
    }
}
