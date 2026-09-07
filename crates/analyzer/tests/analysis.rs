use sid_analyzer::analysis::filter::{Cutoff, FilterRouting, FilterState, Resonance};
use sid_analyzer::analysis::voice::{Adsr, ControlBits, PulseWidth, SidFreq, VoiceState, Waveform};
use sid_analyzer::analysis::{Hertz, SystemClock, Volume, analyze};
use sid_analyzer::header::SidModel;
use sid_analyzer::trace::{
    FrameIndex, FrameTrace, RegisterRead, RegisterWrite, SidRegister, SubFrameOffset, Trace,
};

fn write(reg: u8, value: u8, step: u32) -> RegisterWrite {
    RegisterWrite {
        reg: SidRegister(reg),
        value,
        offset: SubFrameOffset(step),
    }
}

#[test]
fn voice_state_decodes_freq_pulse_control_adsr() {
    // V1: freq = 0x1234, PW = 0xABC, CR = 0x41 (pulse + gate),
    // AD = 0x59 (A=5,D=9), SR = 0xC3 (S=C,R=3).
    let regs: [u8; 7] = [0x34, 0x12, 0xBC, 0x0A, 0x41, 0x59, 0xC3];
    let v = VoiceState::from_regs(&regs);
    assert_eq!(v.freq, SidFreq(0x1234));
    assert_eq!(v.pulse_width, PulseWidth(0x0ABC));
    assert_eq!(
        v.control,
        ControlBits {
            gate: true,
            sync: false,
            ring_mod: false,
            test: false,
            waveform: Waveform {
                triangle: false,
                sawtooth: false,
                pulse: true,
                noise: false,
            },
        }
    );
    assert_eq!(
        v.adsr,
        Adsr {
            attack: 5,
            decay: 9,
            sustain: 0xC,
            release: 3,
        }
    );
}

#[test]
fn pulse_width_high_nibble_is_masked_off() {
    // PWHI's upper nibble is unused; ensure it doesn't leak into the value.
    let regs: [u8; 7] = [0x00, 0x00, 0x00, 0xFA, 0x00, 0x00, 0x00];
    assert_eq!(VoiceState::from_regs(&regs).pulse_width, PulseWidth(0x0A00));
}

#[test]
fn waveform_all_bits_decode() {
    let regs: [u8; 7] = [0, 0, 0, 0, 0xF0, 0, 0];
    let wf = VoiceState::from_regs(&regs).control.waveform;
    assert!(wf.triangle && wf.sawtooth && wf.pulse && wf.noise);
}

#[test]
fn filter_state_decodes_cutoff_resonance_routing_mode() {
    // FCLO = 0x07 (only low 3 bits used), FCHI = 0xA5,
    // RES_FILT = 0x83 (res=8, filter V1+V2), MODE_VOL = 0x3F (LP+BP+vol=F).
    let regs: [u8; 4] = [0x07, 0xA5, 0x83, 0x3F];
    let s = FilterState::from_regs(&regs);
    assert_eq!(s.cutoff, Cutoff((0xA5 << 3) | 0x07));
    assert_eq!(s.resonance, Resonance(8));
    assert_eq!(
        s.routing,
        FilterRouting {
            voice1: true,
            voice2: true,
            voice3: false,
            external: false,
        }
    );
    assert!(s.mode.low_pass);
    assert!(s.mode.band_pass);
    assert!(!s.mode.high_pass);
    assert!(!s.mode.voice3_off);
}

#[test]
fn fclo_ignores_upper_5_bits() {
    let regs: [u8; 4] = [0xFF, 0x00, 0, 0];
    assert_eq!(FilterState::from_regs(&regs).cutoff, Cutoff(0x007));
}

#[test]
fn sid_freq_to_hertz_at_a440_in_pal() {
    // 440 Hz · 2^24 / 985_248 ≈ 7491.6 → SID freq value 0x1D44.
    let hz = SidFreq(0x1D44).to_hertz(SystemClock::Pal);
    assert!((hz.0 - 440.0).abs() < 0.5, "got {}", hz.0);
}

