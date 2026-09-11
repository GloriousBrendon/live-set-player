//! End-anchored spoken-cue scheduling (`docs/SPEC.md` §8).
//!
//! Pure arithmetic, no I/O -- mirrors [`crate::sections`]'s split between resolving a
//! performance order (that module) and, given the resolved order, working out where
//! each section's pre-rendered cue clip must start so it finishes speaking
//! `cue_lead_beats` pulses before the section's downbeat:
//!
//! ```text
//! cue_target_pulse             = section_downbeat_pulse - cue_lead_beats
//! cue_downbeat_relative_sample = grid.pulse_to_sample(cue_target_pulse)
//! wanted_start_sample          = cue_downbeat_relative_sample - clip_length_samples
//! ```
//!
//! Deliberately *not* `cue_lead_beats as f64 * grid.samples_per_pulse()`: every other
//! position in this codebase is a single `round()` from an absolute pulse index
//! (CLAUDE.md invariant 2), and reimplementing that arithmetic a second way here would
//! be a second place for the two to quietly disagree. Subtracting `cue_lead_beats`
//! pulses first and converting once keeps this on the same rounding path as
//! `Grid::pulse_to_sample` everywhere else.
//!
//! If `wanted_start_sample` collides with the previous cue -- i.e. would start before
//! the previous cue has finished speaking -- the cue starts as early as possible
//! instead (right where the previous one ends) and the result records that as a
//! warning (§8). A cue with no preceding cue to collide with has no floor: starting
//! well ahead of its section, even into count-in territory or over earlier
//! (uncued) sections, is exactly what generous lead time is for, not a problem to
//! clamp away.
//!
//! A cue is scheduled once per *performance-order entry* (`ResolvedEntry` with
//! `repeat_index == 0`), not once per loop repeat: entering a loopable section
//! announces it once, however many times it then repeats before a manual advance --
//! see [`crate::sections::PerformanceEntry`]'s own docs for why `repeats` is a
//! render-time stand-in for that live "repeat until advance" behaviour.
//!
//! `follows_loopable_section` flags every cue whose *preceding* section is loopable:
//! live, that section repeats an unknown number of times before a manual advance, so
//! there is no way to know its exit point (hence this cue's target downbeat) far
//! enough ahead to honour `cue_lead_beats` at all -- the live engine can only start
//! this cue immediately when the advance actually lands (`crate::rt`). This is a
//! *structural* fact about the section list, true for every render of it regardless of
//! which concrete resolved order is passed in, which is why it's a field on the
//! output rather than a warning that only fires when a particular render happens to
//! collide.

use crate::project::Song;
use crate::sections::ResolvedEntry;
use crate::timeline::Grid;

/// Why a cue's start was clamped away from its wanted position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CueWarning {
    /// Where the end-anchored formula wanted this cue to start, before clamping.
    pub wanted_start_sample: i64,
}

/// One section's spoken cue, positioned in performance-time samples.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScheduledCue {
    /// Index into the `order` slice `resolved` was built from (`sections::resolve_order`).
    pub entry_index: usize,
    pub section_index: usize,
    pub start_sample: i64,
    /// Exclusive: `start_sample + clip_length_samples`.
    pub end_sample: i64,
    pub warning: Option<CueWarning>,
    /// True when the section immediately preceding this one in the resolved order is
    /// `loopable` -- see module docs. Always false for the first entry.
    pub follows_loopable_section: bool,
}

