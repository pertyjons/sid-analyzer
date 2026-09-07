//! Driver-native song extraction — the `--format synth-native` path.
//!
//! # Why this exists
//!
//! The default `synth` exporter is **derive-not-recover**: it emulates the 6502
//! driver, traps SID register writes, and reconstructs notes/patches from that
//! *flattened* stream (so song structure is inferred, never read). This module
//! is the opposite approach: once a tune's **playroutine** is identified by code
//! signature ([`crate::playerid`]), a driver-specific [`DriverExtractor`] can
//! read that driver's *own* data tables — orderlist, pattern tables, instrument
//! tables — from the post-`init` RAM image, recovering the real structure
//! (exact pattern boundaries, reuse + transpose, true instruments).
//!
//! # Design: one analyzed program, different sources
//!
//! An extractor first builds an internal [`NativeSong`] from driver tables, then
//! [`extract_native`] attaches that recovered intent to the canonical
//! [`crate::analysis::sid_program::AnalyzedSidProgram`]. Both export modes
//! therefore cross the exporter boundary through the same program type.
//!
//! # Status
//!
//! The registry holds seven extractors: [`hubbard`], [`galway`], [`crowther`],
//! [`gremlin`], [`whittaker`], and both [`goattracker`] families. The
//! GoatTracker extractors decode pitch from the player's runtime state and
//! recover orderlists and packed patterns wherever they recognise the grammar;
//! compact variants remain native-decoded without claiming native structure.
//! Galway, Crowther, Gremlin, and Whittaker preserve their native procedural or
//! order/pattern grammar as exact recovered structure. [`extract_native`] does all the dispatch
//! (identify → select extractor → extract) with no file I/O, so a failure never
//! creates a half-written output file.
//!
//! # Extractor layout
//!
//! Keep a small extractor in one `<driver>.rs` file. Once discovery, decoding,
//! authored instruments, or validation become independently substantial, move
//! it to a `<driver>/` directory with a thin `mod.rs` orchestrator. Use the
//! Hubbard layout as the reference: `locator` owns post-init discovery and its
//! evidence, `decode` owns the driver grammar and recovered structure,
//! `instruments` owns authored patch/effect data, and `validation` owns any
//! driver-specific alignment policy. Keep shared validation and output types in
//! this parent module instead of copying them between driver directories.

mod crowther;
mod galway;
mod goattracker;
mod gremlin;
mod hubbard;
mod whittaker;

use crate::analysis::FrameState;
use crate::analysis::SystemClock;
use crate::analysis::effects::EffectSpan;
use crate::analysis::note::{GmProgram, NoteEvent, Velocity, hertz_to_midi};
use crate::analysis::timbre::{NoteCharacteristics, Patch, PatchId};
use crate::analysis::{Hertz, VoiceId};
use crate::emu::{Emulator, PlaybackTiming, TimingInexactReason};
use crate::header::{Header, SubtuneIndex};
use crate::playerid::PlayerDb;
use crate::trace::FrameIndex;
use serde::Serialize;

/// Legacy one-sided agreement threshold retained only for decoder regression
/// tests. Production acceptance uses [`NativeValidationPolicy`].
#[cfg(test)]
pub(crate) const MIN_AGREEMENT: f64 = 0.75;

/// Legacy onset tolerance retained only for regression comparisons.
#[cfg(test)]
pub(crate) const ONSET_TOLERANCE: i64 = 8;

/// Legacy one-sided agreement metric retained only for regression comparisons.
#[cfg(test)]
pub(crate) fn onset_agreement(native: &[NoteEvent], truth: &[NoteEvent]) -> f64 {
    if native.is_empty() {
        return 0.0;
    }
    let hits = native
        .iter()
        .filter(|n| truth.iter().any(|t| onset_match(n, t)))
        .count();
    hits as f64 / native.len() as f64
}

/// The single source of truth for "this decoded note matches this trace
/// note": same voice and pitch, onset within [`ONSET_TOLERANCE`].
#[cfg(test)]
pub(crate) fn onset_match(n: &NoteEvent, t: &NoteEvent) -> bool {
    t.voice == n.voice
        && t.midi == n.midi
        && (i64::from(t.start_frame.0) - i64::from(n.start_frame.0)).abs() <= ONSET_TOLERANCE
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(transparent)]
#[must_use]
pub struct CallResidual(pub i64);

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(transparent)]
#[must_use]
pub struct PitchResidual(pub f32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(transparent)]
#[must_use]
pub struct NoteCount(pub usize);

#[derive(Debug, Clone, Copy, Serialize)]
#[must_use]
pub struct NativeValidationPolicy {
    pub minimum_precision: f64,
    pub minimum_recall: f64,
    pub maximum_onset_calls: CallResidual,
    pub maximum_pitch_cents: PitchResidual,
    pub maximum_duration_calls: CallResidual,
}

