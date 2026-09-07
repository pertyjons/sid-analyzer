mod builder;
pub mod evidence;
pub mod ids;
pub mod interpretation;
pub mod observable;
pub mod query;
pub mod region;
pub mod semantic;
pub mod signal;
pub mod time;
pub mod topology;
mod validate;

use super::VoiceId;
use super::inputs::AnalysisInputs;
use crate::emu::PlaybackTiming;
use crate::emu::capture::CapturedSidExecution;
use crate::header::{Header, SubtuneIndex};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::io;

pub use observable::RenderObservableSet;
pub use semantic::SemanticProgramView;
pub use signal::{SidChipProgram, SidVoiceProgram};
pub use validate::{ProgramValidationError, ProgramValidationReport};

#[derive(Debug, Clone, Serialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct AnalyzedSidProgram {
    pub source: ProgramSource,
    pub capture: CapturedSidExecution,
    pub chip: SidChipProgram,
    pub voices: [SidVoiceProgram; 3],
    pub semantic: SemanticProgramView,
    pub observables: RenderObservableSet,
    pub revisions: ProgramRevisions,
    #[serde(skip)]
    pub(crate) indexes: query::ProgramIndexes,
}

impl AnalyzedSidProgram {
    pub fn from_trace(
        header: &Header,
        subtune: SubtuneIndex,
        timing: PlaybackTiming,
        trace: &crate::trace::Trace,
    ) -> Self {
        let inputs = AnalysisInputs::build(trace, timing.clock);
        Self::from_analysis_inputs(header, subtune, timing, trace.capture.clone(), inputs)
    }

    pub fn from_analysis_inputs(
        header: &Header,
        subtune: SubtuneIndex,
        timing: PlaybackTiming,
        capture: CapturedSidExecution,
        inputs: AnalysisInputs,
    ) -> Self {
        builder::build(header, subtune, timing, capture, inputs)
    }

    #[must_use]
    pub fn frame_count(&self) -> usize {
        self.indexes.frames().len()
    }

    #[must_use = "the cached physical timeline must be consumed"]
    pub fn frames(&self) -> &[query::ProgramFrame] {
        self.indexes.frames()
    }

    #[must_use]
    pub fn project_frames(&self) -> Vec<query::ProgramFrame> {
        self.frames().to_vec()
    }

    pub fn project_filter_program(&self) -> crate::analysis::programs::ChipFilterProgram {
        crate::analysis::programs::build_filter_program(&self.project_frames())
    }

    pub fn register_file_at(&self, cycle: crate::trace::ChipCycle) -> signal::SidRegisterFile {
        self.state_at(cycle).registers
    }

    pub fn state_at(&self, cycle: crate::trace::ChipCycle) -> query::ProgramStateSnapshot {
        query::state_at(self, cycle)
    }

    pub fn events_in(&self, span: time::SourceSpan) -> &[crate::emu::capture::SidBusEvent] {
        query::events_in(self, span)
    }

    pub fn validate(&self) -> Result<ProgramValidationReport, ProgramValidationError> {
        validate::validate(self)
    }

    pub fn write_debug_json(&self, out: &mut dyn io::Write, pretty: bool) -> io::Result<()> {
        let document = ProgramDebugDocument::from_program(self).map_err(io::Error::other)?;
        if pretty {
            serde_json::to_writer_pretty(out, &document).map_err(io::Error::other)
        } else {
            serde_json::to_writer(out, &document).map_err(io::Error::other)
        }
    }

    pub fn read_debug_json(
        input: &mut dyn io::Read,
    ) -> Result<ProgramDebugDocument, ProgramDebugError> {
        let document: ProgramDebugDocument = serde_json::from_reader(input)?;
        document.validate()?;
        Ok(document)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(transparent)]
#[must_use]
pub struct ProgramDebugSchemaVersion(pub u16);

pub const PROGRAM_DEBUG_SCHEMA_VERSION: ProgramDebugSchemaVersion = ProgramDebugSchemaVersion(5);

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct ProgramDebugDocument {
    pub schema_version: ProgramDebugSchemaVersion,
    pub program: serde_json::Value,
}

impl ProgramDebugDocument {
    fn from_program(program: &AnalyzedSidProgram) -> Result<Self, serde_json::Error> {
        Ok(Self {
            schema_version: PROGRAM_DEBUG_SCHEMA_VERSION,
            program: serde_json::to_value(program)?,
        })
    }

    fn validate(&self) -> Result<(), ProgramDebugError> {
        if self.schema_version != PROGRAM_DEBUG_SCHEMA_VERSION {
            return Err(ProgramDebugError::UnsupportedVersion {
                found: self.schema_version,
                supported: PROGRAM_DEBUG_SCHEMA_VERSION,
            });
        }
        let object = self
            .program
            .as_object()
            .ok_or(ProgramDebugError::ProgramNotObject)?;
        let expected: BTreeSet<_> = [
            "source",
            "capture",
            "chip",
            "voices",
            "semantic",
            "observables",
            "revisions",
        ]
        .into_iter()
        .collect();
        let found: BTreeSet<_> = object.keys().map(String::as_str).collect();
        if found != expected {
            return Err(ProgramDebugError::ProgramFields {
                expected: expected.into_iter().map(str::to_owned).collect(),
                found: found.into_iter().map(str::to_owned).collect(),
            });
        }
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ProgramDebugError {
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("program debug schema {found:?} is unsupported; expected {supported:?}")]
    UnsupportedVersion {
        found: ProgramDebugSchemaVersion,
        supported: ProgramDebugSchemaVersion,
    },
    #[error("program debug payload must be an object")]
    ProgramNotObject,
    #[error("program debug fields differ: expected {expected:?}, found {found:?}")]
    ProgramFields {
        expected: Vec<String>,
        found: Vec<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct ProgramRevisions {
    pub analyzer: AnalyzerRevision,
    pub capture_schema: crate::emu::capture::CaptureSchemaVersion,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
#[must_use]
pub struct AnalyzerRevision(pub String);

#[derive(Debug, Clone, Serialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct ProgramSource {
    pub header: Header,
    pub subtune: SubtuneIndex,
    pub timing: PlaybackTiming,
}

#[must_use]
fn voice_array<T>(mut build: impl FnMut(VoiceId) -> T) -> [T; 3] {
    std::array::from_fn(|index| build(VoiceId::from_index(index)))
}
