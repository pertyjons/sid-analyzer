use serde::{Deserialize, Serialize};

pub mod abtest;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(transparent)]
#[must_use]
pub struct SampleRate(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(transparent)]
#[must_use]
pub struct SampleIndex(pub usize);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct AudioWindow {
    pub start: SampleIndex,
    pub length: SampleIndex,
}

impl AudioWindow {
    #[must_use]
    pub fn end(self) -> Option<SampleIndex> {
        self.start.0.checked_add(self.length.0).map(SampleIndex)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize)]
#[serde(transparent)]
#[must_use]
pub struct FrequencyHz(pub f64);

#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize)]
#[serde(transparent)]
#[must_use]
pub struct Decibels(pub f64);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub struct ClippedSampleCount(pub usize);

#[derive(Debug, Clone, PartialEq)]
#[must_use]
pub struct WavData {
    pub sample_rate: SampleRate,
    pub samples: Vec<f64>,
    pub clipped_samples: ClippedSampleCount,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WavEncoding {
    Integer,
    Float,
}

#[derive(Debug, Clone, Copy)]
struct WavFormat {
    format_code: u16,
    encoding: WavEncoding,
    channels: u16,
    sample_rate: SampleRate,
    bits: u16,
}

const PCM_FORMAT: u16 = 1;
const FLOAT_FORMAT: u16 = 3;
const EXTENSIBLE_FORMAT: u16 = 0xfffe;
const EXTENSIBLE_GUID_TAIL: [u8; 12] = [
    0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xaa, 0x00, 0x38, 0x9b, 0x71,
];

pub fn parse_wav(bytes: &[u8]) -> Result<WavData, WavError> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err(WavError::NotRiffWave);
    }
    let mut format: Option<WavFormat> = None;
    let mut data: Option<&[u8]> = None;
    let mut position = 12;
    while position + 8 <= bytes.len() {
        let id = &bytes[position..position + 4];
        let size = u32::from_le_bytes([
            bytes[position + 4],
            bytes[position + 5],
            bytes[position + 6],
            bytes[position + 7],
        ]) as usize;
        let body = bytes
            .get(position + 8..position + 8 + size)
            .ok_or(WavError::TruncatedChunk)?;
        match id {
            b"fmt " if size >= 16 => {
                format = Some(parse_wav_format(body)?);
            }
            b"data" => data = Some(body),
            _ => {}
        }
        position += 8 + size + (size & 1);
    }
    let format = format.ok_or(WavError::MissingFormat)?;
    let data = data.ok_or(WavError::MissingData)?;
    if format.channels == 0 {
        return Err(WavError::ZeroChannels);
    }
    let bytes_per_sample = match (format.encoding, format.bits) {
        (WavEncoding::Integer, 8) => 1,
        (WavEncoding::Integer, 16) => 2,
        (WavEncoding::Integer, 24) => 3,
        (WavEncoding::Integer | WavEncoding::Float, 32) => 4,
        _ => {
            return Err(WavError::UnsupportedFormat {
                format: format.format_code,
                bits: format.bits,
            });
        }
    };
    let channels = usize::from(format.channels);
    let frame_size = bytes_per_sample * channels;
    let frame_count = data.len() / frame_size;
    let mut samples = Vec::with_capacity(frame_count);
    let mut clipped_samples = 0;
    for frame in data.chunks_exact(frame_size) {
        let mut mixed = 0.0;
        for channel in 0..channels {
            let offset = channel * bytes_per_sample;
            let (sample, clipped) = match (format.encoding, format.bits) {
                (WavEncoding::Integer, 8) => {
                    let value = frame[offset];
                    (
                        (f64::from(value) - 128.0) / 128.0,
                        value == u8::MIN || value == u8::MAX,
                    )
                }
                (WavEncoding::Integer, 16) => {
                    let value = i16::from_le_bytes([frame[offset], frame[offset + 1]]);
                    (
                        f64::from(value) / 32_768.0,
                        value == i16::MIN || value == i16::MAX,
                    )
                }
                (WavEncoding::Integer, 24) => {
                    let sign = if frame[offset + 2] & 0x80 == 0 {
                        0
                    } else {
                        0xff
                    };
                    let value = i32::from_le_bytes([
                        frame[offset],
                        frame[offset + 1],
                        frame[offset + 2],
                        sign,
                    ]);
                    (
                        f64::from(value) / 8_388_608.0,
                        value == -8_388_608 || value == 8_388_607,
                    )
                }
                (WavEncoding::Integer, 32) => {
                    let value = i32::from_le_bytes([
                        frame[offset],
                        frame[offset + 1],
                        frame[offset + 2],
                        frame[offset + 3],
                    ]);
                    (
                        f64::from(value) / 2_147_483_648.0,
                        value == i32::MIN || value == i32::MAX,
                    )
                }
                (WavEncoding::Float, 32) => {
                    let value = f32::from_le_bytes([
                        frame[offset],
                        frame[offset + 1],
                        frame[offset + 2],
                        frame[offset + 3],
                    ]);
                    (f64::from(value), value.abs() >= 1.0)
                }
                _ => {
                    return Err(WavError::UnsupportedFormat {
                        format: format.format_code,
                        bits: format.bits,
                    });
                }
            };
            clipped_samples += usize::from(clipped);
            mixed += sample;
        }
        samples.push(mixed / channels as f64);
    }
    Ok(WavData {
        sample_rate: format.sample_rate,
        samples,
        clipped_samples: ClippedSampleCount(clipped_samples),
    })
}

