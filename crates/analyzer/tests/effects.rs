use sid_analyzer::analysis::effects::{
    Effect, EffectSpan, EffectThresholds, VoiceRelation, detect_effects, detect_voice_relations,
};
use sid_analyzer::analysis::filter::{Cutoff, FilterMode, FilterRouting, FilterState, Resonance};
use sid_analyzer::analysis::voice::VoiceState;
use sid_analyzer::analysis::{FrameState, VoiceId, Volume};
use sid_analyzer::emu::CallRate;
use sid_analyzer::emu::sid::EnvLevel;
use sid_analyzer::trace::{
    FrameIndex, FrameTrace, RegisterRead, RegisterWrite, SidRegister, SubFrameOffset, Trace,
};

mod common;
use common::{SidPipeline, empty_trace_for, frame_v1, frame_v1_filtered, pitch_seq, pulse_voice};

const OCEAN_LOADER_1: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../assets/music/Ocean_Loader_1.sid"
);

fn route_v1() -> FilterState {
    FilterState {
        routing: FilterRouting {
            voice1: true,
            ..FilterRouting::default()
        },
        ..FilterState::default()
    }
}

#[test]
fn detects_hard_sync_span_on_voice() {
    let mut on = VoiceState::default();
    on.control.sync = true;
    let states = vec![
        frame_v1(0, VoiceState::default()),
        frame_v1(1, on),
        frame_v1(2, on),
        frame_v1(3, VoiceState::default()),
    ];
    let trace = empty_trace_for(&states);
    let effects = detect_effects(&trace, &states, EffectThresholds::default());

    assert!(
        effects.contains(&EffectSpan {
            effect: Effect::HardSync,
            voice: Some(VoiceId(1)),
            start_frame: FrameIndex(1),
            end_frame: FrameIndex(2),
        }),
        "expected HardSync span: {effects:?}"
    );
}

#[test]
fn detects_ring_mod_span_on_voice() {
    let mut on = VoiceState::default();
    on.control.ring_mod = true;
    let states = vec![frame_v1(0, on), frame_v1(1, on)];
    let trace = empty_trace_for(&states);
    let effects = detect_effects(&trace, &states, EffectThresholds::default());

    assert!(
        effects.iter().any(|e| e.effect == Effect::RingMod),
        "expected RingMod span: {effects:?}"
    );
}

#[test]
fn detects_pwm_when_pw_changes_for_min_frames() {
    let states: Vec<FrameState> = (0..6)
        .map(|n| frame_v1(n, pulse_voice(0x1000, 0x100 + n as u16 * 16, true, true)))
        .collect();
    let trace = empty_trace_for(&states);
    let effects = detect_effects(&trace, &states, EffectThresholds::default());

    let pwm: Vec<_> = effects.iter().filter(|e| e.effect == Effect::Pwm).collect();
    assert_eq!(pwm.len(), 1, "expected one PWM span: {effects:?}");
    assert_eq!(pwm[0].voice, Some(VoiceId(1)));
    // PWM run starts at frame 1 (first frame with a change vs frame 0).
    assert_eq!(pwm[0].start_frame, FrameIndex(1));
    assert_eq!(pwm[0].end_frame, FrameIndex(5));
}

#[test]
fn pwm_below_threshold_is_ignored() {
    // 3 frames of PW change, default min is 4 → no PWM span.
    let states: Vec<FrameState> = (0..4)
        .map(|n| frame_v1(n, pulse_voice(0x1000, 0x100 + n as u16 * 16, true, true)))
        .collect();
    let trace = empty_trace_for(&states);
    let effects = detect_effects(&trace, &states, EffectThresholds::default());

    assert!(
        !effects.iter().any(|e| e.effect == Effect::Pwm),
        "expected no PWM (only 3 changing frames, min=4): {effects:?}"
    );
}

