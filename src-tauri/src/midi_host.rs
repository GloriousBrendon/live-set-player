//! Owns the MIDI input connection on a dedicated thread.
//!
//! `midir::MidiInputConnection` is not `Send` on every backend, so -- exactly like
//! `audio_host.rs` for the (also not-`Send`) `cpal::Stream` -- it never leaves the
//! thread that opened it. Unlike the audio host, this thread does no real work itself:
//! `midir`'s backend invokes the message callback on its own internally-managed
//! thread (confirmed for both WinMM and ALSA), so the callback given to
//! `commands::midi::select_midi_input_port` does the parse/debounce/dispatch (or
//! learn-capture) inline, in place. This thread's only job is to keep the connection
//! alive between `Open` messages (each one closes the previous connection, if any)
//! and hand back the open result.

use lsp_engine::error::MidiError;
use lsp_engine::midi::MidiInputConnection;
use std::sync::mpsc;

/// Called by `midir`'s own callback thread for every message received while a
/// connection is open.
pub type MessageCallback = Box<dyn FnMut(&[u8]) + Send>;

pub enum MidiHostMsg {
    Open {
        port_name: String,
        on_message: MessageCallback,
        reply: mpsc::Sender<Result<(), MidiError>>,
    },
}

/// Spawn the MIDI host thread and return the channel used to talk to it.
pub fn spawn() -> mpsc::Sender<MidiHostMsg> {
    let (tx, rx) = mpsc::channel::<MidiHostMsg>();
    std::thread::Builder::new()
        .name("lsp-midi-host".into())
        .spawn(move || run(rx))
        .expect("failed to spawn MIDI host thread");
    tx
}

fn run(rx: mpsc::Receiver<MidiHostMsg>) {
    // Lives only on this thread for as long as a port is open; dropping it closes
    // the connection.
    let mut connection: Option<MidiInputConnection> = None;

    for msg in rx {
        match msg {
            MidiHostMsg::Open {
                port_name,
                on_message,
                reply,
            } => {
                // Close any previous connection before opening the next one.
                drop(connection.take());
                match lsp_engine::midi::open_midi_input(&port_name, on_message) {
                    Ok(conn) => {
                        connection = Some(conn);
                        let _ = reply.send(Ok(()));
                    }
                    Err(e) => {
                        let _ = reply.send(Err(e));
                    }
                }
            }
        }
    }
}