fn parse_wav_format(body: &[u8]) -> Result<WavFormat, WavError> {
    let format_code = u16::from_le_bytes([body[0], body[1]]);
    let channels = u16::from_le_bytes([body[2], body[3]]);
    let sample_rate = SampleRate(u32::from_le_bytes([body[4], body[5], body[6], body[7]]));
    let bits = u16::from_le_bytes([body[14], body[15]]);
    let encoding = match format_code {
        PCM_FORMAT => WavEncoding::Integer,
        FLOAT_FORMAT => WavEncoding::Float,
        EXTENSIBLE_FORMAT => {
            if body.len() < 40
                || u16::from_le_bytes([body[16], body[17]]) < 22
                || body[28..40] != EXTENSIBLE_GUID_TAIL
            {
                return Err(WavError::InvalidExtensibleFormat);
            }
            match u32::from_le_bytes([body[24], body[25], body[26], body[27]]) {
                1 => WavEncoding::Integer,
                3 => WavEncoding::Float,
                _ => return Err(WavError::UnsupportedExtensibleSubformat),
            }
        }
        _ => {
            return Err(WavError::UnsupportedFormat {
                format: format_code,
                bits,
            });
        }
    };
    Ok(WavFormat {
        format_code,
        encoding,
        channels,
        sample_rate,
        bits,
    })
}