#[test]
fn pwm_requires_pulse_waveform_active() {
    // PW changes every frame, but pulse waveform is OFF — no PWM.
    let states: Vec<FrameState> = (0..6)
        .map(|n| frame_v1(n, pulse_voice(0x1000, 0x100 + n as u16 * 16, false, true)))
        .collect();
    let trace = empty_trace_for(&states);
    let effects = detect_effects(&trace, &states, EffectThresholds::default());

    assert!(
        !effects.iter().any(|e| e.effect == Effect::Pwm),
        "pulse off should suppress PWM: {effects:?}"
    );
}

#[test]
fn detects_filter_sweep_when_cutoff_changes_with_routing() {
    let states: Vec<FrameState> = (0..6)
        .map(|n| {
            frame_v1_filtered(
                n,
                VoiceState::default(),
                FilterState {
                    cutoff: Cutoff(0x100 + n as u16 * 16),
                    ..route_v1()
                },
            )
        })
        .collect();
    let trace = empty_trace_for(&states);
    let effects = detect_effects(&trace, &states, EffectThresholds::default());

    let sweeps: Vec<_> = effects
        .iter()
        .filter(|e| e.effect == Effect::FilterSweep)
        .collect();
    assert_eq!(sweeps.len(), 1, "expected one FilterSweep: {effects:?}");
    assert_eq!(sweeps[0].voice, None);
}

#[test]
fn cutoff_changes_without_routing_do_not_count_as_sweep() {
    // Cutoff changes every frame but no voice is routed through the filter.
    let states: Vec<FrameState> = (0..6)
        .map(|n| {
            frame_v1_filtered(
                n,
                VoiceState::default(),
                FilterState {
                    cutoff: Cutoff(0x100 + n as u16 * 16),
                    ..FilterState::default()
                },
            )
        })
        .collect();
    let trace = empty_trace_for(&states);
    let effects = detect_effects(&trace, &states, EffectThresholds::default());

    assert!(
        !effects.iter().any(|e| e.effect == Effect::FilterSweep),
        "no routing should suppress FilterSweep: {effects:?}"
    );
}

#[test]
fn detects_sample_playback_from_dense_d418_writes() {
    let frame_with_d418_writes = |n: u32, count: usize| FrameTrace {
        frame: FrameIndex(n),
        writes: (0..count)
            .map(|i| RegisterWrite {
                reg: SidRegister(0x18),
                value: i as u8,
                offset: SubFrameOffset(i as u32),
            })
            .collect(),
        ..Default::default()
    };

    let trace = Trace {
        init_writes: Vec::new(),
        frames: vec![
            frame_with_d418_writes(0, 1),  // normal: not a sample
            frame_with_d418_writes(1, 10), // dense: sample
            frame_with_d418_writes(2, 12), // dense: sample (continues)
            frame_with_d418_writes(3, 0),  // gap: ends span
        ],
        ..Default::default()
    };
    let states: Vec<FrameState> = trace
        .frames
        .iter()
        .map(|f| FrameState {
            frame: f.frame,
            ..FrameState::default()
        })
        .collect();

    let effects = detect_effects(&trace, &states, EffectThresholds::default());
    let samples: Vec<_> = effects
        .iter()
        .filter(|e| e.effect == Effect::Sample)
        .collect();
    assert_eq!(samples.len(), 1, "expected one Sample span: {effects:?}");
    assert_eq!(samples[0].start_frame, FrameIndex(1));
    assert_eq!(samples[0].end_frame, FrameIndex(2));
    assert_eq!(samples[0].voice, None);
}

