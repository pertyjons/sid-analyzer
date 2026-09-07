use super::AnalyzedSidProgram;
use super::signal::SidRegisterFile;
use super::time::SourceSpan;
use crate::analysis::filter::FilterState;
use crate::analysis::voice::VoiceState;
use crate::analysis::{DigitalVoiceState, Volume};
use crate::emu::capture::{
    CapturedSidExecution, CheckpointRef, SidAddressClass, SidBusAccess, SidCallId, SidChipId,
};
use crate::emu::sid::{
    DigitalSid, DigitalSidCheckpoint, EnvelopeFrameActivity, EnvelopeSnapshot, OscillatorSnapshot,
};
use crate::trace::{ChipCycle, FrameIndex};
use std::collections::BTreeMap;

/// Cached physical state at one source sampling boundary.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[must_use]
pub struct ProgramFrame {
    pub frame: FrameIndex,
    pub call_duration: crate::trace::CpuCycles,
    pub voices: [VoiceState; 3],
    pub filter: FilterState,
    pub volume: Volume,
    pub envelope_retrigger: [bool; 3],
    pub hard_restart: [bool; 3],
    pub digital_voices: [DigitalVoiceState; 3],
    pub digital_state_exact: bool,
    pub register_writes: Vec<crate::trace::RegisterWrite>,
    pub register_reads: Vec<crate::trace::RegisterRead>,
}

const VOICE_BASES: [usize; 3] = [0x00, 0x07, 0x0e];
const CONTROL_OFFSET: usize = 4;

#[derive(Debug, Clone, Default)]
pub(crate) struct ProgramIndexes {
    calls: BTreeMap<FrameIndex, usize>,
    starts: BTreeMap<FrameIndex, usize>,
    boundaries: BTreeMap<FrameIndex, usize>,
    frames: Vec<ProgramFrame>,
}

impl ProgramIndexes {
    pub(crate) fn cache_frames(&mut self, frames: Vec<ProgramFrame>) {
        self.frames = frames;
    }

    pub(crate) fn frames(&self) -> &[ProgramFrame] {
        &self.frames
    }
}

