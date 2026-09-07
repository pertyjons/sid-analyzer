//! Rob Hubbard native extractor — see [`super`] for the overall design and
//! `docs/drivers/hubbard.md` for the reverse-engineering notes this implements.
//!
//! # The routine is a relocated family
//!
//! 289 HVSC tunes identify as `Rob_Hubbard`. They are relocated variants of one
//! hand-written routine, so there is no fixed memory map — load address, table
//! addresses and zero-page slots all shift between games. What is stable is the
//! *code*: the play loop always loads the pattern pointer from two parallel
//! `abs,Y` tables into a zero-page pointer, reads the pattern byte through
//! `(zp),Y`, masks the status byte's duration with `AND #$1F` (or `#$3F`), and
//! looks the note's pitch up in a 16-bit frequency table. The [`locate`] stage
//! finds those instructions and reads their operands, recovering every table
//! address (and the duration-mask width) without hardcoding.
//!
//! # Status: strict native extraction
//!
//! [`locate`] succeeds on all 11 Rob Hubbard tunes in `assets/music/` (including
//! the high-loaded `$E000` Nemesis, Auf Wiedersehen Monty and the Magnar / Shape
//! Music player). [`decode_song`]
//! walks the recovered orderlist + pattern tables into a note timeline whose
//! onsets match the emulator trace frame-for-frame and pitch-for-pitch, plus a
//! parallel per-note authored instrument index. [`HubbardExtractor::extract`]
//! combines that native note structure with faithful per-frame state/effects
//! from the trace into a [`NativeSong`], binding each note to its *authored*
//! instrument ([`decode_instruments`]) instead of clustering the trace. Still
//! trace-derived (not yet native): the per-voice timbre detail inside each patch
//! (filter, pulse-width motion) and the per-event slide effect. Exact recovered
//! pattern events and instances remain available separately from
//! render-oriented placement grouping.
//!
//! # Variants and self-validation
//!
//! The tunes are not one format — the relocated routines differ in voice
//! count, effect-command width, note encoding, table layout, orderlist commands
//! and the way they pace the song clock. [`locate`] adapts by reading these out
//! of the code/data ([`HubbardLayout::voices`], [`HubbardLayout::effect_bytes`],
//! [`HubbardLayout::pat_stride`], [`HubbardLayout::freq_hi`],
//! [`HubbardLayout::prescale_reload`], [`HubbardLayout::stall_reload`],
//! [`HubbardLayout::embedded_transpose_mask`]), [`decode_pattern`] preserves the
//! bit-7 note flag, and the decoder qualifies both observed gate-continuation
//! dialects: bit-7-only continuation and continuation of every note row while
//! the preceding row leaves the gate open. In the alternate 5-bit dialect, a
//! zero-duration row also keeps the gate open because its player has no
//! intervening gate-off frame. The established dialect remains preferred whenever
//! it passes; the unchanged native validation gate admits the alternate only
//! when the established interpretation fails. [`decode_song`] also honours the
//! orderlist transpose command
//! ([`ORDER_TRANSPOSE`]) and the status-byte duration width
//! ([`HubbardLayout::dur_mask`], 5- or 6-bit), [`row_frames`] simulates the
//! per-frame timing counters so a frame-skip gate stretches the row clock by the
//! exact, often non-integer, factor the chip sees, and [`arpeggio_survivors`]
//! folds a manual (in-pattern) arpeggio back to the single gated note the chip
//! plays. So the **Commando** (1-byte effects), **Sigma Seven** (2-byte effects,
//! bit-7 note reuse), **Auf Wiedersehen Monty** (orderlist transpose),
//! **Knucklebusters** (prescale frame-skip gate — every subtune, including the
//! faster ones), **Warhawk** (whole-play stall gate), **Human Race** (manual
//! arpeggios), **Nemesis** (a one-byte embedded orderlist transpose behind a
//! raster/digi wrapper), and the **Jeroen Tel** relocation (**Ikari Union** and
//! kin: a 6-bit duration mask, an interleaved pattern-pointer table, separate
//! lo/hi frequency tables and a one-byte embedded orderlist transpose) variants
//! are covered by the qualification command. Human Race is rejected because its
//! init does not program a CIA timer period.
//!
//! [`HubbardExtractor::extract`] validates with monotonic one-to-one alignment
//! per physical SID voice. Precision, recall, onset, pitch, and duration
//! residuals are recorded; failure is strict and never silently switches to the
//! trace-derived `--format synth` path.