#[test]
fn detects_galway_packed_nibble_cadence() {
    let values = [2, 13, 4, 11, 6, 9];
    let offsets = [0, 125, 255, 380, 510, 635];
    let trace = Trace {
        frames: vec![FrameTrace {
            frame: FrameIndex(0),
            writes: values
                .into_iter()
                .zip(offsets)
                .map(|(value, offset)| RegisterWrite {
                    reg: SidRegister(0x18),
                    value,
                    offset: SubFrameOffset(offset),
                })
                .collect(),
            ..FrameTrace::default()
        }],
        ..Trace::default()
    };
    let states = vec![FrameState::default()];

    let effects = detect_effects(&trace, &states, EffectThresholds::default());

    assert!(effects.iter().any(|effect| effect.effect == Effect::Sample));
}

#[test]
fn repeated_identical_d418_writes_are_not_sample_playback() {
    let mut frames: Vec<_> = (0..3)
        .map(|frame| FrameTrace {
            frame: FrameIndex(frame),
            writes: (0..4)
                .map(|offset| RegisterWrite {
                    reg: SidRegister(0x18),
                    value: 0x0f,
                    offset: SubFrameOffset(offset * 1_200),
                })
                .collect(),
            ..FrameTrace::default()
        })
        .collect();
    frames.push(FrameTrace {
        frame: FrameIndex(3),
        writes: vec![RegisterWrite {
            reg: SidRegister(0x18),
            value: 0x08,
            offset: SubFrameOffset(0),
        }],
        ..FrameTrace::default()
    });
    let trace = Trace {
        frames,
        ..Trace::default()
    };
    let states = vec![FrameState::default(); 4];

    let effects = detect_effects(&trace, &states, EffectThresholds::default());

    assert!(!effects.iter().any(|effect| effect.effect == Effect::Sample));
}

#[test]
#[cfg_attr(
    not(feature = "asset-tests"),
    ignore = "requires the optional assets/music corpus"
)]
fn ocean_loader_1_master_volume_writes_are_not_galway_digi() {
    let fixture = SidPipeline::run(
        OCEAN_LOADER_1,
        sid_analyzer::header::SubtuneIndex(1),
        10_126,
    );
    let d418_writes = fixture
        .trace
        .frames
        .iter()
        .flat_map(|frame| &frame.writes)
        .filter(|write| write.reg == SidRegister(0x18))
        .count();

    assert!(d418_writes > 0, "fixture must exercise $D418 writes");
    assert!(
        !fixture
            .effects
            .iter()
            .any(|effect| effect.effect == Effect::Sample)
    );
}

#[test]
fn fast_cross_call_d418_stream_is_sample_not_master_tremolo() {
    let frames: Vec<_> = (0..8)
        .map(|frame| FrameTrace {
            frame: FrameIndex(frame),
            writes: vec![RegisterWrite {
                reg: SidRegister(0x18),
                value: if frame % 2 == 0 { 4 } else { 12 },
                offset: SubFrameOffset(0),
            }],
            ..FrameTrace::default()
        })
        .collect();
    let mut trace = Trace {
        frames,
        call_rate: CallRate::new(1_000, 1),
        ..Trace::default()
    };
    let states: Vec<_> = trace
        .frames
        .iter()
        .map(|frame| FrameState {
            frame: frame.frame,
            volume: Volume(frame.writes[0].value & 0x0f),
            ..FrameState::default()
        })
        .collect();
    trace.timing_exact = true;

    let effects = detect_effects(&trace, &states, EffectThresholds::default());
    assert!(effects.iter().any(|effect| {
        effect.effect == Effect::Sample
            && effect.start_frame == FrameIndex(0)
            && effect.end_frame == FrameIndex(7)
    }));
    assert!(
        !effects
            .iter()
            .any(|effect| effect.effect == Effect::Tremolo && effect.voice.is_none())
    );
}