#[derive(Debug, thiserror::Error)]
pub enum WavError {
    #[error("input is not a RIFF/WAVE file")]
    NotRiffWave,
    #[error("WAV chunk is truncated")]
    TruncatedChunk,
    #[error("WAV has no fmt chunk")]
    MissingFormat,
    #[error("WAV has no data chunk")]
    MissingData,
    #[error("WAV declares zero channels")]
    ZeroChannels,
    #[error("WAV extensible fmt chunk is invalid")]
    InvalidExtensibleFormat,
    #[error("WAV extensible subformat is unsupported")]
    UnsupportedExtensibleSubformat,
    #[error("WAV format {format} with {bits}-bit samples is unsupported")]
    UnsupportedFormat { format: u16, bits: u16 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(transparent)]
#[must_use]
pub struct AudioFeatureVersion(pub u16);

pub const AUDIO_FEATURE_VERSION: AudioFeatureVersion = AudioFeatureVersion(3);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct FeatureConfig {
    pub version: AudioFeatureVersion,
    pub fft_size: FftSize,
}

impl Default for FeatureConfig {
    fn default() -> Self {
        Self {
            version: AUDIO_FEATURE_VERSION,
            fft_size: FftSize(8192),
        }
    }
}

impl FeatureConfig {
    pub fn validate(self) -> Result<(), FeatureError> {
        if !self.fft_size.0.is_power_of_two() || self.fft_size.0 < 2 {
            return Err(FeatureError::InvalidFftSize(self.fft_size));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(transparent)]
#[must_use]
pub struct FftSize(pub usize);

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct AudioFeatureProfile {
    pub feature_config: FeatureConfig,
    pub sample_rate: SampleRate,
    pub samples: SampleIndex,
    pub peak: f64,
    pub rms: f64,
    pub onset: Option<SampleIndex>,
    pub peak_at: Option<SampleIndex>,
    pub release_end: Option<SampleIndex>,
    pub fundamental: Option<FrequencyHz>,
    pub fundamental_confidence: f64,
    pub spectral_centroid: FrequencyHz,
    pub spectral_rolloff: FrequencyHz,
    pub spectral_flatness: f64,
    pub zero_crossing_rate: f64,
    pub transient_density: f64,
    pub amplitude_envelopes: Vec<EnvelopeScale>,
    pub band_powers: Vec<f64>,
    pub low_rate_amplitude_modulation: Option<FrequencyHz>,
    pub pitch_trajectory: Vec<FrequencyTrajectoryPoint>,
    pub brightness_trajectory: Vec<FrequencyTrajectoryPoint>,
    pub harmonic_noise_balance: HarmonicNoiseBalance,
    pub low_rate_pitch_modulation: Option<FrequencyHz>,
    pub low_rate_brightness_modulation: Option<FrequencyHz>,
}

#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct FrequencyTrajectoryPoint {
    pub at: SampleIndex,
    pub value: FrequencyHz,
}

#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct HarmonicNoiseBalance {
    pub harmonic_power: f64,
    pub noise_power: f64,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct EnvelopeScale {
    pub window: SampleIndex,
    pub rms: Vec<f64>,
    pub peak: Vec<f64>,
}

pub fn measure_features(
    wav: &WavData,
    config: FeatureConfig,
) -> Result<AudioFeatureProfile, FeatureError> {
    if wav.samples.is_empty() {
        return Err(FeatureError::EmptyAudio);
    }
    config.validate()?;
    let peak = wav
        .samples
        .iter()
        .fold(0.0_f64, |value, sample| value.max(sample.abs()));
    let rms = root_mean_square(&wav.samples);
    let amplitude_envelopes: Vec<EnvelopeScale> = [64, 256, 1024, 4096]
        .into_iter()
        .map(|window| envelope_scale(&wav.samples, SampleIndex(window)))
        .collect();
    let onset_threshold = peak * 0.1;
    let onset = wav
        .samples
        .iter()
        .position(|sample| sample.abs() >= onset_threshold)
        .map(SampleIndex);
    let peak_at = wav
        .samples
        .iter()
        .enumerate()
        .max_by(|left, right| left.1.abs().total_cmp(&right.1.abs()))
        .map(|(index, _)| SampleIndex(index));
    let release_threshold = peak * 0.01;
    let release_end = wav
        .samples
        .iter()
        .rposition(|sample| sample.abs() >= release_threshold)
        .map(SampleIndex);
    let spectrum = average_spectrum(&wav.samples, config.fft_size);
    let bin_hz = f64::from(wav.sample_rate.0) / spectrum.fft_size.0 as f64;
    let total_power: f64 = spectrum.power.iter().sum();
    let spectral_centroid = if total_power > 0.0 {
        spectrum
            .power
            .iter()
            .enumerate()
            .map(|(bin, power)| bin as f64 * bin_hz * power)
            .sum::<f64>()
            / total_power
    } else {
        0.0
    };
    let spectral_rolloff = rolloff(&spectrum.power, bin_hz, 0.85);
    let spectral_flatness = flatness(&spectrum.power);
    let fundamental_bin = spectrum
        .power
        .iter()
        .enumerate()
        .skip(1)
        .max_by(|left, right| left.1.total_cmp(right.1))
        .map(|(bin, power)| (bin, *power));
    let (fundamental, fundamental_confidence) =
        fundamental_bin.map_or((None, 0.0), |(bin, power)| {
            (
                Some(FrequencyHz(bin as f64 * bin_hz)),
                if total_power > 0.0 {
                    power / total_power
                } else {
                    0.0
                },
            )
        });
    let zero_crossings = wav
        .samples
        .windows(2)
        .filter(|pair| pair[0].is_sign_positive() != pair[1].is_sign_positive())
        .count();
    let transient_density = transient_density(&wav.samples);
    let modulation = dominant_modulation(
        &amplitude_envelopes[2].rms,
        wav.sample_rate,
        amplitude_envelopes[2].window,
    );
    let trajectory_window = SampleIndex(2048);
    let pitch_trajectory = frequency_trajectory(
        &wav.samples,
        wav.sample_rate,
        trajectory_window,
        TrajectoryMeasure::Pitch,
    );
    let brightness_trajectory = frequency_trajectory(
        &wav.samples,
        wav.sample_rate,
        trajectory_window,
        TrajectoryMeasure::Brightness,
    );
    let harmonic_noise_balance =
        harmonic_noise_balance(&spectrum, bin_hz, fundamental.map(|value| value.0));
    let pitch_values: Vec<_> = pitch_trajectory.iter().map(|point| point.value.0).collect();
    let brightness_values: Vec<_> = brightness_trajectory
        .iter()
        .map(|point| point.value.0)
        .collect();
    Ok(AudioFeatureProfile {
        feature_config: config,
        sample_rate: wav.sample_rate,
        samples: SampleIndex(wav.samples.len()),
        peak,
        rms,
        onset,
        peak_at,
        release_end,
        fundamental,
        fundamental_confidence,
        spectral_centroid: FrequencyHz(spectral_centroid),
        spectral_rolloff: FrequencyHz(spectral_rolloff),
        spectral_flatness,
        zero_crossing_rate: zero_crossings as f64 / wav.samples.len().max(1) as f64,
        transient_density,
        amplitude_envelopes,
        band_powers: band_powers(
            &wav.samples,
            wav.sample_rate,
            &[FrequencyHz(100.0), FrequencyHz(500.0), FrequencyHz(2000.0)],
        ),
        low_rate_amplitude_modulation: modulation,
        pitch_trajectory,
        brightness_trajectory,
        harmonic_noise_balance,
        low_rate_pitch_modulation: dominant_modulation(
            &pitch_values,
            wav.sample_rate,
            trajectory_window,
        ),
        low_rate_brightness_modulation: dominant_modulation(
            &brightness_values,
            wav.sample_rate,
            trajectory_window,
        ),
    })
}

#[derive(Debug, thiserror::Error)]
pub enum FeatureError {
    #[error("audio has no samples")]
    EmptyAudio,
    #[error("FFT size {0:?} is not a power of two greater than one")]
    InvalidFftSize(FftSize),
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct AudioComparison {
    pub alignment: AlignmentSamples,
    pub rms_error: f64,
    pub peak_error: f64,
    pub level_error_db: Option<Decibels>,
    pub pitch_error_hz: Option<FrequencyHz>,
    pub log_spectral_distance: Decibels,
    pub gain_normalized_log_spectral_distance: Option<Decibels>,
    pub centroid_error_hz: FrequencyHz,
    pub rolloff_error_hz: FrequencyHz,
    pub flatness_error: f64,
    pub zero_crossing_error: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(transparent)]
#[must_use]
pub struct AlignmentSamples(pub i64);

pub fn compare_profiles(
    reference: &AudioFeatureProfile,
    candidate: &AudioFeatureProfile,
) -> AudioComparison {
    let alignment = match (reference.onset, candidate.onset) {
        (Some(reference), Some(candidate)) => {
            AlignmentSamples(candidate.0 as i64 - reference.0 as i64)
        }
        _ => AlignmentSamples(0),
    };
    let pitch_error_hz = reference
        .fundamental
        .zip(candidate.fundamental)
        .map(|(reference, candidate)| FrequencyHz((candidate.0 - reference.0).abs()));
    let level_error_db = level_error_db(reference.rms, candidate.rms).map(Decibels);
    AudioComparison {
        alignment,
        rms_error: (candidate.rms - reference.rms).abs(),
        peak_error: (candidate.peak - reference.peak).abs(),
        level_error_db,
        pitch_error_hz,
        log_spectral_distance: Decibels(log_spectral_distance(
            &reference.band_powers,
            &candidate.band_powers,
        )),
        gain_normalized_log_spectral_distance: gain_normalized_log_spectral_distance(
            &reference.band_powers,
            &candidate.band_powers,
        )
        .map(Decibels),
        centroid_error_hz: FrequencyHz(
            (candidate.spectral_centroid.0 - reference.spectral_centroid.0).abs(),
        ),
        rolloff_error_hz: FrequencyHz(
            (candidate.spectral_rolloff.0 - reference.spectral_rolloff.0).abs(),
        ),
        flatness_error: (candidate.spectral_flatness - reference.spectral_flatness).abs(),
        zero_crossing_error: (candidate.zero_crossing_rate - reference.zero_crossing_rate).abs(),
    }
}

pub fn band_powers(samples: &[f64], rate: SampleRate, edges: &[FrequencyHz]) -> Vec<f64> {
    let spectrum = average_spectrum(samples, FftSize(8192));
    if spectrum.power.is_empty() {
        return vec![0.0; edges.len() + 1];
    }
    let bin_hz = f64::from(rate.0) / spectrum.fft_size.0 as f64;
    let mut bands = vec![0.0; edges.len() + 1];
    for (bin, power) in spectrum.power.iter().enumerate().skip(1) {
        let frequency = bin as f64 * bin_hz;
        let band = edges
            .iter()
            .position(|edge| frequency < edge.0)
            .unwrap_or(edges.len());
        bands[band] += power;
    }
    bands
}

struct Spectrum {
    fft_size: FftSize,
    power: Vec<f64>,
}

fn average_spectrum(samples: &[f64], requested: FftSize) -> Spectrum {
    if samples.len() < 2 {
        return Spectrum {
            fft_size: FftSize(2),
            power: Vec::new(),
        };
    }
    let maximum = samples.len().min(requested.0);
    let size = if maximum.is_power_of_two() {
        maximum
    } else {
        maximum.next_power_of_two() / 2
    }
    .max(2);
    let hop = (size / 2).max(1);
    let hann: Vec<f64> = (0..size)
        .map(|index| {
            let phase = std::f64::consts::PI * index as f64 / size as f64;
            phase.sin().powi(2)
        })
        .collect();
    let mut power = vec![0.0; size / 2];
    let mut segments = 0;
    for window in samples.windows(size).step_by(hop) {
        let mut real: Vec<f64> = window
            .iter()
            .zip(&hann)
            .map(|(sample, weight)| sample * weight)
            .collect();
        let mut imaginary = vec![0.0; size];
        fft(&mut real, &mut imaginary);
        for (bin, slot) in power.iter_mut().enumerate() {
            *slot += real[bin].powi(2) + imaginary[bin].powi(2);
        }
        segments += 1;
    }
    if segments > 0 {
        for value in &mut power {
            *value /= f64::from(segments);
        }
    }
    Spectrum {
        fft_size: FftSize(size),
        power,
    }
}

fn fft(real: &mut [f64], imaginary: &mut [f64]) {
    let length = real.len();
    debug_assert!(length.is_power_of_two() && imaginary.len() == length);
    let mut reversed = 0;
    for index in 1..length {
        let mut bit = length >> 1;
        while reversed & bit != 0 {
            reversed ^= bit;
            bit >>= 1;
        }
        reversed |= bit;
        if index < reversed {
            real.swap(index, reversed);
            imaginary.swap(index, reversed);
        }
    }
    let mut block = 2;
    while block <= length {
        let angle = -2.0 * std::f64::consts::PI / block as f64;
        let (rotation_real, rotation_imaginary) = (angle.cos(), angle.sin());
        for start in (0..length).step_by(block) {
            let (mut current_real, mut current_imaginary) = (1.0, 0.0);
            for index in start..start + block / 2 {
                let upper_real = real[index];
                let upper_imaginary = imaginary[index];
                let lower_real = real[index + block / 2] * current_real
                    - imaginary[index + block / 2] * current_imaginary;
                let lower_imaginary = real[index + block / 2] * current_imaginary
                    + imaginary[index + block / 2] * current_real;
                real[index] = upper_real + lower_real;
                imaginary[index] = upper_imaginary + lower_imaginary;
                real[index + block / 2] = upper_real - lower_real;
                imaginary[index + block / 2] = upper_imaginary - lower_imaginary;
                let next_real =
                    current_real * rotation_real - current_imaginary * rotation_imaginary;
                current_imaginary =
                    current_real * rotation_imaginary + current_imaginary * rotation_real;
                current_real = next_real;
            }
        }
        block <<= 1;
    }
}

fn root_mean_square(samples: &[f64]) -> f64 {
    (samples.iter().map(|sample| sample * sample).sum::<f64>() / samples.len().max(1) as f64).sqrt()
}

fn envelope_scale(samples: &[f64], window: SampleIndex) -> EnvelopeScale {
    let mut rms = Vec::new();
    let mut peak = Vec::new();
    for chunk in samples.chunks(window.0) {
        rms.push(root_mean_square(chunk));
        peak.push(
            chunk
                .iter()
                .fold(0.0_f64, |value, sample| value.max(sample.abs())),
        );
    }
    EnvelopeScale { window, rms, peak }
}

fn rolloff(power: &[f64], bin_hz: f64, fraction: f64) -> f64 {
    let target = power.iter().sum::<f64>() * fraction;
    let mut cumulative = 0.0;
    for (bin, value) in power.iter().enumerate() {
        cumulative += value;
        if cumulative >= target {
            return bin as f64 * bin_hz;
        }
    }
    0.0
}

fn flatness(power: &[f64]) -> f64 {
    if power.is_empty() {
        return 0.0;
    }
    let epsilon = 1.0e-20;
    let geometric = (power
        .iter()
        .map(|value| (value + epsilon).ln())
        .sum::<f64>()
        / power.len() as f64)
        .exp();
    let arithmetic = power.iter().sum::<f64>() / power.len() as f64;
    if arithmetic > 0.0 {
        geometric / arithmetic
    } else {
        0.0
    }
}

fn transient_density(samples: &[f64]) -> f64 {
    if samples.len() < 2 {
        return 0.0;
    }
    let differences: Vec<f64> = samples
        .windows(2)
        .map(|pair| (pair[1] - pair[0]).abs())
        .collect();
    let mean = differences.iter().sum::<f64>() / differences.len() as f64;
    let threshold = mean * 4.0;
    differences
        .iter()
        .filter(|difference| **difference > threshold)
        .count() as f64
        / samples.len() as f64
}

fn dominant_modulation(
    envelope: &[f64],
    sample_rate: SampleRate,
    hop: SampleIndex,
) -> Option<FrequencyHz> {
    if envelope.len() < 8 {
        return None;
    }
    let spectrum = average_spectrum(envelope, FftSize(1024));
    let envelope_rate = f64::from(sample_rate.0) / hop.0 as f64;
    let bin_hz = envelope_rate / spectrum.fft_size.0 as f64;
    spectrum
        .power
        .iter()
        .enumerate()
        .skip(1)
        .filter(|(bin, _)| *bin as f64 * bin_hz <= 30.0)
        .max_by(|left, right| left.1.total_cmp(right.1))
        .map(|(bin, _)| FrequencyHz(bin as f64 * bin_hz))
}

#[derive(Debug, Clone, Copy)]
enum TrajectoryMeasure {
    Pitch,
    Brightness,
}

fn frequency_trajectory(
    samples: &[f64],
    sample_rate: SampleRate,
    window: SampleIndex,
    measure: TrajectoryMeasure,
) -> Vec<FrequencyTrajectoryPoint> {
    samples
        .chunks(window.0)
        .enumerate()
        .filter(|(_, chunk)| chunk.len() >= 2)
        .map(|(index, chunk)| {
            let spectrum = average_spectrum(chunk, FftSize(window.0));
            let bin_hz = f64::from(sample_rate.0) / spectrum.fft_size.0 as f64;
            let total: f64 = spectrum.power.iter().sum();
            let value = match measure {
                TrajectoryMeasure::Pitch => spectrum
                    .power
                    .iter()
                    .enumerate()
                    .skip(1)
                    .max_by(|left, right| left.1.total_cmp(right.1))
                    .map_or(0.0, |(bin, _)| bin as f64 * bin_hz),
                TrajectoryMeasure::Brightness if total > 0.0 => {
                    spectrum
                        .power
                        .iter()
                        .enumerate()
                        .map(|(bin, power)| bin as f64 * bin_hz * power)
                        .sum::<f64>()
                        / total
                }
                TrajectoryMeasure::Brightness => 0.0,
            };
            FrequencyTrajectoryPoint {
                at: SampleIndex(index * window.0),
                value: FrequencyHz(value),
            }
        })
        .collect()
}

fn harmonic_noise_balance(
    spectrum: &Spectrum,
    bin_hz: f64,
    fundamental: Option<f64>,
) -> HarmonicNoiseBalance {
    let Some(fundamental) = fundamental.filter(|value| *value > 0.0) else {
        return HarmonicNoiseBalance {
            harmonic_power: 0.0,
            noise_power: spectrum.power.iter().sum(),
        };
    };
    let fundamental_bin = (fundamental / bin_hz).round() as usize;
    let mut harmonic_power = 0.0;
    let mut marked = vec![false; spectrum.power.len()];
    for harmonic in 1.. {
        let center = fundamental_bin.saturating_mul(harmonic);
        if center >= spectrum.power.len() {
            break;
        }
        let start = center.saturating_sub(1);
        let end = (center + 1).min(spectrum.power.len().saturating_sub(1));
        for (bin, is_marked) in marked.iter_mut().enumerate().take(end + 1).skip(start) {
            if !*is_marked {
                harmonic_power += spectrum.power[bin];
                *is_marked = true;
            }
        }
    }
    let total: f64 = spectrum.power.iter().sum();
    HarmonicNoiseBalance {
        harmonic_power,
        noise_power: (total - harmonic_power).max(0.0),
    }
}

fn log_spectral_distance(reference: &[f64], candidate: &[f64]) -> f64 {
    let count = reference.len().min(candidate.len());
    if count == 0 {
        return 0.0;
    }
    let epsilon = 1.0e-20;
    (reference
        .iter()
        .zip(candidate)
        .take(count)
        .map(|(reference, candidate)| {
            let difference =
                10.0 * (reference + epsilon).log10() - 10.0 * (candidate + epsilon).log10();
            difference * difference
        })
        .sum::<f64>()
        / count as f64)
        .sqrt()
}

fn level_error_db(reference: f64, candidate: f64) -> Option<f64> {
    let epsilon = 1.0e-20;
    (reference > epsilon && candidate > epsilon)
        .then(|| (20.0 * (candidate / reference).log10()).abs())
}

fn gain_normalized_log_spectral_distance(reference: &[f64], candidate: &[f64]) -> Option<f64> {
    let reference_total: f64 = reference.iter().sum();
    let candidate_total: f64 = candidate.iter().sum();
    let epsilon = 1.0e-20;
    if reference_total <= epsilon || candidate_total <= epsilon {
        return None;
    }
    let reference: Vec<_> = reference
        .iter()
        .map(|power| power / reference_total)
        .collect();
    let candidate: Vec<_> = candidate
        .iter()
        .map(|power| power / candidate_total)
        .collect();
    Some(log_spectral_distance(&reference, &candidate))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pcm_wav(sample_rate: SampleRate, channels: u16, samples: &[i16]) -> Vec<u8> {
        let data_len = samples.len() * 2;
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36 + data_len as u32).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16_u32.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&channels.to_le_bytes());
        bytes.extend_from_slice(&sample_rate.0.to_le_bytes());
        bytes.extend_from_slice(&(sample_rate.0 * u32::from(channels) * 2).to_le_bytes());
        bytes.extend_from_slice(&(channels * 2).to_le_bytes());
        bytes.extend_from_slice(&16_u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&(data_len as u32).to_le_bytes());
        for sample in samples {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        bytes
    }

