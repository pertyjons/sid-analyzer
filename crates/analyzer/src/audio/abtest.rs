use super::{
    AudioComparison, AudioFeatureProfile, AudioWindow, Decibels, FeatureConfig, FrequencyHz,
    SampleRate, WavData, compare_profiles, measure_features, parse_wav,
};
use crate::analysis::VoiceId;
use crate::header::SubtuneIndex;
use md5::{Digest, Md5};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize, Serialize)]
#[serde(transparent)]
#[must_use]
pub struct AudioContentDigest(pub [u8; 16]);

impl AudioContentDigest {
    #[must_use]
    pub fn hex(self) -> String {
        self.0.iter().map(|byte| format!("{byte:02x}")).collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct AudioBudget {
    pub maximum_rms_error: f64,
    pub maximum_peak_error: f64,
    #[serde(default)]
    pub maximum_level_error_db: Option<Decibels>,
    pub maximum_pitch_error: FrequencyHz,
    pub maximum_log_spectral_distance: Decibels,
    #[serde(default)]
    pub maximum_gain_normalized_log_spectral_distance: Option<Decibels>,
    pub maximum_centroid_error: FrequencyHz,
    pub maximum_rolloff_error: FrequencyHz,
    pub maximum_flatness_error: f64,
    pub maximum_zero_crossing_error: f64,
}

impl Default for AudioBudget {
    fn default() -> Self {
        Self {
            maximum_rms_error: 0.05,
            maximum_peak_error: 0.1,
            maximum_level_error_db: None,
            maximum_pitch_error: FrequencyHz(10.0),
            maximum_log_spectral_distance: Decibels(6.0),
            maximum_gain_normalized_log_spectral_distance: None,
            maximum_centroid_error: FrequencyHz(250.0),
            maximum_rolloff_error: FrequencyHz(500.0),
            maximum_flatness_error: 0.15,
            maximum_zero_crossing_error: 0.05,
        }
    }
}

impl AudioBudget {
    #[must_use]
    pub fn accepts(self, comparison: &AudioComparison) -> bool {
        comparison.rms_error <= self.maximum_rms_error
            && comparison.peak_error <= self.maximum_peak_error
            && self.maximum_level_error_db.is_none_or(|maximum| {
                comparison
                    .level_error_db
                    .is_some_and(|error| error.0 <= maximum.0)
            })
            && comparison
                .pitch_error_hz
                .is_none_or(|error| error.0 <= self.maximum_pitch_error.0)
            && comparison.log_spectral_distance.0 <= self.maximum_log_spectral_distance.0
            && self
                .maximum_gain_normalized_log_spectral_distance
                .is_none_or(|maximum| {
                    comparison
                        .gain_normalized_log_spectral_distance
                        .is_some_and(|error| error.0 <= maximum.0)
                })
            && comparison.centroid_error_hz.0 <= self.maximum_centroid_error.0
            && comparison.rolloff_error_hz.0 <= self.maximum_rolloff_error.0
            && comparison.flatness_error <= self.maximum_flatness_error
            && comparison.zero_crossing_error <= self.maximum_zero_crossing_error
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(transparent)]
#[must_use]
pub struct FixtureName(pub String);

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(transparent)]
#[must_use]
pub struct BudgetName(pub String);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(transparent)]
#[must_use]
pub struct RenderFixtureSchemaVersion(pub u16);

pub const RENDER_FIXTURE_SCHEMA_VERSION: RenderFixtureSchemaVersion = RenderFixtureSchemaVersion(2);

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct RenderFixtureManifest {
    pub schema_version: RenderFixtureSchemaVersion,
    pub feature_config: FeatureConfig,
    pub budgets: BTreeMap<BudgetName, AudioBudget>,
    pub fixtures: Vec<RenderFixture>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct RenderFixture {
    pub name: FixtureName,
    pub reference: FixtureAudioSource,
    pub candidates: Vec<RenderCandidate>,
    pub provenance: RenderFixtureProvenance,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[must_use]
pub enum RenderFixtureProvenance {
    GeneratedProfile {
        recipe: String,
    },
    SourceAligned {
        #[serde(flatten)]
        receipt: Box<SourceAlignedRenderProvenance>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct SourceAlignedRenderProvenance {
    pub composer: String,
    pub title: String,
    pub sid_path: PathBuf,
    pub sid_md5: Option<Md5DigestPin>,
    pub subtune: SubtuneIndex,
    pub voice: Option<VoiceId>,
    pub window: AudioWindow,
    pub sample_rate: SampleRate,
    pub sid_model: FixtureSidModel,
    pub reference_renderer: ToolPin,
    pub candidate_renderer: ToolPin,
    pub project_schema: String,
    pub analyzer_commit: GitCommitPin,
    pub feature_cache_key: String,
    pub candidate_artifacts: BTreeMap<FixtureName, RenderArtifactPin>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
#[must_use]
pub enum FixtureSidModel {
    Mos6581,
    Mos8580,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(transparent)]
#[must_use]
pub struct Md5DigestPin(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(transparent)]
#[must_use]
pub struct Sha256DigestPin(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(transparent)]
#[must_use]
pub struct GitCommitPin(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct ToolPin {
    pub name: String,
    pub version: Option<String>,
    pub commit: Option<GitCommitPin>,
    pub dirty: bool,
    pub executable_sha256: Option<Sha256DigestPin>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct RenderArtifactPin {
    pub project_sha256: Sha256DigestPin,
    pub rendered_wav_sha256: Sha256DigestPin,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub renderer: Option<ToolPin>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "kind", content = "path", rename_all = "snake_case")]
#[must_use]
pub enum FixtureAudioSource {
    Wav(PathBuf),
    Profile(PathBuf),
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct RenderCandidate {
    pub name: FixtureName,
    pub source: FixtureAudioSource,
    pub budget: BudgetName,
    pub expected: CandidateExpectation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
#[must_use]
pub enum CandidateExpectation {
    Accept,
    Reject,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct RenderFixtureMatrixReport {
    pub schema_version: RenderFixtureSchemaVersion,
    pub manifest_digest: AudioContentDigest,
    pub feature_config: FeatureConfig,
    pub fixtures: Vec<RenderFixtureReport>,
    pub expectations_met: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct RenderFixtureReport {
    pub name: FixtureName,
    pub provenance: RenderFixtureProvenance,
    pub reference_digest: AudioContentDigest,
    pub reference: AudioFeatureProfile,
    pub candidates: Vec<RenderCandidateReport>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct RenderCandidateReport {
    pub name: FixtureName,
    pub candidate_digest: AudioContentDigest,
    pub candidate: AudioFeatureProfile,
    pub comparison: AudioComparison,
    pub budget_name: BudgetName,
    pub budget: AudioBudget,
    pub expected: CandidateExpectation,
    pub accepted: bool,
    pub expectation_met: bool,
}

pub fn run_fixture_matrix(
    manifest_path: &Path,
    cache: Option<&Path>,
) -> Result<RenderFixtureMatrixReport, AbTestError> {
    let manifest_bytes = fs::read(manifest_path).map_err(|source| AbTestError::Read {
        path: manifest_path.to_path_buf(),
        source,
    })?;
    let manifest: RenderFixtureManifest = serde_json::from_slice(&manifest_bytes)?;
    if manifest.schema_version != RENDER_FIXTURE_SCHEMA_VERSION {
        return Err(AbTestError::FixtureSchemaVersion {
            found: manifest.schema_version,
            supported: RENDER_FIXTURE_SCHEMA_VERSION,
        });
    }
    manifest.feature_config.validate()?;
    let base = manifest_path.parent().unwrap_or_else(|| Path::new("."));
    let mut fixture_reports = Vec::with_capacity(manifest.fixtures.len());
    let mut expectations_met = true;
    for fixture in manifest.fixtures {
        validate_fixture_provenance(&fixture, manifest.feature_config)?;
        let (reference_digest, reference) =
            load_fixture_source(base, &fixture.reference, manifest.feature_config, cache)?;
        validate_source_aligned_profile(&fixture.provenance, &reference)?;
        let mut candidates = Vec::with_capacity(fixture.candidates.len());
        for candidate in fixture.candidates {
            let budget = manifest
                .budgets
                .get(&candidate.budget)
                .copied()
                .ok_or_else(|| AbTestError::UnknownBudget(candidate.budget.clone()))?;
            let (candidate_digest, candidate_profile) =
                load_fixture_source(base, &candidate.source, manifest.feature_config, cache)?;
            validate_source_aligned_profile(&fixture.provenance, &candidate_profile)?;
            if reference.sample_rate != candidate_profile.sample_rate {
                return Err(AbTestError::SampleRateMismatch {
                    reference: reference.sample_rate.0,
                    candidate: candidate_profile.sample_rate.0,
                });
            }
            let comparison = compare_profiles(&reference, &candidate_profile);
            let accepted = budget.accepts(&comparison);
            let expectation_met = matches!(
                (candidate.expected, accepted),
                (CandidateExpectation::Accept, true) | (CandidateExpectation::Reject, false)
            );
            expectations_met &= expectation_met;
            candidates.push(RenderCandidateReport {
                name: candidate.name,
                candidate_digest,
                candidate: candidate_profile,
                comparison,
                budget_name: candidate.budget,
                budget,
                expected: candidate.expected,
                accepted,
                expectation_met,
            });
        }
        fixture_reports.push(RenderFixtureReport {
            name: fixture.name,
            provenance: fixture.provenance,
            reference_digest,
            reference,
            candidates,
        });
    }
    Ok(RenderFixtureMatrixReport {
        schema_version: RENDER_FIXTURE_SCHEMA_VERSION,
        manifest_digest: AudioContentDigest(Md5::digest(&manifest_bytes).into()),
        feature_config: manifest.feature_config,
        fixtures: fixture_reports,
        expectations_met,
    })
}

fn validate_fixture_provenance(
    fixture: &RenderFixture,
    feature_config: FeatureConfig,
) -> Result<(), AbTestError> {
    let RenderFixtureProvenance::SourceAligned { receipt } = &fixture.provenance else {
        return Ok(());
    };
    let expected_cache_key = format!(
        "features-v{}-fft{}",
        feature_config.version.0, feature_config.fft_size.0
    );
    if receipt.feature_cache_key != expected_cache_key {
        return Err(AbTestError::FixtureProvenance {
            fixture: fixture.name.clone(),
            reason: format!(
                "feature cache key {:?} does not match {expected_cache_key:?}",
                receipt.feature_cache_key
            ),
        });
    }
    for candidate in &fixture.candidates {
        if !receipt.candidate_artifacts.contains_key(&candidate.name) {
            return Err(AbTestError::FixtureProvenance {
                fixture: fixture.name.clone(),
                reason: format!(
                    "candidate {:?} has no pinned render artifact",
                    candidate.name
                ),
            });
        }
    }
    if receipt.candidate_artifacts.len() != fixture.candidates.len() {
        return Err(AbTestError::FixtureProvenance {
            fixture: fixture.name.clone(),
            reason: "candidate artifact pins do not match the fixture candidates".to_owned(),
        });
    }
    Ok(())
}

fn validate_source_aligned_profile(
    provenance: &RenderFixtureProvenance,
    profile: &AudioFeatureProfile,
) -> Result<(), AbTestError> {
    let RenderFixtureProvenance::SourceAligned { receipt } = provenance else {
        return Ok(());
    };
    if profile.sample_rate != receipt.sample_rate || profile.samples.0 != receipt.window.length.0 {
        return Err(AbTestError::FixtureProvenance {
            fixture: FixtureName(receipt.title.clone()),
            reason: format!(
                "profile has {} samples at {} Hz; expected {} samples at {} Hz",
                profile.samples.0,
                profile.sample_rate.0,
                receipt.window.length.0,
                receipt.sample_rate.0
            ),
        });
    }
    Ok(())
}

fn load_fixture_source(
    base: &Path,
    source: &FixtureAudioSource,
    config: FeatureConfig,
    cache: Option<&Path>,
) -> Result<(AudioContentDigest, AudioFeatureProfile), AbTestError> {
    let path = match source {
        FixtureAudioSource::Wav(path) | FixtureAudioSource::Profile(path) => base.join(path),
    };
    match source {
        FixtureAudioSource::Wav(_) => profile_file(&path, config, cache),
        FixtureAudioSource::Profile(_) => {
            let bytes = fs::read(&path).map_err(|source| AbTestError::Read {
                path: path.clone(),
                source,
            })?;
            let profile: AudioFeatureProfile = serde_json::from_slice(&bytes)?;
            validate_profile_config(&path, &profile, config)?;
            Ok((AudioContentDigest(Md5::digest(&bytes).into()), profile))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct AbTestReport {
    pub schema_version: AbTestSchemaVersion,
    pub window: Option<AudioWindow>,
    pub reference_digest: AudioContentDigest,
    pub candidate_digest: AudioContentDigest,
    pub feature_config: FeatureConfig,
    pub reference: AudioFeatureProfile,
    pub candidate: AudioFeatureProfile,
    pub comparison: AudioComparison,
    pub budget: AudioBudget,
    pub accepted: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(transparent)]
#[must_use]
pub struct AbTestSchemaVersion(pub u16);

pub const ABTEST_SCHEMA_VERSION: AbTestSchemaVersion = AbTestSchemaVersion(2);

pub fn compare_wav_files(
    reference: &Path,
    candidate: &Path,
    feature_config: FeatureConfig,
    budget: AudioBudget,
    cache: Option<&Path>,
) -> Result<AbTestReport, AbTestError> {
    compare_wav_files_windowed(reference, candidate, feature_config, budget, cache, None)
}

pub fn compare_wav_files_windowed(
    reference: &Path,
    candidate: &Path,
    feature_config: FeatureConfig,
    budget: AudioBudget,
    cache: Option<&Path>,
    window: Option<AudioWindow>,
) -> Result<AbTestReport, AbTestError> {
    let (reference_digest, reference) =
        profile_file_windowed(reference, feature_config, cache, window)?;
    let (candidate_digest, candidate) =
        profile_file_windowed(candidate, feature_config, cache, window)?;
    if reference.sample_rate != candidate.sample_rate {
        return Err(AbTestError::SampleRateMismatch {
            reference: reference.sample_rate.0,
            candidate: candidate.sample_rate.0,
        });
    }
    let comparison = compare_profiles(&reference, &candidate);
    let accepted = budget.accepts(&comparison);
    Ok(AbTestReport {
        schema_version: ABTEST_SCHEMA_VERSION,
        window,
        reference_digest,
        candidate_digest,
        feature_config,
        reference,
        candidate,
        comparison,
        budget,
        accepted,
    })
}

fn profile_file(
    path: &Path,
    config: FeatureConfig,
    cache: Option<&Path>,
) -> Result<(AudioContentDigest, AudioFeatureProfile), AbTestError> {
    profile_file_windowed(path, config, cache, None)
}

fn profile_file_windowed(
    path: &Path,
    config: FeatureConfig,
    cache: Option<&Path>,
    window: Option<AudioWindow>,
) -> Result<(AudioContentDigest, AudioFeatureProfile), AbTestError> {
    config.validate()?;
    let bytes = fs::read(path).map_err(|source| AbTestError::Read {
        path: path.to_path_buf(),
        source,
    })?;
    let digest = AudioContentDigest(Md5::digest(&bytes).into());
    let window_key = window.map_or_else(String::new, |window| {
        format!("-start{}-length{}", window.start.0, window.length.0)
    });
    let cache_path = cache.map(|directory| {
        directory.join(format!(
            "{}-features-v{}-fft{}{}.json",
            digest.hex(),
            config.version.0,
            config.fft_size.0,
            window_key
        ))
    });
    if let Some(cache_path) = &cache_path
        && let Ok(cached) = fs::read(cache_path)
    {
        let profile = serde_json::from_slice::<AudioFeatureProfile>(&cached).map_err(|source| {
            AbTestError::CacheJson {
                path: cache_path.clone(),
                source,
            }
        })?;
        validate_profile_config(cache_path, &profile, config)?;
        return Ok((digest, profile));
    }
    let mut wav = parse_wav(&bytes).map_err(|source| AbTestError::Wav {
        path: path.to_path_buf(),
        source,
    })?;
    if let Some(window) = window {
        wav = windowed_wav(path, wav, window)?;
    }
    let measured = measure_features(&wav, config)?;
    let encoded = serde_json::to_vec(&measured)?;
    let profile = serde_json::from_slice(&encoded)?;
    if let Some(cache_path) = cache_path {
        if let Some(parent) = cache_path.parent() {
            fs::create_dir_all(parent).map_err(|source| AbTestError::CacheWrite {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        fs::write(&cache_path, encoded).map_err(|source| AbTestError::CacheWrite {
            path: cache_path,
            source,
        })?;
    }
    Ok((digest, profile))
}

fn windowed_wav(path: &Path, wav: WavData, window: AudioWindow) -> Result<WavData, AbTestError> {
    let Some(end) = window.end() else {
        return Err(AbTestError::WindowOutOfBounds {
            path: path.to_path_buf(),
            start: window.start,
            length: window.length,
            available: super::SampleIndex(wav.samples.len()),
        });
    };
    let Some(samples) = wav.samples.get(window.start.0..end.0) else {
        return Err(AbTestError::WindowOutOfBounds {
            path: path.to_path_buf(),
            start: window.start,
            length: window.length,
            available: super::SampleIndex(wav.samples.len()),
        });
    };
    Ok(WavData {
        sample_rate: wav.sample_rate,
        samples: samples.to_vec(),
        clipped_samples: wav.clipped_samples,
    })
}

fn validate_profile_config(
    path: &Path,
    profile: &AudioFeatureProfile,
    expected: FeatureConfig,
) -> Result<(), AbTestError> {
    if profile.feature_config != expected {
        return Err(AbTestError::ProfileConfig {
            path: path.to_path_buf(),
            found: profile.feature_config,
            expected,
        });
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum AbTestError {
    #[error("failed to read {path}: {source}")]
    Read { path: PathBuf, source: io::Error },
    #[error("failed to parse WAV {path}: {source}")]
    Wav {
        path: PathBuf,
        source: super::WavError,
    },
    #[error(transparent)]
    Features(#[from] super::FeatureError),
    #[error("cached feature profile {path} is invalid: {source}")]
    CacheJson {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("failed to write feature cache {path}: {source}")]
    CacheWrite { path: PathBuf, source: io::Error },
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("sample rates differ: reference is {reference} Hz and candidate is {candidate} Hz")]
    SampleRateMismatch { reference: u32, candidate: u32 },
    #[error(
        "audio window start {start:?} length {length:?} exceeds {available:?} samples in {path}"
    )]
    WindowOutOfBounds {
        path: PathBuf,
        start: super::SampleIndex,
        length: super::SampleIndex,
        available: super::SampleIndex,
    },
    #[error("render fixture schema {found:?} is unsupported; expected {supported:?}")]
    FixtureSchemaVersion {
        found: RenderFixtureSchemaVersion,
        supported: RenderFixtureSchemaVersion,
    },
    #[error("render fixture refers to unknown audio budget {0:?}")]
    UnknownBudget(BudgetName),
    #[error("audio profile {path} has config {found:?}; manifest requires {expected:?}")]
    ProfileConfig {
        path: PathBuf,
        found: FeatureConfig,
        expected: FeatureConfig,
    },
    #[error("render fixture {fixture:?} has invalid provenance: {reason}")]
    FixtureProvenance {
        fixture: FixtureName,
        reason: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::SampleRate;

    fn wav(samples: &[i16]) -> Vec<u8> {
        let data_len = samples.len() * 2;
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36 + data_len as u32).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16_u32.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&44_100_u32.to_le_bytes());
        bytes.extend_from_slice(&88_200_u32.to_le_bytes());
        bytes.extend_from_slice(&2_u16.to_le_bytes());
        bytes.extend_from_slice(&16_u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&(data_len as u32).to_le_bytes());
        for sample in samples {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        bytes
    }

    fn separated_comparison() -> AudioComparison {
        AudioComparison {
            alignment: super::super::AlignmentSamples(0),
            rms_error: 0.0,
            peak_error: 0.0,
            level_error_db: Some(Decibels(4.0)),
            pitch_error_hz: Some(FrequencyHz(0.0)),
            log_spectral_distance: Decibels(4.5),
            gain_normalized_log_spectral_distance: Some(Decibels(2.0)),
            centroid_error_hz: FrequencyHz(0.0),
            rolloff_error_hz: FrequencyHz(0.0),
            flatness_error: 0.0,
            zero_crossing_error: 0.0,
        }
    }

    #[test]
    fn optional_level_and_shape_budgets_are_enforced_independently() {
        let level_reject = AudioBudget {
            maximum_level_error_db: Some(Decibels(3.0)),
            ..AudioBudget::default()
        };
        let shape_reject = AudioBudget {
            maximum_gain_normalized_log_spectral_distance: Some(Decibels(1.0)),
            ..AudioBudget::default()
        };
        let accepted = AudioBudget {
            maximum_level_error_db: Some(Decibels(4.0)),
            maximum_gain_normalized_log_spectral_distance: Some(Decibels(2.0)),
            ..AudioBudget::default()
        };

        assert!(!level_reject.accepts(&separated_comparison()));
        assert!(!shape_reject.accepts(&separated_comparison()));
        assert!(accepted.accepts(&separated_comparison()));

        let mut unavailable = separated_comparison();
        unavailable.level_error_db = None;
        unavailable.gain_normalized_log_spectral_distance = None;
        assert!(!accepted.accepts(&unavailable));
    }

    #[test]
    fn cached_and_uncached_reports_match() {
        let root = std::env::temp_dir().join(format!("sid-analyzer-abtest-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let reference = root.join("reference.wav");
        let candidate = root.join("candidate.wav");
        let samples: Vec<i16> = (0..SampleRate(44_100).0)
            .map(|index| {
                let phase = 2.0 * std::f64::consts::PI * 440.0 * f64::from(index) / 44_100.0;
                (phase.sin() * 16_000.0) as i16
            })
            .collect();
        fs::write(&reference, wav(&samples)).unwrap();
        fs::write(&candidate, wav(&samples)).unwrap();

        let uncached = compare_wav_files(
            &reference,
            &candidate,
            FeatureConfig::default(),
            AudioBudget::default(),
            None,
        )
        .unwrap();
        let cached_first = compare_wav_files(
            &reference,
            &candidate,
            FeatureConfig::default(),
            AudioBudget::default(),
            Some(&root.join("cache")),
        )
        .unwrap();
        let cached_second = compare_wav_files(
            &reference,
            &candidate,
            FeatureConfig::default(),
            AudioBudget::default(),
            Some(&root.join("cache")),
        )
        .unwrap();
        assert_eq!(
            serde_json::to_vec(&uncached).unwrap(),
            serde_json::to_vec(&cached_first).unwrap()
        );
        assert_eq!(
            serde_json::to_vec(&cached_first).unwrap(),
            serde_json::to_vec(&cached_second).unwrap()
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn windowed_comparison_measures_only_the_requested_source_region() {
        let root =
            std::env::temp_dir().join(format!("sid-analyzer-abtest-window-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let reference = root.join("reference.wav");
        let candidate = root.join("candidate.wav");
        let mut reference_samples = vec![0; 88_200];
        let mut candidate_samples = vec![12_000; 44_100];
        candidate_samples.extend(vec![0; 44_100]);
        reference_samples[0] = 1;
        fs::write(&reference, wav(&reference_samples)).unwrap();
        fs::write(&candidate, wav(&candidate_samples)).unwrap();
        let window = AudioWindow {
            start: super::super::SampleIndex(44_100),
            length: super::super::SampleIndex(44_100),
        };

        let report = compare_wav_files_windowed(
            &reference,
            &candidate,
            FeatureConfig::default(),
            AudioBudget::default(),
            Some(&root.join("cache")),
            Some(window),
        )
        .unwrap();

        assert_eq!(report.window, Some(window));
        assert_eq!(report.reference.rms, 0.0);
        assert_eq!(report.candidate.rms, 0.0);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn pinned_profile_requires_the_complete_feature_config() {
        let base = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/render_reference/v1");
        let source = FixtureAudioSource::Profile(PathBuf::from("reference.profile.json"));
        let mismatched = FeatureConfig {
            fft_size: super::super::FftSize(4096),
            ..FeatureConfig::default()
        };
        let error = load_fixture_source(&base, &source, mismatched, None).unwrap_err();
        assert!(matches!(error, AbTestError::ProfileConfig { .. }));
    }

    #[test]
    fn invalid_fixture_fft_size_is_rejected_before_profiles_are_loaded() {
        let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/render_reference/v1/manifest.json");
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(fixture).unwrap()).unwrap();
        manifest["feature_config"]["fft_size"] = serde_json::Value::from(3);
        let root = std::env::temp_dir().join(format!(
            "sid-analyzer-invalid-fixture-{}",
            std::process::id()
        ));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("manifest.json");
        fs::write(&path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        let error = run_fixture_matrix(&path, None).unwrap_err();
        assert!(matches!(
            error,
            AbTestError::Features(super::super::FeatureError::InvalidFftSize(_))
        ));
        fs::remove_dir_all(root).unwrap();
    }
}
