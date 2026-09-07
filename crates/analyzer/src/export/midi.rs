use crate::analysis::note::{Cents, NoteEvent, hertz_to_midi};
use crate::analysis::{FrameState, VoiceId};
use crate::emu::PlaybackTiming;
use midly::num::{u4, u7, u14, u15, u24, u28};
use midly::{
    Format, Header, MetaMessage, MidiMessage, PitchBend, Smf, Timing, Track, TrackEvent,
    TrackEventKind,
};
use std::io::{self, Write};

/// One MIDI tick represents one scheduled play call. With one tick per quarter
/// note, the tempo value is the duration of that call in microseconds.
const TICKS_PER_QUARTER: u16 = 1;

/// `8192` is the centre value of a 14-bit pitch-bend. The export declares a
/// ±12-semitone range so arpeggios and wide portamento remain representable.
const PITCH_BEND_CENTER: u16 = 8192;
const PITCH_BEND_RANGE_SEMITONES: u8 = 12;
const PITCH_BEND_UNITS_PER_CENT: f32 =
    PITCH_BEND_CENTER as f32 / (PITCH_BEND_RANGE_SEMITONES as f32 * 100.0);
const PITCH_BEND_MAX: u16 = 16383;

const NOTE_OFF_VELOCITY: u8 = 64;

/// Write a MIDI file (format 1) covering the analyzed notes.
///
/// Layout:
/// - Track 0 — tempo (conductor track)
/// - Track 1 — SID voice 1
/// - Track 2 — SID voice 2
/// - Track 3 — SID voice 3
///
/// Each note becomes a (program-change?, pitch-bend?, note-on) group at
/// `start_frame` and a note-off at `end_frame` (or `frame_count` if the
/// trace ended before the gate fell). Cents offset is encoded as an initial
/// pitch-bend on the voice's channel; `NoteEvent::program` drives a GM
/// program-change (emitted only when it changes) so the three voices play
/// with distinct timbres, and `NoteEvent::velocity` sets the note-on
/// velocity.
pub fn write_midi(
    notes: &[NoteEvent],
    timing: PlaybackTiming,
    frame_count: usize,
    out: &mut dyn Write,
) -> io::Result<()> {
    write_midi_with_expression(notes, &[], timing, frame_count, out)
}

/// Write format-1 MIDI with physical pitch, envelope/volume, cutoff, and
/// resonance automation derived from the analyzed frame timeline.
pub fn write_midi_with_expression(
    notes: &[NoteEvent],
    states: &[FrameState],
    timing: PlaybackTiming,
    frame_count: usize,
    out: &mut dyn Write,
) -> io::Result<()> {
    let header = Header::new(
        Format::Parallel,
        Timing::Metrical(u15::from(TICKS_PER_QUARTER)),
    );

    let end_tick = frame_count as u32;
    let mut tracks: Vec<Track<'_>> = Vec::with_capacity(4);
    tracks.push(conductor_track(timing));
    for i in 0..3 {
        tracks.push(voice_track(
            notes,
            states,
            timing.clock,
            VoiceId::from_index(i),
            end_tick,
        ));
    }

    let smf = Smf { header, tracks };
    smf.write_std(out)
}

fn conductor_track(timing: PlaybackTiming) -> Track<'static> {
    let usec_per_call = (timing.seconds_per_call() * 1_000_000.0).round() as u32;
    vec![
        TrackEvent {
            delta: u28::from(0),
            kind: TrackEventKind::Meta(MetaMessage::Tempo(u24::from(usec_per_call))),
        },
        TrackEvent {
            delta: u28::from(0),
            kind: TrackEventKind::Meta(MetaMessage::EndOfTrack),
        },
    ]
}