#[test]
fn thresholds_are_tunable() {
    // PW changes for exactly 3 frames; default min is 4 (no detection).
    // Override min_frames = 2 → should detect.
    let states: Vec<FrameState> = (0..4)
        .map(|n| frame_v1(n, pulse_voice(0x1000, 0x100 + n as u16 * 16, true, true)))
        .collect();
    let trace = empty_trace_for(&states);

    let strict = detect_effects(&trace, &states, EffectThresholds::default());
    assert!(!strict.iter().any(|e| e.effect == Effect::Pwm));

    let loose = detect_effects(
        &trace,
        &states,
        EffectThresholds {
            pwm_min_frames: 2,
            ..EffectThresholds::default()
        },
    );
    assert!(loose.iter().any(|e| e.effect == Effect::Pwm));
}

#[test]
fn detects_envelope_tremolo_without_mistaking_adsr_decay_for_oscillation() {
    let levels = [20, 40, 20, 40, 20, 40, 20, 40];
    let states: Vec<_> = levels
        .into_iter()
        .enumerate()
        .map(|(frame, level)| {
            let mut state = frame_v1(frame as u32, pulse_voice(0x1000, 0x800, true, true));
            state.digital_voices[0].envelope.level = EnvLevel(level);
            state
        })
        .collect();
    let effects = detect_effects(
        &empty_trace_for(&states),
        &states,
        EffectThresholds::default(),
    );
    assert!(
        effects.iter().any(|effect| {
            effect.effect == Effect::Tremolo && effect.voice == Some(VoiceId::V1)
        })
    );

    let mut decay = states;
    for (index, state) in decay.iter_mut().enumerate() {
        state.digital_voices[0].envelope.level = EnvLevel(80 - index as u8 * 4);
    }
    let effects = detect_effects(
        &empty_trace_for(&decay),
        &decay,
        EffectThresholds::default(),
    );
    assert!(
        !effects
            .iter()
            .any(|effect| effect.effect == Effect::Tremolo)
    );
}

#[test]
fn detects_master_volume_tremolo_and_filter_mode_and_resonance_modulation() {
    let states: Vec<_> = (0..8)
        .map(|frame| {
            let high = frame % 2 != 0;
            let mut state = frame_v1_filtered(
                frame,
                pulse_voice(0x1000, 0x800, true, true),
                FilterState {
                    resonance: Resonance(if high { 12 } else { 4 }),
                    mode: FilterMode {
                        low_pass: !high,
                        band_pass: high,
                        ..FilterMode::default()
                    },
                    ..route_v1()
                },
            );
            state.volume = Volume(if high { 15 } else { 8 });
            state
        })
        .collect();
    let effects = detect_effects(
        &empty_trace_for(&states),
        &states,
        EffectThresholds::default(),
    );
    for expected in [
        Effect::Tremolo,
        Effect::FilterResonanceSweep,
        Effect::FilterModeModulation,
    ] {
        assert!(
            effects.iter().any(|effect| effect.effect == expected),
            "missing {expected}: {effects:?}"
        );
    }
}

#[test]
fn sparse_master_volume_changes_do_not_become_song_wide_tremolo() {
    let mut states: Vec<_> = (0..40)
        .map(|frame| FrameState {
            frame: FrameIndex(frame),
            volume: Volume(8),
            ..FrameState::default()
        })
        .collect();
    for (frame, volume) in [(5, 15), (15, 8), (25, 15), (35, 8)] {
        states[frame].volume = Volume(volume);
        for state in &mut states[frame + 1..] {
            state.volume = Volume(volume);
        }
    }
    let effects = detect_effects(
        &empty_trace_for(&states),
        &states,
        EffectThresholds::default(),
    );
    assert!(
        !effects
            .iter()
            .any(|effect| { effect.effect == Effect::Tremolo && effect.voice.is_none() })
    );
}