    fn float_wav(sample_rate: SampleRate, samples: &[f32]) -> Vec<u8> {
        let data_len = samples.len() * 4;
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36 + data_len as u32).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16_u32.to_le_bytes());
        bytes.extend_from_slice(&3_u16.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&sample_rate.0.to_le_bytes());
        bytes.extend_from_slice(&(sample_rate.0 * 4).to_le_bytes());
        bytes.extend_from_slice(&4_u16.to_le_bytes());
        bytes.extend_from_slice(&32_u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&(data_len as u32).to_le_bytes());
        for sample in samples {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        bytes
    }

    fn integer_wav(sample_rate: SampleRate, bits: u16, data: &[u8]) -> Vec<u8> {
        let bytes_per_sample = u32::from(bits) / 8;
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16_u32.to_le_bytes());
        bytes.extend_from_slice(&PCM_FORMAT.to_le_bytes());
        bytes.extend_from_slice(&1_u16.to_le_bytes());
        bytes.extend_from_slice(&sample_rate.0.to_le_bytes());
        bytes.extend_from_slice(&(sample_rate.0 * bytes_per_sample).to_le_bytes());
        bytes.extend_from_slice(&(bytes_per_sample as u16).to_le_bytes());
        bytes.extend_from_slice(&bits.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&(data.len() as u32).to_le_bytes());
        bytes.extend_from_slice(data);
        bytes
    }

