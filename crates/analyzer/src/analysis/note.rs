use crate::analysis::voice::{Adsr, Waveform};
use crate::analysis::{FrameState, Hertz, SystemClock, VoiceId};
use crate::trace::FrameIndex;
use serde::Serialize;
use std::fmt;

/// MIDI note number (0..=127). 60 = middle C, 69 = A440.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize)]
#[serde(transparent)]
#[must_use]
pub struct MidiNote(pub u8);

const NOTE_NAMES: [&str; 12] = [
    "C-", "C#", "D-", "D#", "E-", "F-", "F#", "G-", "G#", "A-", "A#", "B-",
];

impl fmt::Display for MidiNote {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let n = (self.0 % 12) as usize;
        let octave = (i32::from(self.0) / 12) - 1;
        write!(f, "{}{}", NOTE_NAMES[n], octave.clamp(0, 9))
    }
}

/// A pitch interval in cents (100 cents = one semitone). Used both for
/// per-note offsets from the nearest MIDI semitone (typically -50.0..=50.0)
/// and for arbitrary intervals between two pitches (unbounded).
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Default, Serialize)]
#[serde(transparent)]
#[must_use]
pub struct Cents(pub f32);

impl Cents {
    pub fn abs(self) -> Self {
        Self(self.0.abs())
    }

    /// This interval expressed in semitones (100 cents = one semitone).
    #[must_use]
    pub fn to_semitones(self) -> f32 {
        self.0 / 100.0
    }
}

impl fmt::Display for Cents {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:+.0}", self.0)
    }
}

/// Convert audible pitch to MIDI note + cent offset.
/// Returns `None` for silence (≤ 0 Hz).
#[must_use]
pub fn hertz_to_midi(hz: Hertz) -> Option<(MidiNote, Cents)> {
    if hz.0 <= 0.0 {
        return None;
    }
    let m = 69.0 + 12.0 * (hz.0 / 440.0).log2();
    let nearest = m.round();
    let cents = (m - nearest) * 100.0;
    let clamped = nearest.clamp(0.0, 127.0) as u8;
    Some((MidiNote(clamped), Cents(cents as f32)))
}

/// General MIDI program number (0..=127), used to give each SID voice a
/// distinct timbre on playback. SID waveforms have no faithful GM
/// equivalent, so these are evocative stand-ins.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(transparent)]
#[must_use]
pub struct GmProgram(pub u8);

impl GmProgram {
    /// GM "Lead 1 (square)" — hollow square/pulse character.
    pub const SQUARE_LEAD: Self = Self(80);
    /// GM "Lead 2 (sawtooth)" — bright, buzzy.
    pub const SAW_LEAD: Self = Self(81);
    /// GM "Flute" — mellow, for the soft triangle waveform.
    pub const FLUTE: Self = Self(73);
    /// GM "Synth Drum" — closest pitched stand-in for the noise waveform.
    pub const SYNTH_DRUM: Self = Self(118);
}

/// MIDI note-on velocity (0..=127). 0 is reserved (it means note-off), so
/// derived velocities are kept in a musical range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(transparent)]
#[must_use]
pub struct Velocity(pub u8);

impl Velocity {
    /// Fallback when no envelope information is available.
    pub const DEFAULT: Self = Self(100);
}

