use super::evidence::Evidence;
use super::evidence::Provenance;
use super::ids::{
    CausalDependencyId, ContinuousOccurrenceId, EffectViewId, NoteViewId, ProgramDefinitionId,
    ProgramOccurrenceId, ProgramReuseGroupId, SoundRegionId,
};
use super::interpretation::InterpretedSignalSource;
use super::interpretation::SignalInterpretation;
use super::region::SoundRegion;
use super::time::SourceSpan;
use crate::analysis::VoiceId;
use crate::analysis::effects::{EffectSpan, VoiceRelationSpan};
use crate::analysis::note::NoteEvent;
use crate::analysis::note::Velocity;
use crate::analysis::timbre::{NoteCharacteristics, Patch, PatchId};
use crate::emu::capture::SidBusEventId;
use crate::emu::sid::{DigitalSidCheckpoint, EnvelopeCheckpoint, OscillatorCheckpoint};
use crate::trace::SidRegister;
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct SemanticProgramView {
    pub notes: Vec<NoteEvent>,
    pub effects: Vec<EffectSpan>,
    pub voice_relations: Vec<VoiceRelationSpan>,
    pub characteristics: Vec<NoteCharacteristics>,
    pub patches: Vec<Patch>,
    pub patch_assignments: Vec<Option<PatchId>>,
    pub regions: Vec<SoundRegion>,
    pub note_views: Vec<MusicalNoteView>,
    pub effect_views: Vec<MusicalEffectView>,
    pub program_definitions: Vec<ProgramDefinition>,
    pub occurrences: Vec<ProgramOccurrence>,
    pub continuous: ContinuousProgramView,
    pub causal_dependencies: Vec<CausalDependency>,
    pub interpretations: Vec<SignalInterpretation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub structure: Option<Vec<crate::export::VoicePlacements>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native: Option<NativeSemanticOverlay>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct CausalDependency {
    pub id: CausalDependencyId,
    pub source: CausalSource,
    pub producer: SidBusEventId,
    pub producer_value: u8,
    pub consumers: Vec<CausalConsumer>,
    pub support: CausalSupport,
    pub evidence: Vec<Evidence>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", content = "source", rename_all = "snake_case")]
#[must_use]
pub enum CausalSource {
    Oscillator3Read,
    Envelope3Read,
    RamCell(RamAddress),
    TableLookup(RamAddress),
    Timer(TimerSource),
    Accumulator(AccumulatorSource),
    Random(RandomSource),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
#[must_use]
pub struct RamAddress(pub u16);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[must_use]
pub enum TimerSource {
    Cia1TimerA,
    Cia1TimerB,
    Raster,
    PlayCall,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", content = "address", rename_all = "snake_case")]
#[must_use]
pub enum AccumulatorSource {
    CpuA,
    CpuX,
    CpuY,
    DriverCell(RamAddress),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
#[must_use]
pub enum RandomSource {
    SidNoise(VoiceId),
    DriverCell(RamAddress),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct CausalConsumer {
    pub event: SidBusEventId,
    pub register: SidRegister,
    pub transform: CausalTransform,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[must_use]
pub enum CausalTransform {
    Identity,
    LowNibble,
    HighNibble,
    Ordered6502Path,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[must_use]
pub enum CausalSupport {
    OrderedSameCallHypothesis,
    ProducerTransformConsumer,
    UnknownConsumer,
}

#[derive(Debug, Clone, Serialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct MusicalNoteView {
    pub id: NoteViewId,
    pub note_index: NoteIndex,
    pub span: SourceSpan,
    pub regions: Vec<SoundRegionId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(transparent)]
#[must_use]
pub struct NoteIndex(pub usize);

#[derive(Debug, Clone, Serialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct MusicalEffectView {
    pub id: EffectViewId,
    pub effect_index: EffectIndex,
    pub span: SourceSpan,
    pub regions: Vec<SoundRegionId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(transparent)]
#[must_use]
pub struct EffectIndex(pub usize);

#[derive(Debug, Clone, Serialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct ProgramDefinition {
    pub id: ProgramDefinitionId,
    pub patch: Option<PatchId>,
    pub content_digest: ProgramContentDigest,
    pub event_count: ProgramEventCount,
    pub duration: crate::trace::ChipCycle,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
#[must_use]
pub struct ProgramContentDigest(pub [u8; 16]);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(transparent)]
#[must_use]
pub struct ProgramEventCount(pub u64);

#[derive(Debug, Clone, Default, Serialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct ContinuousProgramView {
    pub occurrences: Vec<ContinuousProgramOccurrence>,
    pub reuse_groups: Vec<ProgramReuseGroup>,
    pub complexity: ContinuousProgramComplexity,
}

#[derive(Debug, Clone, Serialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct ContinuousProgramOccurrence {
    pub id: ContinuousOccurrenceId,
    pub region: SoundRegionId,
    pub voice: Option<VoiceId>,
    pub kind: super::region::SoundRegionKind,
    pub span: SourceSpan,
    pub content_digest: ProgramContentDigest,
    pub event_count: ProgramEventCount,
    pub initial: ContinuousInitialState,
    pub previous: Option<ContinuousOccurrenceId>,
    pub continuation: Option<ContinuousOccurrenceId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct ContinuousInitialState {
    pub digital_sid: DigitalSidCheckpoint,
}

#[derive(Debug, Clone, Serialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct ProgramReuseGroup {
    pub id: ProgramReuseGroupId,
    pub content_digest: ProgramContentDigest,
    pub voice: Option<VoiceId>,
    pub occurrences: Vec<ContinuousOccurrenceId>,
    pub policy: ReusePolicy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[must_use]
pub enum ReusePolicy {
    Unique,
    ExactInitialState,
    ExplicitInitialState,
    ContinuationRequired,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct ContinuousProgramComplexity {
    pub occurrences: ProgramOccurrenceCount,
    pub reusable_groups: ProgramDefinitionCount,
    pub events: ProgramEventCount,
    pub serialized_size: ProgramSerializedSize,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(transparent)]
#[must_use]
pub struct ProgramOccurrenceCount(pub u64);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(transparent)]
#[must_use]
pub struct ProgramDefinitionCount(pub u64);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(transparent)]
#[must_use]
pub struct ProgramSerializedSize(pub u64);

#[derive(Debug, Clone, Serialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct ProgramOccurrence {
    pub id: ProgramOccurrenceId,
    pub definition: Option<ProgramDefinitionId>,
    pub voice: VoiceId,
    pub span: SourceSpan,
    pub regions: Vec<SoundRegionId>,
    pub initial: OccurrenceInitialState,
    pub local_automation: Vec<LocalAutomationRef>,
    pub previous: Option<ProgramOccurrenceId>,
    pub continuation: Option<ProgramOccurrenceId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct LocalAutomationRef {
    pub source: InterpretedSignalSource,
    pub span: SourceSpan,
}

#[derive(Debug, Clone, Serialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct OccurrenceInitialState {
    pub oscillator: Option<OscillatorCheckpoint>,
    pub envelope: Option<EnvelopeCheckpoint>,
    pub table_position: Option<TablePosition>,
    pub transpose: SemitoneOffset,
    pub velocity: Velocity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(transparent)]
#[must_use]
pub struct TablePosition(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(transparent)]
#[must_use]
pub struct SemitoneOffset(pub i16);

#[derive(Debug, Clone, Serialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct NativeSemanticOverlay {
    pub driver: String,
    pub extractor: String,
    pub fields: Vec<NativeFieldEvidence>,
    pub authored_spans: Vec<NativeAuthoredSpan>,
    pub validation: crate::export::native::NativeValidationReport,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recovered_structure: Option<crate::export::RecoveredStructure>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct NativeFieldEvidence {
    pub field: String,
    pub provenance: Provenance,
    pub samples: EvidenceCount,
    pub mismatches: EvidenceCount,
    pub span: Option<SourceSpan>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct NativeAuthoredSpan {
    pub field: String,
    pub span: SourceSpan,
    pub evidence: Vec<Evidence>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(transparent)]
#[must_use]
pub struct EvidenceCount(pub u64);
