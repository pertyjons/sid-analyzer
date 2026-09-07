use super::evidence::Evidence;
use super::time::SourceSpan;
use crate::analysis::VoiceId;
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(tag = "kind", content = "voice", rename_all = "snake_case")]
#[must_use]
pub enum TopologyNodeId {
    Oscillator(VoiceId),
    Envelope(VoiceId),
    PreFilterTap(VoiceId),
    Filter,
    FilteredBus,
    BypassBus,
    FinalMixer,
    ExternalInput,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[must_use]
pub enum TopologyEdgeKind {
    Audio,
    AmplitudeControl,
    FilterRoute,
    BypassRoute,
    Sync,
    Ring,
    Mix,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct TopologyEdge {
    pub source: TopologyNodeId,
    pub destination: TopologyNodeId,
    pub kind: TopologyEdgeKind,
    pub span: SourceSpan,
    pub evidence: Evidence,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct SidTopology {
    pub nodes: Vec<TopologyNodeId>,
    pub edges: Vec<TopologyEdge>,
}
