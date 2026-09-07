use crate::analysis::{SystemClock, VoiceId};
use crate::emu::CallRate;
use crate::emu::sid::{
    DigitalSid, DigitalSidCheckpoint, EnvelopeFrameActivity, OscillatorCheckpoint,
};
use crate::header::{SidBaseAddress, SidModel, SubtuneIndex};
use crate::trace::{
    ChipCycle, CpuCycles, FrameIndex, FrameTrace, RegisterRead, RegisterWrite, SID_REGISTER_LAST,
    SidRegister, SubFrameOffset, Trace,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

pub const CAPTURE_SCHEMA_VERSION: CaptureSchemaVersion = CaptureSchemaVersion(3);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
#[must_use]
pub struct CaptureSchemaVersion(pub u16);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
#[must_use]
pub struct SourceDigest(pub [u8; 16]);

impl fmt::Display for SourceDigest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct CapturedSourceIdentity {
    pub digest: SourceDigest,
    pub subtune: SubtuneIndex,
}

impl Default for CapturedSourceIdentity {
    fn default() -> Self {
        Self {
            digest: SourceDigest([0; 16]),
            subtune: SubtuneIndex(1),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(transparent)]
#[must_use]
pub struct SidBusEventId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(transparent)]
#[must_use]
pub struct SidChipId(pub u8);

impl SidChipId {
    pub const PRIMARY: Self = Self(0);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct CapturedSidChip {
    pub id: SidChipId,
    pub base_address: SidAddress,
    pub model: SidModel,
}

impl CapturedSidChip {
    pub fn primary(model: SidModel) -> Self {
        Self {
            id: SidChipId::PRIMARY,
            base_address: SidAddress(0xd400),
            model,
        }
    }

    pub fn additional(id: SidChipId, address: SidBaseAddress, model: SidModel) -> Self {
        Self {
            id,
            base_address: SidAddress(address.address()),
            model,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", content = "frame", rename_all = "snake_case")]
#[must_use]
pub enum SidCallId {
    Init,
    Play(FrameIndex),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[must_use]
pub enum SidBusAccess {
    Read,
    Write,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
#[must_use]
pub struct SidAddress(pub u16);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[must_use]
pub enum SidAddressClass {
    Base,
    Mirror,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[must_use]
pub enum EventTimestampQuality {
    Exact,
    InstructionStartBounded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct SidBusEvent {
    pub id: SidBusEventId,
    pub chip: SidChipId,
    pub call: SidCallId,
    pub address: SidAddress,
    pub address_class: SidAddressClass,
    pub register: Option<SidRegister>,
    pub value: u8,
    pub access: SidBusAccess,
    pub cycle: ChipCycle,
    pub offset: SubFrameOffset,
    pub timestamp_quality: EventTimestampQuality,
}

impl SidBusEvent {
    #[must_use]
    pub fn as_write(self) -> Option<RegisterWrite> {
        (self.access == SidBusAccess::Write).then_some(RegisterWrite {
            reg: self.register?,
            value: self.value,
            offset: self.offset,
        })
    }

    #[must_use]
    pub fn as_read(self) -> Option<RegisterRead> {
        (self.access == SidBusAccess::Read).then_some(RegisterRead {
            reg: self.register?,
            value: self.value,
            offset: self.offset,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct CapturedCallSpan {
    pub call: SidCallId,
    pub start_cycle: ChipCycle,
    pub return_cycle: ChipCycle,
    pub sampling_boundary: ChipCycle,
    pub duration: CpuCycles,
    pub overrun: CpuCycles,
    pub first_event: SidBusEventId,
    pub next_event: SidBusEventId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(transparent)]
#[must_use]
pub struct CheckpointId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "frame", rename_all = "snake_case")]
#[must_use]
pub enum CheckpointRef {
    InitReturn,
    FirstPlayBoundary,
    PlayStart(FrameIndex),
    SamplingBoundary(FrameIndex),
    Event(SidBusEventId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
#[must_use]
pub struct SidDataLatch(pub u8);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[must_use]
pub enum MirrorModelStatus {
    UnsupportedRetained,
    Modeled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[must_use]
pub enum ExternalInputModelStatus {
    Disconnected,
    Unsupported,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct SidBusCheckpoint {
    pub id: CheckpointId,
    pub reference: CheckpointRef,
    pub cycle: ChipCycle,
    pub next_event: SidBusEventId,
    pub digital_sid: DigitalSidCheckpoint,
    pub data_latch: SidDataLatch,
    pub mirror_model: MirrorModelStatus,
    pub external_input_model: ExternalInputModelStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[must_use]
pub enum CheckpointPolicy {
    DenseCallBoundaries,
    SparseEveryCalls { calls: CheckpointCadence },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
#[must_use]
pub struct CheckpointCadence(pub u32);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct CapturedObservation {
    pub call: SidCallId,
    pub voice: VoiceId,
    pub start_cycle: ChipCycle,
    pub end_cycle: ChipCycle,
    pub envelope: EnvelopeFrameActivity,
    pub oscillator_start: OscillatorCheckpoint,
    pub oscillator_end: OscillatorCheckpoint,
    pub sync_resets: OscillatorEventCount,
    pub source_msb_edges: OscillatorEventCount,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
#[must_use]
pub struct OscillatorEventCount(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[must_use]
pub enum CaptureDiagnosticKind {
    MirrorAccessUnsupported,
    ExternalInputUnsupported,
    AdditionalSidCheckpointUnsupported,
    TimingInexact,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct CaptureDiagnostic {
    pub kind: CaptureDiagnosticKind,
    pub event: Option<SidBusEventId>,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct CapturedSidExecution {
    pub schema_version: CaptureSchemaVersion,
    pub source: CapturedSourceIdentity,
    pub sid_model: SidModel,
    pub system_clock: SystemClock,
    pub call_rate: CallRate,
    pub timing_exact: bool,
    pub checkpoint_policy: CheckpointPolicy,
    pub chips: Vec<CapturedSidChip>,
    pub events: Vec<SidBusEvent>,
    pub calls: Vec<CapturedCallSpan>,
    pub checkpoints: Vec<SidBusCheckpoint>,
    pub observations: Vec<CapturedObservation>,
    pub diagnostics: Vec<CaptureDiagnostic>,
}

impl Default for CapturedSidExecution {
    fn default() -> Self {
        Self {
            schema_version: CAPTURE_SCHEMA_VERSION,
            source: CapturedSourceIdentity::default(),
            sid_model: SidModel::Unknown,
            system_clock: SystemClock::Pal,
            call_rate: CallRate::default(),
            timing_exact: true,
            checkpoint_policy: CheckpointPolicy::DenseCallBoundaries,
            chips: vec![CapturedSidChip::primary(SidModel::Unknown)],
            events: Vec::new(),
            calls: Vec::new(),
            checkpoints: Vec::new(),
            observations: Vec::new(),
            diagnostics: Vec::new(),
        }
    }
}

impl CapturedSidExecution {
    pub fn to_json(&self, out: &mut dyn std::io::Write, pretty: bool) -> Result<(), CaptureError> {
        if pretty {
            serde_json::to_writer_pretty(out, self)?;
        } else {
            serde_json::to_writer(out, self)?;
        }
        Ok(())
    }

    pub fn from_json(input: impl std::io::Read) -> Result<Self, CaptureError> {
        let capture: Self = serde_json::from_reader(input)?;
        if capture.schema_version != CAPTURE_SCHEMA_VERSION {
            return Err(CaptureError::UnsupportedSchemaVersion {
                found: capture.schema_version,
                supported: CAPTURE_SCHEMA_VERSION,
            });
        }
        capture.validate()?;
        Ok(capture)
    }

    pub fn validate(&self) -> Result<(), CaptureError> {
        if self.chips.first().map(|chip| chip.id) != Some(SidChipId::PRIMARY) {
            return Err(CaptureError::MissingPrimaryChip);
        }
        let primary = self.chips[0];
        if primary.base_address != SidAddress(0xd400) || primary.model != self.sid_model {
            return Err(CaptureError::InvalidPrimaryChip(primary));
        }
        let mut chip_ids = BTreeSet::new();
        let mut chip_bases = BTreeSet::new();
        for chip in &self.chips {
            if !chip_ids.insert(chip.id) {
                return Err(CaptureError::DuplicateChipId(chip.id));
            }
            if !chip_bases.insert(chip.base_address) {
                return Err(CaptureError::DuplicateChipBase(chip.base_address));
            }
        }
        for (index, event) in self.events.iter().enumerate() {
            let expected = SidBusEventId(index as u64);
            if event.id != expected {
                return Err(CaptureError::NonMonotonicEventId {
                    expected,
                    found: event.id,
                });
            }
            if !chip_ids.contains(&event.chip) {
                return Err(CaptureError::UnknownEventChip {
                    event: event.id,
                    chip: event.chip,
                });
            }
        }
        for pair in self.events.windows(2) {
            if pair[0].cycle > pair[1].cycle {
                return Err(CaptureError::EventCycleMovedBackwards {
                    before: pair[0].id,
                    after: pair[1].id,
                });
            }
        }
        for checkpoint in &self.checkpoints {
            if checkpoint.next_event.0 > self.events.len() as u64 {
                return Err(CaptureError::CheckpointPastEventStream {
                    checkpoint: checkpoint.id,
                    next_event: checkpoint.next_event,
                });
            }
        }
        Ok(())
    }

    pub fn retain_sparse_checkpoints(
        &mut self,
        cadence: CheckpointCadence,
    ) -> Result<(), CaptureError> {
        if cadence.0 == 0 {
            return Err(CaptureError::ZeroCheckpointCadence);
        }
        self.checkpoints
            .retain(|checkpoint| match checkpoint.reference {
                CheckpointRef::InitReturn | CheckpointRef::FirstPlayBoundary => true,
                CheckpointRef::PlayStart(frame) | CheckpointRef::SamplingBoundary(frame) => {
                    frame.0 % cadence.0 == 0
                }
                CheckpointRef::Event(_) => true,
            });
        for (index, checkpoint) in self.checkpoints.iter_mut().enumerate() {
            checkpoint.id = CheckpointId(index as u64);
        }
        self.checkpoint_policy = CheckpointPolicy::SparseEveryCalls { calls: cadence };
        Ok(())
    }

    pub fn add_event_checkpoints(
        &mut self,
        events: &BTreeSet<SidBusEventId>,
    ) -> Result<(), CaptureError> {
        let mut sid = DigitalSid::with_model(self.sid_model);
        let mut latch = SidDataLatch(0);
        let mut unmodeled_mirror = false;
        let mut additions = Vec::new();
        for event in &self.events {
            if event.chip != SidChipId::PRIMARY {
                sid.clock_to(event.cycle);
                if events.contains(&event.id) {
                    additions.push(SidBusCheckpoint {
                        id: CheckpointId(0),
                        reference: CheckpointRef::Event(event.id),
                        cycle: event.cycle,
                        next_event: SidBusEventId(event.id.0 + 1),
                        digital_sid: sid.checkpoint(),
                        data_latch: latch,
                        mirror_model: if unmodeled_mirror {
                            MirrorModelStatus::UnsupportedRetained
                        } else {
                            MirrorModelStatus::Modeled
                        },
                        external_input_model: ExternalInputModelStatus::Disconnected,
                    });
                }
                continue;
            }
            match (event.address_class, event.access, event.register) {
                (
                    SidAddressClass::Base | SidAddressClass::Mirror,
                    SidBusAccess::Write,
                    Some(register),
                ) => {
                    sid.write(register, event.value, event.cycle);
                    latch = SidDataLatch(event.value);
                }
                (
                    SidAddressClass::Base | SidAddressClass::Mirror,
                    SidBusAccess::Read,
                    Some(register),
                ) => {
                    let _ = replay_read(&mut sid, latch, register, event.cycle);
                }
                (SidAddressClass::Mirror, _, None) => {
                    unmodeled_mirror = true;
                    sid.clock_to(event.cycle);
                }
                (SidAddressClass::Base, _, None) => {
                    return Err(CaptureError::BaseEventWithoutRegister(event.id));
                }
            }
            if events.contains(&event.id) {
                additions.push(SidBusCheckpoint {
                    id: CheckpointId(0),
                    reference: CheckpointRef::Event(event.id),
                    cycle: event.cycle,
                    next_event: SidBusEventId(event.id.0 + 1),
                    digital_sid: sid.checkpoint(),
                    data_latch: latch,
                    mirror_model: if unmodeled_mirror {
                        MirrorModelStatus::UnsupportedRetained
                    } else {
                        MirrorModelStatus::Modeled
                    },
                    external_input_model: ExternalInputModelStatus::Disconnected,
                });
            }
        }
        self.checkpoints.extend(additions);
        self.checkpoints.sort_by_key(|checkpoint| {
            (
                checkpoint.cycle,
                checkpoint.next_event,
                checkpoint_reference_order(checkpoint.reference),
            )
        });
        for (index, checkpoint) in self.checkpoints.iter_mut().enumerate() {
            checkpoint.id = CheckpointId(index as u64);
        }
        Ok(())
    }

    pub fn replay_from(&self, start: CheckpointId) -> Result<ReplayReport, CaptureError> {
        let start_index = self
            .checkpoints
            .iter()
            .position(|checkpoint| checkpoint.id == start)
            .ok_or(CaptureError::CheckpointNotFound(start))?;
        let initial = &self.checkpoints[start_index];
        let mut sid = DigitalSid::from_checkpoint(initial.digital_sid.clone());
        let mut latch = initial.data_latch;
        let mut event_index = initial.next_event.0 as usize;
        let mut report = ReplayReport::default();

        for checkpoint in &self.checkpoints[start_index + 1..] {
            while let Some(event) = self.events.get(event_index) {
                if event.id.0 >= checkpoint.next_event.0 {
                    break;
                }
                if event.chip != SidChipId::PRIMARY {
                    report.unsupported_events.push(event.id);
                    sid.clock_to(event.cycle);
                    event_index += 1;
                    continue;
                }
                match (event.address_class, event.register) {
                    (SidAddressClass::Mirror, None) => {
                        report.unsupported_events.push(event.id);
                        sid.clock_to(event.cycle);
                    }
                    (SidAddressClass::Base | SidAddressClass::Mirror, Some(register)) => {
                        match event.access {
                            SidBusAccess::Write => {
                                sid.write(register, event.value, event.cycle);
                                latch = SidDataLatch(event.value);
                            }
                            SidBusAccess::Read => {
                                let actual = replay_read(&mut sid, latch, register, event.cycle);
                                if actual != event.value {
                                    report.read_mismatches.push(ReadMismatch {
                                        event: event.id,
                                        captured: event.value,
                                        replayed: actual,
                                    });
                                }
                            }
                        }
                    }
                    (SidAddressClass::Base, None) => {
                        return Err(CaptureError::BaseEventWithoutRegister(event.id));
                    }
                }
                event_index += 1;
            }
            sid.clock_to(checkpoint.cycle);
            let actual = sid.checkpoint();
            if actual != checkpoint.digital_sid || latch != checkpoint.data_latch {
                let mut fields = checkpoint_field_differences(&actual, &checkpoint.digital_sid)?;
                if latch != checkpoint.data_latch {
                    fields.push(CheckpointFieldPath("data_latch".to_owned()));
                }
                report.checkpoint_mismatches.push(CheckpointMismatch {
                    checkpoint: checkpoint.id,
                    digital_state_equal: actual == checkpoint.digital_sid,
                    data_latch_equal: latch == checkpoint.data_latch,
                    fields,
                });
            }
            report.checkpoints_verified += 1;
        }
        Ok(report)
    }

    pub fn replay_all_checkpoints(&self) -> Result<Vec<ReplayReport>, CaptureError> {
        self.checkpoints
            .iter()
            .map(|checkpoint| self.replay_from(checkpoint.id))
            .collect()
    }

    #[must_use]
    pub fn project_trace(&self) -> Trace {
        self.project_trace_for_chip(SidChipId::PRIMARY)
    }

    #[must_use]
    pub fn project_trace_for_chip(&self, chip: SidChipId) -> Trace {
        let mut projections: BTreeMap<SidCallId, (Vec<RegisterWrite>, Vec<RegisterRead>)> =
            BTreeMap::new();
        for event in self.events.iter().filter(|event| event.chip == chip) {
            let projection = projections.entry(event.call).or_default();
            if let Some(write) = event.as_write() {
                projection.0.push(write);
            }
            if let Some(read) = event.as_read() {
                projection.1.push(read);
            }
        }
        let init_call = self.calls.iter().find(|call| call.call == SidCallId::Init);
        let (init_writes, init_reads) = projections.remove(&SidCallId::Init).unwrap_or_default();
        let frames = self
            .calls
            .iter()
            .filter_map(|call| {
                let SidCallId::Play(frame) = call.call else {
                    return None;
                };
                let (writes, reads) = projections.remove(&call.call).unwrap_or_default();
                Some(FrameTrace {
                    frame,
                    start_cycle: call.start_cycle,
                    duration: call.duration,
                    overrun: call.overrun,
                    end_cycle: call.sampling_boundary,
                    writes,
                    reads,
                })
            })
            .collect();
        Trace {
            capture: self.clone(),
            sid_model: self
                .chips
                .iter()
                .find(|candidate| candidate.id == chip)
                .map_or(self.sid_model, |candidate| candidate.model),
            init_writes,
            init_reads,
            init_duration: init_call.map_or(CpuCycles(0), |call| call.duration),
            init_overrun: init_call.map_or(CpuCycles(0), |call| call.overrun),
            timing_exact: self.timing_exact,
            call_rate: self.call_rate,
            frames,
        }
    }
}

fn replay_read(
    sid: &mut DigitalSid,
    latch: SidDataLatch,
    register: SidRegister,
    cycle: ChipCycle,
) -> u8 {
    match register.0 {
        0x00..=0x18 => {
            sid.clock_to(cycle);
            latch.0
        }
        0x19 | 0x1A => {
            sid.clock_to(cycle);
            0xff
        }
        _ => sid.read(register, cycle),
    }
}

fn checkpoint_reference_order(reference: CheckpointRef) -> u8 {
    match reference {
        CheckpointRef::InitReturn => 0,
        CheckpointRef::FirstPlayBoundary => 1,
        CheckpointRef::PlayStart(_) => 2,
        CheckpointRef::Event(_) => 3,
        CheckpointRef::SamplingBoundary(_) => 4,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct ReadMismatch {
    pub event: SidBusEventId,
    pub captured: u8,
    pub replayed: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct CheckpointMismatch {
    pub checkpoint: CheckpointId,
    pub digital_state_equal: bool,
    pub data_latch_equal: bool,
    pub fields: Vec<CheckpointFieldPath>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
#[must_use]
pub struct CheckpointFieldPath(pub String);

fn checkpoint_field_differences(
    actual: &DigitalSidCheckpoint,
    expected: &DigitalSidCheckpoint,
) -> Result<Vec<CheckpointFieldPath>, CaptureError> {
    let actual = serde_json::to_value(actual)?;
    let expected = serde_json::to_value(expected)?;
    let mut differences = Vec::new();
    collect_value_differences("", &actual, &expected, &mut differences);
    Ok(differences)
}

fn collect_value_differences(
    path: &str,
    actual: &serde_json::Value,
    expected: &serde_json::Value,
    differences: &mut Vec<CheckpointFieldPath>,
) {
    match (actual, expected) {
        (serde_json::Value::Object(actual), serde_json::Value::Object(expected)) => {
            for key in actual
                .keys()
                .chain(expected.keys())
                .collect::<BTreeSet<_>>()
            {
                let next = if path.is_empty() {
                    key.to_string()
                } else {
                    format!("{path}.{key}")
                };
                match (actual.get(key), expected.get(key)) {
                    (Some(actual), Some(expected)) => {
                        collect_value_differences(&next, actual, expected, differences);
                    }
                    _ => differences.push(CheckpointFieldPath(next)),
                }
            }
        }
        (serde_json::Value::Array(actual), serde_json::Value::Array(expected)) => {
            for index in 0..actual.len().max(expected.len()) {
                let next = format!("{path}[{index}]");
                match (actual.get(index), expected.get(index)) {
                    (Some(actual), Some(expected)) => {
                        collect_value_differences(&next, actual, expected, differences);
                    }
                    _ => differences.push(CheckpointFieldPath(next)),
                }
            }
        }
        _ if actual != expected => differences.push(CheckpointFieldPath(path.to_owned())),
        _ => {}
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct ReplayReport {
    pub checkpoints_verified: usize,
    pub read_mismatches: Vec<ReadMismatch>,
    pub checkpoint_mismatches: Vec<CheckpointMismatch>,
    pub unsupported_events: Vec<SidBusEventId>,
}

impl ReplayReport {
    #[must_use]
    pub fn exact(&self) -> bool {
        self.read_mismatches.is_empty()
            && self.checkpoint_mismatches.is_empty()
            && self.unsupported_events.is_empty()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CaptureError {
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("capture schema version {found:?} is unsupported; expected {supported:?}")]
    UnsupportedSchemaVersion {
        found: CaptureSchemaVersion,
        supported: CaptureSchemaVersion,
    },
    #[error("checkpoint cadence must be greater than zero")]
    ZeroCheckpointCadence,
    #[error("event ID is not monotonic: expected {expected:?}, found {found:?}")]
    NonMonotonicEventId {
        expected: SidBusEventId,
        found: SidBusEventId,
    },
    #[error("event cycle moved backwards between {before:?} and {after:?}")]
    EventCycleMovedBackwards {
        before: SidBusEventId,
        after: SidBusEventId,
    },
    #[error("checkpoint {checkpoint:?} points past the event stream at {next_event:?}")]
    CheckpointPastEventStream {
        checkpoint: CheckpointId,
        next_event: SidBusEventId,
    },
    #[error("checkpoint {0:?} was not found")]
    CheckpointNotFound(CheckpointId),
    #[error("capture has no primary SID descriptor at chip zero")]
    MissingPrimaryChip,
    #[error("primary SID descriptor disagrees with capture metadata: {0:?}")]
    InvalidPrimaryChip(CapturedSidChip),
    #[error("capture has duplicate SID chip ID {0:?}")]
    DuplicateChipId(SidChipId),
    #[error("capture has duplicate SID base address {0:?}")]
    DuplicateChipBase(SidAddress),
    #[error("event {event:?} refers to unknown SID chip {chip:?}")]
    UnknownEventChip {
        event: SidBusEventId,
        chip: SidChipId,
    },
    #[error("base-window event {0:?} has no resolved register")]
    BaseEventWithoutRegister(SidBusEventId),
}

#[must_use]
pub(crate) fn resolved_register(
    address: SidAddress,
    class: SidAddressClass,
) -> Option<SidRegister> {
    match class {
        SidAddressClass::Base => {
            let offset = address.0.saturating_sub(0xd400) as u8;
            (offset <= SID_REGISTER_LAST.0).then_some(SidRegister(offset))
        }
        SidAddressClass::Mirror => {
            let offset = ((address.0 - 0xd400) & 0x1f) as u8;
            (offset <= SID_REGISTER_LAST.0).then_some(SidRegister(offset))
        }
    }
}