impl ProgramIndexes {
    pub(crate) fn build(capture: &CapturedSidExecution) -> Self {
        let mut indexes = Self::default();
        for (index, call) in capture.calls.iter().enumerate() {
            if let SidCallId::Play(frame) = call.call {
                indexes.calls.insert(frame, index);
            }
        }
        for (index, checkpoint) in capture.checkpoints.iter().enumerate() {
            match checkpoint.reference {
                CheckpointRef::PlayStart(frame) => {
                    indexes.starts.insert(frame, index);
                }
                CheckpointRef::SamplingBoundary(frame) => {
                    indexes.boundaries.insert(frame, index);
                }
                _ => {}
            }
        }
        indexes
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use]
pub struct ProgramStateSnapshot {
    pub at: ChipCycle,
    pub registers: SidRegisterFile,
    pub digital_sid: DigitalSidCheckpoint,
}

pub(super) fn state_at(program: &AnalyzedSidProgram, cycle: ChipCycle) -> ProgramStateSnapshot {
    capture_state_at(&program.capture, cycle)
}

pub(super) fn capture_state_at(
    capture: &CapturedSidExecution,
    cycle: ChipCycle,
) -> ProgramStateSnapshot {
    let initial = capture
        .checkpoints
        .iter()
        .rfind(|checkpoint| checkpoint.cycle <= cycle);
    let (mut sid, first_event) = initial.map_or_else(
        || (DigitalSid::with_model(capture.sid_model), 0_usize),
        |checkpoint| {
            (
                DigitalSid::from_checkpoint(checkpoint.digital_sid.clone()),
                checkpoint.next_event.0 as usize,
            )
        },
    );
    for event in capture.events.iter().skip(first_event) {
        if event.cycle > cycle {
            break;
        }
        if event.chip == SidChipId::PRIMARY
            && matches!(
                event.address_class,
                SidAddressClass::Base | SidAddressClass::Mirror
            )
            && event.access == SidBusAccess::Write
            && let Some(register) = event.register
        {
            sid.write(register, event.value, event.cycle);
        }
    }
    sid.clock_to(cycle);
    let checkpoint = sid.checkpoint();
    ProgramStateSnapshot {
        at: cycle,
        registers: SidRegisterFile(checkpoint.registers),
        digital_sid: checkpoint,
    }
}

pub(super) fn events_in(
    program: &AnalyzedSidProgram,
    span: SourceSpan,
) -> &[crate::emu::capture::SidBusEvent] {
    let start = program
        .capture
        .events
        .partition_point(|event| event.cycle < span.start);
    let end = program
        .capture
        .events
        .partition_point(|event| event.cycle < span.end);
    &program.capture.events[start..end]
}

pub(super) fn reconstruct_frames(program: &AnalyzedSidProgram) -> Vec<ProgramFrame> {
    let mut states = Vec::with_capacity(program.indexes.calls.len());
    for (&frame, &call_index) in &program.indexes.calls {
        let call = &program.capture.calls[call_index];
        let start = program
            .indexes
            .starts
            .get(&frame)
            .map(|index| program.capture.checkpoints[*index].digital_sid.clone())
            .unwrap_or_else(|| state_at(program, call.start_cycle).digital_sid);
        let boundary = program
            .indexes
            .boundaries
            .get(&frame)
            .map(|index| program.capture.checkpoints[*index].digital_sid.clone())
            .unwrap_or_else(|| state_at(program, call.sampling_boundary).digital_sid);
        let events =
            &program.capture.events[call.first_event.0 as usize..call.next_event.0 as usize];
        let writes: Vec<_> = events
            .iter()
            .filter(|event| event.chip == SidChipId::PRIMARY)
            .filter_map(|event| (*event).as_write())
            .collect();
        let reads: Vec<_> = events
            .iter()
            .filter(|event| event.chip == SidChipId::PRIMARY)
            .filter_map(|event| (*event).as_read())
            .collect();
        let entry_gate: [bool; 3] = std::array::from_fn(|voice| {
            start.registers[VOICE_BASES[voice] + CONTROL_OFFSET] & 1 != 0
        });
        let mut gate = entry_gate;
        let mut rising = [0_u32; 3];
        for write in &writes {
            for (voice, base) in VOICE_BASES.iter().enumerate() {
                if usize::from(write.reg.0) == base + CONTROL_OFFSET {
                    let next = write.value & 1 != 0;
                    if next && !gate[voice] {
                        rising[voice] += 1;
                    }
                    gate[voice] = next;
                }
            }
        }
        let hard_restart = std::array::from_fn(|voice| {
            rising[voice] > u32::from(!entry_gate[voice] && gate[voice])
        });
        let activities: [EnvelopeFrameActivity; 3] = std::array::from_fn(|voice| {
            program
                .capture
                .observations
                .iter()
                .find(|observation| {
                    observation.call == SidCallId::Play(frame)
                        && observation.voice.to_index() == voice
                })
                .map_or_else(Default::default, |observation| observation.envelope.clone())
        });
        let envelope_retrigger = std::array::from_fn(|voice| {
            let attacks = activities[voice]
                .events
                .iter()
                .filter(|event| event.kind == crate::emu::sid::EnvelopeEventKind::EnteredAttack)
                .count();
            hard_restart[voice] || attacks > usize::from(!entry_gate[voice] && gate[voice])
        });
        let registers = boundary.registers;
        states.push(ProgramFrame {
            frame,
            call_duration: call.duration,
            voices: std::array::from_fn(|voice| {
                VoiceState::from_regs(&std::array::from_fn(|offset| {
                    registers[VOICE_BASES[voice] + offset]
                }))
            }),
            filter: FilterState::from_regs(&std::array::from_fn(|offset| registers[0x15 + offset])),
            volume: Volume(registers[0x18] & 0x0f),
            envelope_retrigger,
            hard_restart,
            digital_voices: std::array::from_fn(|voice| DigitalVoiceState {
                envelope_start: envelope_snapshot(&start.envelopes[voice]),
                envelope: envelope_snapshot(&boundary.envelopes[voice]),
                envelope_activity: activities[voice].clone(),
                oscillator_start: oscillator_snapshot(
                    &start.oscillators[voice],
                    start.registers[VOICE_BASES[voice] + CONTROL_OFFSET],
                ),
                oscillator: oscillator_delta(
                    &start.oscillators[voice],
                    &boundary.oscillators[voice],
                    boundary.registers[VOICE_BASES[voice] + CONTROL_OFFSET],
                ),
            }),
            digital_state_exact: program.capture.timing_exact,
            register_writes: writes,
            register_reads: reads,
        });
    }
    states
}

fn envelope_snapshot(checkpoint: &crate::emu::sid::EnvelopeCheckpoint) -> EnvelopeSnapshot {
    EnvelopeSnapshot {
        level: checkpoint.level,
        phase: checkpoint.phase,
        rate_counter: checkpoint.rate_counter,
        exponential_counter: checkpoint.exponential_counter,
        exponential_period: checkpoint.exponential_period,
        gate: checkpoint.gate,
    }
}

fn oscillator_snapshot(
    checkpoint: &crate::emu::sid::OscillatorCheckpoint,
    control: u8,
) -> OscillatorSnapshot {
    OscillatorSnapshot {
        accumulator: checkpoint.accumulator,
        noise_shift_register: checkpoint.noise_shift_register,
        sync_resets: checkpoint.sync_resets,
        source_msb_edges: checkpoint.source_msb_edges,
        combined_waveform_exact: (control >> 4).count_ones() <= 1,
        noise_state_poisoned: checkpoint.noise_poisoned,
    }
}

fn oscillator_delta(
    start: &crate::emu::sid::OscillatorCheckpoint,
    end: &crate::emu::sid::OscillatorCheckpoint,
    control: u8,
) -> OscillatorSnapshot {
    let mut snapshot = oscillator_snapshot(end, control);
    snapshot.sync_resets = snapshot.sync_resets.saturating_sub(start.sync_resets);
    snapshot.source_msb_edges = snapshot
        .source_msb_edges
        .saturating_sub(start.source_msb_edges);
    snapshot
}
