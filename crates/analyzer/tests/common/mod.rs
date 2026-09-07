// Each integration-test file compiles `common` fresh, so helpers used by
// only some of the files trigger dead_code per-file; suppress at module
// scope rather than per-fn.
#![allow(dead_code)]

pub mod oracle;

use sid_analyzer::analysis::FrameState;
use sid_analyzer::analysis::effects::{EffectSpan, EffectThresholds, detect_effects};
use sid_analyzer::analysis::filter::FilterState;
use sid_analyzer::analysis::note::{NoteEvent, detect_notes};
use sid_analyzer::analysis::voice::{ControlBits, PulseWidth, SidFreq, VoiceState, Waveform};
use sid_analyzer::analysis::{SystemClock, analyze};
use sid_analyzer::emu;
use sid_analyzer::header::{self, Header, SubtuneIndex};
use sid_analyzer::trace::{FrameIndex, FrameTrace, Trace};

/// Full analyzer pipeline output for one subtune of a SID file.
/// Used by integration tests that need the post-analysis artifacts
/// without re-implementing the read-parse-emu-analyze-detect dance.
pub struct SidPipeline {
    pub header: Header,
    pub subtune: SubtuneIndex,
    pub clock: SystemClock,
    pub trace: Trace,
    pub states: Vec<FrameState>,
    pub notes: Vec<NoteEvent>,
    pub effects: Vec<EffectSpan>,
}

impl SidPipeline {
    /// Run the full pipeline against `sample_path` for `frames` play
    /// frames. Panics on any failure — fine for tests.
    pub fn run(sample_path: &str, subtune: SubtuneIndex, frames: u32) -> Self {
        let bytes = std::fs::read(sample_path).expect("sample SID file present");
        Self::from_bytes(&bytes, subtune, frames)
    }

    pub fn from_bytes(bytes: &[u8], subtune: SubtuneIndex, frames: u32) -> Self {
        let header = header::parse(bytes).expect("parse header");
        let trace = emu::run(&header, bytes, subtune, frames).expect("emu::run");
        let clock = SystemClock::from(header.flags.clock);
        let states = analyze(&trace);
        let notes = detect_notes(&states, clock);
        let effects = detect_effects(&trace, &states, EffectThresholds::default());
        Self {
            header,
            subtune,
            clock,
            trace,
            states,
            notes,
            effects,
        }
    }

    pub fn frame_count(&self) -> usize {
        self.states.len()
    }

    pub fn analyzed_program(&self) -> sid_analyzer::analysis::sid_program::AnalyzedSidProgram {
        sid_analyzer::analysis::sid_program::AnalyzedSidProgram::from_trace(
            &self.header,
            self.subtune,
            sid_analyzer::emu::PlaybackTiming::vblank(self.clock),
            &self.trace,
        )
    }
}

pub fn synthetic_filtered_saw_sid(clock: header::Clock) -> Vec<u8> {
    const HEADER_LEN: usize = 0x7C;
    const LOAD_ADDRESS: u16 = 0x1000;

    fn store_immediate(code: &mut Vec<u8>, value: u8, address: u16) {
        code.extend_from_slice(&[0xA9, value, 0x8D, address as u8, (address >> 8) as u8]);
    }

    let mut code = Vec::new();
    store_immediate(&mut code, 0x51, 0xD400);
    store_immediate(&mut code, 0x07, 0xD401);
    store_immediate(&mut code, 0x00, 0xD402);
    store_immediate(&mut code, 0x08, 0xD403);
    store_immediate(&mut code, 0x00, 0xD405);
    store_immediate(&mut code, 0xF0, 0xD406);
    store_immediate(&mut code, 0x00, 0xD415);
    store_immediate(&mut code, 0x40, 0xD416);
    store_immediate(&mut code, 0x81, 0xD417);
    store_immediate(&mut code, 0x1F, 0xD418);
    store_immediate(&mut code, 0x21, 0xD404);
    code.push(0x60);
    let play_address = LOAD_ADDRESS + u16::try_from(code.len()).expect("fixture code fits in RAM");
    code.push(0x60);

    let mut sid = vec![0; HEADER_LEN];
    sid[0x00..0x04].copy_from_slice(b"PSID");
    sid[0x04..0x06].copy_from_slice(&2u16.to_be_bytes());
    sid[0x06..0x08].copy_from_slice(&(HEADER_LEN as u16).to_be_bytes());
    sid[0x08..0x0A].copy_from_slice(&LOAD_ADDRESS.to_be_bytes());
    sid[0x0A..0x0C].copy_from_slice(&LOAD_ADDRESS.to_be_bytes());
    sid[0x0C..0x0E].copy_from_slice(&play_address.to_be_bytes());
    sid[0x0E..0x10].copy_from_slice(&1u16.to_be_bytes());
    sid[0x10..0x12].copy_from_slice(&1u16.to_be_bytes());
    sid[0x16..0x2A].copy_from_slice(b"Synthetic filter SID");
    let clock_flag = match clock {
        header::Clock::Pal => 0x0004,
        header::Clock::Ntsc => 0x0008,
        header::Clock::Both => 0x000C,
        header::Clock::Unknown => 0x0000,
    };
    sid[0x76..0x78].copy_from_slice(&(clock_flag | 0x0010u16).to_be_bytes());
    sid.extend_from_slice(&code);
    sid
}

/// Build a `VoiceState` with the given freq, pulse width, pulse-waveform bit,
/// and gate bit (other control bits clear, other waveforms off).
pub fn pulse_voice(freq: u16, pw: u16, pulse: bool, gate: bool) -> VoiceState {
    VoiceState {
        freq: SidFreq(freq),
        pulse_width: PulseWidth(pw),
        control: ControlBits {
            gate,
            waveform: Waveform {
                pulse,
                ..Waveform::default()
            },
            ..ControlBits::default()
        },
        ..VoiceState::default()
    }
}

/// Shortcut for a pulse-waveform voice with the given freq and gate, PW=0.
pub fn pulse_gated(freq: u16, gate: bool) -> VoiceState {
    pulse_voice(freq, 0, true, gate)
}

/// Build a single-voice `FrameState` with the given V1 state, silent V2/V3,
/// and a custom filter.
pub fn frame_v1_filtered(n: u32, v1: VoiceState, filter: FilterState) -> FrameState {
    FrameState {
        frame: FrameIndex(n),
        voices: [v1, VoiceState::default(), VoiceState::default()],
        filter,
        ..FrameState::default()
    }
}

/// Shortcut for a single-voice `FrameState` with default filter.
pub fn frame_v1(n: u32, v1: VoiceState) -> FrameState {
    frame_v1_filtered(n, v1, FilterState::default())
}

/// Build a sequence of frames where V1 plays a pulse waveform with the
/// given frequency at each frame, gate held throughout.
pub fn pitch_seq(freqs: &[u16]) -> Vec<FrameState> {
    freqs
        .iter()
        .enumerate()
        .map(|(i, &f)| frame_v1(i as u32, pulse_voice(f, 0, true, true)))
        .collect()
}

/// Build a `Trace` matching the given states with empty FrameTrace writes.
pub fn empty_trace_for(states: &[FrameState]) -> Trace {
    Trace {
        init_writes: Vec::new(),
        frames: states
            .iter()
            .map(|s| FrameTrace {
                frame: s.frame,
                writes: Vec::new(),
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    }
}
