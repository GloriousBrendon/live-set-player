//! Auto-continue: chaining one song into the next (`docs/SPEC.md` §9.2).
//!
//! **This runs on its own thread in the host, never in the audio callback.** Chaining
//! means loading the next song's audio from disk, which is I/O and allocation — both
//! forbidden on the audio thread (invariant 1). The audio thread's only involvement
//! is reporting, through the ordinary status snapshot, that the transport stopped.
//!
//! It is equally deliberately *not* in the frontend. Beyond invariant 7, a webview
//! throttles timers when its window is backgrounded or the screen sleeps — a set that
//! silently stops chaining because someone tabbed away is exactly the stage failure
//! design principle 1 exists to prevent.
//!
//! **Distinguishing a natural end from a human stop.** The engine reaches
//! `TransportState::Stopped` both ways, but a human stop always passes through
//! `Stopping` (the 5–10 ms ramp) while a natural end goes straight to `Stopped`.
//! Polling could miss that brief `Stopping` window, and the cost of missing it is
//! starting the next song after someone hit stop — the worst possible bug here. So
//! this does not infer intent from the state machine at all: every human transport
//! action bumps `AppState::chain_epoch` *before* the command reaches the engine, and
//! a pending chain fires only if the epoch it captured is still current.

use crate::commands::transport;
use crate::state::AppState;
use lsp_engine::rt::TransportState;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Manager};

/// Status poll interval. Fast enough that the gap starts promptly after the final
/// decay tail, slow enough to be invisible on a CPU budget.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

pub fn spawn(handle: AppHandle) {
    std::thread::Builder::new()
        .name("lsp-setlist-driver".into())
        .spawn(move || run(handle))
        .expect("failed to spawn setlist driver thread");
}

fn run(handle: AppHandle) {
    let mut prev_state: Option<TransportState> = None;

    loop {
        std::thread::sleep(POLL_INTERVAL);

        let state = handle.state::<AppState>();
        let state = state.inner();

        let Some(status) = transport::peek_status(state) else {
            prev_state = None;
            continue;
        };

        let ended_naturally =
            prev_state == Some(TransportState::Playing) && status.state == TransportState::Stopped;
        prev_state = Some(status.state);

        if !ended_naturally || !status.song_loaded {
            continue;
        }

        // §9.2: only the song that just finished decides whether the set continues.
        let (should_chain, gap) = {
            let guard = state.project.lock().unwrap();
            match guard.as_ref() {
                Some(ps) => {
                    let chain = ps
                        .current_song_index
                        .and_then(|i| ps.project.songs.get(i))
                        .map(|s| s.auto_continue)
                        .unwrap_or(false);
                    (chain, ps.project.gap_seconds)
                }
                None => (false, 0.0),
            }
        };
        if !should_chain {
            continue;
        }

        let epoch = state.chain_epoch.load(Ordering::SeqCst);
        if !wait_out_gap(&handle, epoch, gap) {
            continue; // cancelled by a human transport action
        }

        // Re-check under the epoch one last time: `wait_out_gap` returning true means
        // nothing cancelled *during* the gap, and this catches a cancel landing
        // between the final sleep and here.
        if state.chain_epoch.load(Ordering::SeqCst) != epoch {
            continue;
        }

        match transport::arm_next_song_for_chain(state) {
            Ok(Some(_)) => {
                if state.chain_epoch.load(Ordering::SeqCst) != epoch {
                    continue;
                }
                let _ = transport::play_for_chain(state);
                prev_state = None; // resync; the next poll observes the new song
            }
            // End of the setlist, or nothing armable: stop chaining, stay stopped.
            Ok(None) => {}
            Err(_) => {}
        }
    }
}

/// Sleep out `gap` seconds in `POLL_INTERVAL` slices, aborting the moment a human
/// transport action bumps the epoch. Returns false if cancelled.
///
/// Sliced rather than one long sleep so that `stop` during the gap is felt within one
/// poll interval — §9.2 requires the gap be a cancellable scheduled transition, not a
/// blocking sleep the user has to wait out.
fn wait_out_gap(handle: &AppHandle, epoch: u64, gap: f64) -> bool {
    // A non-finite gap can't reach here (`Project::validate` rejects it), but
    // treating it as "no gap" rather than as an infinite sleep keeps a bad value
    // from wedging the chain silently.
    if !gap.is_finite() || gap <= 0.0 {
        return true;
    }
    let deadline = Instant::now() + Duration::from_secs_f64(gap);
    while Instant::now() < deadline {
        let state = handle.state::<AppState>();
        if state.inner().chain_epoch.load(Ordering::SeqCst) != epoch {
            return false;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        std::thread::sleep(remaining.min(POLL_INTERVAL));
    }
    true
}
