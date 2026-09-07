use midly::{Format, MetaMessage, MidiMessage, Smf, Timing, TrackEventKind};
use sid_analyzer::analysis::VoiceId;
use sid_analyzer::analysis::filter::{Cutoff, Resonance};
use sid_analyzer::analysis::note::{Cents, GmProgram, MidiNote, NoteEvent, Velocity};
use sid_analyzer::analysis::voice::SidFreq;
use sid_analyzer::analysis::{FrameState, SystemClock, Volume};
use sid_analyzer::emu::sid::EnvLevel;
use sid_analyzer::emu::{CallRate, CiaTimerPeriod, PlaybackTiming};
use sid_analyzer::export::midi::{write_midi, write_midi_with_expression};
use sid_analyzer::trace::FrameIndex;

fn note(voice: u8, start: u32, end: Option<u32>, midi: u8, cents: f32) -> NoteEvent {
    NoteEvent {
        voice: VoiceId(voice),
        start_frame: FrameIndex(start),
        end_frame: end.map(FrameIndex),
        midi: MidiNote(midi),
        cents: Cents(cents),
        program: GmProgram::SQUARE_LEAD,
        velocity: Velocity::DEFAULT,
    }
}

fn note_timbre(
    voice: u8,
    start: u32,
    end: u32,
    midi: u8,
    program: GmProgram,
    velocity: Velocity,
) -> NoteEvent {
    NoteEvent {
        voice: VoiceId(voice),
        start_frame: FrameIndex(start),
        end_frame: Some(FrameIndex(end)),
        midi: MidiNote(midi),
        cents: Cents(0.0),
        program,
        velocity,
    }
}

fn smf_bytes(notes: &[NoteEvent], clock: SystemClock, frame_count: usize) -> Vec<u8> {
    let mut buf = Vec::new();
    write_midi(notes, PlaybackTiming::vblank(clock), frame_count, &mut buf).expect("write_midi");
    buf
}

#[test]
fn midi_format_is_parallel_with_one_conductor_plus_three_voice_tracks() {
    let notes = vec![note(1, 0, Some(10), 60, 0.0)];
    let bytes = smf_bytes(&notes, SystemClock::Pal, 50);
    let smf = Smf::parse(&bytes).expect("parse smf");
    assert!(matches!(smf.header.format, Format::Parallel));
    assert_eq!(smf.tracks.len(), 4, "conductor + 3 voice tracks");
}

#[test]
fn timing_uses_one_tick_per_play_call() {
    let pal_bytes = smf_bytes(&[], SystemClock::Pal, 0);
    let ntsc_bytes = smf_bytes(&[], SystemClock::Ntsc, 0);
    let pal = Smf::parse(&pal_bytes).unwrap();
    let ntsc = Smf::parse(&ntsc_bytes).unwrap();
    let pal_tpqn = match pal.header.timing {
        Timing::Metrical(n) => n.as_int(),
        Timing::Timecode(_, _) => panic!("expected metrical timing"),
    };
    let ntsc_tpqn = match ntsc.header.timing {
        Timing::Metrical(n) => n.as_int(),
        Timing::Timecode(_, _) => panic!("expected metrical timing"),
    };
    assert_eq!(pal_tpqn, 1);
    assert_eq!(ntsc_tpqn, 1);
}

#[test]
fn conductor_track_carries_resolved_call_duration() {
    let bytes = smf_bytes(&[], SystemClock::Pal, 0);
    let smf = Smf::parse(&bytes).unwrap();
    let tempo = smf.tracks[0]
        .iter()
        .find_map(|e| match e.kind {
            TrackEventKind::Meta(MetaMessage::Tempo(t)) => Some(t.as_int()),
            _ => None,
        })
        .expect("conductor has tempo");
    assert_eq!(tempo, 19_950);
}

#[test]
fn cia_timing_controls_midi_playback_speed() {
    let timing = PlaybackTiming {
        clock: SystemClock::Pal,
        call_rate: CallRate::new(100, 1),
        cia_timed: true,
        cia_period: Some(CiaTimerPeriod::new(9_852)),
    };
    let mut bytes = Vec::new();
    write_midi(&[], timing, 100, &mut bytes).unwrap();
    let smf = Smf::parse(&bytes).unwrap();
    let tempo = smf.tracks[0]
        .iter()
        .find_map(|event| match event.kind {
            TrackEventKind::Meta(MetaMessage::Tempo(value)) => Some(value.as_int()),
            _ => None,
        })
        .unwrap();
    assert_eq!(tempo, 10_000);
}

