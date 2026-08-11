//! Performance-order resolution (`docs/SPEC.md` §7).
//!
//! `Section::start_bar` refers to the position in the **source audio**; the order
//! sections are *played* in is a separate list, [`PerformanceEntry`] slices, entirely
//! independent of the order sections were authored in. Resolving that list against a
//! [`crate::project::Song`] is the only place performance time and source-frame time
//! meet: every entry's performance-time span comes from **integer bar accumulation**
//! (exact, never drifts), and its source-frame span comes from a single
//! `round(pulse * samples_per_pulse)` conversion (never accumulated either). See
//! [`crate::timeline`] for why both of those matter.

use crate::error::TimelineError;
use crate::project::Song;
use crate::timeline::{Grid, SourceMap};

/// One entry in a performance order: play `section_index` (into `Song::sections`),
/// `repeats` times back to back. `repeats` must be at least 1; the live engine's
/// manual-advance looping (§7) doesn't have a fixed repeat count, but the offline
/// renderer needs one to produce a deterministic, finite render.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PerformanceEntry {
    pub section_index: usize,
    pub repeats: u32,
}

impl PerformanceEntry {
    pub fn once(section_index: usize) -> Self {
        PerformanceEntry {
            section_index,
            repeats: 1,
        }
    }
}

/// A single play-through of one section, with both its performance-time span (where
/// it sits in the rendered/transport timeline) and its source-frame span (where its
/// audio comes from in the backtrack file) resolved to exact sample positions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedEntry {
    /// Index into the `order` slice passed to [`resolve_order`].
    pub entry_index: usize,
    pub section_index: usize,
    /// 0-based repeat count within this entry (0 for the first play-through).
    pub repeat_index: u32,
    /// 0-based bar index in the performance timeline.
    pub perf_start_bar: u64,
    pub length_bars: u32,
    pub perf_start_pulse: i64,
    /// Exclusive.
    pub perf_end_pulse: i64,
    pub perf_start_sample: i64,
    /// Exclusive.
    pub perf_end_sample: i64,
    pub source_start_sample: i64,
    /// Exclusive.
    pub source_end_sample: i64,
}

impl ResolvedEntry {
    pub fn perf_length_samples(&self) -> i64 {
        self.perf_end_sample - self.perf_start_sample
    }
}

/// Resolve a performance order against a song's grid and section list.
///
/// Performance-time bar positions accumulate as plain `u64` bar counts -- exact
/// integer arithmetic, per CLAUDE.md invariant 2 -- and are converted to sample
/// positions with a single `round(pulse * samples_per_pulse)` each, never by summing
/// per-entry sample lengths (which would drift at non-integer `samples_per_pulse`,
/// exactly the failure mode the drift tests in [`crate::timeline`] guard against).
pub fn resolve_order(
    song: &Song,
    sample_rate: u32,
    order: &[PerformanceEntry],
) -> Result<Vec<ResolvedEntry>, TimelineError> {
    let grid = Grid::new(sample_rate, song.bpm, song.time_signature)?;
    let source_map = SourceMap::new(grid, song.offset_samples);

    let mut resolved = Vec::new();
    let mut perf_bar_cursor: u64 = 0;

    for (entry_index, entry) in order.iter().enumerate() {
        let section = song.sections.get(entry.section_index).ok_or(
            TimelineError::SectionIndexOutOfRange {
                entry_index,
                section_index: entry.section_index,
                section_count: song.sections.len(),
            },
        )?;
        if section.start_bar == 0 {
            return Err(TimelineError::ZeroStartBar {
                section_index: entry.section_index,
                name: section.name.clone(),
            });
        }
        if section.length_bars == 0 {
            return Err(TimelineError::ZeroLengthSection {
                section_index: entry.section_index,
                name: section.name.clone(),
            });
        }
        if entry.repeats == 0 {
            return Err(TimelineError::ZeroRepeats { entry_index });
        }

        let source_start_bar0 = (section.start_bar - 1) as i64; // 1-based -> 0-based
        let length_bars = section.length_bars;

        for repeat_index in 0..entry.repeats {
            let perf_start_bar = perf_bar_cursor;
            let perf_end_bar = perf_start_bar + length_bars as u64;

            let perf_start_pulse = grid.bar_to_pulse(perf_start_bar as i64);
            let perf_end_pulse = grid.bar_to_pulse(perf_end_bar as i64);
            let perf_start_sample = grid.pulse_to_sample(perf_start_pulse);
            let perf_end_sample = grid.pulse_to_sample(perf_end_pulse);

            let source_start_pulse = grid.bar_to_pulse(source_start_bar0);
            let source_end_pulse = grid.bar_to_pulse(source_start_bar0 + length_bars as i64);
            let source_start_sample = source_map.source_frame(source_start_pulse);
            let source_end_sample = source_map.source_frame(source_end_pulse);

            resolved.push(ResolvedEntry {
                entry_index,
                section_index: entry.section_index,
                repeat_index,
                perf_start_bar,
                length_bars,
                perf_start_pulse,
                perf_end_pulse,
                perf_start_sample,
                perf_end_sample,
                source_start_sample,
                source_end_sample,
            });

            perf_bar_cursor = perf_end_bar;
        }
    }

    Ok(resolved)
}

