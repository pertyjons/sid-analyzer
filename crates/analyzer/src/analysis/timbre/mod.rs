//! M7 timbre layer — per-note characterization (Slice 1) and patch
//! clustering (Slice 2).
//!
//! Slice 1 populates a [`NoteCharacteristics`] per detected
//! [`NoteEvent`](crate::analysis::note::NoteEvent) via
//! [`extract_characteristics`]. Slice 2 aggregates those into
//! [`Patch`]es via [`extract_patches`]. The end-to-end
//! [`extract_timbre`] wraps both for the common consumer flow.
//!
//! Slice 3 ([`apply_voice3_lfo_detection`]) is a post-pass that
//! flags voice-1/voice-2 notes whose frame range overlaps any
//! `$D41B`/`$D41C` read — the canonical voice-3-as-LFO idiom.

pub mod bytecode;
mod characteristics;
pub mod loops;
mod patch;

use crate::analysis::FrameState;
use crate::analysis::SystemClock;
use crate::analysis::VoiceId;
use crate::analysis::effects::EffectSpan;
use crate::analysis::note::NoteEvent;

pub use bytecode::{BytecodePatch, BytecodeTrick, Op, ProgramKey};
pub use characteristics::{
    AttackClass, ContourKind, DrumSubclass, FilterContour, HardwareTrick, NoteCharacteristics,
    PitchBehavior, PwEnvelope, ReleaseClass, RoleTags, SeqOrLoop, WaveformCombo, derive_role_tags,
    extract_characteristics,
};
pub use patch::{
    AuthoredEffects, AuthoredFilterPreset, AuthoredInstrumentDefinition, AuthoredLoopMode,
    AuthoredModulationProgram, AuthoredModulationStage, AuthoredPwm, AuthoredVibrato,
    FilterRoutingMask, ModulationDelta, Patch, PatchId, PatchVoiceProfile, ProgramFrames,
    SidVolume, extract_patches,
};
pub(crate) use patch::{extract_patches_grouped, median_hertz};

/// Run Slice 1 + Slice 2 + Slice 3 over a fully-analyzed subtune.
///
/// Returns `(characteristics, patches, patch_assignments)` where:
/// - `characteristics[i]` is the per-note timbre fingerprint for `notes[i]`
/// - `patches` is the per-subtune patch table
/// - `patch_assignments[i]` is `Some(patch_id)` if `notes[i]` was clustered,
///   `None` for singleton-fallback notes
///
/// `voice3_reads_per_frame[i]` is the count of `$D41B`/`$D41C` reads
/// observed in frame `i`. Pass `&[]` to skip Slice 3 detection. Must
/// be at least `states.len()` long when non-empty.
///
/// Three parallel arrays of length `notes.len()` (for characteristics +
/// assignments) plus the short patch table. Cheap enough (~1-2 ms per
/// HVSC subtune) that callers can run it eagerly whenever they want
/// the full Slice-1+2+3 view.
#[must_use]
pub fn extract_timbre(
    notes: &[NoteEvent],
    states: &[FrameState],
    spans: &[EffectSpan],
    voice3_reads_per_frame: &[u32],
    clock: SystemClock,
) -> (Vec<NoteCharacteristics>, Vec<Patch>, Vec<Option<PatchId>>) {
    let mut characteristics: Vec<NoteCharacteristics> = notes
        .iter()
        .map(|n| extract_characteristics(n, states, spans, clock))
        .collect();
    apply_voice3_lfo_detection(&mut characteristics, notes, voice3_reads_per_frame);
    let (patches, assignments) = extract_patches(notes, &characteristics);
    (characteristics, patches, assignments)
}