#[test]
fn notes_emit_note_on_and_note_off_on_correct_channel() {
    // V1: A4 (midi 69) from frame 0..10
    // V3: C5 (midi 72) from frame 5..15
    let notes = vec![note(1, 0, Some(10), 69, 0.0), note(3, 5, Some(15), 72, 0.0)];
    let bytes = smf_bytes(&notes, SystemClock::Pal, 20);
    let smf = Smf::parse(&bytes).unwrap();

    let v1 = midi_msgs(&smf.tracks[1]);
    let v3 = midi_msgs(&smf.tracks[3]);

    assert!(
        v1.iter().any(|(ch, m)| *ch == 0
            && matches!(m, MidiMessage::NoteOn { key, .. } if key.as_int() == 69)),
        "V1 should NoteOn key=69 on channel 0: {v1:?}"
    );
    assert!(
        v1.iter().any(|(ch, m)| *ch == 0
            && matches!(m, MidiMessage::NoteOff { key, .. } if key.as_int() == 69)),
        "V1 should NoteOff key=69 on channel 0"
    );
    assert!(
        v3.iter().any(|(ch, m)| *ch == 2
            && matches!(m, MidiMessage::NoteOn { key, .. } if key.as_int() == 72)),
        "V3 should NoteOn key=72 on channel 2"
    );
}

#[test]
fn note_left_open_at_trace_end_gets_note_off_at_frame_count() {
    let notes = vec![note(1, 0, None, 60, 0.0)];
    let bytes = smf_bytes(&notes, SystemClock::Pal, 40);
    let smf = Smf::parse(&bytes).unwrap();
    let v1 = &smf.tracks[1];
    // Sum of deltas up to and including the NoteOff should equal frame_count.
    let mut tick = 0_u32;
    let mut note_off_tick = None;
    for e in v1 {
        tick += e.delta.as_int();
        if matches!(
            e.kind,
            TrackEventKind::Midi {
                message: MidiMessage::NoteOff { .. },
                ..
            }
        ) {
            note_off_tick = Some(tick);
            break;
        }
    }
    assert_eq!(note_off_tick, Some(40));
}

#[test]
fn cents_offset_emits_pitch_bend_before_note_on() {
    // +50 cents: should produce a pitch-bend with bend > 8192.
    let notes = vec![note(1, 0, Some(10), 60, 50.0)];
    let bytes = smf_bytes(&notes, SystemClock::Pal, 20);
    let smf = Smf::parse(&bytes).unwrap();
    let v1 = midi_msgs(&smf.tracks[1]);

    let bend_idx = v1
        .iter()
        .position(|(_, m)| matches!(m, MidiMessage::PitchBend { .. }))
        .expect("expected pitch-bend");
    let on_idx = v1
        .iter()
        .position(|(_, m)| matches!(m, MidiMessage::NoteOn { .. }))
        .expect("expected note-on");
    assert!(bend_idx < on_idx, "pitch-bend should precede note-on");

    let bend_value = match v1[bend_idx].1 {
        MidiMessage::PitchBend { bend } => bend.0.as_int(),
        _ => unreachable!(),
    };
    assert!(bend_value > 8192, "50¢ should bend up: {bend_value}");
}

#[test]
fn zero_cents_omits_pitch_bend() {
    let notes = vec![note(1, 0, Some(10), 60, 0.0)];
    let bytes = smf_bytes(&notes, SystemClock::Pal, 20);
    let smf = Smf::parse(&bytes).unwrap();
    let v1 = midi_msgs(&smf.tracks[1]);
    assert!(
        !v1.iter()
            .any(|(_, m)| matches!(m, MidiMessage::PitchBend { .. })),
        "no cents ⇒ no pitch-bend"
    );
}

