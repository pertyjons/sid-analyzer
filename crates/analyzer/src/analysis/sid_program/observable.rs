use super::ids::{RenderArtifactId, SoundRegionId};
use super::time::SourceSpan;
use crate::analysis::VoiceId;
use crate::audio::{AUDIO_FEATURE_VERSION, AudioFeatureProfile, AudioFeatureVersion, SampleRate};
use crate::emu::capture::{EventTimestampQuality, SidBusAccess, SidBusEventId, SidChipId};
use crate::header::SidModel;
use crate::trace::ChipCycle;
use crate::trace::SidRegister;
use md5::{Digest, Md5};
use serde::Serialize;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct RenderObservableSet {
    pub artifacts: Vec<RenderArtifactRef>,
    pub profiles: Vec<RenderObservableProfile>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct RenderArtifactRef {
    pub id: RenderArtifactId,
    pub content_digest: RenderContentDigest,
    pub location: ArtifactLocation,
    pub source_span: Option<SourceSpan>,
    pub source_region: Option<SoundRegionId>,
    pub tap: TapId,
    pub config: RenderConfig,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(transparent)]
#[must_use]
pub struct RenderContentDigest(pub [u8; 16]);

impl RenderContentDigest {
    #[must_use]
    pub fn hex(self) -> String {
        self.0.iter().map(|byte| format!("{byte:02x}")).collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
#[must_use]
pub struct ArtifactLocation(pub String);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[must_use]
pub enum TapId {
    FinalMix,
    Voice(VoiceId),
    PreFilter(VoiceId),
    FilteredBus,
    BypassBus,
    Digi,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct RenderConfig {
    pub renderer: RendererIdentity,
    pub sid_model: SidModel,
    pub sample_rate: SampleRate,
    pub normalization: RenderNormalization,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct RendererIdentity {
    pub name: String,
    pub revision: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[must_use]
pub enum RenderNormalization {
    None,
    Peak,
    Rms,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct RenderObservableProfile {
    pub artifact: RenderArtifactId,
    pub feature_version: AudioFeatureVersion,
    pub features: AudioFeatureProfile,
}

impl RenderObservableProfile {
    pub fn new(artifact: RenderArtifactId, features: AudioFeatureProfile) -> Self {
        Self {
            artifact,
            feature_version: AUDIO_FEATURE_VERSION,
            features,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct DigiPcmReconstruction {
    pub region: SoundRegionId,
    pub sample_rate: SampleRate,
    pub source_rate: DigiSampleRate,
    pub source_span: SourceSpan,
    pub source_events: Vec<DigiSourceEvent>,
    pub samples: Vec<DigiPcmSample>,
    pub timing: DigiTimingValidity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(transparent)]
#[must_use]
pub struct DigiSampleRate(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct DigiSourceEvent {
    pub event: SidBusEventId,
    pub at: ChipCycle,
    pub level: DigiLevel,
    pub timing: EventTimestampQuality,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(transparent)]
#[must_use]
pub struct DigiLevel(pub u8);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(transparent)]
#[must_use]
pub struct DigiPcmSample(pub i16);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[must_use]
pub enum DigiTimingValidity {
    Exact,
    InstructionStartBounded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct DigiStreamSummary {
    pub region: SoundRegionId,
    pub source_span: SourceSpan,
    pub source_events: DigiEventCount,
    pub source_rate: DigiSampleRate,
    pub timing: DigiTimingValidity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(transparent)]
#[must_use]
pub struct DigiEventCount(pub u64);

pub fn summarize_d418_streams(
    program: &super::AnalyzedSidProgram,
) -> Result<Vec<DigiStreamSummary>, DigiReconstructionError> {
    Ok(reconstruct_d418_streams(program, SampleRate(8_000))?
        .into_iter()
        .map(|stream| DigiStreamSummary {
            region: stream.region,
            source_span: stream.source_span,
            source_events: DigiEventCount(stream.source_events.len() as u64),
            source_rate: stream.source_rate,
            timing: stream.timing,
        })
        .collect())
}

pub fn reconstruct_d418_pcm(
    program: &super::AnalyzedSidProgram,
    sample_rate: SampleRate,
) -> Result<Option<DigiPcmReconstruction>, DigiReconstructionError> {
    Ok(reconstruct_d418_streams(program, sample_rate)?
        .into_iter()
        .next())
}

pub fn reconstruct_d418_streams(
    program: &super::AnalyzedSidProgram,
    sample_rate: SampleRate,
) -> Result<Vec<DigiPcmReconstruction>, DigiReconstructionError> {
    if sample_rate.0 == 0 {
        return Err(DigiReconstructionError::ZeroSampleRate);
    }
    let mut reconstructions = Vec::new();
    for region in program
        .semantic
        .regions
        .iter()
        .filter(|region| region.kind == super::region::SoundRegionKind::DigiStream)
    {
        let source_events: Vec<_> = program
            .capture
            .events
            .iter()
            .filter(|event| {
                event.chip == SidChipId::PRIMARY
                    && event.access == SidBusAccess::Write
                    && event.register == Some(SidRegister(0x18))
                    && region.span.start <= event.cycle
                    && event.cycle < region.span.end
            })
            .map(|event| DigiSourceEvent {
                event: event.id,
                at: event.cycle,
                level: DigiLevel(event.value & 0x0f),
                timing: event.timestamp_quality,
            })
            .collect();
        if source_events.len() < 2 {
            continue;
        }
        reconstructions.push(reconstruct_d418_stream(
            program,
            region.id,
            region.span,
            source_events,
            sample_rate,
        ));
    }
    Ok(reconstructions)
}

fn reconstruct_d418_stream(
    program: &super::AnalyzedSidProgram,
    region: SoundRegionId,
    source_span: SourceSpan,
    source_events: Vec<DigiSourceEvent>,
    sample_rate: SampleRate,
) -> DigiPcmReconstruction {
    let clock = u128::from(program.capture.system_clock.phi2_hz());
    let sample_count = ((u128::from(source_span.end.0 - source_span.start.0)
        * u128::from(sample_rate.0))
    .div_ceil(clock)) as usize;
    let mut samples = Vec::with_capacity(sample_count);
    let mut event_index = 0;
    let mut volume = source_events[0].level.0;
    for sample_index in 0..sample_count {
        let cycle = ChipCycle(
            source_span.start.0
                + ((sample_index as u128 * clock) / u128::from(sample_rate.0)) as u64,
        );
        while let Some(event) = source_events.get(event_index + 1)
            && event.at <= cycle
        {
            event_index += 1;
            volume = event.level.0;
        }
        let centered = i32::from(volume) * 4_369 - 32_767;
        samples.push(DigiPcmSample(centered.clamp(-32_768, 32_767) as i16));
    }
    let mut intervals: Vec<_> = source_events
        .windows(2)
        .filter_map(|events| {
            let interval = events[1].at.0.saturating_sub(events[0].at.0);
            (interval > 0).then_some(interval)
        })
        .collect();
    intervals.sort_unstable();
    let source_rate = intervals
        .get(intervals.len() / 2)
        .map_or(DigiSampleRate(0), |interval| {
            DigiSampleRate((u64::from(program.capture.system_clock.phi2_hz()) / interval) as u32)
        });
    DigiPcmReconstruction {
        region,
        sample_rate,
        source_rate,
        source_span,
        timing: if program.capture.timing_exact
            && source_events
                .iter()
                .all(|event| event.timing == EventTimestampQuality::Exact)
        {
            DigiTimingValidity::Exact
        } else {
            DigiTimingValidity::InstructionStartBounded
        },
        source_events,
        samples,
    }
}

pub fn persist_d418_wav(
    program: &mut super::AnalyzedSidProgram,
    sample_rate: SampleRate,
    directory: &Path,
) -> Result<Option<RenderArtifactRef>, DigiArtifactError> {
    Ok(persist_d418_wavs(program, sample_rate, directory)?
        .into_iter()
        .next())
}

pub fn persist_d418_wavs(
    program: &mut super::AnalyzedSidProgram,
    sample_rate: SampleRate,
    directory: &Path,
) -> Result<Vec<RenderArtifactRef>, DigiArtifactError> {
    let reconstructions = reconstruct_d418_streams(program, sample_rate)?;
    let mut artifacts = Vec::with_capacity(reconstructions.len());
    for reconstruction in reconstructions {
        artifacts.push(persist_d418_reconstruction(
            program,
            &reconstruction,
            directory,
        )?);
    }
    Ok(artifacts)
}

fn persist_d418_reconstruction(
    program: &mut super::AnalyzedSidProgram,
    reconstruction: &DigiPcmReconstruction,
    directory: &Path,
) -> Result<RenderArtifactRef, DigiArtifactError> {
    let bytes = encode_pcm16_wav(reconstruction);
    let digest = RenderContentDigest(Md5::digest(&bytes).into());
    fs::create_dir_all(directory).map_err(|source| DigiArtifactError::Write {
        path: directory.to_path_buf(),
        source,
    })?;
    let path = directory.join(format!("{}.wav", digest.hex()));
    if !path.exists() {
        let temporary = directory.join(format!(".{}.{}.tmp", digest.hex(), std::process::id()));
        fs::write(&temporary, &bytes).map_err(|source| DigiArtifactError::Write {
            path: temporary.clone(),
            source,
        })?;
        fs::rename(&temporary, &path).map_err(|source| DigiArtifactError::Write {
            path: path.clone(),
            source,
        })?;
    }
    let artifact = RenderArtifactRef {
        id: RenderArtifactId(program.observables.artifacts.len() as u64),
        content_digest: digest,
        location: ArtifactLocation(path.to_string_lossy().into_owned()),
        source_span: Some(reconstruction.source_span),
        source_region: Some(reconstruction.region),
        tap: TapId::Digi,
        config: RenderConfig {
            renderer: RendererIdentity {
                name: "sid-analyzer-d418-reconstruction".to_owned(),
                revision: env!("CARGO_PKG_VERSION").to_owned(),
            },
            sid_model: program.capture.sid_model,
            sample_rate: reconstruction.sample_rate,
            normalization: RenderNormalization::None,
        },
    };
    let wav = crate::audio::WavData {
        sample_rate: reconstruction.sample_rate,
        samples: reconstruction
            .samples
            .iter()
            .map(|sample| f64::from(sample.0) / 32_768.0)
            .collect(),
        clipped_samples: crate::audio::ClippedSampleCount(0),
    };
    let features = crate::audio::measure_features(&wav, crate::audio::FeatureConfig::default())?;
    program
        .observables
        .profiles
        .push(RenderObservableProfile::new(artifact.id, features));
    program.observables.artifacts.push(artifact.clone());
    Ok(artifact)
}

fn encode_pcm16_wav(reconstruction: &DigiPcmReconstruction) -> Vec<u8> {
    let data_len = reconstruction.samples.len().saturating_mul(2);
    let riff_len = 36_u32.saturating_add(u32::try_from(data_len).unwrap_or(u32::MAX));
    let byte_rate = reconstruction.sample_rate.0.saturating_mul(2);
    let mut bytes = Vec::with_capacity(data_len.saturating_add(44));
    bytes.extend_from_slice(b"RIFF");
    bytes.extend_from_slice(&riff_len.to_le_bytes());
    bytes.extend_from_slice(b"WAVEfmt ");
    bytes.extend_from_slice(&16_u32.to_le_bytes());
    bytes.extend_from_slice(&1_u16.to_le_bytes());
    bytes.extend_from_slice(&1_u16.to_le_bytes());
    bytes.extend_from_slice(&reconstruction.sample_rate.0.to_le_bytes());
    bytes.extend_from_slice(&byte_rate.to_le_bytes());
    bytes.extend_from_slice(&2_u16.to_le_bytes());
    bytes.extend_from_slice(&16_u16.to_le_bytes());
    bytes.extend_from_slice(b"data");
    bytes.extend_from_slice(&u32::try_from(data_len).unwrap_or(u32::MAX).to_le_bytes());
    for sample in &reconstruction.samples {
        bytes.extend_from_slice(&sample.0.to_le_bytes());
    }
    bytes
}

#[derive(Debug, thiserror::Error)]
pub enum DigiReconstructionError {
    #[error("D418 reconstruction sample rate must be greater than zero")]
    ZeroSampleRate,
}

#[derive(Debug, thiserror::Error)]
pub enum DigiArtifactError {
    #[error(transparent)]
    Reconstruction(#[from] DigiReconstructionError),
    #[error(transparent)]
    Features(#[from] crate::audio::FeatureError),
    #[error("failed to write digi artifact {path}: {source}")]
    Write { path: PathBuf, source: io::Error },
}