impl Default for NativeValidationPolicy {
    fn default() -> Self {
        Self {
            minimum_precision: 0.75,
            minimum_recall: 0.75,
            maximum_onset_calls: CallResidual(8),
            maximum_pitch_cents: PitchResidual(75.0),
            maximum_duration_calls: CallResidual(64),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ResidualSummary<T> {
    pub median: Option<T>,
    pub p95: Option<T>,
    pub max: Option<T>,
}

#[derive(Debug, Clone, Serialize)]
pub struct VoiceValidationReport {
    pub voice: VoiceId,
    pub native: NoteCount,
    pub truth: NoteCount,
    pub matched: NoteCount,
    pub inserted: NoteCount,
    pub deleted: NoteCount,
    pub precision: f64,
    pub recall: f64,
    pub onset: ResidualSummary<CallResidual>,
    pub pitch: ResidualSummary<PitchResidual>,
    pub duration: ResidualSummary<CallResidual>,
}

#[derive(Debug, Clone, Serialize)]
pub struct NativeValidationReport {
    pub timing: PlaybackTiming,
    pub decoder_phase: DecoderPhaseResolution,
    pub native: NoteCount,
    pub truth: NoteCount,
    pub matched: NoteCount,
    pub inserted: NoteCount,
    pub deleted: NoteCount,
    pub precision: f64,
    pub recall: f64,
    pub onset: ResidualSummary<CallResidual>,
    pub pitch: ResidualSummary<PitchResidual>,
    pub duration: ResidualSummary<CallResidual>,
    pub voices: Vec<VoiceValidationReport>,
    pub accepted: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum DecoderPhaseResolution {
    Direct,
    BoundedFit {
        offset: CallResidual,
        search_radius: CallResidual,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldProvenance {
    AuthoredVerified,
    AuthoredDecoded,
    AuthoredPartial,
    TraceMeasured,
    TraceCorrected,
    Inferred,
    Unsupported,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProvenanceEvidence {
    pub field: String,
    pub provenance: FieldProvenance,
    pub samples: usize,
    pub mismatches: usize,
}

impl NativeValidationReport {
    #[must_use]
    pub fn reason_summary(&self) -> String {
        format!(
            "precision {:.1}%, recall {:.1}% ({} matched, {} inserted, {} deleted)",
            self.precision * 100.0,
            self.recall * 100.0,
            self.matched.0,
            self.inserted.0,
            self.deleted.0,
        )
    }
}

#[derive(Debug, Clone, Copy)]
enum AlignmentStep {
    Match(usize, usize),
    Insert,
    Delete,
}

struct VoiceAlignment {
    report: VoiceValidationReport,
    onsets: Vec<CallResidual>,
    pitches: Vec<PitchResidual>,
    durations: Vec<CallResidual>,
}

fn note_duration(note: &NoteEvent) -> Option<i64> {
    note.end_frame
        .map(|end| i64::from(end.0) - i64::from(note.start_frame.0))
}

fn pitch_cents(note: &NoteEvent) -> f32 {
    f32::from(note.midi.0) * 100.0 + note.cents.0
}

fn pair_residuals(
    native: &NoteEvent,
    truth: &NoteEvent,
) -> (CallResidual, PitchResidual, Option<CallResidual>) {
    let onset =
        CallResidual((i64::from(native.start_frame.0) - i64::from(truth.start_frame.0)).abs());
    let pitch = PitchResidual((pitch_cents(native) - pitch_cents(truth)).abs());
    let duration = note_duration(native)
        .zip(note_duration(truth))
        .map(|(left, right)| CallResidual((left - right).abs()));
    (onset, pitch, duration)
}

fn match_cost(
    native: &NoteEvent,
    truth: &NoteEvent,
    policy: NativeValidationPolicy,
) -> Option<f64> {
    let (onset, pitch, duration) = pair_residuals(native, truth);
    if onset.0 > policy.maximum_onset_calls.0
        || pitch.0 > policy.maximum_pitch_cents.0
        || duration.is_some_and(|d| d.0 > policy.maximum_duration_calls.0)
    {
        return None;
    }
    let onset_cost = onset.0 as f64 / policy.maximum_onset_calls.0.max(1) as f64;
    let pitch_cost = f64::from(pitch.0 / policy.maximum_pitch_cents.0.max(1.0));
    let duration_cost = duration.map_or(0.0, |d| {
        d.0 as f64 / policy.maximum_duration_calls.0.max(1) as f64
    });
    Some((onset_cost + pitch_cost + duration_cost) / 3.0)
}

fn align_voice(
    voice: VoiceId,
    native: &[&NoteEvent],
    truth: &[&NoteEvent],
    policy: NativeValidationPolicy,
) -> VoiceAlignment {
    let width = truth.len() + 1;
    let mut costs = vec![0.0; (native.len() + 1) * width];
    let mut previous = vec![AlignmentStep::Insert; (native.len() + 1) * width];
    for i in 1..=native.len() {
        costs[i * width] = i as f64;
        previous[i * width] = AlignmentStep::Insert;
    }
    for j in 1..=truth.len() {
        costs[j] = j as f64;
        previous[j] = AlignmentStep::Delete;
    }
    for i in 1..=native.len() {
        for j in 1..=truth.len() {
            let insert = costs[(i - 1) * width + j] + 1.0;
            let delete = costs[i * width + j - 1] + 1.0;
            let mut best = (insert, AlignmentStep::Insert);
            if delete < best.0 {
                best = (delete, AlignmentStep::Delete);
            }
            if let Some(pair) = match_cost(native[i - 1], truth[j - 1], policy) {
                let matched = costs[(i - 1) * width + j - 1] + pair;
                if matched <= best.0 {
                    best = (matched, AlignmentStep::Match(i - 1, j - 1));
                }
            }
            costs[i * width + j] = best.0;
            previous[i * width + j] = best.1;
        }
    }

    let mut i = native.len();
    let mut j = truth.len();
    let mut matched_pairs = Vec::new();
    let mut inserted = 0usize;
    let mut deleted = 0usize;
    while i > 0 || j > 0 {
        match previous[i * width + j] {
            AlignmentStep::Match(ni, ti) => {
                matched_pairs.push((ni, ti));
                i -= 1;
                j -= 1;
            }
            AlignmentStep::Insert => {
                inserted += 1;
                i -= 1;
            }
            AlignmentStep::Delete => {
                deleted += 1;
                j -= 1;
            }
        }
    }
    matched_pairs.reverse();
    let mut onsets = Vec::new();
    let mut pitches = Vec::new();
    let mut durations = Vec::new();
    for (ni, ti) in matched_pairs.iter().copied() {
        let (onset, pitch, duration) = pair_residuals(native[ni], truth[ti]);
        onsets.push(onset);
        pitches.push(pitch);
        if let Some(duration) = duration {
            durations.push(duration);
        }
    }
    let matched = matched_pairs.len();
    VoiceAlignment {
        report: VoiceValidationReport {
            voice,
            native: NoteCount(native.len()),
            truth: NoteCount(truth.len()),
            matched: NoteCount(matched),
            inserted: NoteCount(inserted),
            deleted: NoteCount(deleted),
            precision: ratio(matched, native.len()),
            recall: ratio(matched, truth.len()),
            onset: summarize_calls(onsets.clone()),
            pitch: summarize_pitch(pitches.clone()),
            duration: summarize_calls(durations.clone()),
        },
        onsets,
        pitches,
        durations,
    }
}

fn ratio(numerator: usize, denominator: usize) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        numerator as f64 / denominator as f64
    }
}

fn percentile_index(len: usize, numerator: usize, denominator: usize) -> usize {
    len.saturating_sub(1)
        .saturating_mul(numerator)
        .div_ceil(denominator)
        .min(len.saturating_sub(1))
}

fn summarize_calls(mut values: Vec<CallResidual>) -> ResidualSummary<CallResidual> {
    values.sort_unstable_by_key(|value| value.0);
    ResidualSummary {
        median: values.get(percentile_index(values.len(), 1, 2)).copied(),
        p95: values.get(percentile_index(values.len(), 95, 100)).copied(),
        max: values.last().copied(),
    }
}

fn summarize_pitch(mut values: Vec<PitchResidual>) -> ResidualSummary<PitchResidual> {
    values.sort_unstable_by(|left, right| left.0.total_cmp(&right.0));
    ResidualSummary {
        median: values.get(percentile_index(values.len(), 1, 2)).copied(),
        p95: values.get(percentile_index(values.len(), 95, 100)).copied(),
        max: values.last().copied(),
    }
}

#[must_use]
pub(crate) fn validate_native_notes(
    native: &[NoteEvent],
    truth: &[NoteEvent],
    timing: PlaybackTiming,
    policy: NativeValidationPolicy,
) -> NativeValidationReport {
    let alignments = [VoiceId::V1, VoiceId::V2, VoiceId::V3]
        .into_iter()
        .map(|voice| {
            let native_voice: Vec<_> = native.iter().filter(|note| note.voice == voice).collect();
            let truth_voice: Vec<_> = truth.iter().filter(|note| note.voice == voice).collect();
            align_voice(voice, &native_voice, &truth_voice, policy)
        })
        .collect::<Vec<_>>();
    let matched = alignments
        .iter()
        .map(|alignment| alignment.report.matched.0)
        .sum::<usize>();
    let inserted = alignments
        .iter()
        .map(|alignment| alignment.report.inserted.0)
        .sum::<usize>();
    let deleted = alignments
        .iter()
        .map(|alignment| alignment.report.deleted.0)
        .sum::<usize>();
    let native_count = native.len();
    let truth_count = truth.len();
    let precision = ratio(matched, native_count);
    let recall = ratio(matched, truth_count);
    let mut onsets = Vec::new();
    let mut pitches = Vec::new();
    let mut durations = Vec::new();
    for alignment in &alignments {
        onsets.extend_from_slice(&alignment.onsets);
        pitches.extend_from_slice(&alignment.pitches);
        durations.extend_from_slice(&alignment.durations);
    }
    let voices = alignments
        .into_iter()
        .map(|alignment| alignment.report)
        .collect();
    NativeValidationReport {
        timing,
        decoder_phase: DecoderPhaseResolution::Direct,
        native: NoteCount(native_count),
        truth: NoteCount(truth_count),
        matched: NoteCount(matched),
        inserted: NoteCount(inserted),
        deleted: NoteCount(deleted),
        precision,
        recall,
        onset: summarize_calls(onsets),
        pitch: summarize_pitch(pitches),
        duration: summarize_calls(durations),
        voices,
        accepted: timing.exact()
            && precision >= policy.minimum_precision
            && recall >= policy.minimum_recall,
    }
}

/// Build a [`NoteEvent`] from a raw 16-bit SID frequency-register value — the
/// pitch model every native decoder shares: `raw * phi2 / 2^24` Hz, mapped to
/// MIDI + cents.
pub(crate) fn note_from_raw_freq(
    raw: u32,
    clock: SystemClock,
    voice: VoiceId,
    start: u32,
    end: u32,
) -> Option<NoteEvent> {
    let hz = Hertz(f64::from(raw) * f64::from(clock.phi2_hz()) / 16_777_216.0);
    let (midi, cents) = hertz_to_midi(hz)?;
    Some(NoteEvent {
        voice,
        start_frame: FrameIndex(start),
        end_frame: Some(FrameIndex(end)),
        midi,
        cents,
        program: GmProgram::SQUARE_LEAD,
        velocity: Velocity(100),
    })
}

/// Borrowed inputs handed to a [`DriverExtractor`]. Holds everything needed to
/// emulate the tune (to obtain the post-`init` RAM image) and to label output.
pub(crate) struct NativeContext<'a> {
    pub header: &'a Header,
    pub bytes: &'a [u8],
    pub subtune: SubtuneIndex,
    pub timing: PlaybackTiming,
    /// Play frames to emulate (mirrors `--frames`).
    pub frames: u32,
    /// The driver name that matched (a `sidid.cfg` player name).
    pub driver: &'a str,
}

impl NativeContext<'_> {
    pub(crate) fn validation_timing(
        &self,
        trace: &crate::trace::Trace,
        extractor: &'static str,
    ) -> Result<PlaybackTiming, NativeError> {
        let timing = self.timing.resolved_from_trace(trace);
        if let Some(reason) = timing.inexact_reason() {
            return Err(NativeError::TimingInexact {
                driver: self.driver.to_owned(),
                extractor,
                reason,
            });
        }
        Ok(timing)
    }
}

/// Private extractor workspace built from a driver's native tables before its
/// fields are attached to the canonical analyzed program.
pub(crate) struct NativeSong {
    pub capture: crate::emu::capture::CapturedSidExecution,
    pub states: Vec<FrameState>,
    pub notes: Vec<NoteEvent>,
    pub patches: Vec<Patch>,
    /// Per-note patch assignment, parallel to `notes`.
    pub patch_assignments: Vec<Option<PatchId>>,
    /// Per-note characteristics, parallel to `notes`.
    pub characteristics: Vec<NoteCharacteristics>,
    pub effects: Vec<EffectSpan>,
    /// The driver's own per-voice pattern placement timeline, when the extractor
    /// recovers it (Hubbard). Lets the synth exporter ship the real song
    /// structure — short reused pattern blocks in orderlist order — instead of
    /// one whole-song pattern per voice. `None` falls back to the flat layout.
    pub structure: Option<Vec<super::VoicePlacements>>,
    /// Exact recovered driver grammar before render-oriented grouping.
    pub recovered_structure: Option<super::RecoveredStructure>,
    pub validation: NativeValidationReport,
    pub provenance: Vec<ProvenanceEvidence>,
}

fn validate_structure_pair(
    recovered: Option<&super::RecoveredStructure>,
    render: Option<&[super::VoicePlacements]>,
) -> Result<(), String> {
    let (recovered, render) = match (recovered, render) {
        (None, None) => return Ok(()),
        (Some(_), None) => {
            return Err("recovered source structure has no render placement timeline".to_owned());
        }
        (None, Some(_)) => {
            return Err("render placement timeline has no recovered source structure".to_owned());
        }
        (Some(recovered), Some(render)) => (recovered, render),
    };

    let mut recovered_voices = std::collections::BTreeSet::new();
    for voice in &recovered.voices {
        if !recovered_voices.insert(voice.voice) {
            return Err(format!(
                "voice {} occurs twice in recovered structure",
                voice.voice
            ));
        }
        for instance in &voice.instances {
            if !recovered.patterns.contains_key(&instance.pattern) {
                return Err(format!(
                    "voice {} instance references missing pattern {}",
                    voice.voice, instance.pattern.0
                ));
            }
        }
    }

    let mut render_voices = std::collections::BTreeSet::new();
    for voice in render {
        if !render_voices.insert(voice.voice) {
            return Err(format!(
                "voice {} occurs twice in render structure",
                voice.voice
            ));
        }
        for placement in &voice.placements {
            if !recovered.patterns.contains_key(&placement.pattern_number) {
                return Err(format!(
                    "voice {} placement references missing pattern {}",
                    voice.voice, placement.pattern_number.0
                ));
            }
        }
    }
    if recovered_voices != render_voices {
        return Err("recovered and render structures contain different voice sets".to_owned());
    }

    for recovered_voice in &recovered.voices {
        let render_voice = render
            .iter()
            .find(|voice| voice.voice == recovered_voice.voice)
            .ok_or_else(|| {
                format!(
                    "voice {} has recovered instances but no render timeline",
                    recovered_voice.voice
                )
            })?;
        if recovered_voice.instances.len() != render_voice.placements.len() {
            return Err(format!(
                "voice {} has {} recovered instances but {} render placements",
                recovered_voice.voice,
                recovered_voice.instances.len(),
                render_voice.placements.len()
            ));
        }
        for (instance, placement) in recovered_voice
            .instances
            .iter()
            .zip(&render_voice.placements)
        {
            if instance.pattern != placement.pattern_number
                || instance.transpose != placement.transpose
                || instance.start_frame != placement.start_frame
                || placement.order_offset != Some(instance.order_offset)
                || placement.repeat_ordinal != Some(instance.repeat_ordinal)
            {
                return Err(format!(
                    "voice {} source instance at order offset {} changed during placement lowering",
                    recovered_voice.voice, instance.order_offset.0
                ));
            }
        }
    }
    Ok(())
}

impl NativeSong {
    fn validate(self, driver: &str, extractor: &'static str) -> Result<Self, NativeError> {
        let notes = self.notes.len();
        if self.patch_assignments.len() != notes || self.characteristics.len() != notes {
            return Err(NativeError::Invariant {
                driver: driver.to_owned(),
                extractor,
                notes,
                patch_assignments: self.patch_assignments.len(),
                characteristics: self.characteristics.len(),
            });
        }
        if let Err(reason) =
            validate_structure_pair(self.recovered_structure.as_ref(), self.structure.as_deref())
        {
            return Err(NativeError::StructureInvariant {
                driver: driver.to_owned(),
                extractor,
                reason,
            });
        }
        Ok(self)
    }

    fn into_analyzed_program(
        self,
        header: &Header,
        subtune: SubtuneIndex,
        driver: &str,
        extractor: &str,
    ) -> crate::analysis::sid_program::AnalyzedSidProgram {
        let Self {
            capture,
            states,
            notes,
            patches,
            patch_assignments,
            characteristics,
            effects,
            structure,
            recovered_structure,
            validation,
            provenance,
        } = self;
        let voice_relations = crate::analysis::effects::detect_voice_relations(
            &states,
            crate::analysis::effects::EffectThresholds::default(),
        );
        let inputs = crate::analysis::inputs::AnalysisInputs {
            states,
            notes,
            effects,
            voice_relations,
            characteristics,
            patches,
            patch_assignments,
        };
        let timing = validation.timing;
        let mut program = crate::analysis::sid_program::AnalyzedSidProgram::from_analysis_inputs(
            header, subtune, timing, capture, inputs,
        );
        let trace_span = crate::analysis::sid_program::time::SourceSpan {
            start: crate::trace::ChipCycle(0),
            end: program
                .capture
                .checkpoints
                .last()
                .map_or(crate::trace::ChipCycle(0), |checkpoint| checkpoint.cycle),
        };
        let authored_spans = provenance
            .iter()
            .map(
                |item| crate::analysis::sid_program::semantic::NativeAuthoredSpan {
                    field: item.field.clone(),
                    span: trace_span,
                    evidence: vec![crate::analysis::sid_program::evidence::Evidence {
                        provenance: program_provenance(item.provenance),
                        confidence: crate::analysis::sid_program::evidence::ConfidencePermille(
                            if item.mismatches == 0 { 1000 } else { 750 },
                        ),
                        source: crate::analysis::sid_program::evidence::SourceReference::Span(
                            trace_span,
                        ),
                        validity: crate::analysis::sid_program::evidence::EvidenceValidity::Valid,
                    }],
                },
            )
            .collect();
        program.semantic.native = Some(
            crate::analysis::sid_program::semantic::NativeSemanticOverlay {
                driver: driver.to_owned(),
                extractor: extractor.to_owned(),
                fields: provenance
                    .iter()
                    .map(
                        |item| crate::analysis::sid_program::semantic::NativeFieldEvidence {
                            field: item.field.clone(),
                            provenance: program_provenance(item.provenance),
                            samples: crate::analysis::sid_program::semantic::EvidenceCount(
                                item.samples as u64,
                            ),
                            mismatches: crate::analysis::sid_program::semantic::EvidenceCount(
                                item.mismatches as u64,
                            ),
                            span: Some(trace_span),
                        },
                    )
                    .collect(),
                authored_spans,
                validation,
                recovered_structure,
            },
        );
        program.semantic.structure = structure;
        program
    }
}

fn program_provenance(
    provenance: FieldProvenance,
) -> crate::analysis::sid_program::evidence::Provenance {
    use crate::analysis::sid_program::evidence::Provenance;
    match provenance {
        FieldProvenance::AuthoredVerified => Provenance::AuthoredVerified,
        FieldProvenance::AuthoredDecoded => Provenance::AuthoredDecoded,
        FieldProvenance::AuthoredPartial => Provenance::AuthoredPartial,
        FieldProvenance::TraceMeasured => Provenance::TraceMeasured,
        FieldProvenance::TraceCorrected => Provenance::TraceCorrected,
        FieldProvenance::Inferred => Provenance::Inferred,
        FieldProvenance::Unsupported => Provenance::Unsupported,
    }
}

/// Stage at which emulation failed during native extraction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmulationStage {
    TimingLoad,
    TimingInit,
    ExtractorSetup,
    ExtractorLoad,
    ExtractorInit,
    NativeSampling,
    Trace,
}

/// Why native extraction could not produce a project.
#[derive(Debug, thiserror::Error)]
pub enum NativeError {
    #[error(
        "could not identify the playroutine — no native extractor applies (use --format synth)"
    )]
    Unidentified,
    #[error(
        "identified driver {driver:?}, but no native extractor handles it yet (use --format synth)"
    )]
    NoExtractor { driver: String },
    #[error(
        "identified driver {driver:?} (handled by the {extractor:?} extractor), but its native table parser is not implemented yet (use --format synth)"
    )]
    NotImplemented {
        driver: String,
        extractor: &'static str,
    },
    #[error(
        "identified driver {driver:?} (handled by the {extractor:?} extractor), but this configuration is outside its native scope: {reason} (use --format synth)"
    )]
    UnsupportedConfiguration {
        driver: String,
        extractor: &'static str,
        reason: String,
    },
    #[error(
        "native extraction for driver {driver:?} could not emulate the player during {stage:?}: {reason}"
    )]
    Emulation {
        driver: String,
        stage: EmulationStage,
        reason: String,
    },
    #[error(
        "identified driver {driver:?} (handled by the {extractor:?} extractor), but its data tables could not be located in the post-init RAM image: {reason} (use --format synth)"
    )]
    LocateFailed {
        driver: String,
        extractor: &'static str,
        reason: String,
    },
    #[error(
        "identified driver {driver:?} (handled by the {extractor:?} extractor), but multiple equally strong native table layouts remained: {reason}"
    )]
    LocateAmbiguous {
        driver: String,
        extractor: &'static str,
        reason: String,
    },
    #[error(
        "identified driver {driver:?} (handled by the {extractor:?} extractor), located its tables, but decoded no notes (use --format synth)"
    )]
    DecodeEmpty {
        driver: String,
        extractor: &'static str,
    },
    #[error(
        "identified driver {driver:?} (handled by the {extractor:?} extractor), but native table decoding failed: {reason}"
    )]
    DecodeFailed {
        driver: String,
        extractor: &'static str,
        reason: String,
    },
    #[error(
        "identified driver {driver:?} (handled by the {extractor:?} extractor), but its decoded notes failed native validation: {reason} — this variant is not supported yet (use --format synth explicitly)"
    )]
    DecodeUnreliable {
        driver: String,
        extractor: &'static str,
        reason: String,
    },
    #[error(
        "identified driver {driver:?} (handled by the {extractor:?} extractor), but native validation requires exact call timing and {reason} (use --format synth explicitly for trace-derived output)"
    )]
    TimingInexact {
        driver: String,
        extractor: &'static str,
        reason: TimingInexactReason,
    },
    #[error(
        "native extractor {extractor:?} for driver {driver:?} returned inconsistent parallel arrays: {notes} notes, {patch_assignments} patch assignments, {characteristics} characteristics"
    )]
    Invariant {
        driver: String,
        extractor: &'static str,
        notes: usize,
        patch_assignments: usize,
        characteristics: usize,
    },
    #[error(
        "native extractor {extractor:?} for driver {driver:?} returned inconsistent recovered/render structure: {reason}"
    )]
    StructureInvariant {
        driver: String,
        extractor: &'static str,
        reason: String,
    },
}

