use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sid_analyzer::emu::sid::{DigitalSid, DigitalSidCheckpoint, EnvPhase};
use sid_analyzer::header::SidModel;
use sid_analyzer::trace::{ChipCycle, SID_REGISTER_LAST, SidRegister};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

pub const ORACLE_SCHEMA_VERSION: OracleSchemaVersion = OracleSchemaVersion(1);
pub const ORACLE_POLICY_SCHEMA_VERSION: OraclePolicySchemaVersion = OraclePolicySchemaVersion(1);
const LIBRESIDFP_VERSION: &str = "1.1.2";
const LIBRESIDFP_REVISION: &str = "a5cd8f2486d627c40ea8c7c7a25827db73837002";
const LIBRESIDFP_ARCHIVE_SHA256: &str =
    "a753d61fb0ae554a0f9224363ea57ed0c43741169edb98ec32c53b96d0412719";
const LIBRESIDFP_BUILD_FLAGS: &str =
    "libresidfp CXXFLAGS=-O2 CPPFLAGS=<empty> LDFLAGS=<empty>; generator CXXFLAGS=-std=c++23 -O2";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
#[must_use]
pub struct OracleSchemaVersion(pub u16);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
#[must_use]
pub struct OraclePolicySchemaVersion(pub u16);

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
#[must_use]
pub struct OracleCaseId(pub String);

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
#[must_use]
pub struct OracleObservationId(pub String);

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
#[must_use]
pub struct OracleFieldPath(pub String);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
#[must_use]
pub struct OracleSequence(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
#[must_use]
pub struct SidRegisterValue(pub u8);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct OracleSource {
    pub engine: String,
    pub repository_url: String,
    pub version: String,
    pub release_tag: String,
    pub revision: String,
    pub source_archive_sha256: String,
    pub generator_revision: String,
    pub compiler: String,
    pub build_flags: String,
    pub combined_waveforms: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct SidOracleDocument {
    pub schema_version: OracleSchemaVersion,
    pub source: OracleSource,
    pub cases: Vec<SidOracleCase>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct SidOracleCase {
    pub id: OracleCaseId,
    pub description: String,
    pub sid_model: OracleSidModel,
    pub operations: Vec<OracleOperation>,
    pub observations: Vec<OracleObservation>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[must_use]
pub enum OracleSidModel {
    Mos6581,
    Mos8580,
}

impl From<OracleSidModel> for SidModel {
    fn from(value: OracleSidModel) -> Self {
        match value {
            OracleSidModel::Mos6581 => Self::Mos6581,
            OracleSidModel::Mos8580 => Self::Mos8580,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
#[must_use]
pub enum OracleOperation {
    Write {
        sequence: OracleSequence,
        cycle: ChipCycle,
        register: SidRegister,
        value: SidRegisterValue,
    },
    Observe {
        sequence: OracleSequence,
        cycle: ChipCycle,
        observation: OracleObservationId,
    },
}

impl OracleOperation {
    fn sequence(&self) -> OracleSequence {
        match self {
            Self::Write { sequence, .. } | Self::Observe { sequence, .. } => *sequence,
        }
    }

    fn cycle(&self) -> ChipCycle {
        match self {
            Self::Write { cycle, .. } | Self::Observe { cycle, .. } => *cycle,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct OracleObservation {
    pub id: OracleObservationId,
    pub cycle: ChipCycle,
    pub values: BTreeMap<OracleFieldPath, Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct OraclePolicyManifest {
    pub schema_version: OraclePolicySchemaVersion,
    #[serde(default)]
    pub divergences: BTreeMap<String, OracleDivergenceDefinition>,
    pub rules: Vec<OraclePolicyRule>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct OracleDivergenceDefinition {
    pub reason: String,
    pub project_policy: String,
    pub planned_resolution: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct OraclePolicyRule {
    pub case: OracleCaseId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub observation: Option<OracleObservationId>,
    pub field: OracleFieldPath,
    #[serde(rename = "policy")]
    pub disposition: OracleDisposition,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "disposition", rename_all = "snake_case")]
#[must_use]
pub enum OracleDisposition {
    MustMatch,
    KnownDivergence { issue_id: String },
    NotComparable { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use]
pub struct OracleMismatch {
    pub case: OracleCaseId,
    pub observation: OracleObservationId,
    pub cycle: ChipCycle,
    pub field: OracleFieldPath,
    pub expected: Value,
    pub actual: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[must_use]
pub struct OracleReport {
    pub fields_checked: usize,
    pub known_divergences: Vec<OracleMismatch>,
}

#[derive(Debug, thiserror::Error)]
pub enum OracleError {
    #[error("failed to read oracle file {path}: {source}")]
    Read {
        path: String,
        source: std::io::Error,
    },
    #[error("failed to parse oracle file {path}: {source}")]
    Json {
        path: String,
        source: serde_json::Error,
    },
    #[error("unsupported oracle schema version {found:?}")]
    UnsupportedSchema { found: OracleSchemaVersion },
    #[error("invalid oracle source metadata: {reason}")]
    InvalidSource { reason: String },
    #[error("unsupported oracle policy schema version {found:?}")]
    UnsupportedPolicySchema { found: OraclePolicySchemaVersion },
    #[error("oracle case IDs must be sorted and unique: {before:?} before {after:?}")]
    NonCanonicalCases {
        before: OracleCaseId,
        after: OracleCaseId,
    },
    #[error("case {case:?} has duplicate sequence {sequence:?}")]
    DuplicateSequence {
        case: OracleCaseId,
        sequence: OracleSequence,
    },
    #[error("case {case:?} sequence moves backwards from {before:?} to {after:?}")]
    SequenceMovedBackwards {
        case: OracleCaseId,
        before: OracleSequence,
        after: OracleSequence,
    },
    #[error("case {case:?} moves backwards from cycle {before:?} to {after:?}")]
    CycleMovedBackwards {
        case: OracleCaseId,
        before: ChipCycle,
        after: ChipCycle,
    },
    #[error("case {case:?} writes unsupported SID register {register:?}")]
    UnsupportedRegister {
        case: OracleCaseId,
        register: SidRegister,
    },
    #[error("case {case:?} has duplicate observation {observation:?}")]
    DuplicateObservation {
        case: OracleCaseId,
        observation: OracleObservationId,
    },
    #[error("case {case:?} references missing observation {observation:?}")]
    MissingObservation {
        case: OracleCaseId,
        observation: OracleObservationId,
    },
    #[error("case {case:?} observation {observation:?} is declared at the wrong cycle")]
    ObservationCycleMismatch {
        case: OracleCaseId,
        observation: OracleObservationId,
    },
    #[error("case {case:?} observation {observation:?} is never referenced")]
    UnreferencedObservation {
        case: OracleCaseId,
        observation: OracleObservationId,
    },
    #[error(
        "duplicate oracle policy for case {case:?}, observation {observation:?}, field {field:?}"
    )]
    DuplicatePolicy {
        case: OracleCaseId,
        observation: Option<OracleObservationId>,
        field: OracleFieldPath,
    },
    #[error(
        "known divergence requires an exact observation selector for case {case:?}, field {field:?}"
    )]
    BroadKnownDivergence {
        case: OracleCaseId,
        field: OracleFieldPath,
    },
    #[error("known divergence references undefined issue {issue_id}")]
    UndefinedDivergence { issue_id: String },
    #[error(
        "oracle policy does not select a generated field: case {case:?}, observation {observation:?}, field {field:?}"
    )]
    UnusedPolicy {
        case: OracleCaseId,
        observation: Option<OracleObservationId>,
        field: OracleFieldPath,
    },
    #[error("missing policy for case {case:?}, observation {observation:?}, field {field:?}")]
    MissingPolicy {
        case: OracleCaseId,
        observation: OracleObservationId,
        field: OracleFieldPath,
    },
    #[error("multiple policies match case {case:?}, observation {observation:?}, field {field:?}")]
    AmbiguousPolicy {
        case: OracleCaseId,
        observation: OracleObservationId,
        field: OracleFieldPath,
    },
    #[error("unknown oracle field {0:?}")]
    UnknownField(OracleFieldPath),
    #[error("oracle mismatch in case {mismatch:?}")]
    Mismatch { mismatch: Box<OracleMismatch> },
    #[error(
        "known divergence unexpectedly matches in case {case:?}, observation {observation:?}, field {field:?}"
    )]
    DivergenceDisappeared {
        case: OracleCaseId,
        observation: OracleObservationId,
        field: OracleFieldPath,
    },
}

pub fn load_document(path: &Path) -> Result<SidOracleDocument, OracleError> {
    let bytes = std::fs::read(path).map_err(|source| OracleError::Read {
        path: path.display().to_string(),
        source,
    })?;
    serde_json::from_slice(&bytes).map_err(|source| OracleError::Json {
        path: path.display().to_string(),
        source,
    })
}

pub fn load_manifest(path: &Path) -> Result<OraclePolicyManifest, OracleError> {
    let bytes = std::fs::read(path).map_err(|source| OracleError::Read {
        path: path.display().to_string(),
        source,
    })?;
    serde_json::from_slice(&bytes).map_err(|source| OracleError::Json {
        path: path.display().to_string(),
        source,
    })
}

pub fn verify_document(
    document: &SidOracleDocument,
    manifest: &OraclePolicyManifest,
) -> Result<OracleReport, OracleError> {
    validate_document(document)?;
    validate_manifest(manifest)?;
    let mut report = OracleReport::default();
    for case in &document.cases {
        verify_case(case, manifest, &mut report)?;
    }
    Ok(report)
}

pub fn verify_documents(
    documents: &[SidOracleDocument],
    manifest: &OraclePolicyManifest,
) -> Result<OracleReport, OracleError> {
    validate_manifest(manifest)?;
    let mut report = OracleReport::default();
    let mut available = BTreeSet::new();
    for document in documents {
        validate_document(document)?;
        for case in &document.cases {
            for observation in &case.observations {
                for field in observation.values.keys() {
                    available.insert((case.id.clone(), observation.id.clone(), field.clone()));
                }
            }
            verify_case(case, manifest, &mut report)?;
        }
    }
    for rule in &manifest.rules {
        let selected = available.iter().any(|(case, observation, field)| {
            case == &rule.case
                && field == &rule.field
                && rule
                    .observation
                    .as_ref()
                    .is_none_or(|expected| expected == observation)
        });
        if !selected {
            return Err(OracleError::UnusedPolicy {
                case: rule.case.clone(),
                observation: rule.observation.clone(),
                field: rule.field.clone(),
            });
        }
    }
    Ok(report)
}

pub fn audit_document(document: &SidOracleDocument) -> Result<Vec<OracleMismatch>, OracleError> {
    validate_document(document)?;
    let mut mismatches = Vec::new();
    for case in &document.cases {
        let observations: BTreeMap<_, _> = case
            .observations
            .iter()
            .map(|observation| (&observation.id, observation))
            .collect();
        let mut sid = DigitalSid::with_model(case.sid_model.into());
        for operation in &case.operations {
            match operation {
                OracleOperation::Write {
                    cycle,
                    register,
                    value,
                    ..
                } => sid.write(*register, value.0, *cycle),
                OracleOperation::Observe {
                    cycle, observation, ..
                } => {
                    sid.clock_to(*cycle);
                    let expected = observations.get(observation).copied().ok_or_else(|| {
                        OracleError::MissingObservation {
                            case: case.id.clone(),
                            observation: observation.clone(),
                        }
                    })?;
                    let checkpoint = sid.checkpoint();
                    for (field, expected_value) in &expected.values {
                        let actual = field_value(&mut sid, &checkpoint, field)?;
                        if actual != *expected_value {
                            mismatches.push(OracleMismatch {
                                case: case.id.clone(),
                                observation: observation.clone(),
                                cycle: *cycle,
                                field: field.clone(),
                                expected: expected_value.clone(),
                                actual,
                            });
                        }
                    }
                }
            }
        }
    }
    Ok(mismatches)
}

fn validate_document(document: &SidOracleDocument) -> Result<(), OracleError> {
    if document.schema_version != ORACLE_SCHEMA_VERSION {
        return Err(OracleError::UnsupportedSchema {
            found: document.schema_version,
        });
    }
    validate_source(&document.source)?;
    for pair in document.cases.windows(2) {
        if pair[0].id >= pair[1].id {
            return Err(OracleError::NonCanonicalCases {
                before: pair[0].id.clone(),
                after: pair[1].id.clone(),
            });
        }
    }
    for case in &document.cases {
        validate_case(case)?;
    }
    Ok(())
}

fn validate_source(source: &OracleSource) -> Result<(), OracleError> {
    match source.engine.as_str() {
        "hand-authored-harness-smoke" => {
            if source.version.is_empty() || source.generator_revision.is_empty() {
                return Err(OracleError::InvalidSource {
                    reason: "hand-authored source metadata is incomplete".to_owned(),
                });
            }
        }
        "libresidfp" => {
            if source.version != LIBRESIDFP_VERSION
                || source.revision != LIBRESIDFP_REVISION
                || source.source_archive_sha256 != LIBRESIDFP_ARCHIVE_SHA256
                || source.generator_revision.len() != 64
                || !source
                    .generator_revision
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit())
                || source.compiler != "GCC 16.1.1"
                || source.build_flags != LIBRESIDFP_BUILD_FLAGS
                || source.repository_url != "https://github.com/libsidplayfp/libresidfp"
                || source.release_tag != "v1.1.2"
                || source.combined_waveforms != "average"
            {
                return Err(OracleError::InvalidSource {
                    reason: "libresidfp identity does not match the pinned source and toolchain"
                        .to_owned(),
                });
            }
        }
        engine => {
            return Err(OracleError::InvalidSource {
                reason: format!("unsupported engine {engine}"),
            });
        }
    }
    Ok(())
}

fn validate_case(case: &SidOracleCase) -> Result<(), OracleError> {
    let mut sequences = BTreeSet::new();
    let mut previous_cycle = ChipCycle(0);
    let mut previous_sequence = None;
    let mut observations = BTreeMap::new();
    for observation in &case.observations {
        if observations.insert(&observation.id, observation).is_some() {
            return Err(OracleError::DuplicateObservation {
                case: case.id.clone(),
                observation: observation.id.clone(),
            });
        }
    }
    let mut referenced = BTreeSet::new();
    for operation in &case.operations {
        if !sequences.insert(operation.sequence()) {
            return Err(OracleError::DuplicateSequence {
                case: case.id.clone(),
                sequence: operation.sequence(),
            });
        }
        if let Some(before) = previous_sequence
            && operation.sequence() < before
        {
            return Err(OracleError::SequenceMovedBackwards {
                case: case.id.clone(),
                before,
                after: operation.sequence(),
            });
        }
        previous_sequence = Some(operation.sequence());
        if operation.cycle() < previous_cycle {
            return Err(OracleError::CycleMovedBackwards {
                case: case.id.clone(),
                before: previous_cycle,
                after: operation.cycle(),
            });
        }
        previous_cycle = operation.cycle();
        match operation {
            OracleOperation::Write { register, .. } if *register > SID_REGISTER_LAST => {
                return Err(OracleError::UnsupportedRegister {
                    case: case.id.clone(),
                    register: *register,
                });
            }
            OracleOperation::Observe {
                cycle, observation, ..
            } => {
                let Some(expected) = observations.get(observation) else {
                    return Err(OracleError::MissingObservation {
                        case: case.id.clone(),
                        observation: observation.clone(),
                    });
                };
                if expected.cycle != *cycle {
                    return Err(OracleError::ObservationCycleMismatch {
                        case: case.id.clone(),
                        observation: observation.clone(),
                    });
                }
                referenced.insert(observation.clone());
            }
            OracleOperation::Write { .. } => {}
        }
    }
    for observation in observations.keys() {
        if !referenced.contains(*observation) {
            return Err(OracleError::UnreferencedObservation {
                case: case.id.clone(),
                observation: (*observation).clone(),
            });
        }
    }
    Ok(())
}

fn validate_manifest(manifest: &OraclePolicyManifest) -> Result<(), OracleError> {
    if manifest.schema_version != ORACLE_POLICY_SCHEMA_VERSION {
        return Err(OracleError::UnsupportedPolicySchema {
            found: manifest.schema_version,
        });
    }
    let mut rules = BTreeSet::new();
    for rule in &manifest.rules {
        let key = (
            rule.case.clone(),
            rule.observation.clone(),
            rule.field.clone(),
        );
        if !rules.insert(key) {
            return Err(OracleError::DuplicatePolicy {
                case: rule.case.clone(),
                observation: rule.observation.clone(),
                field: rule.field.clone(),
            });
        }
        if matches!(rule.disposition, OracleDisposition::KnownDivergence { .. })
            && rule.observation.is_none()
        {
            return Err(OracleError::BroadKnownDivergence {
                case: rule.case.clone(),
                field: rule.field.clone(),
            });
        }
        if let OracleDisposition::KnownDivergence { issue_id } = &rule.disposition
            && !manifest.divergences.contains_key(issue_id)
        {
            return Err(OracleError::UndefinedDivergence {
                issue_id: issue_id.clone(),
            });
        }
    }
    Ok(())
}

fn verify_case(
    case: &SidOracleCase,
    manifest: &OraclePolicyManifest,
    report: &mut OracleReport,
) -> Result<(), OracleError> {
    let observations: BTreeMap<_, _> = case
        .observations
        .iter()
        .map(|observation| (&observation.id, observation))
        .collect();
    let mut sid = DigitalSid::with_model(case.sid_model.into());
    for operation in &case.operations {
        match operation {
            OracleOperation::Write {
                cycle,
                register,
                value,
                ..
            } => sid.write(*register, value.0, *cycle),
            OracleOperation::Observe {
                cycle, observation, ..
            } => {
                sid.clock_to(*cycle);
                let expected = observations.get(observation).copied().ok_or_else(|| {
                    OracleError::MissingObservation {
                        case: case.id.clone(),
                        observation: observation.clone(),
                    }
                })?;
                verify_observation(case, expected, &mut sid, manifest, report)?;
            }
        }
    }
    Ok(())
}

fn verify_observation(
    case: &SidOracleCase,
    observation: &OracleObservation,
    sid: &mut DigitalSid,
    manifest: &OraclePolicyManifest,
    report: &mut OracleReport,
) -> Result<(), OracleError> {
    let checkpoint = sid.checkpoint();
    for (field, expected) in &observation.values {
        let exact_rules: Vec<_> = manifest
            .rules
            .iter()
            .filter(|rule| {
                rule.case == case.id
                    && rule.field == *field
                    && rule.observation.as_ref() == Some(&observation.id)
            })
            .collect();
        let generic_rules: Vec<_> = manifest
            .rules
            .iter()
            .filter(|rule| {
                rule.case == case.id && rule.field == *field && rule.observation.is_none()
            })
            .collect();
        let rules = if exact_rules.is_empty() {
            generic_rules
        } else {
            exact_rules
        };
        let rule = match rules.as_slice() {
            [] => {
                return Err(OracleError::MissingPolicy {
                    case: case.id.clone(),
                    observation: observation.id.clone(),
                    field: field.clone(),
                });
            }
            [rule] => *rule,
            _ => {
                return Err(OracleError::AmbiguousPolicy {
                    case: case.id.clone(),
                    observation: observation.id.clone(),
                    field: field.clone(),
                });
            }
        };
        if matches!(rule.disposition, OracleDisposition::NotComparable { .. }) {
            continue;
        }
        let actual = field_value(sid, &checkpoint, field)?;
        let mismatch = OracleMismatch {
            case: case.id.clone(),
            observation: observation.id.clone(),
            cycle: observation.cycle,
            field: field.clone(),
            expected: expected.clone(),
            actual,
        };
        match &rule.disposition {
            OracleDisposition::MustMatch if mismatch.actual != mismatch.expected => {
                return Err(OracleError::Mismatch {
                    mismatch: Box::new(mismatch),
                });
            }
            OracleDisposition::KnownDivergence { .. } if mismatch.actual == mismatch.expected => {
                return Err(OracleError::DivergenceDisappeared {
                    case: case.id.clone(),
                    observation: observation.id.clone(),
                    field: field.clone(),
                });
            }
            OracleDisposition::KnownDivergence { .. } => {
                report.known_divergences.push(mismatch);
            }
            OracleDisposition::MustMatch => {
                report.fields_checked += 1;
            }
            OracleDisposition::NotComparable { .. } => unreachable!(),
        }
    }
    Ok(())
}

fn field_value(
    sid: &mut DigitalSid,
    checkpoint: &DigitalSidCheckpoint,
    field: &OracleFieldPath,
) -> Result<Value, OracleError> {
    match field.0.as_str() {
        "public.env3" => Ok(json!(sid.read(SidRegister(0x1c), checkpoint.cycle))),
        "public.osc3" => Ok(json!(sid.read(SidRegister(0x1b), checkpoint.cycle))),
        _ => checkpoint_field_value(checkpoint, field)
            .ok_or_else(|| OracleError::UnknownField(field.clone())),
    }
}

fn checkpoint_field_value(
    checkpoint: &DigitalSidCheckpoint,
    field: &OracleFieldPath,
) -> Option<Value> {
    for voice in 0..3 {
        let prefix = format!("voices.{}", voice + 1);
        let Some(name) = field.0.strip_prefix(&format!("{prefix}.")) else {
            continue;
        };
        let oscillator = &checkpoint.oscillators[voice];
        let envelope = &checkpoint.envelopes[voice];
        let value = match name {
            "accumulator" => json!(oscillator.accumulator),
            "shift_register" => json!(oscillator.noise_shift_register),
            "noise_clock_count" => {
                json!(normalized_noise_clock_count(
                    oscillator.noise_shift_register,
                    oscillator.shift_pipeline.0
                )?)
            }
            "sync_resets" => json!(oscillator.sync_resets),
            "source_msb_edges" => json!(oscillator.source_msb_edges),
            "noise_poisoned" => json!(oscillator.noise_poisoned),
            "test_fill_at" => json!(oscillator.test_fill.0),
            "envelope.level" => json!(envelope.level.0),
            "envelope.phase" => json!(match envelope.phase {
                EnvPhase::Attack => "attack",
                EnvPhase::DecaySustain => "decay_sustain",
                EnvPhase::Release => "release",
            }),
            "envelope.rate_counter" => json!(envelope.rate_counter.0),
            "envelope.rate_period" => json!(envelope.rate_period.0),
            "envelope.exponential_counter" => json!(envelope.exponential_counter.0),
            "envelope.exponential_period" => json!(envelope.exponential_period.0),
            "envelope.gate" => json!(envelope.gate),
            "envelope.hold_zero" => json!(envelope.hold_zero),
            _ => return None,
        };
        return Some(value);
    }
    None
}

fn normalized_noise_clock_count(register: u32, pipeline: u8) -> Option<u64> {
    let mut current = 0x3f_ffff;
    for count in 0..=0x7f_ffff_u64 {
        if current == register {
            return Some(count + u64::from(pipeline != 0));
        }
        let feedback = ((current ^ (current >> 5)) & 1) << 22;
        current = (current >> 1) | feedback;
    }
    None
}