/// Schedule cues for every `repeat_index == 0` entry in `resolved` whose section has
/// non-empty effective cue text, using `clip_length_samples` to look up each
/// section's pre-rendered clip length (`None` means "no cue for this section," which
/// is also what a missing/empty `cue_text` with an empty section name would mean --
/// callers pass `None` for any section that shouldn't get a cue at all, keeping this
/// function ignorant of the text/rendering side entirely).
///
/// `resolved` must be in performance order (as produced by
/// [`crate::sections::resolve_order`]); `song` supplies each section's `loopable`
/// flag and `cue_lead_beats`.
pub fn schedule_cues(
    grid: &Grid,
    song: &Song,
    resolved: &[ResolvedEntry],
    clip_length_samples: impl Fn(usize) -> Option<i64>,
) -> Vec<ScheduledCue> {
    let mut out = Vec::new();
    let mut previous_cue_end_sample: Option<i64> = None;
    let mut previous_section_loopable = false;

    for entry in resolved.iter().filter(|e| e.repeat_index == 0) {
        let Some(section) = song.sections.get(entry.section_index) else {
            continue; // resolve_order already validates this; defensive only.
        };
        let follows_loopable_section = previous_section_loopable;
        previous_section_loopable = section.loopable;

        let Some(clip_length_samples) = clip_length_samples(entry.section_index) else {
            continue;
        };

        let (start_sample, end_sample, warning) = cue_start_and_end(
            grid,
            entry.perf_start_pulse,
            section.cue_lead_beats,
            clip_length_samples,
            previous_cue_end_sample,
        );

        out.push(ScheduledCue {
            entry_index: entry.entry_index,
            section_index: entry.section_index,
            start_sample,
            end_sample,
            warning,
            follows_loopable_section,
        });
        previous_cue_end_sample = Some(end_sample);
    }

    out
}