/// Best-effort labels for known driver data cells in a post-`init` RAM
/// image, for annotating `sid-re dis` output. Tries every driver locator
/// that works from a bare RAM image; a
/// tune that matches none simply yields no labels. Multiple labels can land on
/// one address.
#[must_use]
pub fn layout_labels(ram: &[u8]) -> Vec<(u16, String)> {
    let mut labels: Vec<(u16, String)> = Vec::new();
    if ram.len() < 0x10000 {
        return labels;
    }

    let read = |addr: u16| ram[addr as usize];
    if let Some(l) = hubbard::locate(&read, 0x0200, 0xFFFF) {
        let mut push = |addr: u16, name: &str| labels.push((addr, format!("hub.{name}")));
        push(l.note_mask, "note_mask");
        push(l.pat_read, "pat_read");
        push(u16::from(l.zp_ptr), "pat_zp_lo");
        push(u16::from(l.zp_ptr) + 1, "pat_zp_hi");
        if l.pat_stride == 2 {
            push(l.pat_ptr_lo, "pat_ptr (interleaved)");
        } else {
            push(l.pat_ptr_lo, "pat_ptr_lo");
            push(l.pat_ptr_hi, "pat_ptr_hi");
        }
        push(l.seq_ptr_lo, "seq_ptr_lo");
        push(l.seq_ptr_hi, "seq_ptr_hi");
        if let Some(hi) = l.freq_hi {
            push(l.freq_table, "freq_lo");
            push(hi, "freq_hi");
        } else {
            push(l.freq_table, "freq (interleaved)");
        }
        match l.inst_table {
            Some(hubbard::InstrumentTable::Packed { base }) => push(base, "inst_table"),
            Some(hubbard::InstrumentTable::Columnar { pwhi, ad, sr }) => {
                push(pwhi, "inst_pwhi");
                push(ad, "inst_ad");
                push(sr, "inst_sr");
            }
            None => {}
        }
    }

    if let Some(l) = crowther::locate(ram) {
        let mut push = |addr: u16, name: &str| labels.push((addr, format!("cro.{name}")));
        push(l.seq_lo, "seq_lo");
        push(l.seq_hi, "seq_hi");
        push(l.freq_table, "freq (hi-first)");
        if let Some(imm) = l.divider_imm {
            push(imm, "divider_imm");
        }
    }

    if let Some(l) = goattracker::locate_v2(ram) {
        let mut push = |addr: u16, name: &str| labels.push((addr, format!("gt2.{name}")));
        push(l.note_indices, "note_indices");
        push(l.resolved_frequencies, "resolved_freq");
        push(l.song_pointer_lo, "song_ptr_lo");
        push(l.song_pointer_hi, "song_ptr_hi");
        push(l.pattern_pointer_lo, "pattern_ptr_lo");
        push(l.pattern_pointer_hi, "pattern_ptr_hi");
    }

    if let Some(l) = goattracker::locate_v1(ram) {
        let mut push = |addr: u16, name: &str| labels.push((addr, format!("gt1.{name}")));
        push(l.frequency_lo, "freq_lo");
        push(l.frequency_hi, "freq_hi");
        push(l.playing_frequency_lo, "playing_freq_lo");
        push(l.playing_frequency_hi, "playing_freq_hi");
        push(l.pattern_pointer_lo, "pattern_ptr_lo");
        push(l.pattern_pointer_hi, "pattern_ptr_hi");
        push(l.pattern_numbers, "pattern_numbers");
        push(l.pattern_positions, "pattern_positions");
        push(l.order_positions, "order_positions");
        if let Some(note_numbers) = l.note_numbers {
            push(note_numbers, "note_numbers");
        }
        match l.order {
            goattracker::OrderLayout::Indexed {
                table_lo,
                table_hi,
                index_state,
            } => {
                push(table_lo, "song_ptr_lo");
                push(table_hi, "song_ptr_hi");
                push(index_state, "song_index");
            }
            goattracker::OrderLayout::VoicePointer {
                pointer_lo,
                pointer_hi,
                ..
            } => {
                push(pointer_lo, "order_ptr_lo");
                push(pointer_hi, "order_ptr_hi");
            }
        }
    }

    if let Some(l) = galway::locate(ram) {
        let mut push = |addr: u16, name: String| labels.push((addr, format!("gal.{name}")));
        push(l.freq_lo, "freq_lo".to_string());
        push(l.freq_hi, "freq_hi".to_string());
        push(l.active_mask_zp, "active_mask".to_string());
        for (i, v) in l.voices.iter().enumerate() {
            let n = i + 1;
            push(v.ptr_zp, format!("v{n}.ptr_lo"));
            push(v.ptr_zp + 1, format!("v{n}.ptr_hi"));
            push(v.durctr_zp, format!("v{n}.durctr"));
            push(v.jump_table, format!("v{n}.jump_table"));
            push(v.dur_table, format!("v{n}.dur_table"));
            push(v.transpose, format!("v{n}.transpose"));
        }
    }

    if let Some(l) = whittaker::locate(ram) {
        labels.push((l.frequency_table, "whi.freq (interleaved)".to_owned()));
        if let whittaker::WhittakerVariant::StateRecords { voice_states, .. } = l.variant {
            for (index, state) in voice_states.into_iter().enumerate() {
                labels.push((state, format!("whi.v{}.state", index + 1)));
            }
        }
    }

    labels.sort_unstable();
    labels
}