fn voice_track(
    notes: &[NoteEvent],
    states: &[FrameState],
    clock: crate::analysis::SystemClock,
    voice: VoiceId,
    frame_count: u32,
) -> Track<'static> {
    // Voice 1 → MIDI channel 0, voice 2 → channel 1, voice 3 → channel 2.
    let channel = u4::from(voice.0 - 1);

    // Notes for this voice in start order, so program changes are emitted in
    // the order they take effect.
    let mut voice_notes: Vec<&NoteEvent> = notes.iter().filter(|n| n.voice == voice).collect();
    voice_notes.sort_by_key(|n| n.start_frame.0);

    // (tick, order, kind). `order` breaks ties at the same tick so a
    // re-trigger on one frame is well-formed: note-off (0) < program-change
    // (1) < pitch-bend (2) < expression (3) < note-on (4).
    let mut events: Vec<(u32, u8, TrackEventKind<'static>)> = Vec::new();
    let mut cur_program: Option<u8> = None;

    if !voice_notes.is_empty() {
        push_pitch_bend_range(channel, &mut events);
    }

    for note in voice_notes {
        let start_tick = note.start_frame.0;
        let end_tick = note.end_frame.map_or(frame_count, |f| f.0);
        if end_tick <= start_tick {
            continue;
        }
        if cur_program != Some(note.program.0) {
            cur_program = Some(note.program.0);
            events.push((
                start_tick,
                1,
                TrackEventKind::Midi {
                    channel,
                    message: MidiMessage::ProgramChange {
                        program: u7::from(note.program.0.min(127)),
                    },
                },
            ));
        }
        let key = u7::from(note.midi.0.min(127));
        let bend = cents_to_pitch_bend(note.cents);
        if states.is_empty() && bend != PITCH_BEND_CENTER {
            events.push((
                start_tick,
                2,
                TrackEventKind::Midi {
                    channel,
                    message: MidiMessage::PitchBend {
                        bend: PitchBend(u14::from(bend)),
                    },
                },
            ));
        }
        events.push((
            start_tick,
            4,
            TrackEventKind::Midi {
                channel,
                message: MidiMessage::NoteOn {
                    key,
                    vel: u7::from(note.velocity.0.min(127)),
                },
            },
        ));
        if !states.is_empty() {
            push_note_expression(note, states, clock, channel, end_tick, &mut events);
        }
        events.push((
            end_tick,
            0,
            TrackEventKind::Midi {
                channel,
                message: MidiMessage::NoteOff {
                    key,
                    vel: u7::from(NOTE_OFF_VELOCITY),
                },
            },
        ));
    }

    events.sort_by_key(|(tick, order, _)| (*tick, *order));

    let mut track: Track<'static> = Vec::with_capacity(events.len() + 1);
    let mut prev_tick: u32 = 0;
    for (tick, _order, kind) in events {
        let delta = tick - prev_tick;
        track.push(TrackEvent {
            delta: u28::from(delta),
            kind,
        });
        prev_tick = tick;
    }
    track.push(TrackEvent {
        delta: u28::from(0),
        kind: TrackEventKind::Meta(MetaMessage::EndOfTrack),
    });
    track
}

fn push_note_expression(
    note: &NoteEvent,
    states: &[FrameState],
    clock: crate::analysis::SystemClock,
    channel: u4,
    end_tick: u32,
    events: &mut Vec<(u32, u8, TrackEventKind<'static>)>,
) {
    let voice = note.voice.to_index();
    let start = note.start_frame.0 as usize;
    let end = (end_tick as usize).min(states.len());
    let mut previous_bend = None;
    let mut previous_expression = None;
    let mut previous_cutoff = None;
    let mut previous_resonance = None;
    for state in states.iter().take(end).skip(start) {
        let physical = state.voices[voice];
        if !physical.control.waveform.is_noise_only()
            && let Some((midi, cents)) = hertz_to_midi(physical.freq.to_hertz(clock))
        {
            let relative =
                Cents(f32::from(i16::from(midi.0) - i16::from(note.midi.0)) * 100.0 + cents.0);
            let bend = cents_to_pitch_bend(relative);
            if previous_bend != Some(bend) {
                events.push((
                    state.frame.0,
                    2,
                    TrackEventKind::Midi {
                        channel,
                        message: MidiMessage::PitchBend {
                            bend: PitchBend(u14::from(bend)),
                        },
                    },
                ));
                previous_bend = Some(bend);
            }
        }
        let envelope = u32::from(state.digital_voices[voice].envelope.level.0);
        let master = u32::from(state.volume.0.min(15));
        let expression = ((envelope * master * 127) / (255 * 15)) as u8;
        if previous_expression != Some(expression) {
            events.push((
                state.frame.0,
                3,
                TrackEventKind::Midi {
                    channel,
                    message: MidiMessage::Controller {
                        controller: u7::from(11),
                        value: u7::from(expression),
                    },
                },
            ));
            previous_expression = Some(expression);
        }
        if state.filter.routing.contains(note.voice) {
            let cutoff = ((u32::from(state.filter.cutoff.0) * 127) / 2047) as u8;
            if previous_cutoff != Some(cutoff) {
                push_controller(state.frame.0, 3, channel, 74, cutoff, events);
                previous_cutoff = Some(cutoff);
            }
            let resonance = (u16::from(state.filter.resonance.0) * 127 / 15) as u8;
            if previous_resonance != Some(resonance) {
                push_controller(state.frame.0, 3, channel, 71, resonance, events);
                previous_resonance = Some(resonance);
            }
        }
    }
}

fn push_pitch_bend_range(channel: u4, events: &mut Vec<(u32, u8, TrackEventKind<'static>)>) {
    for (controller, value) in [
        (101, 0),
        (100, 0),
        (6, PITCH_BEND_RANGE_SEMITONES),
        (38, 0),
        (101, 127),
        (100, 127),
    ] {
        push_controller(0, 0, channel, controller, value, events);
    }
}

fn push_controller(
    tick: u32,
    order: u8,
    channel: u4,
    controller: u8,
    value: u8,
    events: &mut Vec<(u32, u8, TrackEventKind<'static>)>,
) {
    events.push((
        tick,
        order,
        TrackEventKind::Midi {
            channel,
            message: MidiMessage::Controller {
                controller: u7::from(controller),
                value: u7::from(value),
            },
        },
    ));
}

fn cents_to_pitch_bend(cents: Cents) -> u16 {
    let offset = (cents.0 * PITCH_BEND_UNITS_PER_CENT) as i32;
    (i32::from(PITCH_BEND_CENTER) + offset).clamp(0, i32::from(PITCH_BEND_MAX)) as u16
}