impl Default for Velocity {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Pick a GM program that evokes the SID waveform. Noise dominates (it
/// reads as percussion); otherwise pulse → square lead, sawtooth → saw
/// lead, triangle → flute. Silent/unknown falls back to the square lead.
pub fn gm_program_for_waveform(waveform: Waveform) -> GmProgram {
    if waveform.noise {
        GmProgram::SYNTH_DRUM
    } else if waveform.pulse {
        GmProgram::SQUARE_LEAD
    } else if waveform.sawtooth {
        GmProgram::SAW_LEAD
    } else if waveform.triangle {
        GmProgram::FLUTE
    } else {
        GmProgram::SQUARE_LEAD
    }
}

/// Map a 4-bit SID envelope level (0..=15) to a note-on velocity in 48..=127.
pub fn velocity_for_sustain(level: u8) -> Velocity {
    let s = u16::from(level.min(15));
    Velocity(48 + (s * 79 / 15) as u8)
}

/// Note-on velocity from the full voice envelope. A sustaining note's loudness
/// is its sustain level. A percussive note (`sustain == 0`) falls silent right
/// after the attack/decay, so its decay time stands in for loudness instead —
/// a long decay reads as a fuller, louder hit than a short tick, which gives
/// retriggered drum channels real dynamics rather than one flat velocity.
pub fn velocity_for_envelope(adsr: Adsr) -> Velocity {
    let level = if adsr.sustain > 0 {
        adsr.sustain
    } else {
        adsr.decay
    };
    velocity_for_sustain(level)
}

/// A note inferred from a gate transition: rises start a note, falls end it.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[must_use]
pub struct NoteEvent {
    pub voice: VoiceId,
    pub start_frame: FrameIndex,
    /// Exclusive release endpoint. `None` means the note was not released in
    /// the captured trace; it is never silently closed at trace end.
    pub end_frame: Option<FrameIndex>,
    pub midi: MidiNote,
    pub cents: Cents,
    /// GM program for playback, derived from the voice's waveform.
    pub program: GmProgram,
    /// Note-on velocity, derived from the voice's sustain level.
    pub velocity: Velocity,
}

impl NoteEvent {
    #[must_use]
    pub fn release_frame(&self) -> Option<FrameIndex> {
        self.end_frame
    }

    /// Exclusive audible endpoint from the computed envelope. `None` means
    /// the release is censored by the end of the captured trace.
    #[must_use]
    pub fn sound_end_frame(&self, states: &[FrameState]) -> Option<FrameIndex> {
        let release = self.release_frame()?;
        let voice = self.voice.to_index();
        let end = states.iter().skip(release.0 as usize).find(|state| {
            state.digital_voices[voice]
                .envelope_activity
                .active_cycles
                .0
                == 0
                || state.digital_voices[voice]
                    .envelope_activity
                    .events
                    .iter()
                    .any(|event| event.kind == crate::emu::sid::EnvelopeEventKind::EnteredAttack)
        })?;
        Some(end.frame)
    }

    /// The note's active frames as a half-open `[start, end)` index range
    /// into a per-frame slice of length `total` — `end_frame` is exclusive,
    /// so the gate-off/parking frame is **not** part of the range. Clamps
    /// both bounds so the resulting range is always valid (`end >= start`,
    /// both ≤ `total`); pass it straight to `slice[r.start..r.end]` without
    /// further checks.
    ///
    /// `end_frame == None` (note still playing at trace end) means the
    /// range runs to `total`.
    #[must_use]
    pub fn frame_range(&self, total: usize) -> std::ops::Range<usize> {
        let start = (self.start_frame.0 as usize).min(total);
        let end = self
            .end_frame
            .map_or(total, |f| (f.0 as usize).min(total))
            .max(start);
        start..end
    }
}

/// Walk per-frame states and emit one `NoteEvent` per gate-rise → gate-fall
/// pair (plus any note still on at the end of the trace).
///
/// Frequency changes while the gate stays high are *not* treated as new
/// notes — that's portamento / arpeggio territory and belongs to effect
/// detection.
#[must_use]
pub fn detect_notes(states: &[FrameState], clock: SystemClock) -> Vec<NoteEvent> {
    let mut events: Vec<NoteEvent> = Vec::new();
    let mut prev_gate = [false; 3];
    let mut active: [Option<usize>; 3] = [None; 3];

    for state in states {
        for (i, voice) in state.voices.iter().enumerate() {
            let gate = voice.control.gate;
            let was = prev_gate[i];

            // A rising gate edge across frames, or a hard-restart retrigger
            // within the frame (gate dipped low and recovered), both re-attack
            // the envelope and start a fresh note.
            let onset = (gate && !was) || state.envelope_retrigger[i];

            if onset {
                // Only close the held note once we have a replacement: a
                // retrigger landing on a frame whose frequency is unmappable
                // (e.g. 0 Hz) should keep the current note rather than truncate
                // the voice to silence.
                if let Some((midi, cents)) = hertz_to_midi(voice.freq.to_hertz(clock)) {
                    if let Some(idx) = active[i].take() {
                        events[idx].end_frame = Some(state.frame);
                    }
                    active[i] = Some(events.len());
                    events.push(NoteEvent {
                        voice: VoiceId::from_index(i),
                        start_frame: state.frame,
                        end_frame: None,
                        midi,
                        cents,
                        program: gm_program_for_waveform(voice.control.waveform),
                        velocity: velocity_for_envelope(voice.adsr),
                    });
                    if !gate && let Some(idx) = active[i].take() {
                        events[idx].end_frame = Some(state.frame);
                    }
                }
            } else if !gate
                && was
                && let Some(idx) = active[i].take()
            {
                events[idx].end_frame = Some(state.frame);
            }

            prev_gate[i] = gate;
        }
    }

    events
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wf(triangle: bool, sawtooth: bool, pulse: bool, noise: bool) -> Waveform {
        Waveform {
            triangle,
            sawtooth,
            pulse,
            noise,
        }
    }

    #[test]
    fn waveform_maps_to_distinct_programs() {
        assert_eq!(
            gm_program_for_waveform(wf(true, false, false, false)),
            GmProgram::FLUTE
        );
        assert_eq!(
            gm_program_for_waveform(wf(false, true, false, false)),
            GmProgram::SAW_LEAD
        );
        assert_eq!(
            gm_program_for_waveform(wf(false, false, true, false)),
            GmProgram::SQUARE_LEAD
        );
        assert_eq!(
            gm_program_for_waveform(wf(false, false, false, true)),
            GmProgram::SYNTH_DRUM
        );
    }

    #[test]
    fn noise_dominates_combined_waveforms() {
        // Noise reads as percussion regardless of other bits.
        assert_eq!(
            gm_program_for_waveform(wf(true, true, true, true)),
            GmProgram::SYNTH_DRUM
        );
    }

    #[test]
    fn silent_waveform_falls_back_to_square() {
        assert_eq!(
            gm_program_for_waveform(wf(false, false, false, false)),
            GmProgram::SQUARE_LEAD
        );
    }

    fn gated_frame(frame: u32, freq: u16, retrigger: bool) -> FrameState {
        // FREQLO/HI from `freq`, control byte `0x81` (noise + gate), sustain 0.
        let regs = [(freq & 0xFF) as u8, (freq >> 8) as u8, 0, 0, 0x81, 0, 0];
        FrameState {
            frame: FrameIndex(frame),
            voices: [
                crate::analysis::voice::VoiceState::from_regs(&regs),
                Default::default(),
                Default::default(),
            ],
            envelope_retrigger: [retrigger, false, false],
            ..FrameState::default()
        }
    }

    #[test]
    fn within_frame_envelope_retrigger_splits_into_separate_notes() {
        // Gate stays high across every frame's end state, but a hard-restart
        // pulse re-attacks at frame 2: that must become a second note rather
        // than one continuously held note (the Ark Pandora V2 drum bug).
        let states = vec![
            gated_frame(0, 0x1000, false),
            gated_frame(1, 0x1000, false),
            gated_frame(2, 0x2000, true),
            gated_frame(3, 0x2000, false),
        ];
        let notes = detect_notes(&states, SystemClock::Pal);
        assert_eq!(notes.len(), 2, "retrigger should yield two notes");
        assert_eq!(notes[0].start_frame, FrameIndex(0));
        assert_eq!(notes[0].end_frame, Some(FrameIndex(2)));
        assert_eq!(notes[1].start_frame, FrameIndex(2));
    }

    #[test]
    fn frame_range_excludes_the_gate_off_frame() {
        // `end_frame` is exclusive (the gate-off frame): a note gated over
        // frames 1..=3 with the gate falling at frame 4 is active on exactly
        // [1, 4) — the gate-off/parking frame must not leak into per-frame
        // characterization (the old `+1` did exactly that).
        let ev = NoteEvent {
            voice: VoiceId(1),
            start_frame: FrameIndex(1),
            end_frame: Some(FrameIndex(4)),
            midi: MidiNote(60),
            cents: Cents(0.0),
            program: GmProgram::SQUARE_LEAD,
            velocity: Velocity(100),
        };
        assert_eq!(ev.frame_range(10), 1..4);
        // Still-playing note runs to the slice end.
        let open = NoteEvent {
            end_frame: None,
            ..ev
        };
        assert_eq!(open.frame_range(10), 1..10);
        // Clamped: end beyond the slice, and end before start, stay valid.
        assert_eq!(ev.frame_range(3), 1..3);
        let inverted = NoteEvent {
            end_frame: Some(FrameIndex(0)),
            ..ev
        };
        assert_eq!(inverted.frame_range(10), 1..1);
    }

    #[test]
    fn sound_end_uses_envelope_zero_and_censors_open_releases() {
        let event = NoteEvent {
            voice: VoiceId::V1,
            start_frame: FrameIndex(0),
            end_frame: Some(FrameIndex(1)),
            midi: MidiNote(60),
            cents: Cents(0.0),
            program: GmProgram::SQUARE_LEAD,
            velocity: Velocity(100),
        };
        let mut states = vec![FrameState::default(); 4];
        for (frame, state) in states.iter_mut().enumerate() {
            state.frame = FrameIndex(frame as u32);
        }
        states[1].digital_voices[0].envelope_activity.active_cycles = crate::trace::CpuCycles(10);
        states[2].digital_voices[0].envelope_activity.active_cycles = crate::trace::CpuCycles(5);
        assert_eq!(event.release_frame(), Some(FrameIndex(1)));
        assert_eq!(event.sound_end_frame(&states), Some(FrameIndex(3)));
        states[3].digital_voices[0].envelope_activity.active_cycles = crate::trace::CpuCycles(1);
        assert_eq!(event.sound_end_frame(&states), None);
    }

    #[test]
    fn detect_notes_ends_at_the_gate_off_frame_exclusive() {
        // Gate high on frames 0..=2, low on frame 3: the note's active range
        // is [0, 3) and `end_frame` records the gate-off frame itself.
        let mut states = vec![
            gated_frame(0, 0x1000, false),
            gated_frame(1, 0x1000, false),
            gated_frame(2, 0x1000, false),
            gated_frame(3, 0x1000, false),
        ];
        states[3].voices[0].control.gate = false;
        let notes = detect_notes(&states, SystemClock::Pal);
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].end_frame, Some(FrameIndex(3)));
        assert_eq!(notes[0].frame_range(4), 0..3);
    }