#[test]
fn voice3_off_with_hardware_or_read_consumers_is_a_typed_modulator() {
    let mut state = frame_v1(0, VoiceState::default());
    state.filter.mode.voice3_off = true;
    state.voices[0].control.sync = true;
    state.voices[2] = pulse_voice(0x1000, 0x800, true, true);
    let states = vec![
        state.clone(),
        FrameState {
            frame: FrameIndex(1),
            ..state
        },
    ];
    let mut trace = empty_trace_for(&states);
    trace.frames[0].reads.push(RegisterRead {
        reg: SidRegister(0x1b),
        value: 0,
        offset: SubFrameOffset(0),
    });
    let effects = detect_effects(&trace, &states, EffectThresholds::default());
    assert!(effects.iter().any(|effect| {
        effect.effect == Effect::Voice3Modulator && effect.voice == Some(VoiceId::V3)
    }));
}

#[test]
fn filtered_voice3_is_audible_even_when_voice3_off_is_set() {
    let mut state = frame_v1(0, VoiceState::default());
    state.filter.mode.voice3_off = true;
    state.filter.routing.voice3 = true;
    state.voices[0].control.sync = true;
    state.voices[2] = pulse_voice(0x1000, 0x800, true, true);
    let states = vec![state];
    let mut trace = empty_trace_for(&states);
    trace.frames[0].reads.push(RegisterRead {
        reg: SidRegister(0x1b),
        value: 0,
        offset: SubFrameOffset(0),
    });
    let effects = detect_effects(&trace, &states, EffectThresholds::default());
    assert!(
        !effects
            .iter()
            .any(|effect| effect.effect == Effect::Voice3Modulator)
    );
}

#[test]
fn noise_only_frequency_is_not_pitch_modulation() {
    let mut states = pitch_seq(&[
        0x1000, 0x7000, 0x1000, 0x6000, 0x1000, 0x5000, 0x1000, 0x4000,
    ]);
    for index in [1, 3, 5, 7] {
        states[index].voices[0].control.waveform.pulse = false;
        states[index].voices[0].control.waveform.noise = true;
    }
    let effects = detect_effects(
        &empty_trace_for(&states),
        &states,
        EffectThresholds::default(),
    );
    assert!(!effects.iter().any(|effect| {
        matches!(
            effect.effect,
            Effect::Arpeggio | Effect::Vibrato | Effect::Portamento
        )
    }));
}

#[test]
fn classifies_detune_octave_and_delayed_echo_voice_relations() {
    let paired = |second_freq: u16| {
        (0..5)
            .map(|frame| {
                let mut state = frame_v1(frame, pulse_voice(0x1000, 0x800, true, true));
                state.voices[1] = pulse_voice(second_freq, 0x800, true, true);
                state
            })
            .collect::<Vec<_>>()
    };
    let detune = detect_voice_relations(&paired(0x1010), EffectThresholds::default());
    assert!(
        detune
            .iter()
            .any(|relation| relation.relation == VoiceRelation::Detune)
    );
    let octave = detect_voice_relations(&paired(0x2000), EffectThresholds::default());
    assert!(
        octave
            .iter()
            .any(|relation| relation.relation == VoiceRelation::Octave)
    );

    let mut echo = paired(0x1000);
    echo[0].voices[1].control.gate = false;
    echo[1].voices[1].control.gate = false;
    let echo = detect_voice_relations(&echo, EffectThresholds::default());
    assert!(echo.iter().any(|relation| {
        relation.relation == VoiceRelation::Echo
            && relation.source == VoiceId::V1
            && relation.destination == VoiceId::V2
    }));
}

#[test]
fn detects_portamento_for_monotonic_freq_slide() {
    // Gate held, frequency slides up by ~70 cents over 5 frames.
    let states = pitch_seq(&[0x1000, 0x1020, 0x1040, 0x1060, 0x1080, 0x10A0]);
    let trace = empty_trace_for(&states);
    let effects = detect_effects(&trace, &states, EffectThresholds::default());

    let porto: Vec<_> = effects
        .iter()
        .filter(|e| e.effect == Effect::Portamento)
        .collect();
    assert_eq!(porto.len(), 1, "expected one Portamento: {effects:?}");
    assert_eq!(porto[0].voice, Some(VoiceId(1)));
    assert_eq!(porto[0].start_frame, FrameIndex(0));
    assert_eq!(porto[0].end_frame, FrameIndex(5));
}

