use super::AnalyzedSidProgram;
use crate::analysis::VoiceId;
use crate::emu::capture::{CaptureError, SidBusEventId};
use crate::trace::FrameIndex;
use serde::Serialize;
use std::collections::BTreeSet;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct ProgramValidationReport {
    pub events: usize,
    pub checkpoints: usize,
    pub frames: usize,
    pub repeated_equal_writes: usize,
    pub unsupported_events: usize,
}

pub(super) fn validate(
    program: &AnalyzedSidProgram,
) -> Result<ProgramValidationReport, ProgramValidationError> {
    program.capture.validate()?;
    for (index, voice) in program.voices.iter().enumerate() {
        let expected = VoiceId::from_index(index);
        if voice.voice != expected {
            return Err(ProgramValidationError::VoiceOrder {
                expected,
                found: voice.voice,
            });
        }
    }
    let event_ids: BTreeSet<SidBusEventId> = program
        .capture
        .events
        .iter()
        .map(|event| event.id)
        .collect();
    for voice in &program.voices {
        validate_event_signal(&voice.frequency, &event_ids)?;
        validate_event_signal(&voice.pulse_width, &event_ids)?;
        validate_event_signal(&voice.control, &event_ids)?;
        validate_event_signal(&voice.adsr, &event_ids)?;
    }
    for checkpoint in &program.capture.checkpoints {
        let projected = program.register_file_at(checkpoint.cycle);
        if projected.0 != checkpoint.digital_sid.registers {
            return Err(ProgramValidationError::RegisterProjectionMismatch(
                checkpoint.id,
            ));
        }
    }
    for occurrence in &program.semantic.continuous.occurrences {
        let exact = program.state_at(occurrence.span.start).digital_sid;
        if occurrence.initial.digital_sid != exact {
            return Err(ProgramValidationError::ContinuousInitialStateMismatch(
                occurrence.id,
            ));
        }
        let linked = occurrence.previous.is_some() || occurrence.continuation.is_some();
        if linked && (!occurrence.kind.is_physical_timeline() || occurrence.voice.is_none()) {
            return Err(ProgramValidationError::InvalidContinuousLineage(
                occurrence.id,
            ));
        }
        if let Some(previous) = occurrence.previous {
            let Some(previous) = program
                .semantic
                .continuous
                .occurrences
                .get(previous.0 as usize)
            else {
                return Err(ProgramValidationError::InvalidContinuousLineage(
                    occurrence.id,
                ));
            };
            if previous.continuation != Some(occurrence.id)
                || previous.voice != occurrence.voice
                || !previous.kind.is_physical_timeline()
                || previous.span.end > occurrence.span.start
            {
                return Err(ProgramValidationError::InvalidContinuousLineage(
                    occurrence.id,
                ));
            }
        }
        if let Some(continuation) = occurrence.continuation {
            let Some(continuation) = program
                .semantic
                .continuous
                .occurrences
                .get(continuation.0 as usize)
            else {
                return Err(ProgramValidationError::InvalidContinuousLineage(
                    occurrence.id,
                ));
            };
            if continuation.previous != Some(occurrence.id)
                || continuation.voice != occurrence.voice
                || !continuation.kind.is_physical_timeline()
                || occurrence.span.end > continuation.span.start
            {
                return Err(ProgramValidationError::InvalidContinuousLineage(
                    occurrence.id,
                ));
            }
        }
    }
    let projected = super::query::reconstruct_frames(program);
    if projected != program.frames() {
        let frame = projected
            .iter()
            .zip(program.frames())
            .position(|(actual, expected)| actual != expected)
            .map_or(FrameIndex(0), |index| FrameIndex(index as u32));
        return Err(ProgramValidationError::FrameProjectionMismatch(frame));
    }
    let mut unsupported_events = BTreeSet::new();
    for report in program.capture.replay_all_checkpoints()? {
        if let Some(mismatch) = report.read_mismatches.first() {
            return Err(ProgramValidationError::ReplayReadMismatch(mismatch.event));
        }
        if let Some(mismatch) = report.checkpoint_mismatches.first() {
            return Err(ProgramValidationError::ReplayCheckpointMismatch(
                mismatch.checkpoint,
            ));
        }
        unsupported_events.extend(report.unsupported_events);
    }
    let repeated_equal_writes = program
        .capture
        .events
        .windows(2)
        .filter(|pair| {
            pair[0].access == crate::emu::capture::SidBusAccess::Write
                && pair[1].access == crate::emu::capture::SidBusAccess::Write
                && pair[0].register == pair[1].register
                && pair[0].value == pair[1].value
        })
        .count();
    Ok(ProgramValidationReport {
        events: program.capture.events.len(),
        checkpoints: program.capture.checkpoints.len(),
        frames: projected.len(),
        repeated_equal_writes,
        unsupported_events: unsupported_events.len(),
    })
}

fn validate_event_signal<T>(
    signal: &super::signal::EventSignal<T>,
    event_ids: &BTreeSet<SidBusEventId>,
) -> Result<(), ProgramValidationError> {
    for point in &signal.0 {
        let Some(source) = point.value.evidence.first() else {
            return Err(ProgramValidationError::MissingEvidence(point.id.0));
        };
        if let super::evidence::SourceReference::Event(event) = source.source
            && !event_ids.contains(&event)
        {
            return Err(ProgramValidationError::MissingSourceEvent(event));
        }
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum ProgramValidationError {
    #[error(transparent)]
    Capture(#[from] CaptureError),
    #[error("voice program order mismatch: expected {expected}, found {found}")]
    VoiceOrder { expected: VoiceId, found: VoiceId },
    #[error("signal point {0} has no evidence")]
    MissingEvidence(u64),
    #[error("signal references missing SID bus event {0:?}")]
    MissingSourceEvent(SidBusEventId),
    #[error("frame projection differs at frame {0}")]
    FrameProjectionMismatch(FrameIndex),
    #[error("register projection differs at checkpoint {0:?}")]
    RegisterProjectionMismatch(crate::emu::capture::CheckpointId),
    #[error("continuous occurrence {0:?} does not retain its exact initial SID state")]
    ContinuousInitialStateMismatch(super::ids::ContinuousOccurrenceId),
    #[error("continuous occurrence {0:?} has invalid physical lineage")]
    InvalidContinuousLineage(super::ids::ContinuousOccurrenceId),
    #[error("capture replay differs at read event {0:?}")]
    ReplayReadMismatch(SidBusEventId),
    #[error("capture replay differs at checkpoint {0:?}")]
    ReplayCheckpointMismatch(crate::emu::capture::CheckpointId),
}
