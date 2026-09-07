use super::FrameState;
use super::note::NoteEvent;
use crate::emu::sid::{EnvLevel, EnvPhase};
use crate::trace::{FrameIndex, SidRegister, SubFrameOffset};
use serde::Serialize;
use std::collections::BTreeMap;

const VOICE_STRIDE: u8 = 7;
const CONTROL_OFFSET: u8 = 4;
const AD_OFFSET: u8 = 5;
const SR_OFFSET: u8 = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
#[must_use]
pub enum OnsetEventKind {
    Frequency,
    PulseWidth,
    Control,
    AttackDecay,
    SustainRelease,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[must_use]
pub struct OnsetEvent {
    pub offset: SubFrameOffset,
    pub kind: OnsetEventKind,
    pub value: u8,
    pub before_gate: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
#[must_use]
pub enum OnsetClass {
    Plain,
    HardRestart,
    SyncAttack,
    NoiseClick,
    TestRecovery,
    WaveformTransient,
    Multiple,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
#[must_use]
pub enum OscillatorStart {
    FreeRunning,
    PhaseContinued,
    SyncReset,
    TestReset,
    SequenceRestart,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
#[must_use]
pub enum EnvelopeRecipe {
    NormalAdsr,
    RateCarry,
    DelayBug,
    RetriggerWithoutReset,
    HardRestart,
    MeasuredOnly,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Default)]
#[serde(rename_all = "snake_case")]
#[must_use]
pub enum FilterProgram {
    Static,
    Ramp,
    Triangle,
    SteppedTable,
    EnvelopeShaped,
    Periodic,
    Irregular,
    #[default]
    Inactive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
#[serde(transparent)]
#[must_use]
pub struct FilterRoutingMask(pub u8);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
#[serde(transparent)]
#[must_use]
pub struct FilterModeMask(pub u8);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[must_use]
pub struct FilterProgramPoint {
    pub frame: FrameIndex,
    pub cutoff: super::filter::Cutoff,
    pub resonance: super::filter::Resonance,
    pub routing_mask: FilterRoutingMask,
    pub mode_mask: FilterModeMask,
    pub volume: super::Volume,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Default)]
#[must_use]
pub struct ChipFilterProgram {
    pub kind: FilterProgram,
    pub points: Vec<FilterProgramPoint>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
#[must_use]
pub enum CrossVoiceKind {
    Sync,
    Ring,
    SyncAndRing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
#[must_use]
pub enum ModulatorRole {
    Audible,
    SilentModulator,
    Shared,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[must_use]
pub struct CrossVoiceEvidence {
    pub source: super::VoiceId,
    pub destination: super::VoiceId,
    pub kind: CrossVoiceKind,
    pub role: ModulatorRole,
    pub active_calls: u32,
    pub configured_inactive_calls: u32,
    pub source_frequency: super::voice::SidFreq,
    pub source_frequency_stable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[must_use]
pub struct NoteProgram {
    pub voice: super::VoiceId,
    pub start: FrameIndex,
    pub onset: OnsetClass,
    pub onset_events: Vec<OnsetEvent>,
    pub oscillator_start: OscillatorStart,
    pub envelope: EnvelopeRecipe,
    pub cross_voice: Option<CrossVoiceEvidence>,
    pub exact: bool,
}

#[derive(Debug, Clone, Default, Serialize)]
#[must_use]
pub struct ProgramCensus {
    pub onsets: BTreeMap<OnsetClass, u64>,
    pub onset_events: u64,
    pub pre_gate_events: u64,
    pub oscillator_starts: BTreeMap<OscillatorStart, u64>,
    pub envelope_recipes: BTreeMap<EnvelopeRecipe, u64>,
    pub cross_voice: BTreeMap<CrossVoiceKind, u64>,
    pub modulator_roles: BTreeMap<ModulatorRole, u64>,
    pub filter: ChipFilterProgram,
    pub oscillator_restart_native: u64,
    pub oscillator_phase_fallback: u64,
    pub envelope_recipe_native: u64,
    pub envelope_measured_fallback: u64,
    pub cross_voice_native: u64,
    pub cross_voice_fallback: u64,
    pub chip_filter_shared_native: bool,
    pub chip_filter_module_fallback: bool,
    pub inexact_notes: u64,
}

pub fn analyze_programs(notes: &[&NoteEvent], states: &[FrameState]) -> ProgramCensus {
    let mut census = ProgramCensus::default();
    for &note in notes {
        let program = analyze_note(note, states);
        *census.onsets.entry(program.onset).or_default() += 1;
        census.onset_events += program.onset_events.len() as u64;
        census.pre_gate_events += program
            .onset_events
            .iter()
            .filter(|event| event.before_gate)
            .count() as u64;
        *census
            .oscillator_starts
            .entry(program.oscillator_start)
            .or_default() += 1;
        match program.oscillator_start {
            OscillatorStart::SyncReset
            | OscillatorStart::TestReset
            | OscillatorStart::SequenceRestart => census.oscillator_restart_native += 1,
            OscillatorStart::FreeRunning
            | OscillatorStart::PhaseContinued
            | OscillatorStart::Unknown => census.oscillator_phase_fallback += 1,
        }
        *census.envelope_recipes.entry(program.envelope).or_default() += 1;
        match program.envelope {
            EnvelopeRecipe::NormalAdsr => census.envelope_recipe_native += 1,
            _ => census.envelope_measured_fallback += 1,
        }
        if let Some(evidence) = program.cross_voice {
            *census.cross_voice.entry(evidence.kind).or_default() += 1;
            match (evidence.role, evidence.source_frequency_stable) {
                (ModulatorRole::SilentModulator, true) => census.cross_voice_native += 1,
                _ => census.cross_voice_fallback += 1,
            }
            *census.modulator_roles.entry(evidence.role).or_default() += 1;
        }
        census.inexact_notes += u64::from(!program.exact);
    }
    census.filter = build_filter_program(states);
    census.chip_filter_module_fallback = census.filter.kind != FilterProgram::Inactive;
    census
}

pub fn analyze_note(note: &NoteEvent, states: &[FrameState]) -> NoteProgram {
    let voice = note.voice.to_index();
    let start = note.start_frame.0 as usize;
    let state = states.get(start);
    let events = state.map_or_else(Vec::new, |frame| onset_events(frame, voice));
    let onset = state.map_or(OnsetClass::Unknown, |frame| {
        classify_onset(frame, voice, &events)
    });
    let oscillator_start = state.map_or(OscillatorStart::Unknown, |frame| {
        classify_oscillator_start(frame, voice, onset)
    });
    let envelope = classify_envelope(note, states, onset);
    let cross_voice = classify_cross_voice(note, states);
    NoteProgram {
        voice: note.voice,
        start: note.start_frame,
        onset,
        onset_events: events,
        oscillator_start,
        envelope,
        cross_voice,
        exact: state.is_some_and(|frame| frame.digital_state_exact),
    }
}

fn onset_events(state: &FrameState, voice: usize) -> Vec<OnsetEvent> {
    let base = voice as u8 * VOICE_STRIDE;
    let control = SidRegister(base + CONTROL_OFFSET);
    let gate_offset = state
        .register_writes
        .iter()
        .find(|write| write.reg == control && write.value & 1 != 0)
        .map(|write| write.offset);
    state
        .register_writes
        .iter()
        .filter_map(|write| {
            let relative = write.reg.0.checked_sub(base)?;
            let kind = match relative {
                0 | 1 => OnsetEventKind::Frequency,
                2 | 3 => OnsetEventKind::PulseWidth,
                CONTROL_OFFSET => OnsetEventKind::Control,
                AD_OFFSET => OnsetEventKind::AttackDecay,
                SR_OFFSET => OnsetEventKind::SustainRelease,
                _ => return None,
            };
            Some(OnsetEvent {
                offset: write.offset,
                kind,
                value: write.value,
                before_gate: gate_offset.is_some_and(|gate| write.offset < gate),
            })
        })
        .collect()
}

fn classify_onset(state: &FrameState, voice: usize, events: &[OnsetEvent]) -> OnsetClass {
    let control_values: Vec<u8> = events
        .iter()
        .filter(|event| event.kind == OnsetEventKind::Control)
        .map(|event| event.value)
        .collect();
    let mut classes = Vec::new();
    if state.hard_restart[voice] {
        classes.push(OnsetClass::HardRestart);
    }
    if state.voices[voice].control.sync && state.digital_voices[voice].oscillator.sync_resets > 0 {
        classes.push(OnsetClass::SyncAttack);
    }
    if control_values
        .windows(2)
        .any(|pair| pair[0] & 0x08 != 0 && pair[1] & 0x08 == 0)
    {
        classes.push(OnsetClass::TestRecovery);
    }
    if state.voices[voice].control.waveform.noise {
        classes.push(OnsetClass::NoiseClick);
    }
    let waveforms: Vec<u8> = control_values.iter().map(|value| value & 0xF0).collect();
    if waveforms.windows(2).any(|pair| pair[0] != pair[1]) {
        classes.push(OnsetClass::WaveformTransient);
    }
    classes.sort_unstable();
    classes.dedup();
    match classes.as_slice() {
        [] if control_values.iter().any(|value| value & 1 != 0) => OnsetClass::Plain,
        [] => OnsetClass::Unknown,
        [only] => *only,
        _ => OnsetClass::Multiple,
    }
}

fn classify_oscillator_start(
    state: &FrameState,
    voice: usize,
    onset: OnsetClass,
) -> OscillatorStart {
    let digital = &state.digital_voices[voice];
    if matches!(onset, OnsetClass::TestRecovery | OnsetClass::Multiple)
        && state.register_writes.iter().any(|write| {
            write.reg.0 == voice as u8 * VOICE_STRIDE + CONTROL_OFFSET && write.value & 8 == 0
        })
    {
        return OscillatorStart::TestReset;
    }
    if digital.oscillator.sync_resets > 0 {
        return OscillatorStart::SyncReset;
    }
    if matches!(onset, OnsetClass::WaveformTransient) {
        return OscillatorStart::SequenceRestart;
    }
    if digital.envelope_start.gate {
        return OscillatorStart::PhaseContinued;
    }
    if digital.oscillator_start.accumulator != 0 {
        return OscillatorStart::FreeRunning;
    }
    OscillatorStart::Unknown
}

fn classify_envelope(note: &NoteEvent, states: &[FrameState], onset: OnsetClass) -> EnvelopeRecipe {
    let voice = note.voice.to_index();
    let Some(first) = states.get(note.start_frame.0 as usize) else {
        return EnvelopeRecipe::Unknown;
    };
    if !first.digital_state_exact {
        return EnvelopeRecipe::MeasuredOnly;
    }
    if matches!(onset, OnsetClass::HardRestart | OnsetClass::Multiple) {
        return EnvelopeRecipe::HardRestart;
    }
    let snapshot = first.digital_voices[voice].envelope_start;
    if snapshot.gate && snapshot.phase == EnvPhase::Release {
        return EnvelopeRecipe::RetriggerWithoutReset;
    }
    if snapshot.rate_counter.0 != 0 || snapshot.exponential_counter.0 != 0 {
        return EnvelopeRecipe::RateCarry;
    }
    let activity = &first.digital_voices[voice].envelope_activity;
    if activity
        .events
        .iter()
        .any(|event| event.kind == crate::emu::sid::EnvelopeEventKind::EnteredAttack)
        && activity.first_nonzero.is_none()
        && activity.end_level == EnvLevel(0)
    {
        return EnvelopeRecipe::DelayBug;
    }
    if activity
        .events
        .iter()
        .any(|event| event.kind == crate::emu::sid::EnvelopeEventKind::EnteredAttack)
    {
        EnvelopeRecipe::NormalAdsr
    } else {
        EnvelopeRecipe::MeasuredOnly
    }
}

fn classify_cross_voice(note: &NoteEvent, states: &[FrameState]) -> Option<CrossVoiceEvidence> {
    let destination = note.voice.to_index();
    let source = (destination + 2) % 3;
    let mut sync = false;
    let mut ring = false;
    let mut active_calls = 0u32;
    let mut configured_inactive_calls = 0u32;
    let mut source_audible = false;
    let mut source_shared = false;
    let mut frequencies = Vec::new();
    for state in &states[note.frame_range(states.len())] {
        let destination_voice = state.voices[destination];
        let source_voice = state.voices[source];
        let source_oscillator = state.digital_voices[source].oscillator;
        let sync_active = destination_voice.control.sync
            && state.digital_voices[destination].oscillator.sync_resets > 0;
        let ring_active = destination_voice.control.ring_mod
            && destination_voice.control.waveform.triangle
            && (source_oscillator.source_msb_edges > 0
                || source_oscillator.accumulator & 0x0080_0000 != 0);
        sync |= sync_active;
        ring |= ring_active;
        active_calls += u32::from(sync_active || ring_active);
        configured_inactive_calls += u32::from(
            (destination_voice.control.sync || destination_voice.control.ring_mod)
                && !sync_active
                && !ring_active,
        );
        let audible = source_voice.control.gate
            && !source_voice.control.waveform.is_silent()
            && !(source == 2 && state.filter.mode.voice3_off);
        source_audible |= audible;
        source_shared |= audible && (source_voice.control.sync || source_voice.control.ring_mod);
        if source_voice.freq.0 > 0 {
            frequencies.push(source_voice.freq.0);
        }
    }
    let kind = match (sync, ring) {
        (true, true) => Some(CrossVoiceKind::SyncAndRing),
        (true, false) => Some(CrossVoiceKind::Sync),
        (false, true) => Some(CrossVoiceKind::Ring),
        (false, false) => None,
    }?;
    frequencies.sort_unstable();
    let source_frequency_stable = frequencies
        .first()
        .zip(frequencies.last())
        .is_some_and(|(first, last)| first == last);
    let source_frequency =
        super::voice::SidFreq(frequencies.get(frequencies.len() / 2).copied().unwrap_or(0));
    let role = if source_shared {
        ModulatorRole::Shared
    } else if source_audible {
        ModulatorRole::Audible
    } else if source_frequency.0 > 0 {
        ModulatorRole::SilentModulator
    } else {
        ModulatorRole::Unknown
    };
    Some(CrossVoiceEvidence {
        source: super::VoiceId::from_index(source),
        destination: note.voice,
        kind,
        role,
        active_calls,
        configured_inactive_calls,
        source_frequency,
        source_frequency_stable,
    })
}

pub fn classify_filter_program(states: &[FrameState]) -> FilterProgram {
    let routed: Vec<&FrameState> = states
        .iter()
        .filter(|state| state.filter.routing.any())
        .collect();
    if routed.is_empty() {
        return FilterProgram::Inactive;
    }
    let values: Vec<i32> = routed
        .iter()
        .map(|state| i32::from(state.filter.cutoff.0))
        .collect();
    if values.windows(2).all(|pair| pair[0] == pair[1]) {
        return FilterProgram::Static;
    }
    let signs: Vec<i8> = values
        .windows(2)
        .filter_map(|pair| (pair[1] - pair[0]).signum().try_into().ok())
        .filter(|sign: &i8| *sign != 0)
        .collect();
    let reversals = signs.windows(2).filter(|pair| pair[0] != pair[1]).count();
    if reversals == 0 {
        return FilterProgram::Ramp;
    }
    if reversals == 1 {
        return FilterProgram::Triangle;
    }
    let mut unique = values.clone();
    unique.sort_unstable();
    unique.dedup();
    if unique.len() <= 8 {
        return FilterProgram::SteppedTable;
    }
    if values.len() >= 8 {
        for period in 2..=values.len() / 2 {
            if values[period..]
                .iter()
                .zip(&values[..values.len() - period])
                .all(|(left, right)| left == right)
            {
                return FilterProgram::Periodic;
            }
        }
    }
    let envelope_like = (0..3).any(|voice| {
        let pairs: Vec<(i32, i32)> = routed
            .iter()
            .map(|state| {
                (
                    i32::from(state.filter.cutoff.0),
                    i32::from(state.digital_voices[voice].envelope.level.0),
                )
            })
            .collect();
        let agreeing = pairs
            .windows(2)
            .filter(|pair| {
                let cutoff = pair[1].0 - pair[0].0;
                let envelope = pair[1].1 - pair[0].1;
                cutoff != 0 && envelope != 0 && cutoff.signum() == envelope.signum()
            })
            .count();
        agreeing * 4 >= pairs.len().saturating_sub(1) * 3
    });
    if envelope_like {
        FilterProgram::EnvelopeShaped
    } else {
        FilterProgram::Irregular
    }
}

pub fn build_filter_program(states: &[FrameState]) -> ChipFilterProgram {
    let mut points = Vec::new();
    let mut previous: Option<FilterProgramPoint> = None;
    for state in states {
        let routing = state.filter.routing;
        let routing_mask = FilterRoutingMask(
            u8::from(routing.voice1)
                | (u8::from(routing.voice2) << 1)
                | (u8::from(routing.voice3) << 2)
                | (u8::from(routing.external) << 3),
        );
        let mode = state.filter.mode;
        let mode_mask = FilterModeMask(
            u8::from(mode.low_pass)
                | (u8::from(mode.band_pass) << 1)
                | (u8::from(mode.high_pass) << 2)
                | (u8::from(mode.voice3_off) << 3),
        );
        let point = FilterProgramPoint {
            frame: state.frame,
            cutoff: state.filter.cutoff,
            resonance: state.filter.resonance,
            routing_mask,
            mode_mask,
            volume: state.volume,
        };
        if previous.is_none_or(|last| {
            last.cutoff != point.cutoff
                || last.resonance != point.resonance
                || last.routing_mask != point.routing_mask
                || last.mode_mask != point.mode_mask
                || last.volume != point.volume
        }) {
            points.push(point);
            previous = Some(point);
        }
    }
    ChipFilterProgram {
        kind: classify_filter_program(states),
        points,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::filter::{Cutoff, FilterRouting};
    use crate::analysis::voice::{ControlBits, SidFreq, Waveform};
    use crate::trace::{RegisterWrite, SubFrameOffset};

    #[test]
    fn classifies_pre_gate_hard_restart_as_multiple_when_noise_is_present() {
        let mut state = FrameState {
            digital_state_exact: true,
            ..FrameState::default()
        };
        state.hard_restart[0] = true;
        state.voices[0].control = ControlBits {
            gate: true,
            waveform: Waveform {
                noise: true,
                ..Waveform::default()
            },
            ..ControlBits::default()
        };
        state.register_writes = vec![
            RegisterWrite {
                reg: SidRegister(0),
                value: 1,
                offset: SubFrameOffset(2),
            },
            RegisterWrite {
                reg: SidRegister(4),
                value: 0,
                offset: SubFrameOffset(3),
            },
            RegisterWrite {
                reg: SidRegister(4),
                value: 0x81,
                offset: SubFrameOffset(7),
            },
        ];
        let events = onset_events(&state, 0);
        assert_eq!(classify_onset(&state, 0, &events), OnsetClass::Multiple);
        assert!(events[0].before_gate);
    }

    #[test]
    fn filter_classifier_distinguishes_static_ramp_and_triangle() {
        let make = |cutoff| {
            let mut state = FrameState::default();
            state.filter.cutoff = Cutoff(cutoff);
            state.filter.routing = FilterRouting {
                voice1: true,
                ..FilterRouting::default()
            };
            state
        };
        assert_eq!(
            classify_filter_program(&[make(10), make(10)]),
            FilterProgram::Static
        );
        assert_eq!(
            classify_filter_program(&[make(10), make(20), make(30)]),
            FilterProgram::Ramp
        );
        assert_eq!(
            classify_filter_program(&[make(10), make(20), make(10)]),
            FilterProgram::Triangle
        );
    }

    #[test]
    fn chip_filter_program_keeps_global_routing_and_mode_boundaries() {
        let mut first = FrameState {
            frame: FrameIndex(4),
            ..FrameState::default()
        };
        first.filter.cutoff = Cutoff(100);
        first.filter.routing.voice1 = true;
        first.filter.mode.low_pass = true;
        let mut held = first.clone();
        held.frame = FrameIndex(5);
        let mut changed = held.clone();
        changed.frame = FrameIndex(6);
        changed.filter.routing.voice1 = false;
        changed.filter.routing.voice2 = true;
        changed.filter.mode.low_pass = false;
        changed.filter.mode.band_pass = true;
        let program = build_filter_program(&[first, held, changed]);
        assert_eq!(program.points.len(), 2);
        assert_eq!(program.points[0].routing_mask, FilterRoutingMask(1));
        assert_eq!(program.points[1].routing_mask, FilterRoutingMask(2));
        assert_eq!(program.points[1].mode_mask, FilterModeMask(2));
    }

    #[test]
    fn sync_uses_previous_physical_voice_as_source() {
        use crate::analysis::note::{Cents, GmProgram, MidiNote, Velocity};
        let mut state = FrameState::default();
        state.voices[0].control.sync = true;
        state.digital_voices[0].oscillator.sync_resets = 1;
        state.voices[2].freq = SidFreq(1000);
        state.digital_voices[2].oscillator.source_msb_edges = 1;
        let note = NoteEvent {
            voice: crate::analysis::VoiceId::V1,
            start_frame: FrameIndex(0),
            end_frame: Some(FrameIndex(1)),
            midi: MidiNote(60),
            cents: Cents(0.0),
            program: GmProgram::SQUARE_LEAD,
            velocity: Velocity(100),
        };
        let evidence = classify_cross_voice(&note, &[state]).expect("sync evidence");
        assert_eq!(evidence.source, crate::analysis::VoiceId::V3);
        assert_eq!(evidence.destination, crate::analysis::VoiceId::V1);
        assert_eq!(evidence.kind, CrossVoiceKind::Sync);
        assert_eq!(evidence.role, ModulatorRole::SilentModulator);
        assert_eq!(evidence.active_calls, 1);
        assert!(evidence.source_frequency_stable);
    }
}