#[test]
fn analyze_replays_writes_into_per_frame_snapshots() {
    // Frame 0: write V1 freq lo/hi to $1234 and gate the pulse waveform.
    // Frame 1: bump V1 freq to $1500, drop gate.
    let trace = Trace {
        init_writes: Vec::new(),
        frames: vec![
            FrameTrace {
                frame: FrameIndex(0),
                reads: vec![RegisterRead {
                    reg: SidRegister(0x1B),
                    value: 0x55,
                    offset: SubFrameOffset(0),
                }],
                writes: vec![
                    write(0x00, 0x34, 1),
                    write(0x01, 0x12, 2),
                    write(0x04, 0x41, 3), // pulse + gate
                    write(0x18, 0x0F, 4), // master volume
                ],
                ..Default::default()
            },
            FrameTrace {
                frame: FrameIndex(1),
                start_cycle: sid_analyzer::trace::ChipCycle(10),
                writes: vec![
                    write(0x00, 0x00, 1),
                    write(0x01, 0x15, 2),
                    write(0x04, 0x40, 3), // pulse, no gate
                ],
                ..Default::default()
            },
        ],
        ..Default::default()
    };

    let states = analyze(&trace);
    assert_eq!(states.len(), 2);

    assert_eq!(states[0].voices[0].freq, SidFreq(0x1234));
    assert!(states[0].voices[0].control.gate);
    assert_eq!(states[0].volume, Volume(0x0F));
    assert_eq!(states[0].register_reads, trace.frames[0].reads);
    assert_eq!(states[0].register_writes, trace.frames[0].writes);

    assert_eq!(states[1].voices[0].freq, SidFreq(0x1500));
    assert!(!states[1].voices[0].control.gate);
    // Volume carried over from frame 0 (no write in frame 1).
    assert_eq!(states[1].volume, Volume(0x0F));
}

#[test]
fn writes_above_d41c_are_ignored_in_analysis() {
    // RegisterWrite::reg cannot exceed SID_REGISTER_LAST in practice (the bus
    // doesn't trap higher addresses), but analyze() should still be robust.
    let trace = Trace {
        init_writes: Vec::new(),
        frames: vec![FrameTrace {
            frame: FrameIndex(0),
            writes: vec![write(0xFF, 0xAB, 1)],
            ..Default::default()
        }],
        ..Default::default()
    };
    let states = analyze(&trace);
    assert_eq!(states.len(), 1);
    assert_eq!(states[0].voices[0].freq, SidFreq(0));
}

#[test]
fn hertz_default_is_zero() {
    assert_eq!(Hertz::default(), Hertz(0.0));
}

#[test]
fn envelope_replay_reports_snapshot_activity_and_ordered_events() {
    use sid_analyzer::emu::sid::{EnvLevel, EnvPhase, EnvelopeEventKind};
    use sid_analyzer::trace::{ChipCycle, CpuCycles};

    let trace = Trace {
        frames: vec![FrameTrace {
            frame: FrameIndex(0),
            start_cycle: ChipCycle(6038),
            end_cycle: ChipCycle(6128),
            duration: CpuCycles(9),
            writes: vec![write(0x04, 0x41, 0)],
            ..Default::default()
        }],
        ..Default::default()
    };
    let states = analyze(&trace);
    let digital = &states[0].digital_voices[0];
    assert_eq!(digital.envelope.level, EnvLevel(10));
    assert_eq!(digital.envelope.phase, EnvPhase::Attack);
    assert_eq!(digital.envelope_activity.start_level, EnvLevel(0));
    assert_eq!(digital.envelope_activity.end_level, EnvLevel(10));
    assert_eq!(digital.envelope_activity.peak_level, EnvLevel(10));
    assert_eq!(digital.envelope_activity.active_cycles, CpuCycles(86));
    assert_eq!(
        digital.envelope_activity.first_nonzero,
        Some(SubFrameOffset(4))
    );
    assert_eq!(
        digital
            .envelope_activity
            .events
            .iter()
            .map(|event| event.kind)
            .collect::<Vec<_>>(),
        vec![
            EnvelopeEventKind::EnteredAttack,
            EnvelopeEventKind::LeftZero
        ]
    );
}

#[test]
fn analysis_replays_test_hold_with_the_trace_sid_model() {
    use sid_analyzer::trace::{ChipCycle, CpuCycles};

    let trace_for = |sid_model| Trace {
        sid_model,
        init_writes: vec![write(0x12, 0x88, 0)],
        frames: vec![FrameTrace {
            frame: FrameIndex(0),
            end_cycle: ChipCycle(50_000),
            duration: CpuCycles(50_000),
            ..Default::default()
        }],
        ..Default::default()
    };

    let state_6581 = analyze(&trace_for(SidModel::Mos6581));
    let state_8580 = analyze(&trace_for(SidModel::Mos8580));
    assert_eq!(
        state_6581[0].digital_voices[2]
            .oscillator
            .noise_shift_register,
        0x7F_FFFF
    );
    assert_eq!(
        state_8580[0].digital_voices[2]
            .oscillator
            .noise_shift_register,
        0x3F_FFFF
    );
}