/// Total performance-time length of a resolved order, in samples. Entries are gapless
/// by construction, so this is just the last entry's end (0 for an empty order).
pub fn total_length_samples(entries: &[ResolvedEntry]) -> i64 {
    entries.last().map(|e| e.perf_end_sample).unwrap_or(0)
}

/// Find the resolved entry containing performance-time `sample`, if any. Linear scan;
/// this is offline-renderer/editor-time code, not real-time.
pub fn entry_at_sample(entries: &[ResolvedEntry], sample: i64) -> Option<usize> {
    entries
        .iter()
        .position(|e| e.perf_start_sample <= sample && sample < e.perf_end_sample)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::{Section, Song, Track};
    use crate::timeline::TimeSignature;

    fn song_with_sections(bpm: f64, ts: TimeSignature, offset_samples: i64) -> Song {
        Song {
            id: "s1".into(),
            title: "Test Song".into(),
            bpm,
            time_signature: ts,
            offset_samples,
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
            disabled: false,
        }
    }

    /// Performance order differs from source bar order: Chorus, Verse, Chorus,
    /// Bridge, Intro. Every entry's performance bar position and source sample
    /// position is checked against an independent computation.
    #[test]
    fn resolves_out_of_order_performance_programme() {
        let ts = TimeSignature::FOUR_FOUR;
        let song = song_with_sections(178.0, ts, 2400);
        let sample_rate = 48000u32;
        let grid = Grid::new(sample_rate, song.bpm, ts).unwrap();
        let source_map = SourceMap::new(grid, song.offset_samples);

        // section indices: 0=Intro 1=Verse 2=Chorus 3=Bridge
        let order = [
            PerformanceEntry::once(2), // Chorus (8 bars)
            PerformanceEntry::once(1), // Verse (8 bars)
            PerformanceEntry::once(2), // Chorus (8 bars)
            PerformanceEntry::once(3), // Bridge (4 bars)
            PerformanceEntry::once(0), // Intro (4 bars)
        ];

        let resolved = resolve_order(&song, sample_rate, &order).unwrap();
        assert_eq!(resolved.len(), 5);

        let expected_perf_start_bars = [0u64, 8, 16, 24, 28];
        let expected_source_start_bars0 = [12i64, 4, 12, 20, 0]; // 0-based: Chorus=12, Verse=4, Bridge=20, Intro=0
        for (i, entry) in resolved.iter().enumerate() {
            assert_eq!(
                entry.perf_start_bar, expected_perf_start_bars[i],
                "entry {i} perf_start_bar"
            );
            let expected_perf_start_sample =
                grid.bar_beat_to_sample(expected_perf_start_bars[i] as i64, 0);
            assert_eq!(
                entry.perf_start_sample, expected_perf_start_sample,
                "entry {i} perf_start_sample"
            );

            let expected_source_start_sample =
                source_map.source_frame(grid.bar_to_pulse(expected_source_start_bars0[i]));
            assert_eq!(
                entry.source_start_sample, expected_source_start_sample,
                "entry {i} source_start_sample"
            );
        }

        // Final performance bar: 28 + 4 (Intro) = 32.
        assert_eq!(
            resolved[4].perf_start_bar + resolved[4].length_bars as u64,
            32
        );
        assert_eq!(
            total_length_samples(&resolved),
            grid.bar_beat_to_sample(32, 0)
        );
    }

    /// Regression guard for the same failure mode as the timeline drift tests, at the
    /// section-resolution layer: many short (1-bar) entries at a non-integer
    /// samples-per-pulse must still land exactly where a fresh
    /// `round(pulse * samples_per_pulse)` says they should, not where summing each
    /// entry's own (rounded) length would put them.
    #[test]
    fn many_short_entries_stay_gapless_and_drift_free() {
        let ts = TimeSignature::FOUR_FOUR;
        let sample_rate = 44100u32;
        let bpm = 143.5;
        let mut song = song_with_sections(bpm, ts, 0);
        // Overwrite with 40 one-bar sections.
        song.sections = (0..40)
            .map(|i| Section {
                name: format!("Bar{i}"),
                start_bar: 1 + i as u32,
                length_bars: 1,
                loopable: false,
                cue_text: None,
                cue_lead_beats: 4,
            })
            .collect();
        let order: Vec<PerformanceEntry> = (0..40).map(PerformanceEntry::once).collect();

        let grid = Grid::new(sample_rate, bpm, ts).unwrap();
        let resolved = resolve_order(&song, sample_rate, &order).unwrap();

        for (i, entry) in resolved.iter().enumerate() {
            let expected_start = grid.bar_beat_to_sample(i as i64, 0);
            let expected_end = grid.bar_beat_to_sample(i as i64 + 1, 0);
            assert_eq!(entry.perf_start_sample, expected_start, "entry {i} start");
            assert_eq!(entry.perf_end_sample, expected_end, "entry {i} end");
            if i > 0 {
                // Gapless: this entry starts exactly where the previous one ended.
                assert_eq!(entry.perf_start_sample, resolved[i - 1].perf_end_sample);
            }
        }
    }

    #[test]
    fn repeats_expand_into_multiple_resolved_entries() {
        let ts = TimeSignature::FOUR_FOUR;
        let song = song_with_sections(178.0, ts, 0);
        let sample_rate = 48000u32;
        let order = [PerformanceEntry {
            section_index: 2, // Chorus, loopable, 8 bars
            repeats: 3,
        }];
        let resolved = resolve_order(&song, sample_rate, &order).unwrap();
        assert_eq!(resolved.len(), 3);
        assert_eq!(resolved[0].repeat_index, 0);
        assert_eq!(resolved[1].repeat_index, 1);
        assert_eq!(resolved[2].repeat_index, 2);
        assert_eq!(resolved[0].perf_start_bar, 0);
        assert_eq!(resolved[1].perf_start_bar, 8);
        assert_eq!(resolved[2].perf_start_bar, 16);
        // All three repeats read from the same source range (Chorus).
        assert_eq!(
            resolved[0].source_start_sample,
            resolved[1].source_start_sample
        );
        assert_eq!(
            resolved[0].source_start_sample,
            resolved[2].source_start_sample
        );
    }

    #[test]
    fn out_of_range_section_index_is_an_error() {
        let ts = TimeSignature::FOUR_FOUR;
        let song = song_with_sections(178.0, ts, 0);
        let order = [PerformanceEntry::once(99)];
        let err = resolve_order(&song, 48000, &order).unwrap_err();
        assert!(matches!(
            err,
            TimelineError::SectionIndexOutOfRange {
                section_index: 99,
                ..
            }
        ));
    }

    #[test]
    fn zero_repeats_is_an_error() {
        let ts = TimeSignature::FOUR_FOUR;
        let song = song_with_sections(178.0, ts, 0);
        let order = [PerformanceEntry {
            section_index: 0,
            repeats: 0,
        }];
        let err = resolve_order(&song, 48000, &order).unwrap_err();
        assert!(matches!(err, TimelineError::ZeroRepeats { entry_index: 0 }));
    }

    #[test]
    fn zero_length_section_is_an_error() {
        let ts = TimeSignature::FOUR_FOUR;
        let mut song = song_with_sections(178.0, ts, 0);
        song.sections[0].length_bars = 0;
        let order = [PerformanceEntry::once(0)];
        let err = resolve_order(&song, 48000, &order).unwrap_err();
        assert!(matches!(err, TimelineError::ZeroLengthSection { .. }));
    }

    #[test]
    fn entry_at_sample_finds_the_containing_entry() {
        let ts = TimeSignature::FOUR_FOUR;
        let song = song_with_sections(178.0, ts, 0);
        let order = [
            PerformanceEntry::once(0), // Intro, 4 bars
            PerformanceEntry::once(1), // Verse, 8 bars
        ];
        let resolved = resolve_order(&song, 48000, &order).unwrap();
        assert_eq!(entry_at_sample(&resolved, 0), Some(0));
        assert_eq!(
            entry_at_sample(&resolved, resolved[0].perf_end_sample),
            Some(1)
        );
        assert_eq!(
            entry_at_sample(&resolved, resolved[1].perf_end_sample),
            None
        );
    }
}
