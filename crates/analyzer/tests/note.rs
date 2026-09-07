use sid_analyzer::analysis::note::{
    Cents, GmProgram, MidiNote, NoteEvent, Velocity, detect_notes, hertz_to_midi,
};
use sid_analyzer::analysis::{Hertz, SystemClock, VoiceId};
use sid_analyzer::trace::FrameIndex;

mod common;
use common::{frame_v1 as frame, pulse_gated};

#[test]
fn midi_note_display_uses_three_char_tracker_format() {
    assert_eq!(MidiNote(60).to_string(), "C-4");
    assert_eq!(MidiNote(69).to_string(), "A-4");
    assert_eq!(MidiNote(61).to_string(), "C#4");
    assert_eq!(MidiNote(127).to_string(), "G-9");
}

#[test]
fn hertz_to_midi_at_a440_is_69_with_zero_cents() {
    let (m, c) = hertz_to_midi(Hertz(440.0)).expect("440 Hz is audible");
    assert_eq!(m, MidiNote(69));
    assert!(c.0.abs() < 0.001, "cents = {}", c.0);
}

#[test]
fn hertz_to_midi_off_pitch_reports_cents() {
    // 444 Hz is ~15.7 cents above A440.
    let (m, c) = hertz_to_midi(Hertz(444.0)).unwrap();
    assert_eq!(m, MidiNote(69));
    assert!((c.0 - 15.66).abs() < 0.1, "cents = {}", c.0);
}

#[test]
fn hertz_to_midi_silence_returns_none() {
    assert_eq!(hertz_to_midi(Hertz(0.0)), None);
    assert_eq!(hertz_to_midi(Hertz(-1.0)), None);
}

#[test]
fn cents_display_includes_sign() {
    assert_eq!(Cents(15.6).to_string(), "+16");
    assert_eq!(Cents(-23.4).to_string(), "-23");
    assert_eq!(Cents(0.0).to_string(), "+0");
}

#[test]
fn detect_notes_emits_event_per_gate_cycle() {
    // Three frames: gate rises at frame 0, holds, falls at frame 2.
    let states = vec![
        frame(0, pulse_gated(0x1D44, true)),  // A4
        frame(1, pulse_gated(0x1D44, true)),  // sustain
        frame(2, pulse_gated(0x1D44, false)), // gate falls
    ];
    let events = detect_notes(&states, SystemClock::Pal);
    assert_eq!(events.len(), 1);
    assert_eq!(
        events[0],
        NoteEvent {
            voice: VoiceId(1),
            start_frame: FrameIndex(0),
            end_frame: Some(FrameIndex(2)),
            midi: MidiNote(69),
            cents: events[0].cents,
            // Pulse waveform → square lead; default ADSR (sustain 0) → min velocity.
            program: GmProgram::SQUARE_LEAD,
            velocity: Velocity(48),
        }
    );
}

#[test]
fn detect_notes_handles_back_to_back_gate_cycles() {
    let states = vec![
        frame(0, pulse_gated(0x1D44, true)),  // A4 on
        frame(1, pulse_gated(0x1D44, false)), // off
        frame(2, pulse_gated(0x2266, true)),  // C5 on (approx)
        frame(3, pulse_gated(0x2266, false)), // off
    ];
    let events = detect_notes(&states, SystemClock::Pal);
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].midi, MidiNote(69));
    assert_eq!(events[0].end_frame, Some(FrameIndex(1)));
    assert_eq!(events[1].start_frame, FrameIndex(2));
    assert_eq!(events[1].end_frame, Some(FrameIndex(3)));
}

#[test]
fn detect_notes_keeps_note_open_at_end_of_trace() {
    let states = vec![
        frame(0, pulse_gated(0x1D44, true)),
        frame(1, pulse_gated(0x1D44, true)),
    ];
    let events = detect_notes(&states, SystemClock::Pal);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].end_frame, None);
}

#[test]
fn detect_notes_ignores_gate_on_with_zero_freq() {
    let states = vec![frame(0, pulse_gated(0x0000, true))];
    let events = detect_notes(&states, SystemClock::Pal);
    assert!(events.is_empty(), "got {events:?}");
}
