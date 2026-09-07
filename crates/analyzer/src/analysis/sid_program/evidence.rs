use super::time::SourceSpan;
use crate::emu::capture::{CheckpointId, EventTimestampQuality, SidBusEventId};
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
#[must_use]
pub enum Provenance {
    ExactEmulation,
    AuthoredVerified,
    AuthoredDecoded,
    AuthoredPartial,
    TraceMeasured,
    TraceCorrected,
    RenderMeasured,
    Inferred,
    Approximated,
    Unsupported,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[must_use]
pub enum EvidenceValidity {
    Valid,
    TimingBounded,
    Unsupported,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
#[must_use]
pub enum SourceReference {
    PowerOn,
    Event(SidBusEventId),
    Checkpoint(CheckpointId),
    Span(SourceSpan),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(transparent)]
#[must_use]
pub struct ConfidencePermille(pub u16);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct Evidence {
    pub provenance: Provenance,
    pub confidence: ConfidencePermille,
    pub source: SourceReference,
    pub validity: EvidenceValidity,
}

impl Evidence {
    pub fn power_on() -> Self {
        Self {
            provenance: Provenance::ExactEmulation,
            confidence: ConfidencePermille(1000),
            source: SourceReference::PowerOn,
            validity: EvidenceValidity::Valid,
        }
    }

    pub fn exact_event(event: SidBusEventId, quality: EventTimestampQuality) -> Self {
        Self {
            provenance: Provenance::ExactEmulation,
            confidence: ConfidencePermille(1000),
            source: SourceReference::Event(event),
            validity: match quality {
                EventTimestampQuality::Exact => EvidenceValidity::Valid,
                EventTimestampQuality::InstructionStartBounded => EvidenceValidity::TimingBounded,
            },
        }
    }

    pub fn exact_checkpoint(checkpoint: CheckpointId) -> Self {
        Self {
            provenance: Provenance::ExactEmulation,
            confidence: ConfidencePermille(1000),
            source: SourceReference::Checkpoint(checkpoint),
            validity: EvidenceValidity::Valid,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct Evidenced<T> {
    pub value: T,
    pub evidence: Vec<Evidence>,
}

impl<T> Evidenced<T> {
    pub fn one(value: T, evidence: Evidence) -> Self {
        Self {
            value,
            evidence: vec![evidence],
        }
    }
}