/// A driver-specific reader of native song structure.
pub(crate) trait DriverExtractor {
    /// Stable short name for diagnostics (e.g. `"goattracker"`).
    fn name(&self) -> &'static str;

    /// Whether this extractor handles the given `sidid.cfg` driver name.
    fn handles(&self, driver: &str) -> bool;

    /// Read the driver's native tables into a [`NativeSong`].
    fn extract(&self, ctx: &NativeContext<'_>) -> Result<NativeSong, NativeError>;
}

/// All registered extractors, in priority order.
#[must_use]
fn registry() -> Vec<Box<dyn DriverExtractor>> {
    vec![
        Box::new(goattracker::GoatTracker),
        Box::new(goattracker::GoatTrackerV1),
        Box::new(hubbard::HubbardExtractor),
        Box::new(galway::GalwayExtractor),
        Box::new(crowther::CrowtherExtractor),
        Box::new(gremlin::GremlinExtractor),
        Box::new(whittaker::WhittakerExtractor),
    ]
}

/// Identify the tune's driver and run the matching extractor. Performs **no
/// I/O** — on any error nothing is written, so the caller can open the output
/// file only after this succeeds. Returns the matched driver name, the
/// extractor's name, and the extracted song.
fn extract_native_song(
    db: &PlayerDb,
    header: &Header,
    bytes: &[u8],
    subtune: SubtuneIndex,
    timing: PlaybackTiming,
    frames: u32,
) -> Result<(String, &'static str, NativeSong), NativeError> {
    let driver = db.identify(bytes).ok_or(NativeError::Unidentified)?;
    let extractors = registry();
    let mut matching_extractors: Vec<_> = extractors
        .iter()
        .filter(|extractor| extractor.handles(driver))
        .collect();
    let preferred = match driver {
        "GoatTracker_V1.x" => Some("goattracker-v1"),
        "GoatTracker_V2.x" => Some("goattracker-v2"),
        _ => None,
    };
    matching_extractors.sort_by_key(|extractor| Some(extractor.name()) != preferred);
    let Some(timing_extractor) = matching_extractors.first() else {
        return Err(NativeError::NoExtractor {
            driver: driver.to_string(),
        });
    };
    let mut timing_emulator = Emulator::with_timing(timing);
    timing_emulator
        .load(header, bytes)
        .map_err(|error| NativeError::Emulation {
            driver: driver.to_owned(),
            stage: EmulationStage::TimingLoad,
            reason: error.to_string(),
        })?;
    timing_emulator
        .call_init(header.init_address, subtune, header.songs)
        .map_err(|error| NativeError::Emulation {
            driver: driver.to_owned(),
            stage: EmulationStage::TimingInit,
            reason: error.to_string(),
        })?;
    let timing = timing_emulator.playback_timing();
    if let Some(reason) = timing.inexact_reason() {
        return Err(NativeError::TimingInexact {
            driver: driver.to_owned(),
            extractor: timing_extractor.name(),
            reason,
        });
    }
    let ctx = NativeContext {
        header,
        bytes,
        subtune,
        timing,
        frames,
        driver,
    };
    let mut best_error = None;
    for extractor in matching_extractors {
        match extractor
            .extract(&ctx)
            .and_then(|song| song.validate(driver, extractor.name()))
        {
            Ok(song) => return Ok((driver.to_string(), extractor.name(), song)),
            Err(error) => {
                let replace = best_error.as_ref().is_none_or(|previous| {
                    matches!(previous, NativeError::LocateFailed { .. })
                        && !matches!(error, NativeError::LocateFailed { .. })
                });
                if replace {
                    best_error = Some(error);
                }
            }
        }
    }
    Err(best_error.unwrap_or(NativeError::NoExtractor {
        driver: driver.to_string(),
    }))
}

/// Identify the tune's driver, recover its authored structure, and attach that
/// structure to the canonical analyzed program.
pub fn extract_native(
    db: &PlayerDb,
    header: &Header,
    bytes: &[u8],
    subtune: SubtuneIndex,
    timing: PlaybackTiming,
    frames: u32,
) -> Result<
    (
        String,
        &'static str,
        crate::analysis::sid_program::AnalyzedSidProgram,
    ),
    NativeError,
> {
    let (driver, extractor, song) =
        extract_native_song(db, header, bytes, subtune, timing, frames)?;
    let program = song.into_analyzed_program(header, subtune, &driver, extractor);
    Ok((driver, extractor, program))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn structure_pair() -> (
        super::super::RecoveredStructure,
        Vec<super::super::VoicePlacements>,
    ) {
        let pattern = super::super::PatternNumber(3);
        let order_offset = super::super::OrderOffset(2);
        let repeat_ordinal = super::super::RepeatOrdinal(1);
        let transpose = super::super::PatternTranspose(-2);
        let start_frame = FrameIndex(48);
        let recovered = super::super::RecoveredStructure {
            patterns: [(pattern, Vec::new())].into_iter().collect(),
            voices: vec![super::super::RecoveredVoiceStructure {
                voice: VoiceId::V2,
                order_loop_offset: None,
                order_commands: vec![super::super::RecoveredOrderCommand::Pattern {
                    order_offset,
                    pattern,
                    repeat: super::super::PatternRepeatCount(1),
                }],
                instances: vec![super::super::RecoveredPatternInstance {
                    pattern,
                    transpose,
                    repeat_ordinal,
                    order_offset,
                    start_tick: super::super::NativeRowTick(12),
                    start_frame,
                }],
            }],
        };
        let render = vec![super::super::VoicePlacements {
            voice: VoiceId::V2,
            placements: vec![super::super::NativePlacement {
                pattern_number: pattern,
                start_frame,
                transpose,
                order_offset: Some(order_offset),
                repeat_ordinal: Some(repeat_ordinal),
            }],
        }];
        (recovered, render)
    }

    fn validation_note(voice: VoiceId, start: u32, end: u32, midi: u8, cents: f32) -> NoteEvent {
        NoteEvent {
            voice,
            start_frame: FrameIndex(start),
            end_frame: Some(FrameIndex(end)),
            midi: crate::analysis::note::MidiNote(midi),
            cents: crate::analysis::note::Cents(cents),
            program: GmProgram::SQUARE_LEAD,
            velocity: Velocity::DEFAULT,
        }
    }

    #[test]
    fn validation_counts_missing_truth_notes_as_deletions() {
        let native = [validation_note(VoiceId::V1, 0, 4, 60, 0.0)];
        let truth = [
            validation_note(VoiceId::V1, 0, 4, 60, 0.0),
            validation_note(VoiceId::V1, 8, 12, 62, 0.0),
        ];
        let report = validate_native_notes(
            &native,
            &truth,
            PlaybackTiming::vblank(SystemClock::Pal),
            NativeValidationPolicy::default(),
        );
        assert_eq!(report.matched, NoteCount(1));
        assert_eq!(report.deleted, NoteCount(1));
        assert_eq!(report.recall, 0.5);
        assert!(!report.accepted);
    }

    #[test]
    fn validation_does_not_reuse_one_truth_note() {
        let native = [
            validation_note(VoiceId::V1, 0, 4, 60, 0.0),
            validation_note(VoiceId::V1, 1, 4, 60, 0.0),
        ];
        let truth = [validation_note(VoiceId::V1, 0, 4, 60, 0.0)];
        let report = validate_native_notes(
            &native,
            &truth,
            PlaybackTiming::vblank(SystemClock::Pal),
            NativeValidationPolicy::default(),
        );
        assert_eq!(report.matched, NoteCount(1));
        assert_eq!(report.inserted, NoteCount(1));
        assert_eq!(report.precision, 0.5);
    }

    #[test]
    fn validation_measures_cents_and_duration() {
        let native = [validation_note(VoiceId::V2, 4, 14, 64, 30.0)];
        let truth = [validation_note(VoiceId::V2, 6, 12, 64, -10.0)];
        let report = validate_native_notes(
            &native,
            &truth,
            PlaybackTiming::vblank(SystemClock::Pal),
            NativeValidationPolicy::default(),
        );
        assert_eq!(report.onset.max, Some(CallResidual(2)));
        assert_eq!(report.pitch.max, Some(PitchResidual(40.0)));
        assert_eq!(report.duration.max, Some(CallResidual(4)));
    }

    #[test]
    fn validation_rejects_inexact_timing_even_for_identical_notes() {
        let notes = [validation_note(VoiceId::V3, 0, 4, 48, 0.0)];
        let timing = PlaybackTiming {
            cia_timed: true,
            ..PlaybackTiming::vblank(SystemClock::Pal)
        };
        let report =
            validate_native_notes(&notes, &notes, timing, NativeValidationPolicy::default());
        assert!(!report.accepted);
    }

    #[test]
    fn validation_accepts_recovered_cia_timing() {
        let notes = [validation_note(VoiceId::V3, 0, 4, 48, 0.0)];
        let timing = PlaybackTiming {
            cia_timed: true,
            ..PlaybackTiming::vblank(SystemClock::Pal)
        }
        .with_cia_period(crate::emu::CiaTimerPeriod::new(0x4026));
        let report =
            validate_native_notes(&notes, &notes, timing, NativeValidationPolicy::default());
        assert!(report.accepted);
        assert_eq!(
            report.timing.call_rate,
            crate::emu::CallRate::new(985_248, 0x4026)
        );
    }

    #[test]
    fn structure_contract_round_trips_placement_identity() {
        let (recovered, render) = structure_pair();
        assert!(validate_structure_pair(Some(&recovered), Some(&render)).is_ok());
    }

    #[test]
    fn structure_contract_rejects_lossy_placement_lowering() {
        let (recovered, mut render) = structure_pair();
        render[0].placements[0].transpose = super::super::PatternTranspose(0);
        assert!(
            validate_structure_pair(Some(&recovered), Some(&render))
                .is_err_and(|reason| reason.contains("changed during placement lowering"))
        );
    }

    #[test]
    fn structure_contract_rejects_one_sided_or_orphaned_structure() {
        let (mut recovered, render) = structure_pair();
        assert!(validate_structure_pair(Some(&recovered), None).is_err());
        assert!(validate_structure_pair(None, Some(&render)).is_err());
        recovered.patterns.clear();
        assert!(
            validate_structure_pair(Some(&recovered), Some(&render))
                .is_err_and(|reason| reason.contains("missing pattern"))
        );
    }

    #[test]
    fn registry_dispatches_by_driver_name() {
        let reg = registry();
        assert!(reg.iter().any(|e| e.handles("GoatTracker_V2.x")));
        assert!(reg.iter().any(|e| e.handles("GoatTracker_V1.x")));
        // The size-optimised V2 players share no signature with either
        // extractor, so they stay unclaimed rather than failing to locate.
        assert!(!reg.iter().any(|e| e.handles("GoatTracker_V2/Mini")));
        assert!(!reg.iter().any(|e| e.handles("GoatTracker_V2/Mini2")));
        assert!(reg.iter().any(|e| e.handles("Rob_Hubbard")));
        assert!(reg.iter().any(|e| e.handles("Antony_Crowther_V3")));
    }

    #[test]
    #[cfg_attr(
        not(feature = "asset-tests"),
        ignore = "requires the optional assets/music corpus"
    )]
    fn unidentified_bytes_report_unidentified() {
        // Identification runs on the raw bytes before the header is used, so a
        // real header parsed from an asset paired with a non-matching buffer
        // exercises the Unidentified path. A buffer of zeros matches no
        // signature.
        let bytes = std::fs::read("../../assets/music/Commando.sid").unwrap();
        let header = crate::header::parse(&bytes).unwrap();
        let db = PlayerDb::embedded();
        let zeros = vec![0u8; 4096];
        let result = extract_native_song(
            &db,
            &header,
            &zeros,
            SubtuneIndex(0),
            PlaybackTiming::vblank(SystemClock::Pal),
            100,
        );
        assert!(matches!(result, Err(NativeError::Unidentified)));
    }

    #[test]
    #[cfg_attr(
        not(feature = "asset-tests"),
        ignore = "requires the optional assets/music corpus"
    )]
    fn galway_extracts_neverending_story() {
        // Neverending_Story is identified as Martin_Galway, dispatched to the
        // Galway extractor, whose player simulation decodes a note timeline
        // that passes the onset-agreement gate.
        let bytes = std::fs::read("../../assets/music/Neverending_Story.sid").unwrap();
        let header = crate::header::parse(&bytes).unwrap();
        let db = PlayerDb::embedded();
        let (driver, extractor, song) = extract_native_song(
            &db,
            &header,
            &bytes,
            SubtuneIndex(1),
            PlaybackTiming::vblank(SystemClock::Pal),
            400,
        )
        .unwrap();
        assert_eq!(driver, "Martin_Galway");
        assert_eq!(extractor, "galway");
        assert!(!song.notes.is_empty());
        assert!(
            song.patches
                .iter()
                .all(|patch| patch.authored_definition.is_some())
        );
        assert!(song.patches.iter().any(|patch| {
            patch
                .authored_definition
                .as_ref()
                .is_some_and(|definition| definition.duration_table.len() == 17)
        }));
        assert!(
            song.patches
                .iter()
                .all(|patch| patch.waveform != 0 && patch.waveform.count_ones() == 1)
        );
        let recovered = song.recovered_structure.as_ref().unwrap();
        assert!(!recovered.patterns.is_empty());
        assert!(
            recovered
                .voices
                .iter()
                .all(|voice| !voice.instances.is_empty())
        );
        assert!(
            recovered
                .voices
                .iter()
                .flat_map(|voice| &voice.instances)
                .all(|instance| recovered.patterns.contains_key(&instance.pattern))
        );
        assert!(
            recovered
                .patterns
                .values()
                .flatten()
                .any(|event| event.command.is_some())
        );
        assert!(
            recovered
                .voices
                .iter()
                .flat_map(|voice| &voice.order_commands)
                .any(|command| matches!(
                    command,
                    crate::export::RecoveredOrderCommand::Call { .. }
                        | crate::export::RecoveredOrderCommand::Jump { .. }
                        | crate::export::RecoveredOrderCommand::Return { .. }
                ))
        );
    }

    #[test]
    #[cfg_attr(
        not(feature = "asset-tests"),
        ignore = "requires the optional assets/music corpus"
    )]
    fn galway_extracts_ocean_loader_1() {
        // Ocean_Loader_1 runs the *later* generation of Galway's player
        // (range-checked stack ops, end-voice `$80`, no tie) at a relocated
        // address — exercising locate plus the Ocean-generation handler
        // classifications end-to-end.
        let bytes = std::fs::read("../../assets/music/Ocean_Loader_1.sid").unwrap();
        let header = crate::header::parse(&bytes).unwrap();
        let db = PlayerDb::embedded();
        let (driver, extractor, song) = extract_native_song(
            &db,
            &header,
            &bytes,
            SubtuneIndex(1),
            PlaybackTiming::vblank(SystemClock::Pal),
            400,
        )
        .unwrap();
        assert_eq!(driver, "Martin_Galway");
        assert_eq!(extractor, "galway");
        assert!(!song.notes.is_empty());
        assert!(
            song.patches
                .iter()
                .all(|patch| patch.authored_definition.is_some())
        );
        assert!(song.patches.iter().any(|patch| {
            patch
                .authored_definition
                .as_ref()
                .is_some_and(|definition| {
                    definition.pitch.stages.len() == 4 && definition.pulse_width.stages.len() == 2
                })
        }));
        let recovered = song.recovered_structure.as_ref().unwrap();
        assert!(!recovered.patterns.is_empty());
        assert!(
            recovered
                .voices
                .iter()
                .all(|voice| !voice.instances.is_empty())
        );
        assert!(
            recovered
                .patterns
                .values()
                .flatten()
                .any(|event| event.duration_index.is_some())
        );
        assert!(
            recovered
                .voices
                .iter()
                .all(|voice| !voice.order_commands.is_empty() && voice.order_loop_offset.is_none())
        );
    }

    #[test]
    #[cfg_attr(
        not(feature = "asset-tests"),
        ignore = "requires the optional assets/music corpus"
    )]
    fn layout_labels_cover_native_driver_families() {
        let post_init_ram = |asset: &str| -> Vec<u8> {
            let bytes = std::fs::read(format!("../../assets/music/{asset}")).unwrap();
            let header = crate::header::parse(&bytes).unwrap();
            let mut emu = crate::emu::Emulator::new();
            emu.load(&header, &bytes).unwrap();
            emu.call_init(header.init_address, header.start_song, header.songs)
                .unwrap();
            emu.ram_image()
        };

        let hub = layout_labels(&post_init_ram("Commando.sid"));
        assert!(hub.iter().any(|(_, n)| n == "hub.pat_ptr_lo"));
        assert!(hub.iter().any(|(_, n)| n == "hub.inst_table"));

        let gal = layout_labels(&post_init_ram("Neverending_Story.sid"));
        assert!(gal.iter().any(|(_, n)| n == "gal.freq_lo"));
        assert!(gal.iter().any(|(_, n)| n == "gal.v1.jump_table"));

        let whi = layout_labels(&post_init_ram("Defcom.sid"));
        assert!(whi.iter().any(|(_, n)| n == "whi.freq (interleaved)"));
        assert!(whi.iter().any(|(_, n)| n == "whi.v1.state"));

        let gt2 = layout_labels(&post_init_ram("GoatTracker_V2_Tomb_of_the_Pharao.sid"));
        assert!(gt2.iter().any(|(_, n)| n == "gt2.resolved_freq"));
        assert!(gt2.iter().any(|(_, n)| n == "gt2.pattern_ptr_lo"));

        let gt1 = layout_labels(&post_init_ram("GoatTracker_V1_Jamaik2.sid"));
        assert!(gt1.iter().any(|(_, n)| n == "gt1.playing_freq_lo"));
        assert!(gt1.iter().any(|(_, n)| n == "gt1.song_ptr_lo"));
        let gt1_pointer = layout_labels(&post_init_ram("GoatTracker_V1_Lazy_Jones.sid"));
        assert!(gt1_pointer.iter().any(|(_, n)| n == "gt1.order_ptr_lo"));
    }

    #[test]
    #[cfg_attr(
        not(feature = "asset-tests"),
        ignore = "requires the optional assets/music corpus"
    )]
    fn hubbard_extracts_commando_song() {
        // Commando is identified as Rob_Hubbard, dispatched to the Hubbard
        // extractor, which locates its tables and decodes a note timeline.
        let bytes = std::fs::read("../../assets/music/Commando.sid").unwrap();
        let header = crate::header::parse(&bytes).unwrap();
        let db = PlayerDb::embedded();
        let (driver, extractor, song) = extract_native_song(
            &db,
            &header,
            &bytes,
            SubtuneIndex(1),
            PlaybackTiming::vblank(SystemClock::Pal),
            400,
        )
        .unwrap();
        assert_eq!(driver, "Rob_Hubbard");
        assert_eq!(extractor, "hubbard");
        assert!(!song.notes.is_empty());
    }

    #[test]
    #[cfg_attr(
        not(feature = "asset-tests"),
        ignore = "requires the optional assets/music corpus"
    )]
    fn human_race_reports_the_missing_init_timer_at_the_hubbard_boundary() {
        let bytes = std::fs::read("../../assets/music/Human_Race.sid").unwrap();
        let header = crate::header::parse(&bytes).unwrap();
        let db = PlayerDb::embedded();
        let timing = PlaybackTiming::for_subtune(&header, header.start_song);
        let result = extract_native_song(&db, &header, &bytes, header.start_song, timing, 400);
        assert!(matches!(
            result,
            Err(NativeError::TimingInexact {
                driver,
                extractor: "hubbard",
                reason: TimingInexactReason::CiaPeriodUnknown,
            }) if driver == "Rob_Hubbard"
        ));
    }

    /// Driver families with gate-verified `locate` ground truth, used by the
    /// taint corpus sweep below.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum TaintFam {
        Hubbard,
        Galway,
        Crowther,
    }

    /// One tune's taint-vs-`locate` comparison.
    struct TaintRec {
        fam: TaintFam,
        name: String,
        /// The driver's `locate` succeeded (ground truth exists).
        located: bool,
        /// Taint's top freq-register source matched the located freq table.
        freq_match: bool,
        /// Taint's stream pointers contained the located zp pointer
        /// (`Some` only for Hubbard, whose layout records `zp_ptr`).
        ptr_match: Option<bool>,
        /// Diagnosis: taint's v1 freq_lo/freq_hi top sources and the
        /// ground-truth freq base set, for inspecting misses.
        taint_freq: Vec<u16>,
        freq_set: Vec<u16>,
    }

    fn taint_no_truth(fam: TaintFam, name: String) -> TaintRec {
        TaintRec {
            fam,
            name,
            located: false,
            freq_match: false,
            ptr_match: None,
            taint_freq: Vec::new(),
            freq_set: Vec::new(),
        }
    }

    fn taint_compare(bytes: &[u8], fam: TaintFam, name: String, frames: u32) -> Option<TaintRec> {
        let header = crate::header::parse(bytes).ok()?;
        let mut emu = crate::emu::Emulator::new();
        emu.load(&header, bytes).ok()?;
        emu.call_init(header.init_address, header.start_song, header.songs)
            .ok()?;
        let ram = emu.ram_image();

        // Ground-truth freq-table base set (lo/hi pair tolerated) and, for
        // Hubbard, the zero-page stream pointer.
        let (freq_set, zp_truth): (Vec<u16>, Option<u16>) = match fam {
            TaintFam::Hubbard => {
                let read = |addr: u16| ram[addr as usize];
                let Some(l) = hubbard::locate(&read, 0x0200, 0xFFFF) else {
                    return Some(taint_no_truth(fam, name));
                };
                let mut g = vec![l.freq_table, l.freq_table.wrapping_add(1)];
                g.extend(l.freq_hi);
                (g, Some(u16::from(l.zp_ptr)))
            }
            TaintFam::Galway => {
                let Some(l) = galway::locate(&ram) else {
                    return Some(taint_no_truth(fam, name));
                };
                (vec![l.freq_lo, l.freq_hi], None)
            }
            TaintFam::Crowther => {
                let Some(l) = crowther::locate(&ram) else {
                    return Some(taint_no_truth(fam, name));
                };
                (vec![l.freq_table, l.freq_table.wrapping_add(1)], None)
            }
        };

        let report = emu.run_taint(header.play_address, frames);
        // v1 freq_lo/freq_hi are SID register offsets 0 and 1; the table is
        // shared across voices, so v1 is representative.
        let taint_freq: Vec<u16> = [0u8, 1]
            .iter()
            .filter_map(|&r| report.top_source(r))
            .collect();
        let freq_match = taint_freq.iter().any(|t| freq_set.contains(t));
        let ptr_match = zp_truth.map(|zp| report.stream_ptrs.iter().any(|(c, _)| *c == zp));
        Some(TaintRec {
            fam,
            name,
            located: true,
            freq_match,
            ptr_match,
            taint_freq,
            freq_set,
        })
    }

    /// Corpus-wide precision of `sid-re taint`: across every HVSC tune
    /// identified as one of the three gate-verified driver families, compare
    /// the taint-discovered freq table (and, for Hubbard, the stream pointer)
    /// against what that driver's `locate` reports as ground truth. M3 of the
    /// taint plan. CI-safe: no-op without `SID_HVSC_ROOT`. Run:
    /// `SID_HVSC_ROOT=… cargo test -p sid-analyzer --lib export::native::tests::dbg_taint_corpus_sweep -- --ignored --nocapture`
    #[test]
    #[ignore = "manual corpus-precision tool; needs SID_HVSC_ROOT"]
    fn dbg_taint_corpus_sweep() {
        use rayon::prelude::*;

        let Ok(root) = std::env::var("SID_HVSC_ROOT") else {
            eprintln!("SID_HVSC_ROOT unset; skipping taint corpus sweep");
            return;
        };
        // Default high: a freq table is only re-sourced into the per-voice
        // playing-frequency cell at a note-on, and many tunes have multi-
        // second intros, so a short window leaves the playing-freq cell at
        // its stale (untracked) value and the table never dominates the sink.
        // 1500 frames (~30 s PAL) clears almost every intro.
        let frames: u32 = std::env::var("SID_DBG_FRAMES")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(1500);

        let mut paths = Vec::new();
        let mut stack = vec![std::path::PathBuf::from(root)];
        while let Some(dir) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&dir) else {
                continue;
            };
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().and_then(|x| x.to_str()) == Some("sid") {
                    paths.push(p);
                }
            }
        }
        paths.sort();

        let db = PlayerDb::embedded();
        let deadline = std::time::Duration::from_secs(15);
        let recs: Vec<TaintRec> = paths
            .par_iter()
            .filter_map(|path| {
                let bytes = std::fs::read(path).ok()?;
                let fam = match db.identify(&bytes)? {
                    "Rob_Hubbard" => TaintFam::Hubbard,
                    "Martin_Galway" => TaintFam::Galway,
                    "Antony_Crowther_V3" => TaintFam::Crowther,
                    _ => return None,
                };
                let name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("?")
                    .to_string();
                let (tx, rx) = std::sync::mpsc::channel();
                std::thread::spawn(move || {
                    let _ = tx.send(taint_compare(&bytes, fam, name, frames));
                });
                rx.recv_timeout(deadline).ok().flatten()
            })
            .collect();

        let families = [
            ("Hubbard", TaintFam::Hubbard),
            ("Galway", TaintFam::Galway),
            ("Crowther", TaintFam::Crowther),
        ];
        eprintln!("\n=== taint corpus precision ({frames}f/tune) ===");
        eprintln!("family    identified  located  freq-match      ptr-match");
        let pct = |n: usize, d: usize| {
            if d == 0 {
                0.0
            } else {
                100.0 * n as f64 / d as f64
            }
        };
        for (name, fam) in families {
            let fam_recs: Vec<&TaintRec> = recs.iter().filter(|r| r.fam == fam).collect();
            let identified = fam_recs.len();
            let located = fam_recs.iter().filter(|r| r.located).count();
            let freq_hits = fam_recs
                .iter()
                .filter(|r| r.located && r.freq_match)
                .count();
            let ptr_total = fam_recs
                .iter()
                .filter(|r| r.located && r.ptr_match.is_some())
                .count();
            let ptr_hits = fam_recs
                .iter()
                .filter(|r| r.ptr_match == Some(true))
                .count();
            let ptr_col = if ptr_total == 0 {
                "        —".to_string()
            } else {
                format!(
                    "{ptr_hits:4}/{ptr_total:<4} {:5.1}%",
                    pct(ptr_hits, ptr_total)
                )
            };
            eprintln!(
                "{name:<9} {identified:>9}  {located:>7}  {freq_hits:4}/{located:<4} {:5.1}%   {ptr_col}",
                pct(freq_hits, located),
            );
        }

        // Diagnosis: list the freq-table misses (located, but taint's top
        // freq source did not hit the ground-truth table) with what taint
        // reported instead — the M2 refinement signal.
        eprintln!("\n--- freq-table misses (taint v1 freq sources vs ground truth) ---");
        let mut misses = 0;
        for r in recs.iter().filter(|r| r.located && !r.freq_match) {
            let fam = match r.fam {
                TaintFam::Hubbard => "hub",
                TaintFam::Galway => "gal",
                TaintFam::Crowther => "cro",
            };
            let taint: Vec<String> = r.taint_freq.iter().map(|a| format!("${a:04X}")).collect();
            let gt: Vec<String> = r.freq_set.iter().map(|a| format!("${a:04X}")).collect();
            eprintln!(
                "  {fam} {:<28} taint[{}] vs gt[{}]",
                r.name,
                taint.join(","),
                gt.join(",")
            );
            misses += 1;
            if misses >= 50 {
                eprintln!("  … (capped at 50)");
                break;
            }
        }
    }

    /// Coverage tool: every HVSC tune identified as Rob_Hubbard whose
    /// `hubbard::locate` *fails*, with the freq table and stream pointer taint
    /// recovers cold. Each row is a variant the static anchors miss — point
    /// `sid-re dis` at the taint address to see the instruction shape and
    /// extend the anchor. CI-safe: no-op without `SID_HVSC_ROOT`. Run:
    /// `SID_HVSC_ROOT=… cargo test -p sid-analyzer --lib export::native::tests::dbg_hubbard_locate_gaps -- --ignored --nocapture`
    #[test]
    #[ignore = "coverage tool; needs SID_HVSC_ROOT"]
    fn dbg_hubbard_locate_gaps() {
        use rayon::prelude::*;

        let Ok(root) = std::env::var("SID_HVSC_ROOT") else {
            eprintln!("SID_HVSC_ROOT unset; skipping");
            return;
        };

        let mut paths = Vec::new();
        let mut stack = vec![std::path::PathBuf::from(root)];
        while let Some(dir) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&dir) else {
                continue;
            };
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                } else if p.extension().and_then(|x| x.to_str()) == Some("sid") {
                    paths.push(p);
                }
            }
        }
        paths.sort();

        // The per-tune work: emulate, skip if the static anchors already
        // locate, else taint to recover the addresses. Returns `None` for a
        // located tune. Run behind a per-tune deadline because some tunes hit
        // pathological play loops that the cycle guard alone cannot bound.
        type Gap = (String, Option<u16>, Option<u16>, Option<u16>);

        fn gap_one(bytes: Vec<u8>, name: String) -> Option<Gap> {
            let header = crate::header::parse(&bytes).ok()?;
            let mut emu = crate::emu::Emulator::new();
            emu.load(&header, &bytes).ok()?;
            emu.call_init(header.init_address, header.start_song, header.songs)
                .ok()?;
            let ram = emu.ram_image();
            let read = |a: u16| ram[a as usize];
            if hubbard::locate(&read, 0x0200, 0xFFFF).is_some() {
                return None;
            }
            let report = emu.run_taint(header.play_address, 1500);
            Some((
                name,
                report.top_source(0),
                report.top_source(1),
                report.stream_ptrs.first().map(|(z, _)| *z),
            ))
        }

        // (name, taint freq_lo source, taint freq_hi source, top stream ptr)
        let db = PlayerDb::embedded();
        let deadline = std::time::Duration::from_secs(15);
        let gaps: Vec<Gap> = paths
            .par_iter()
            .filter_map(|path| {
                let bytes = std::fs::read(path).ok()?;
                if db.identify(&bytes)? != "Rob_Hubbard" {
                    return None;
                }
                let name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("?")
                    .to_string();
                let (tx, rx) = std::sync::mpsc::channel();
                std::thread::spawn(move || {
                    let _ = tx.send(gap_one(bytes, name));
                });
                rx.recv_timeout(deadline).ok().flatten()
            })
            .collect();

        eprintln!(
            "\n=== Hubbard locate-fails ({}) with taint-recovered addresses ===",
            gaps.len()
        );
        for (name, lo, hi, ptr) in &gaps {
            eprintln!("  {name:34} freq_lo={lo:04X?} freq_hi={hi:04X?} ptr={ptr:02X?}");
        }
    }
}