    fn extensible_float_wav(sample_rate: SampleRate, channels: u16, samples: &[f32]) -> Vec<u8> {
        let data_len = samples.len() * 4;
        let block_align = channels * 4;
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(60 + data_len as u32).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&40_u32.to_le_bytes());
        bytes.extend_from_slice(&EXTENSIBLE_FORMAT.to_le_bytes());
        bytes.extend_from_slice(&channels.to_le_bytes());
        bytes.extend_from_slice(&sample_rate.0.to_le_bytes());
        bytes.extend_from_slice(&(sample_rate.0 * u32::from(block_align)).to_le_bytes());
        bytes.extend_from_slice(&block_align.to_le_bytes());
        bytes.extend_from_slice(&32_u16.to_le_bytes());
        bytes.extend_from_slice(&22_u16.to_le_bytes());
        bytes.extend_from_slice(&32_u16.to_le_bytes());
        bytes.extend_from_slice(&3_u32.to_le_bytes());
        bytes.extend_from_slice(&u32::from(FLOAT_FORMAT).to_le_bytes());
        bytes.extend_from_slice(&EXTENSIBLE_GUID_TAIL);
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&(data_len as u32).to_le_bytes());
        for sample in samples {
            bytes.extend_from_slice(&sample.to_le_bytes());
        }
        bytes
    }

    fn sine(sample_rate: SampleRate, frequency: FrequencyHz, amplitude: f64) -> WavData {
        let samples = (0..sample_rate.0)
            .map(|index| {
                let time = f64::from(index) / f64::from(sample_rate.0);
                amplitude * (2.0 * std::f64::consts::PI * frequency.0 * time).sin()
            })
            .collect();
        WavData {
            sample_rate,
            samples,
            clipped_samples: ClippedSampleCount(0),
        }
    }

    #[test]
    fn parses_pcm_and_float_wav() {
        let rate = SampleRate(8_000);
        let pcm = parse_wav(&pcm_wav(rate, 2, &[i16::MAX, i16::MAX, -1_000, -3_000])).unwrap();
        assert_eq!(pcm.sample_rate, rate);
        assert_eq!(pcm.clipped_samples, ClippedSampleCount(2));
        assert_eq!(pcm.samples.len(), 2);
        assert!((pcm.samples[1] - (-2_000.0 / 32_768.0)).abs() < 1.0e-9);

        let float = parse_wav(&float_wav(rate, &[0.25, -0.5, 1.0])).unwrap();
        assert_eq!(float.samples, vec![0.25, -0.5, 1.0]);
        assert_eq!(float.clipped_samples, ClippedSampleCount(1));
    }

    #[test]
    fn parses_every_pertylizer_integer_depth() {
        let rate = SampleRate(8_000);
        let pcm8 = parse_wav(&integer_wav(rate, 8, &[0, 128, 255])).unwrap();
        assert_eq!(pcm8.samples, vec![-1.0, 0.0, 127.0 / 128.0]);
        assert_eq!(pcm8.clipped_samples, ClippedSampleCount(2));

        let pcm24 = parse_wav(&integer_wav(
            rate,
            24,
            &[0, 0, 0x80, 0, 0, 0, 0xff, 0xff, 0x7f],
        ))
        .unwrap();
        assert_eq!(pcm24.samples, vec![-1.0, 0.0, 8_388_607.0 / 8_388_608.0]);
        assert_eq!(pcm24.clipped_samples, ClippedSampleCount(2));

        let mut pcm32_data = Vec::new();
        for sample in [i32::MIN, 0, i32::MAX] {
            pcm32_data.extend_from_slice(&sample.to_le_bytes());
        }
        let pcm32 = parse_wav(&integer_wav(rate, 32, &pcm32_data)).unwrap();
        assert_eq!(
            pcm32.samples,
            vec![-1.0, 0.0, f64::from(i32::MAX) / 2_147_483_648.0]
        );
        assert_eq!(pcm32.clipped_samples, ClippedSampleCount(2));
    }

    #[test]
    fn parses_pertylizer_extensible_float_and_downmixes_stereo() {
        let rate = SampleRate(44_100);
        let wav = extensible_float_wav(rate, 2, &[0.25, 0.75, -0.5, -0.25, 1.0, 1.0]);
        let parsed = parse_wav(&wav).unwrap();
        assert_eq!(parsed.sample_rate, rate);
        assert_eq!(parsed.samples, vec![0.5, -0.375, 1.0]);
        assert_eq!(parsed.clipped_samples, ClippedSampleCount(2));
    }

    #[test]
    fn wavebands_locate_a_one_kilohertz_sine() {
        let wav = sine(SampleRate(44_100), FrequencyHz(1_000.0), 0.6);
        let bands = band_powers(
            &wav.samples,
            wav.sample_rate,
            &[FrequencyHz(100.0), FrequencyHz(500.0), FrequencyHz(2_000.0)],
        );
        let total: f64 = bands.iter().sum();
        assert!(bands[2] / total > 0.95, "bands: {bands:?}");
    }

    #[test]
    fn feature_profiles_are_deterministic_and_track_perturbations() {
        let reference = sine(SampleRate(44_100), FrequencyHz(440.0), 0.5);
        let louder = sine(SampleRate(44_100), FrequencyHz(880.0), 0.8);
        let first = measure_features(&reference, FeatureConfig::default()).unwrap();
        let second = measure_features(&reference, FeatureConfig::default()).unwrap();
        assert_eq!(
            serde_json::to_vec(&first).unwrap(),
            serde_json::to_vec(&second).unwrap()
        );

        let changed = measure_features(&louder, FeatureConfig::default()).unwrap();
        let comparison = compare_profiles(&first, &changed);
        assert!(comparison.rms_error > 0.1);
        assert!(
            comparison
                .pitch_error_hz
                .is_some_and(|error| error.0 > 400.0)
        );
        assert!(comparison.centroid_error_hz.0 > 300.0);
    }

    #[test]
    fn profile_comparison_separates_level_from_spectral_shape() {
        let rate = SampleRate(44_100);
        let reference = sine(rate, FrequencyHz(440.0), 0.5);
        let quieter = sine(rate, FrequencyHz(440.0), 0.25);
        let different_shape = WavData {
            sample_rate: rate,
            samples: (0..rate.0)
                .map(|index| {
                    let time = f64::from(index) / f64::from(rate.0);
                    0.4 * (2.0 * std::f64::consts::PI * 440.0 * time).sin()
                        + 0.3 * (2.0 * std::f64::consts::PI * 1_320.0 * time).sin()
                })
                .collect(),
            clipped_samples: ClippedSampleCount(0),
        };
        let config = FeatureConfig::default();
        let reference = measure_features(&reference, config).unwrap();
        let quieter = measure_features(&quieter, config).unwrap();
        let different_shape = measure_features(&different_shape, config).unwrap();

        let level_only = compare_profiles(&reference, &quieter);
        assert!(
            (level_only.level_error_db.unwrap().0 - 6.020_599_913).abs() < 1.0e-6,
            "{level_only:?}"
        );
        assert!(
            level_only
                .gain_normalized_log_spectral_distance
                .is_some_and(|distance| distance.0 < 1.0e-9),
            "{level_only:?}"
        );

        let shape_only = compare_profiles(&reference, &different_shape);
        assert!(
            shape_only
                .level_error_db
                .is_some_and(|error| error.0 < 1.0e-9),
            "{shape_only:?}"
        );
        assert!(
            shape_only
                .gain_normalized_log_spectral_distance
                .is_some_and(|distance| distance.0 > 10.0),
            "{shape_only:?}"
        );
    }
}
