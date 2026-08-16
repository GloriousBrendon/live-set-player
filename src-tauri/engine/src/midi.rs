//! MIDI control input (`docs/SPEC.md` §9): learn-mode binding of arbitrary note-on/CC/
//! program-change messages to transport actions, ~150 ms per-action debounce for
//! footswitch bounce, and WinMM's 31-character device-name truncation on Windows.
//!
//! The parsing/matching/debounce core below ([`MidiMessage`], [`Action`],
//! [`MidiBindingConfig`], [`parse_midi_message`], [`MidiDebouncer`], [`MidiRouter`],
//! [`port_name_matches`]) is pure and has no `midir` dependency, so it is unit-tested
//! with synthetic byte sequences -- no MIDI hardware or virtual port is needed to
//! verify binding logic, only to verify the thin `midir` glue at the bottom of this
//! module (mirroring how [`crate::device`] wraps `cpal`).

use crate::error::MidiError;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// One MIDI trigger, identified by message type + channel + note/controller/program
/// number. Deliberately excludes velocity/CC-value/note-off: a binding matches on
/// "which button", never on "how hard" or "released vs pressed" -- see
/// [`parse_midi_message`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MidiMessage {
    NoteOn { channel: u8, note: u8 },
    ControlChange { channel: u8, controller: u8 },
    ProgramChange { channel: u8, program: u8 },
}

/// The five transport actions bindable to a MIDI trigger or a keyboard shortcut
/// (`docs/SPEC.md` §9). Serialises to the same strings as the corresponding Tauri
/// command names, so the wire format of `dispatch_action` lines up 1:1 with
/// `commands/transport.rs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    ArmNextSong,
    Play,
    AdvanceSection,
    Stop,
    PanicStop,
}

/// Every bindable action, in the order the settings UI lists them.
pub const ALL_ACTIONS: [Action; 5] = [
    Action::ArmNextSong,
    Action::Play,
    Action::AdvanceSection,
    Action::Stop,
    Action::PanicStop,
];

/// One persisted binding: pressing `message` fires `action`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MidiBindingConfig {
    pub message: MidiMessage,
    pub action: Action,
}

/// Parse a raw MIDI short message into the subset of message types learn mode
/// captures (note on, CC, program change). Returns `None` for anything else,
/// including note-off and velocity-0 note-on (the standard MIDI note-off alias) --
/// a binding fires on "the button was pressed", not on "a note-off arrived". CC and
/// program-change always parse regardless of value/program-number: the ~150 ms
/// debounce below, not value inspection, is what absorbs a footswitch's press+release
/// pair (`docs/SPEC.md` §9).
pub fn parse_midi_message(bytes: &[u8]) -> Option<MidiMessage> {
    let status = *bytes.first()?;
    let channel = status & 0x0F;
    match status & 0xF0 {
        0x90 => {
            let note = *bytes.get(1)?;
            let velocity = *bytes.get(2)?;
            (velocity > 0).then_some(MidiMessage::NoteOn { channel, note })
        }
        0xB0 => {
            let controller = *bytes.get(1)?;
            Some(MidiMessage::ControlChange {
                channel,
                controller,
            })
        }
        0xC0 => {
            let program = *bytes.get(1)?;
            Some(MidiMessage::ProgramChange { channel, program })
        }
        _ => None,
    }
}

/// ~150 ms debounce window (`docs/SPEC.md` §9: "footswitches bounce").
pub const DEBOUNCE_WINDOW: Duration = Duration::from_millis(150);

/// Suppresses repeat triggers of the same action within [`DEBOUNCE_WINDOW`], keyed
/// **per action** -- a single global timestamp would let a fast press on one pedal
/// wrongly suppress an unrelated action fired moments earlier on a different pedal
/// (e.g. an "advance section" pedal and a "panic stop" pedal tapped in the same beat).
#[derive(Debug, Default)]
pub struct MidiDebouncer {
    last_fired: HashMap<Action, Instant>,
}

impl MidiDebouncer {
    pub fn new() -> Self {
        Self::default()
    }

    /// `now` is a parameter rather than an internal `Instant::now()` call so tests can
    /// drive fake time deterministically, without sleeping.
    pub fn should_fire(&mut self, action: Action, now: Instant) -> bool {
        if let Some(&last) = self.last_fired.get(&action) {
            if now.duration_since(last) < DEBOUNCE_WINDOW {
                return false;
            }
        }
        self.last_fired.insert(action, now);
        true
    }
}

/// Matches raw MIDI bytes against the current bindings and debounces the result --
/// the entire hardware-free "did the pedal actually fire an action" decision, driven
/// directly by the `midir` callback (see `src-tauri/src/midi_host.rs`) once bound.
#[derive(Debug, Default)]
pub struct MidiRouter {
    bindings: Vec<MidiBindingConfig>,
    debouncer: MidiDebouncer,
}

impl MidiRouter {
    pub fn new(bindings: Vec<MidiBindingConfig>) -> Self {
        Self {
            bindings,
            debouncer: MidiDebouncer::new(),
        }
    }