/// Slice 3 post-pass: tag any V1/V2 note whose frame range overlaps a
/// frame where the code read `$D41B`/`$D41C` (voice-3 used as
/// modulation source).
///
/// V3 is the modulation source, so V3 notes are skipped — the tag
/// marks targets only. The tag is idempotent (won't duplicate
/// `HardwareTrick::Voice3LfoSource` entries). An empty
/// `voice3_reads_per_frame` slice is a no-op so callers without
/// trace data can skip Slice 3 entirely.
pub fn apply_voice3_lfo_detection(
    characteristics: &mut [NoteCharacteristics],
    notes: &[NoteEvent],
    voice3_reads_per_frame: &[u32],
) {
    if voice3_reads_per_frame.is_empty() {
        return;
    }
    debug_assert_eq!(
        characteristics.len(),
        notes.len(),
        "characteristics and notes must be parallel arrays"
    );
    for (note, c) in notes.iter().zip(characteristics.iter_mut()) {
        if note.voice == VoiceId::V3 {
            continue;
        }
        let range = note.frame_range(voice3_reads_per_frame.len());
        if voice3_reads_per_frame[range].iter().any(|&n| n > 0)
            && !c
                .hardware_tricks
                .iter()
                .any(|t| matches!(t, HardwareTrick::Voice3LfoSource))
        {
            c.hardware_tricks.push(HardwareTrick::Voice3LfoSource);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::note::{Cents, GmProgram, MidiNote, Velocity};
    use crate::trace::FrameIndex;

    fn mk_note(voice: u8, start: u32, end: u32) -> NoteEvent {
        NoteEvent {
            voice: VoiceId(voice),
            start_frame: FrameIndex(start),
            end_frame: Some(FrameIndex(end)),
            midi: MidiNote(60),
            cents: Cents(0.0),
            program: GmProgram::SQUARE_LEAD,
            velocity: Velocity::DEFAULT,
        }
    }

    #[test]
    fn voice3_lfo_tag_added_when_reads_overlap_v1_note() {
        let notes = vec![mk_note(1, 5, 10)];
        let mut chars = vec![NoteCharacteristics::default()];
        // Read happens at frame 7, inside the note's range.
        let reads = vec![0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0];
        apply_voice3_lfo_detection(&mut chars, &notes, &reads);
        assert!(
            chars[0]
                .hardware_tricks
                .contains(&HardwareTrick::Voice3LfoSource)
        );
    }

    #[test]
    fn voice3_lfo_tag_skipped_when_reads_outside_note_range() {
        let notes = vec![mk_note(1, 5, 10)];
        let mut chars = vec![NoteCharacteristics::default()];
        // Read at frame 2 — before the note starts.
        let reads = vec![0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        apply_voice3_lfo_detection(&mut chars, &notes, &reads);
        assert!(
            !chars[0]
                .hardware_tricks
                .contains(&HardwareTrick::Voice3LfoSource)
        );
    }

    #[test]
    fn voice3_notes_themselves_are_not_tagged() {
        // The LFO *source* (V3) shouldn't be tagged as its own target.
        let notes = vec![mk_note(3, 5, 10)];
        let mut chars = vec![NoteCharacteristics::default()];
        let reads = vec![0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0];
        apply_voice3_lfo_detection(&mut chars, &notes, &reads);
        assert!(
            !chars[0]
                .hardware_tricks
                .contains(&HardwareTrick::Voice3LfoSource)
        );
    }

    #[test]
    fn voice3_lfo_idempotent_when_already_tagged() {
        let notes = vec![mk_note(2, 5, 10)];
        let mut chars = vec![NoteCharacteristics {
            hardware_tricks: vec![HardwareTrick::Voice3LfoSource],
            ..Default::default()
        }];
        let reads = vec![0; 12];
        let mut reads = reads;
        reads[7] = 3;
        apply_voice3_lfo_detection(&mut chars, &notes, &reads);
        let count = chars[0]
            .hardware_tricks
            .iter()
            .filter(|t| matches!(t, HardwareTrick::Voice3LfoSource))
            .count();
        assert_eq!(count, 1, "should not duplicate the tag");
    }

    #[test]
    fn empty_reads_slice_is_a_noop() {
        let notes = vec![mk_note(1, 0, 5)];
        let mut chars = vec![NoteCharacteristics::default()];
        apply_voice3_lfo_detection(&mut chars, &notes, &[]);
        assert!(chars[0].hardware_tricks.is_empty());
    }

    #[test]
    fn note_extending_past_reads_slice_handles_bounds() {
        // Note runs frames 5..=50 but reads vec is only 30 long.
        let notes = vec![mk_note(1, 5, 50)];
        let mut chars = vec![NoteCharacteristics::default()];
        let mut reads = vec![0u32; 30];
        reads[20] = 1;
        apply_voice3_lfo_detection(&mut chars, &notes, &reads);
        assert!(
            chars[0]
                .hardware_tricks
                .contains(&HardwareTrick::Voice3LfoSource)
        );
    }
}
