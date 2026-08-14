//! Owns the audio device connection on a dedicated thread.
//!
//! `cpal::Stream` (inside `lsp_engine::device::OpenedStream`) is not `Send`, so it can
//! never leave the thread that opened it (see `docs/SPEC.md` §1, §3). This module is
//! that thread: it owns `OpenedStream` and `GarbageDrain` for as long as a device is
//! open, drains the garbage queue on a timer, and republishes the `Send`-able
//! `EngineHandle` into shared state every time it (re)opens a device, so Tauri
//! commands on other threads can send transport commands and poll status.

use lsp_engine::device::{self, OpenedStream};
use lsp_engine::error::DeviceError;
use lsp_engine::rt::{EngineHandle, GarbageDrain};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How often the audio host drains the garbage queue while idling between messages.
/// Freed songs sit in the queue no longer than this before their `Arc<[f32]>`s are
/// actually dropped -- generous relative to a song swap, negligible relative to a gig.
const GARBAGE_DRAIN_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Clone)]
pub struct OpenReport {
    pub engine_rate: u32,
    pub channels: u16,
    /// §1 informational notice (Windows project/device rate mismatch), if any.
    pub notice: Option<String>,
}

pub enum AudioHostMsg {
    Open {
        device_name: String,
        project_rate: u32,
        reply: mpsc::Sender<Result<OpenReport, DeviceError>>,
    },
    /// Poll the open stream's device-error flag (§1: device loss must surface, never
    /// panic). `None` if nothing is open or nothing has gone wrong.
    CheckError { reply: mpsc::Sender<Option<u32>> },
}

/// Spawn the audio host thread and return the channel used to talk to it. `engine` is
/// shared with the Tauri command layer: every successful `Open` replaces its contents
/// with a fresh `EngineHandle`, so commands can tell "no device open" (`None`) from
/// "device open" by locking it.
pub fn spawn(engine: Arc<Mutex<Option<EngineHandle>>>) -> mpsc::Sender<AudioHostMsg> {
    let (tx, rx) = mpsc::channel::<AudioHostMsg>();
    std::thread::Builder::new()
        .name("lsp-audio-host".into())
        .spawn(move || run(rx, engine))
        .expect("failed to spawn audio host thread");
    tx
}

fn run(rx: mpsc::Receiver<AudioHostMsg>, engine: Arc<Mutex<Option<EngineHandle>>>) {
    // Both live only on this thread for as long as a device is open. Dropping
    // `opened` stops and closes the stream.
    let mut opened: Option<OpenedStream> = None;
    let mut garbage: Option<GarbageDrain> = None;

    loop {
        match rx.recv_timeout(GARBAGE_DRAIN_INTERVAL) {
            Ok(AudioHostMsg::Open {
                device_name,
                project_rate,
                reply,
            }) => match device::open_output(&device_name, project_rate, None) {
                Ok((stream, handle, drain)) => {
                    let report = OpenReport {
                        engine_rate: stream.engine_rate,
                        channels: stream.channels,
                        notice: stream.notice.clone(),
                    };
                    opened = Some(stream);
                    garbage = Some(drain);
                    *engine.lock().unwrap() = Some(handle);
                    let _ = reply.send(Ok(report));
                }
                Err(e) => {
                    let _ = reply.send(Err(e));
                }
            },
            Ok(AudioHostMsg::CheckError { reply }) => {
                let _ = reply.send(opened.as_ref().and_then(|o| o.device_error()));
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if let Some(g) = garbage.as_mut() {
                    g.drain();
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
}