    pub fn set_bindings(&mut self, bindings: Vec<MidiBindingConfig>) {
        self.bindings = bindings;
    }

    pub fn bindings(&self) -> &[MidiBindingConfig] {
        &self.bindings
    }

    /// Parse `bytes`, look up a bound action, and debounce it. `None` means "nothing
    /// to dispatch": the bytes aren't a bindable message, nothing is bound to it, or
    /// it's a bounce within [`DEBOUNCE_WINDOW`] of the last fire.
    pub fn handle_bytes(&mut self, bytes: &[u8], now: Instant) -> Option<Action> {
        let message = parse_midi_message(bytes)?;
        let action = self
            .bindings
            .iter()
            .find(|b| b.message == message)
            .map(|b| b.action)?;
        self.debouncer.should_fire(action, now).then_some(action)
    }
}

/// WinMM (`midir`'s Windows backend) truncates device names to `MAXPNAMELEN` = 32
/// bytes including the null terminator, i.e. 31 usable characters.
const WINMM_MAX_NAME_LEN: usize = 31;

fn truncate_chars(s: &str, max_len: usize) -> &str {
    match s.char_indices().nth(max_len) {
        Some((idx, _)) => &s[..idx],
        None => s,
    }
}

/// True if `persisted` and `candidate` name the same MIDI port, tolerating WinMM's
/// 31-character truncation in either direction (a name persisted from a full
/// enumeration must still match a truncated one on a later run, and vice versa).
/// Exact match is tried first. Note that two distinct devices sharing the same first
/// 31 characters are indistinguishable after truncation -- a known WinMM limitation,
/// not a bug in this matcher; always show full names in the port picker so the user
/// can tell them apart before that ambiguity matters.
pub fn port_name_matches(persisted: &str, candidate: &str) -> bool {
    if persisted == candidate {
        return true;
    }
    truncate_chars(persisted, WINMM_MAX_NAME_LEN) == truncate_chars(candidate, WINMM_MAX_NAME_LEN)
}

// --- midir-backed device enumeration (not unit tested -- needs a real MIDI subsystem,
// same rationale as cpal device enumeration in `crate::device`) -----------------------

/// Enumerate MIDI input port names. A fresh `midir::MidiInput` is created per call
/// since `midir` ties port listing to the connection object that's about to be
/// consumed by `open_midi_input`.
pub fn list_midi_input_ports() -> Result<Vec<String>, MidiError> {
    let input =
        midir::MidiInput::new("lsp-midi-probe").map_err(|e| MidiError::Backend(e.to_string()))?;
    Ok(input
        .ports()
        .iter()
        .filter_map(|p| input.port_name(p).ok())
        .collect())
}

/// Re-exported so callers (the Tauri app crate's `midi_host.rs`) can name the
/// connection type without taking their own direct dependency on `midir`.
pub type MidiInputConnection = midir::MidiInputConnection<()>;

