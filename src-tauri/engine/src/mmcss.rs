//! MMCSS "Pro Audio" registration for the Windows audio callback thread.
//!
//! Verified against cpal 0.17.3 source: cpal's WASAPI backend does **not** register
//! its callback thread with MMCSS. By default it only calls
//! `SetThreadPriority(THREAD_PRIORITY_TIME_CRITICAL)`; its optional
//! `audio_thread_priority` feature (off by default, and deliberately left off in our
//! Cargo.toml) would register the thread under the **"Audio"** task class, which is
//! MMCSS scheduling category "Medium". We want **"Pro Audio"** (category "High"),
//! and a thread can belong to only one MMCSS task at a time — a second
//! `AvSetMmThreadCharacteristics` call fails with `ERROR_ALREADY_EXISTS` — so cpal's
//! feature must stay off and we register ourselves, once, from the first data
//! callback.
//!
//! The registration handle is intentionally never reverted: it is only valid on the
//! callback thread, and that thread's lifetime is the stream's lifetime — the OS
//! reclaims the association when the thread exits.

/// Register the *current* thread with MMCSS as a "Pro Audio" task. Returns whether
/// registration succeeded. Call once from the audio callback thread; calling again
/// on the same thread will fail harmlessly (`false`).
///
/// Real-time discipline note: this is a one-time syscall made on the first callback
/// invocation, before any audio has been rendered — not part of the steady-state
/// path. It does not allocate.
#[cfg(target_os = "windows")]
pub fn register_current_thread_pro_audio() -> bool {
    use std::ffi::c_void;

    #[link(name = "avrt")]
    extern "system" {
        // https://learn.microsoft.com/en-us/windows/win32/api/avrt/nf-avrt-avsetmmthreadcharacteristicsw
        fn AvSetMmThreadCharacteristicsW(
            task_name: *const u16,
            task_index: *mut u32,
        ) -> *mut c_void;
    }

    // "Pro Audio" as a NUL-terminated UTF-16 string, built as a const so the call
    // site allocates nothing.
    const TASK_NAME: [u16; 10] = [
        b'P' as u16,
        b'r' as u16,
        b'o' as u16,
        b' ' as u16,
        b'A' as u16,
        b'u' as u16,
        b'd' as u16,
        b'i' as u16,
        b'o' as u16,
        0,
    ];

    let mut task_index: u32 = 0;
    // SAFETY: the only unsafe code in the audio path (approved 2026-08-11).
    // `AvSetMmThreadCharacteristicsW` reads a valid NUL-terminated UTF-16 string and
    // writes through a valid `*mut u32`, both provided above; it has no other memory
    // effects visible to Rust. A null return simply means "not registered" (e.g.
    // MMCSS unavailable or the thread already belongs to a task), which we report as
    // `false` rather than treating as fatal.
    let handle = unsafe { AvSetMmThreadCharacteristicsW(TASK_NAME.as_ptr(), &mut task_index) };
    !handle.is_null()
}

/// Non-Windows platforms have no MMCSS; reported as "not registered".
#[cfg(not(target_os = "windows"))]
pub fn register_current_thread_pro_audio() -> bool {
    false
}