use super::{
    CallResidual, DecoderPhaseResolution, DriverExtractor, FieldProvenance, NativeContext,
    NativeError, NativeSong, NativeValidationPolicy, NativeValidationReport, ProvenanceEvidence,
    note_from_raw_freq, validate_native_notes,
};
#[cfg(test)]
use super::{MIN_AGREEMENT, onset_agreement};
use crate::analysis::effects::{EffectThresholds, detect_effects};
use crate::analysis::note::{NoteEvent, detect_notes};
use crate::analysis::timbre::{
    AuthoredEffects, AuthoredPwm, AuthoredVibrato, apply_voice3_lfo_detection,
    extract_characteristics, extract_patches, extract_patches_grouped,
};
use crate::analysis::voice::{Adsr, ControlBits, PulseWidth};
use crate::analysis::{SystemClock, VoiceId, analyze};
use crate::emu::{self, Emulator};
use crate::export::{
    FrequencyTableIndex, InstrumentNumber, NativeEffectByte, NativePlacement, NativeRowTick,
    OrderOffset as PlacementOrderOffset, PatternByteOffset, PatternDuration, PatternNumber,
    PatternRepeatCount, PatternTranspose, RecoveredOrderCommand, RecoveredPatternEvent,
    RecoveredPatternInstance, RecoveredStructure, RecoveredVoiceStructure, RepeatOrdinal,
    VoicePlacements,
};
use crate::trace::FrameIndex;

mod decode;
mod instruments;
mod locator;
mod validation;

use decode::*;
use instruments::*;
#[cfg(test)]
use locator::LocatorEvidence;
pub(crate) use locator::{HubbardLayout, InstrumentTable, locate};
use locator::{HubbardLocateError, SCAN_HI, SCAN_LO, locate_checked};
use validation::*;

/// Native extractor for the Rob Hubbard driver family.
pub struct HubbardExtractor;

fn candidate_validation_is_better(
    candidate: &NativeValidationReport,
    current: &NativeValidationReport,
) -> bool {
    if current.accepted {
        return false;
    }
    if candidate.accepted {
        return true;
    }
    let candidate_edits = candidate.inserted.0.saturating_add(candidate.deleted.0);
    let current_edits = current.inserted.0.saturating_add(current.deleted.0);
    candidate_edits < current_edits
        || (candidate_edits == current_edits && candidate.matched.0 > current.matched.0)
}

struct DecodedCandidate {
    notes: Vec<NoteEvent>,
    instruments: Vec<Option<u8>>,
    validation: NativeValidationReport,
    phase_offset: i64,
}