#[test]
fn portamento_requires_min_total_cents() {
    // Slides 3 steps of 1 SID unit — tiny pitch change (< 50 cents).
    let states = pitch_seq(&[0x1000, 0x1001, 0x1002, 0x1003, 0x1004, 0x1005]);
    let trace = empty_trace_for(&states);
    let effects = detect_effects(&trace, &states, EffectThresholds::default());

    assert!(
        !effects.iter().any(|e| e.effect == Effect::Portamento),
        "tiny pitch change should not register as Portamento: {effects:?}"
    );
}

#[test]
fn portamento_only_runs_within_held_gate() {
    // Frequency keeps climbing across a gate-off frame — two separate gate
    // regions, not a single portamento.
    let states: Vec<FrameState> = (0..10)
        .map(|n| {
            let gate = n != 4; // gate falls at frame 4
            frame_v1(n, pulse_voice(0x1000 + (n as u16) * 0x20, 0, true, gate))
        })
        .collect();
    let trace = empty_trace_for(&states);
    let effects = detect_effects(&trace, &states, EffectThresholds::default());

    // At most two portamento spans; neither should cross frame 4.
    for span in effects.iter().filter(|e| e.effect == Effect::Portamento) {
        assert!(
            !(span.start_frame.0 <= 4 && span.end_frame.0 > 4),
            "Portamento span should not cross gate-off at frame 4: {span:?}"
        );
    }
}

#[test]
fn portamento_tolerates_held_frames_mid_slide() {
    // A slide that pauses one frame between steps (driver updates pitch every
    // other frame). The single held frames must not fragment the run below
    // the 4-frame minimum.
    let states = pitch_seq(&[
        0x1000, 0x1000, 0x1030, 0x1030, 0x1060, 0x1060, 0x1090, 0x1090,
    ]);
    let trace = empty_trace_for(&states);
    let effects = detect_effects(&trace, &states, EffectThresholds::default());

    let porto: Vec<_> = effects
        .iter()
        .filter(|e| e.effect == Effect::Portamento)
        .collect();
    assert_eq!(
        porto.len(),
        1,
        "single held frames should not split the slide: {effects:?}"
    );
    assert_eq!(porto[0].voice, Some(VoiceId(1)));
}

#[test]
fn portamento_cut_when_holds_exceed_tolerance() {
    // Two real steps, then a long plateau (4 held frames) — exceeds the
    // default tolerance of 1, so the run ends and is too short (< 4 frames)
    // to register as a slide.
    let states = pitch_seq(&[0x1000, 0x1030, 0x1030, 0x1030, 0x1030, 0x1030]);
    let trace = empty_trace_for(&states);
    let effects = detect_effects(&trace, &states, EffectThresholds::default());

    assert!(
        !effects.iter().any(|e| e.effect == Effect::Portamento),
        "a 2-frame slide padded by holds should not register: {effects:?}"
    );
}

#[test]
fn detects_arpeggio_for_4_pitch_cycle() {
    // Four-note (7th-chord) arpeggio: root, 3rd, 5th, 7th, repeating.
    let states = pitch_seq(&[
        0x1000, 0x1300, 0x1700, 0x1A00, 0x1000, 0x1300, 0x1700, 0x1A00,
    ]);
    let trace = empty_trace_for(&states);
    let effects = detect_effects(&trace, &states, EffectThresholds::default());

    let arp: Vec<_> = effects
        .iter()
        .filter(|e| e.effect == Effect::Arpeggio)
        .collect();
    assert_eq!(arp.len(), 1, "expected one 4-note Arpeggio: {effects:?}");
    assert_eq!(arp[0].voice, Some(VoiceId(1)));
}