    #[test]
    fn held_gate_without_retrigger_stays_one_note() {
        let states = vec![
            gated_frame(0, 0x1000, false),
            gated_frame(1, 0x1000, false),
            gated_frame(2, 0x1000, false),
        ];
        let notes = detect_notes(&states, SystemClock::Pal);
        assert_eq!(notes.len(), 1);
    }

    #[test]
    fn sustain_maps_into_musical_velocity_range() {
        assert_eq!(velocity_for_sustain(0), Velocity(48));
        assert_eq!(velocity_for_sustain(15), Velocity(127));
        // Saturates rather than overflowing for out-of-range input.
        assert_eq!(velocity_for_sustain(255), Velocity(127));
        // Monotonic in between.
        assert!(velocity_for_sustain(8).0 > velocity_for_sustain(4).0);
    }

    #[test]
    fn percussive_velocity_tracks_decay_when_sustain_is_zero() {
        let tick = Adsr {
            attack: 1,
            decay: 1,
            sustain: 0,
            release: 4,
        };
        let fuller = Adsr {
            attack: 0,
            decay: 7,
            sustain: 0,
            release: 4,
        };
        // A longer-decaying percussive hit reads louder than a short tick.
        assert!(velocity_for_envelope(fuller).0 > velocity_for_envelope(tick).0);
        // A sustaining note still uses its sustain level, not decay.
        let sustained = Adsr {
            attack: 0,
            decay: 1,
            sustain: 12,
            release: 0,
        };
        assert_eq!(velocity_for_envelope(sustained), velocity_for_sustain(12));
    }
}