/// Open `port_name` (matched via [`port_name_matches`] against the live enumeration,
/// so a WinMM-truncated persisted name still finds its port) and forward every
/// message `midir` delivers to `on_message`. The returned connection must be kept
/// alive for as long as input is wanted; dropping it closes the port. `midir`'s
/// backend invokes `on_message` on its own internally-managed thread, not the thread
/// that calls this function.
pub fn open_midi_input(
    port_name: &str,
    mut on_message: impl FnMut(&[u8]) + Send + 'static,
) -> Result<MidiInputConnection, MidiError> {
    let input =
        midir::MidiInput::new("lsp-midi-input").map_err(|e| MidiError::Backend(e.to_string()))?;
    let ports = input.ports();
    let mut available = Vec::with_capacity(ports.len());
    let mut matched = None;
    for p in &ports {
        let name = input
            .port_name(p)
            .map_err(|e| MidiError::Backend(e.to_string()))?;
        if port_name_matches(port_name, &name) {
            matched = Some(p.clone());
        }
        available.push(name);
    }
    let port = matched.ok_or_else(|| MidiError::PortNotFound {
        name: port_name.to_string(),
        available,
    })?;
    input
        .connect(
            &port,
            "lsp-midi-input-conn",
            move |_stamp, message, ()| on_message(message),
            (),
        )
        .map_err(|e| MidiError::Backend(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- parse_midi_message ---

    #[test]
    fn note_on_with_velocity_parses() {
        assert_eq!(
            parse_midi_message(&[0x90, 64, 100]),
            Some(MidiMessage::NoteOn {
                channel: 0,
                note: 64
            })
        );
    }

    #[test]
    fn note_on_velocity_zero_is_treated_as_note_off() {
        assert_eq!(parse_midi_message(&[0x90, 64, 0]), None);
    }

    #[test]
    fn note_off_parses_to_none() {
        assert_eq!(parse_midi_message(&[0x80, 64, 100]), None);
    }

    #[test]
    fn channel_is_read_from_the_low_nibble() {
        assert_eq!(
            parse_midi_message(&[0x91, 10, 5]),
            Some(MidiMessage::NoteOn {
                channel: 1,
                note: 10
            })
        );
    }

    #[test]
    fn control_change_parses_regardless_of_value() {
        assert_eq!(
            parse_midi_message(&[0xB0, 7, 127]),
            Some(MidiMessage::ControlChange {
                channel: 0,
                controller: 7
            })
        );
        assert_eq!(
            parse_midi_message(&[0xB0, 7, 0]),
            Some(MidiMessage::ControlChange {
                channel: 0,
                controller: 7
            })
        );
    }

    #[test]
    fn program_change_parses() {
        assert_eq!(
            parse_midi_message(&[0xC0, 5]),
            Some(MidiMessage::ProgramChange {
                channel: 0,
                program: 5
            })
        );
    }

    #[test]
    fn system_realtime_and_short_or_empty_messages_are_none() {
        assert_eq!(parse_midi_message(&[0xF8]), None);
        assert_eq!(parse_midi_message(&[0x90]), None);
        assert_eq!(parse_midi_message(&[]), None);
    }

    // --- MidiDebouncer ---

    #[test]
    fn debouncer_fires_once_then_suppresses_within_window() {
        let mut d = MidiDebouncer::new();
        let t0 = Instant::now();
        assert!(d.should_fire(Action::Play, t0));
        assert!(!d.should_fire(Action::Play, t0 + Duration::from_millis(100)));
        assert!(d.should_fire(Action::Play, t0 + Duration::from_millis(151)));
    }

    #[test]
    fn debouncer_is_keyed_per_action() {
        // Regression test: a global (rather than per-action) timestamp would wrongly
        // suppress a different action pressed at the same instant.
        let mut d = MidiDebouncer::new();
        let t0 = Instant::now();
        assert!(d.should_fire(Action::AdvanceSection, t0));
        assert!(d.should_fire(Action::PanicStop, t0));
    }

    // --- MidiRouter ---

    fn router_with(message: MidiMessage, action: Action) -> MidiRouter {
        MidiRouter::new(vec![MidiBindingConfig { message, action }])
    }

    #[test]
    fn unbound_message_produces_nothing() {
        let mut r = router_with(
            MidiMessage::ControlChange {
                channel: 0,
                controller: 1,
            },
            Action::Stop,
        );
        assert_eq!(r.handle_bytes(&[0xB0, 2, 127], Instant::now()), None);
    }

    #[test]
    fn bound_message_fires_once_then_debounces_then_rearms() {
        let mut r = router_with(
            MidiMessage::ControlChange {
                channel: 0,
                controller: 1,
            },
            Action::AdvanceSection,
        );
        let t0 = Instant::now();
        assert_eq!(
            r.handle_bytes(&[0xB0, 1, 127], t0),
            Some(Action::AdvanceSection)
        );
        assert_eq!(
            r.handle_bytes(&[0xB0, 1, 0], t0 + Duration::from_millis(20)),
            None,
            "press+release pair within the debounce window must not double-fire"
        );
        assert_eq!(
            r.handle_bytes(&[0xB0, 1, 127], t0 + Duration::from_millis(200)),
            Some(Action::AdvanceSection)
        );
    }

    #[test]
    fn note_on_velocity_zero_never_reaches_the_binding_lookup() {
        let mut r = router_with(
            MidiMessage::NoteOn {
                channel: 0,
                note: 60,
            },
            Action::PanicStop,
        );
        assert_eq!(r.handle_bytes(&[0x90, 60, 0], Instant::now()), None);
    }

    // --- port_name_matches ---

    #[test]
    fn identical_names_match() {
        assert!(port_name_matches("Roland UM-ONE", "Roland UM-ONE"));
    }

    #[test]
    fn winmm_truncated_candidate_matches_full_persisted_name() {
        let full = "Some Really Long USB MIDI Footswitch Interface";
        let truncated = truncate_chars(full, WINMM_MAX_NAME_LEN);
        assert_eq!(truncated.chars().count(), WINMM_MAX_NAME_LEN);
        assert!(port_name_matches(full, truncated));
        assert!(port_name_matches(truncated, full));
    }

    #[test]
    fn different_names_do_not_match() {
        assert!(!port_name_matches("Footswitch A", "Footswitch B"));
    }

    #[test]
    fn exactly_31_char_identical_names_match() {
        let name = "a".repeat(WINMM_MAX_NAME_LEN);
        assert!(port_name_matches(&name, &name));
    }

    #[test]
    fn two_distinct_devices_sharing_a_31_char_prefix_are_indistinguishable() {
        // Documents the known WinMM limitation: this is the correct (if unfortunate)
        // behaviour, not something the matcher can fix.
        let prefix = "a".repeat(WINMM_MAX_NAME_LEN);
        let device_one = format!("{prefix} Mk1");
        let device_two = format!("{prefix} Mk2");
        assert!(port_name_matches(&prefix, &device_one));
        assert!(port_name_matches(&prefix, &device_two));
    }
}
