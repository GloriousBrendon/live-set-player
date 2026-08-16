pub mod cues;
pub mod device;
pub mod midi;
pub mod project;
pub mod transport;

/// Generate a short, unique-enough-per-session id for a newly created song/track.
/// Not a UUID -- there's no cross-machine collision requirement (ids only need to be
/// unique within one project file), so a counter plus a coarse timestamp is enough
/// and avoids pulling in a dependency for it.
pub fn gen_id(prefix: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("{prefix}-{millis:x}-{n}")
}