#[test]
fn detects_vibrato_for_oscillating_freq() {
    // Gate held; frequency wobbles ±0x10 around 0x1000 for 12 frames.
    let states = pitch_seq(&[
        0x1000, 0x1010, 0x1000, 0x1010, 0x1000, 0x1010, 0x1000, 0x1010, 0x1000, 0x1010, 0x1000,
        0x1010,
    ]);
    let trace = empty_trace_for(&states);
    let effects = detect_effects(&trace, &states, EffectThresholds::default());

    let vib: Vec<_> = effects
        .iter()
        .filter(|e| e.effect == Effect::Vibrato)
        .collect();
    assert_eq!(vib.len(), 1, "expected one Vibrato: {effects:?}");
    assert_eq!(vib[0].voice, Some(VoiceId(1)));
}

#[test]
fn vibrato_rejects_wide_excursions() {
    // Oscillation but with peak-to-peak well over 100 cents — looks more
    // like portamento or instrument change, not vibrato.
    let states = pitch_seq(&[
        0x1000, 0x1400, 0x1000, 0x1400, 0x1000, 0x1400, 0x1000, 0x1400,
    ]);
    let trace = empty_trace_for(&states);
    let effects = detect_effects(&trace, &states, EffectThresholds::default());

    assert!(
        !effects.iter().any(|e| e.effect == Effect::Vibrato),
        "wide excursion should not register as Vibrato: {effects:?}"
    );
}

#[test]
fn detects_arpeggio_for_3_pitch_cycle() {
    // Classic chord arpeggio: A, C, E, A, C, E, A, C
    let states = pitch_seq(&[
        0x1000, 0x1300, 0x1700, 0x1000, 0x1300, 0x1700, 0x1000, 0x1300,
    ]);
    let trace = empty_trace_for(&states);
    let effects = detect_effects(&trace, &states, EffectThresholds::default());

    let arp: Vec<_> = effects
        .iter()
        .filter(|e| e.effect == Effect::Arpeggio)
        .collect();
    assert_eq!(arp.len(), 1, "expected one Arpeggio: {effects:?}");
    assert_eq!(arp[0].voice, Some(VoiceId(1)));
}

#[test]
fn arpeggio_rejects_too_many_distinct_pitches() {
    // 5 distinct pitches — more than the max-distinct default (4).
    let states = pitch_seq(&[
        0x1000, 0x1100, 0x1200, 0x1300, 0x1400, 0x1000, 0x1100, 0x1200,
    ]);
    let trace = empty_trace_for(&states);
    let effects = detect_effects(&trace, &states, EffectThresholds::default());

    assert!(
        !effects.iter().any(|e| e.effect == Effect::Arpeggio),
        "5 distinct pitches shouldn't match Arpeggio: {effects:?}"
    );
}

#[test]
fn arpeggio_rejects_static_pitch() {
    // Single repeating pitch — distinct count = 1 < min (2).
    let states: Vec<FrameState> = (0..6)
        .map(|n| frame_v1(n, pulse_voice(0x1000, 0, true, true)))
        .collect();
    let trace = empty_trace_for(&states);
    let effects = detect_effects(&trace, &states, EffectThresholds::default());

    assert!(
        !effects.iter().any(|e| e.effect == Effect::Arpeggio),
        "static pitch shouldn't match Arpeggio: {effects:?}"
    );
}

#[test]
fn flushes_run_open_at_trace_end() {
    // Effect bit stays high through the last frame — should still emit a span.
    let mut on = VoiceState::default();
    on.control.sync = true;
    let states = vec![frame_v1(0, on), frame_v1(1, on)];
    let trace = empty_trace_for(&states);
    let effects = detect_effects(&trace, &states, EffectThresholds::default());

    let syncs: Vec<_> = effects
        .iter()
        .filter(|e| e.effect == Effect::HardSync)
        .collect();
    assert_eq!(syncs.len(), 1);
    assert_eq!(syncs[0].end_frame, FrameIndex(1));
}