/// The end-anchored formula itself (module docs), factored out so both the static
/// scheduler above and the live engine's immediate-start fallback
/// (`crate::rt` -- a section following a loopable predecessor, or an out-of-plan
/// manual seek, where there's no lead time to work with and "earliest_allowed" is
/// simply "right now") go through exactly one implementation. `earliest_allowed` is
/// the clamp floor -- the previous cue's end for the static path, or the current
/// performance-time position for the live immediate-start path; `None` means no
/// floor (the very first cue, nothing to collide with).
pub fn cue_start_and_end(
    grid: &Grid,
    section_downbeat_pulse: i64,
    cue_lead_beats: u32,
    clip_length_samples: i64,
    earliest_allowed: Option<i64>,
) -> (i64, i64, Option<CueWarning>) {
    let cue_target_pulse = section_downbeat_pulse - cue_lead_beats as i64;
    let cue_downbeat_relative_sample = grid.pulse_to_sample(cue_target_pulse);
    let wanted_start_sample = cue_downbeat_relative_sample - clip_length_samples;

    let (start_sample, warning) = match earliest_allowed {
        Some(floor) if wanted_start_sample < floor => (
            floor,
            Some(CueWarning {
                wanted_start_sample,
            }),
        ),
        _ => (wanted_start_sample, None),
    };
    (start_sample, start_sample + clip_length_samples, warning)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::{Section, Song, Track};
    use crate::sections::{resolve_order, PerformanceEntry};
    use crate::timeline::TimeSignature;

    fn song_with_sections() -> Song {
        Song {
            id: "s1".into(),
            title: "Test Song".into(),
            bpm: 178.0,
            time_signature: TimeSignature::FOUR_FOUR,
            offset_samples: 0,
            count_in_bars: 1,
            accent_pattern: vec![],
            sections: vec![
                Section {
                    name: "Intro".into(),
                    start_bar: 1,
                    length_bars: 4,
                    loopable: false,
                    cue_text: None,
                    cue_lead_beats: 4,
                },
                Section {
                    name: "Verse".into(),
                    start_bar: 5,
                    length_bars: 8,
                    loopable: false,
                    cue_text: None,
                    cue_lead_beats: 4,
                },
                Section {
                    name: "Chorus".into(),
                    start_bar: 13,
                    length_bars: 8,
                    loopable: true,
                    cue_text: None,
                    cue_lead_beats: 4,
                },
                Section {
                    name: "Bridge".into(),
                    start_bar: 21,
                    length_bars: 4,
                    loopable: false,
                    cue_text: None,
                    cue_lead_beats: 4,
                },
            ],
            tracks: Vec::<Track>::new(),
            auto_continue: false,
            disabled: false,
        }
    }

    fn grid() -> Grid {
        Grid::new(48000, 178.0, TimeSignature::FOUR_FOUR).unwrap()
    }

    /// The "done when" criterion from the original ask: a short cue ("Verse", ~0.4s
    /// worth of samples) and a long cue ("Bridge", ~2s worth) both finish speaking at
    /// exactly `cue_lead_beats` pulses before their section's downbeat, regardless of
    /// how long the clip itself is.
    #[test]
    fn short_and_long_cues_both_finish_the_same_distance_before_the_downbeat() {
        let grid = grid();
        let song = song_with_sections();
        // Intro, Verse, Bridge: Verse gets a short clip, Bridge a long one. Neither
        // follows a loopable section and they're spaced out enough not to collide.
        let order = [
            PerformanceEntry::once(0), // Intro, 4 bars
            PerformanceEntry::once(1), // Verse, 8 bars
            PerformanceEntry::once(3), // Bridge, 4 bars
        ];
        let resolved = resolve_order(&song, 48000, &order).unwrap();

        let short_clip = (0.4 * 48000.0) as i64;
        let long_clip = (2.0 * 48000.0) as i64;
        let lengths = |section_index: usize| match section_index {
            1 => Some(short_clip),
            3 => Some(long_clip),
            _ => None,
        };

        let scheduled = schedule_cues(&grid, &song, &resolved, lengths);
        assert_eq!(scheduled.len(), 2);

        for cue in &scheduled {
            assert!(cue.warning.is_none(), "unexpected clamp: {cue:?}");
            let section = &song.sections[cue.section_index];
            let downbeat_entry = resolved
                .iter()
                .find(|e| e.section_index == cue.section_index && e.repeat_index == 0)
                .unwrap();
            let expected_end_pulse =
                downbeat_entry.perf_start_pulse - section.cue_lead_beats as i64;
            let expected_end_sample = grid.pulse_to_sample(expected_end_pulse);
            assert_eq!(
                cue.end_sample, expected_end_sample,
                "section {} should finish exactly cue_lead_beats before its downbeat",
                section.name
            );
        }
    }

    #[test]
    fn cue_with_no_clip_length_is_skipped() {
        let grid = grid();
        let song = song_with_sections();
        let order = [PerformanceEntry::once(0), PerformanceEntry::once(1)];
        let resolved = resolve_order(&song, 48000, &order).unwrap();
        let scheduled = schedule_cues(&grid, &song, &resolved, |_| None);
        assert!(scheduled.is_empty());
    }

    /// A cue whose wanted start collides with the previous cue's end clamps to start
    /// immediately after it and records the wanted (pre-clamp) start as a warning.
    #[test]
    fn collision_with_previous_cue_clamps_and_warns() {
        let grid = grid();
        let song = song_with_sections();
        // Intro (4 bars) then Verse (8 bars): both short sections back to back, each
        // wanting a long cue -- long enough that Verse's cue would want to start
        // before Intro's cue has finished.
        let order = [PerformanceEntry::once(0), PerformanceEntry::once(1)];
        let resolved = resolve_order(&song, 48000, &order).unwrap();
        let long_clip = (10.0 * 48000.0) as i64; // 10s, far longer than a 4-bar section
        let lengths = |section_index: usize| match section_index {
            0 => Some(long_clip),
            1 => Some(long_clip),
            _ => None,
        };
        let scheduled = schedule_cues(&grid, &song, &resolved, lengths);
        assert_eq!(scheduled.len(), 2);
        assert!(scheduled[0].warning.is_none(), "first cue never clamps");
        let second = &scheduled[1];
        assert!(second.warning.is_some(), "expected a collision warning");
        assert_eq!(second.start_sample, scheduled[0].end_sample);
        assert!(second.warning.unwrap().wanted_start_sample < second.start_sample);
    }

    /// A collision can come from an extreme `cue_lead_beats` just as easily as from a
    /// long clip: Verse's huge lead time pushes its wanted start earlier than Intro's
    /// (short-clip) cue has finished, so it clamps to right after Intro's cue ends.
    #[test]
    fn extreme_lead_time_collides_with_previous_cue_and_clamps() {
        let grid = grid();
        let mut song = song_with_sections();
        song.sections[1].cue_lead_beats = 1000; // absurdly large lead time
        let order = [PerformanceEntry::once(0), PerformanceEntry::once(1)];
        let resolved = resolve_order(&song, 48000, &order).unwrap();
        let short_clip = (0.1 * 48000.0) as i64;
        let scheduled = schedule_cues(&grid, &song, &resolved, |_| Some(short_clip));
        assert_eq!(scheduled.len(), 2);
        assert!(scheduled[0].warning.is_none());
        let verse = &scheduled[1];
        assert!(verse.warning.is_some());
        assert_eq!(verse.start_sample, scheduled[0].end_sample);
        assert!(verse.warning.unwrap().wanted_start_sample < verse.start_sample);
    }

    /// The very first cue in a song has no previous cue to collide with, so even an
    /// enormous lead time never clamps -- it just starts further into count-in
    /// territory, which is legitimate, not an error.
    #[test]
    fn first_cue_never_clamps_with_no_preceding_cue() {
        let grid = grid();
        let mut song = song_with_sections();
        song.sections[0].cue_lead_beats = 1_000_000;
        let order = [PerformanceEntry::once(0)];
        let resolved = resolve_order(&song, 48000, &order).unwrap();
        let clip = (0.1 * 48000.0) as i64;
        let scheduled = schedule_cues(&grid, &song, &resolved, |_| Some(clip));
        assert_eq!(scheduled.len(), 1);
        assert!(scheduled[0].warning.is_none());
        assert!(scheduled[0].start_sample < 0);
    }

    /// Loop repeats (`repeats > 1`) only produce one cue -- entering the section once,
    /// not once per loop pass.
    #[test]
    fn loop_repeats_produce_only_one_cue() {
        let grid = grid();
        let song = song_with_sections();
        let order = [PerformanceEntry {
            section_index: 2, // Chorus, loopable, repeats 3x
            repeats: 3,
        }];
        let resolved = resolve_order(&song, 48000, &order).unwrap();
        let scheduled = schedule_cues(&grid, &song, &resolved, |_| Some(48000));
        assert_eq!(scheduled.len(), 1);
        assert_eq!(scheduled[0].section_index, 2);
    }

    /// The section immediately after a loopable one is flagged: live, that section's
    /// downbeat isn't knowable far enough ahead to honour `cue_lead_beats`.
    #[test]
    fn section_following_a_loopable_predecessor_is_flagged() {
        let grid = grid();
        let song = song_with_sections();
        // Chorus (loopable) then Bridge.
        let order = [
            PerformanceEntry {
                section_index: 2,
                repeats: 1,
            },
            PerformanceEntry::once(3),
        ];
        let resolved = resolve_order(&song, 48000, &order).unwrap();
        let scheduled = schedule_cues(&grid, &song, &resolved, |_| Some(48000));
        assert_eq!(scheduled.len(), 2);
        assert!(!scheduled[0].follows_loopable_section); // Chorus: first entry
        assert!(scheduled[1].follows_loopable_section); // Bridge: follows Chorus
    }

    #[test]
    fn non_loopable_predecessor_is_not_flagged() {
        let grid = grid();
        let song = song_with_sections();
        let order = [PerformanceEntry::once(0), PerformanceEntry::once(1)];
        let resolved = resolve_order(&song, 48000, &order).unwrap();
        let scheduled = schedule_cues(&grid, &song, &resolved, |_| Some(48000));
        assert_eq!(scheduled.len(), 2);
        assert!(!scheduled[1].follows_loopable_section);
    }
}