impl DriverExtractor for HubbardExtractor {
    fn name(&self) -> &'static str {
        "hubbard"
    }

    fn handles(&self, driver: &str) -> bool {
        driver == "Rob_Hubbard"
    }

    fn extract(&self, ctx: &NativeContext<'_>) -> Result<NativeSong, NativeError> {
        let emu_err = |e: crate::emu::EmuError| NativeError::Emulation {
            driver: ctx.driver.to_string(),
            stage: super::EmulationStage::ExtractorSetup,
            reason: e.to_string(),
        };

        // Post-`init` RAM image: scan it for the driver's tables.
        let mut img = Emulator::with_timing(ctx.timing);
        img.load(ctx.header, ctx.bytes).map_err(emu_err)?;
        img.call_init(ctx.header.init_address, ctx.subtune, ctx.header.songs)
            .map_err(emu_err)?;
        let read = |a| img.read_ram(a);
        let layout = locate_checked(&read, SCAN_LO, SCAN_HI).map_err(|error| match error {
            HubbardLocateError::NotFound => NativeError::LocateFailed {
                driver: ctx.driver.to_string(),
                extractor: self.name(),
                reason: "required Hubbard signatures were not found".to_owned(),
            },
            HubbardLocateError::Ambiguous { evidence } => NativeError::LocateAmbiguous {
                driver: ctx.driver.to_string(),
                extractor: self.name(),
                reason: format!("{evidence:?}"),
            },
        })?;

        // Per-frame voice state and effects come from a faithful trace (the
        // emulator is ground truth for what the chip plays); the note timeline
        // comes from the driver's own orderlist + pattern tables, recovering the
        // real song structure rather than re-inferring it from the flat trace.
        let trace =
            emu::run_with_timing(ctx.header, ctx.bytes, ctx.subtune, ctx.frames, ctx.timing)
                .map_err(emu_err)?;
        let validation_timing = ctx.validation_timing(&trace, self.name())?;
        let states = analyze(&trace);
        let frame_count = states.len();
        let effects = detect_effects(&trace, &states, EffectThresholds::default());

        let decode_err = |error: HubbardDecodeError| NativeError::DecodeFailed {
            driver: ctx.driver.to_owned(),
            extractor: self.name(),
            reason: error.to_string(),
        };
        let ir = decode_ir(&read, &layout, frame_count as u32).map_err(decode_err)?;
        let truth = detect_notes(&states, ctx.timing.clock);
        let mut best_candidate = None;
        // Relocated players disagree on whether a regular note row after a
        // sustained status retriggers internally or only changes pitch under
        // the open gate. Both interpretations retain the same source rows;
        // independent trace alignment selects the accepted one.
        for continuation_mode in [
            GateContinuationMode::BitSevenNotes,
            GateContinuationMode::EverySustainedRow,
        ] {
            let (notes_raw, inst_raw) = decode_song_from_ir_with_mode(
                &read,
                &layout,
                &ir,
                ctx.timing.clock,
                frame_count as u32,
                continuation_mode,
            );
            let survivors = arpeggio_survivors(&notes_raw, &effects);
            let notes_unaligned: Vec<NoteEvent> = survivors
                .iter()
                .map(|&(i, end)| {
                    let mut note = notes_raw[i];
                    note.end_frame = end;
                    note
                })
                .collect();
            if notes_unaligned.is_empty() {
                continue;
            }
            let instruments: Vec<Option<u8>> =
                survivors.iter().map(|&(i, _)| inst_raw[i]).collect();
            let (notes, validation, phase_offset) = validate_with_phase_fit(
                &notes_unaligned,
                &truth,
                validation_timing,
                frame_count as u32,
            );
            let replace = best_candidate
                .as_ref()
                .is_none_or(|current: &DecodedCandidate| {
                    candidate_validation_is_better(&validation, &current.validation)
                });
            if replace {
                best_candidate = Some(DecodedCandidate {
                    notes,
                    instruments,
                    validation,
                    phase_offset,
                });
            }
        }
        let Some(DecodedCandidate {
            notes,
            instruments,
            validation,
            phase_offset,
        }) = best_candidate
        else {
            return Err(NativeError::DecodeEmpty {
                driver: ctx.driver.to_string(),
                extractor: self.name(),
            });
        };

        if !validation.accepted {
            return Err(NativeError::DecodeUnreliable {
                driver: ctx.driver.to_string(),
                extractor: self.name(),
                reason: validation.reason_summary(),
            });
        }

        // Per-note timbre fingerprints (Slice 1 + Slice 3). The patch *table* is
        // built separately below, so this skips Slice 2's heuristic clustering —
        // the native path replaces it and the fallback runs it on demand.
        let voice3_reads = trace.voice3_reads_per_frame();
        let mut characteristics: Vec<_> = notes
            .iter()
            .map(|n| extract_characteristics(n, &states, &effects, ctx.timing.clock))
            .collect();
        apply_voice3_lfo_detection(&mut characteristics, &notes, &voice3_reads);

        // Native patches: bind each note to its *authored* instrument (recovered
        // as the per-note instrument index) instead of clustering trace timbre by
        // a heuristic key. This replaces the over-segmented ad-hoc patches with
        // the driver's real instrument set — one patch per authored instrument,
        // stamped with its authored ADSR + waveform while the per-voice timbre
        // (filter, pulse width) stays trace-derived. Falls back to the trace
        // clustering when the instrument table could not be located (no native
        // instruments) — so unsupported variants behave exactly as before.
        let inst_count = instruments
            .iter()
            .flatten()
            .copied()
            .max()
            .map_or(0, |m| usize::from(m) + 1);
        let native_instruments = decode_instruments(&read, &layout, inst_count);
        // The columnar (Jeroen Tel) layout authors ADSR only — its waveform comes
        // from an unrecovered per-voice program, so keep the trace waveform there.
        let waveform_authored = matches!(layout.inst_table, Some(InstrumentTable::Packed { .. }));
        let (patches, patch_assignments) = if native_instruments.is_empty() {
            extract_patches(&notes, &characteristics)
        } else {
            extract_patches_grouped(&notes, &characteristics, &instruments, |idx| {
                let inst = native_instruments[usize::from(idx)];
                let waveform = waveform_authored.then(|| inst.control.waveform.to_control_byte());
                (
                    inst.adsr,
                    waveform,
                    inst.effects.drum_drop,
                    Some(authored_patch_effects(&inst, ctx.timing)),
                    None,
                )
            })
        };
        let provenance = provenance_evidence(
            &notes,
            &instruments,
            &native_instruments,
            &states,
            waveform_authored,
        );

        // The driver's own pattern structure, in the same frame domain as the
        // notes above — lets the synth exporter ship Hubbard's reused blocks in
        // orderlist order instead of one whole-song pattern.
        let mut structure = resolve_placements(&ir, &layout, frame_count as u32);
        for voice in &mut structure {
            for placement in &mut voice.placements {
                placement.start_frame =
                    shift_frame(placement.start_frame, phase_offset, frame_count as u32);
            }
        }
        let mut recovered_structure =
            recovered_structure(&read, &ir, &layout, frame_count as u32).map_err(decode_err)?;
        for voice in &mut recovered_structure.voices {
            for instance in &mut voice.instances {
                instance.start_frame =
                    shift_frame(instance.start_frame, phase_offset, frame_count as u32);
            }
        }

        Ok(NativeSong {
            capture: trace.capture.clone(),
            states,
            notes,
            patches,
            patch_assignments,
            characteristics,
            effects,
            structure: Some(structure),
            recovered_structure: Some(recovered_structure),
            validation,
            provenance,
        })
    }
}

#[cfg(test)]
mod tests;
