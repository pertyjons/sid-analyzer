use super::evidence::Evidence;
use super::ids::SoundRegionId;
use super::time::SourceSpan;
use crate::analysis::VoiceId;
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[must_use]
pub enum SoundRegionKind {
    TonalAttack,
    Sustain,
    ReleaseTail,
    NoiseTransient,
    WaveformTransient,
    ContinuousTexture,
    SilentModulator,
    SilentParked,
    DigiStream,
}

impl SoundRegionKind {
    #[must_use]
    pub(crate) fn is_physical_timeline(self) -> bool {
        matches!(
            self,
            Self::TonalAttack
                | Self::Sustain
                | Self::ReleaseTail
                | Self::NoiseTransient
                | Self::SilentModulator
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct SoundRegion {
    pub id: SoundRegionId,
    pub voice: Option<VoiceId>,
    pub kind: SoundRegionKind,
    pub span: SourceSpan,
    pub evidence: Vec<Evidence>,
}