#[test]
fn program_change_precedes_note_on_with_the_notes_program() {
    let notes = vec![note_timbre(
        2,
        0,
        10,
        60,
        GmProgram::SAW_LEAD,
        Velocity::DEFAULT,
    )];
    let bytes = smf_bytes(&notes, SystemClock::Pal, 20);
    let smf = Smf::parse(&bytes).unwrap();
    let v2 = midi_msgs(&smf.tracks[2]);

    let prog_idx = v2
        .iter()
        .position(|(ch, m)| {
            *ch == 1
                && matches!(m, MidiMessage::ProgramChange { program } if program.as_int() == 81)
        })
        .expect("expected program-change to SAW_LEAD (81) on channel 1");
    let on_idx = v2
        .iter()
        .position(|(_, m)| matches!(m, MidiMessage::NoteOn { .. }))
        .expect("expected note-on");
    assert!(prog_idx < on_idx, "program-change should precede note-on");
}

#[test]
fn program_change_is_emitted_only_when_it_changes() {
    // Two square-lead notes then one saw-lead note, all on voice 1.
    let notes = vec![
        note_timbre(1, 0, 5, 60, GmProgram::SQUARE_LEAD, Velocity::DEFAULT),
        note_timbre(1, 5, 10, 62, GmProgram::SQUARE_LEAD, Velocity::DEFAULT),
        note_timbre(1, 10, 15, 64, GmProgram::SAW_LEAD, Velocity::DEFAULT),
    ];
    let bytes = smf_bytes(&notes, SystemClock::Pal, 20);
    let smf = Smf::parse(&bytes).unwrap();
    let programs: Vec<u8> = midi_msgs(&smf.tracks[1])
        .iter()
        .filter_map(|(_, m)| match m {
            MidiMessage::ProgramChange { program } => Some(program.as_int()),
            _ => None,
        })
        .collect();
    assert_eq!(programs, vec![80, 81], "one change per distinct program");
}

#[test]
fn physical_timeline_emits_dynamic_pitch_bend_and_expression_controller() {
    let notes = vec![note(1, 0, Some(4), 69, 0.0)];
    let states: Vec<_> = [0x1d45, 0x1d65, 0x1d25, 0x1d45]
        .into_iter()
        .enumerate()
        .map(|(frame, frequency)| {
            let mut state = FrameState {
                frame: FrameIndex(frame as u32),
                volume: Volume(15),
                ..FrameState::default()
            };
            state.voices[0].freq = SidFreq(frequency);
            state.voices[0].control.gate = true;
            state.voices[0].control.waveform.pulse = true;
            state.filter.routing.voice1 = true;
            state.filter.cutoff = Cutoff(512 + frame as u16 * 256);
            state.filter.resonance = Resonance(4 + frame as u8);
            state.digital_voices[0].envelope.level = EnvLevel(64 + frame as u8 * 32);
            state
        })
        .collect();
    let mut bytes = Vec::new();
    write_midi_with_expression(
        &notes,
        &states,
        PlaybackTiming::vblank(SystemClock::Pal),
        states.len(),
        &mut bytes,
    )
    .unwrap();
    let smf = Smf::parse(&bytes).unwrap();
    let messages = midi_msgs(&smf.tracks[1]);
    assert!(
        messages
            .iter()
            .filter(|(_, message)| matches!(message, MidiMessage::PitchBend { .. }))
            .count()
            >= 3
    );
    assert!(messages.iter().any(|(_, message)| {
        matches!(
            message,
            MidiMessage::Controller { controller, .. } if controller.as_int() == 11
        )
    }));
    for controller in [71, 74, 100, 101] {
        assert!(messages.iter().any(|(_, message)| {
            matches!(
                message,
                MidiMessage::Controller { controller: actual, .. }
                    if actual.as_int() == controller
            )
        }));
    }
}

#[test]
fn note_on_uses_the_events_velocity() {
    let notes = vec![note_timbre(1, 0, 10, 60, GmProgram::FLUTE, Velocity(42))];
    let bytes = smf_bytes(&notes, SystemClock::Pal, 20);
    let smf = Smf::parse(&bytes).unwrap();
    let vel = midi_msgs(&smf.tracks[1])
        .iter()
        .find_map(|(_, m)| match m {
            MidiMessage::NoteOn { vel, .. } => Some(vel.as_int()),
            _ => None,
        })
        .expect("expected note-on");
    assert_eq!(vel, 42);
}

fn midi_msgs<'a>(track: &'a [midly::TrackEvent<'a>]) -> Vec<(u8, MidiMessage)> {
    track
        .iter()
        .filter_map(|e| match e.kind {
            TrackEventKind::Midi { channel, message } => Some((channel.as_int(), message)),
            _ => None,
        })
        .collect()
}
