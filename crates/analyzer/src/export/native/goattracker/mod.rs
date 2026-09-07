//! Native extraction for the GoatTracker driver families.
//!
//! Both families relocate the same shape of player: split low/high frequency
//! tables indexed by an authored note number, and a columnar three-voice
//! runtime state addressed with the voice's SID register offset. The player
//! resolves the authored note through the frequency tables into a per-voice
//! playing-frequency cell before writing it to the chip. Every extractor here
//! locates those operands in the relocated player, samples the cells while it
//! runs, and combines the decoded pitches with trace-measured articulation.
//!
//! [`v2`] handles `GoatTracker_V2.x`; [`v1`] handles `GoatTracker_V1.x`, whose
//! orderlist reader comes in three shapes that all drive one pattern grammar.
//! The generations share the sampling, decoder-phase, and song-assembly
//! scaffolding in this module and differ only in discovery and grammar.

mod v1;
mod v2;

pub(crate) use v1::locate as locate_v1;
pub(super) use v1::{GoatTrackerV1, OrderLayout};
pub(super) use v2::GoatTracker;
pub(crate) use v2::locate as locate_v2;

use super::{
    CallResidual, DecoderPhaseResolution, FieldProvenance, NativeContext, NativeError, NativeSong,
    NativeValidationPolicy, NativeValidationReport, ProvenanceEvidence, validate_native_notes,
};
use crate::analysis::analyze;
use crate::analysis::effects::{EffectThresholds, detect_effects};
use crate::analysis::note::{NoteEvent, detect_notes};
use crate::analysis::timbre::{
    apply_voice3_lfo_detection, extract_characteristics, extract_patches,
};
use crate::emu::{self, Emulator};
use crate::export::{
    NativePlacement, NativeRowTick, OrderOffset, PatternNumber, PatternTranspose,
    RecoveredOrderCommand, RecoveredPatternEvent, RecoveredPatternInstance, RecoveredStructure,
    RepeatOrdinal, VoicePlacements,
};
use crate::trace::FrameIndex;
use std::collections::BTreeMap;

/// GoatTracker indexes its per-voice state with the voice's SID register
/// offset, so voice *n*'s cell of any state array lives at `base + 7 * n`.
pub(super) const VOICE_OFFSETS: [u16; 3] = [0, 7, 14];

pub(super) fn absolute_operand(bytes: &[u8]) -> u16 {
    u16::from_le_bytes([bytes[0], bytes[1]])
}

/// A relocated split-pointer-table reader:
///
/// ```text
/// LDY index,X ; LDA table_lo,Y ; STA zp ; LDA table_hi,Y ; STA zp+1
/// … LDY position,X ; LDA (zp),Y ; [INY] ; CMP #terminal
/// ```
///
/// Both families walk their orderlists and their patterns through this shape,
/// and the immediate the first fetched byte is compared against says which of
/// the two — and which generation's grammar — was found.
pub(super) struct PointerTableReader {
    pub table_lo: u16,
    pub table_hi: u16,
    pub index_state: u16,
    pub position_state: u16,
    pub code_address: usize,
}

/// Bytes of unrelated code tolerated between the pointer pair and the cursor
/// load. One V1 generation tests its orderlist repeat counter in between.
const READER_GAP: usize = 24;
/// `LDY index,X` through `STA zp+1`.
const READER_HEAD: usize = 13;
/// `LDY position,X` through `CMP #terminal`, with the optional `INY`.
const READER_TAIL: usize = 10;

impl PointerTableReader {
    fn operands(&self) -> (u16, u16, u16, u16) {
        (
            self.table_lo,
            self.table_hi,
            self.index_state,
            self.position_state,
        )
    }
}

/// Collapse candidates that resolve to the same operands, keeping the first.
///
/// One reader reached from several code sites — a player linked into the image
/// more than once, or an entry point the packer duplicated — is a single
/// finding, not an ambiguity. Only *disagreeing* operands mean discovery could
/// not tell two layouts apart, and those still leave more than one candidate.
pub(super) fn dedup_by_operands<T, K: Ord>(candidates: &mut Vec<T>, operands: impl Fn(&T) -> K) {
    candidates.sort_by_key(&operands);
    candidates.dedup_by(|left, right| operands(left) == operands(right));
}

pub(super) fn pointer_table_readers(ram: &[u8], terminal: u8) -> Vec<PointerTableReader> {
    let mut readers = Vec::new();
    let Some(last_head_start) = ram.len().checked_sub(READER_HEAD) else {
        return readers;
    };
    for start in 0..=last_head_start {
        let head = &ram[start..start + READER_HEAD];
        if head[0] != 0xBC
            || head[3] != 0xB9
            || head[6] != 0x85
            || head[8] != 0xB9
            || head[11] != 0x85
            || head[12] != head[7].wrapping_add(1)
        {
            continue;
        }
        let zero_page = head[7];
        for gap in 0..=READER_GAP {
            let tail_start = start + READER_HEAD + gap;
            let Some(tail_end) = tail_start.checked_add(READER_TAIL) else {
                break;
            };
            let Some(tail) = ram.get(tail_start..tail_end) else {
                break;
            };
            if tail[0] != 0xBC || tail[3] != 0xB1 || tail[4] != zero_page {
                continue;
            }
            let compare = 5 + usize::from(tail[5] == 0xC8);
            if tail[compare] == 0xC9 && tail[compare + 1] == terminal {
                readers.push(PointerTableReader {
                    table_lo: absolute_operand(&head[4..6]),
                    table_hi: absolute_operand(&head[9..11]),
                    index_state: absolute_operand(&head[1..3]),
                    position_state: absolute_operand(&tail[1..3]),
                    code_address: start,
                });
                break;
            }
        }
    }
    // Sorting by code address first keeps the earliest site of a duplicated
    // reader, which is the one whose surroundings V2 probes for its grammar.
    readers.sort_by_key(|reader| (reader.operands(), reader.code_address));
    dedup_by_operands(&mut readers, PointerTableReader::operands);
    readers
}

/// The single candidate in `candidates`, or `None` when discovery was
/// inconclusive. Every locator here rejects ambiguity rather than guessing.
pub(super) fn unique<T>(mut candidates: Vec<T>) -> Option<T> {
    match candidates.len() {
        1 => candidates.pop(),
        _ => None,
    }
}

/// Read the pitch the player itself resolved for each trace-detected note
/// onset. The playing-frequency cell already carries slides and vibrato, so it
/// is the value the chip saw; articulation stays trace-measured.
pub(super) fn decode_pitches(
    clock: crate::analysis::SystemClock,
    truth: &[NoteEvent],
    samples: &[NativeFrameSample],
    buffered_delay: usize,
) -> Vec<NoteEvent> {
    let mut notes = Vec::with_capacity(truth.len());
    for note in truth {
        let sample_index = (note.start_frame.0 as usize).saturating_sub(buffered_delay);
        let Some(frame) = samples.get(sample_index) else {
            continue;
        };
        let raw = u32::from(frame.resolved_frequencies[note.voice.to_index()]);
        let end = note.end_frame.unwrap_or(note.start_frame).0;
        if let Some(mut decoded) =
            super::note_from_raw_freq(raw, clock, note.voice, note.start_frame.0, end)
        {
            decoded.end_frame = note.end_frame;
            decoded.program = note.program;
            decoded.velocity = note.velocity;
            notes.push(decoded);
        }
    }
    notes
}

/// The per-voice cursors a generation exposes, resolved to absolute addresses
/// in the relocated player.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct CursorLayout {
    /// Playing-frequency cells. The high byte is not always above the low one.
    pub frequency_state_lo: u16,
    pub frequency_state_hi: u16,
    pub order_positions: u16,
    pub pattern_numbers: u16,
    pub pattern_positions: u16,
}

pub(super) struct NativeFrameSample {
    pub resolved_frequencies: [u16; 3],
    pub order_positions: [u8; 3],
    pub pattern_numbers: [u8; 3],
    pub pattern_positions: [u8; 3],
}

pub(super) fn sample_native_state(
    emulator: &mut Emulator,
    cursors: &CursorLayout,
    play: crate::header::PlayAddress,
    frames: u32,
) -> Result<Vec<NativeFrameSample>, crate::emu::EmuError> {
    let mut samples = Vec::with_capacity(frames as usize);
    for frame in 0..frames {
        emulator.run_play_frame(play, FrameIndex(frame))?;
        let column = |base: u16| VOICE_OFFSETS.map(|o| emulator.read_ram(base.wrapping_add(o)));
        samples.push(NativeFrameSample {
            resolved_frequencies: VOICE_OFFSETS.map(|offset| {
                u16::from_le_bytes([
                    emulator.read_ram(cursors.frequency_state_lo.wrapping_add(offset)),
                    emulator.read_ram(cursors.frequency_state_hi.wrapping_add(offset)),
                ])
            }),
            order_positions: column(cursors.order_positions),
            pattern_numbers: column(cursors.pattern_numbers),
            pattern_positions: column(cursors.pattern_positions),
        });
    }
    Ok(samples)
}

/// Qualify the decoder at direct through four-call-delayed phases and keep the
/// best fit. Buffered players write the chip after they resolve the note, so
/// the delayed phase is the correct reading for those generations.
pub(super) fn resolve_phase(
    decode: impl Fn(usize) -> Vec<NoteEvent>,
    truth: &[NoteEvent],
    timing: crate::emu::PlaybackTiming,
) -> (Vec<NoteEvent>, NativeValidationReport) {
    let policy = NativeValidationPolicy::default();
    let mut best = None;
    for delay in 0..=4 {
        let notes = decode(delay);
        let mut validation = validate_native_notes(&notes, truth, timing, policy);
        if delay > 0 {
            validation.decoder_phase = DecoderPhaseResolution::BoundedFit {
                offset: CallResidual(-(delay as i64)),
                search_radius: CallResidual(4),
            };
        }
        if best
            .as_ref()
            .is_none_or(|(_, previous): &(Vec<NoteEvent>, NativeValidationReport)| {
                validation.matched.0 > previous.matched.0
            })
        {
            best = Some((notes, validation));
        }
    }
    best.unwrap_or_else(|| {
        let notes = Vec::new();
        let validation = validate_native_notes(&notes, truth, timing, policy);
        (notes, validation)
    })
}

/// The address one past the last byte of the loaded module — the bound every
/// table read is checked against, so a mislocated pointer cannot walk into
/// unrelated RAM.
pub(super) fn module_end(
    ctx: &NativeContext<'_>,
    extractor: &'static str,
) -> Result<u16, NativeError> {
    let payload_offset =
        usize::from(ctx.header.data_offset) + usize::from(ctx.header.load_address.0 == 0) * 2;
    let load_address = ctx
        .header
        .effective_load_address(ctx.bytes)
        .map_err(|error| NativeError::DecodeFailed {
            driver: ctx.driver.to_owned(),
            extractor,
            reason: error.to_string(),
        })?;
    let module_len = ctx.bytes.len().saturating_sub(payload_offset);
    Ok(load_address
        .0
        .saturating_add(u16::try_from(module_len).unwrap_or(u16::MAX)))
}

/// The trace-side inputs both generations derive articulation from.
pub(super) struct TraceContext {
    pub trace: crate::trace::Trace,
    pub timing: crate::emu::PlaybackTiming,
    pub states: Vec<crate::analysis::FrameState>,
    pub effects: Vec<crate::analysis::effects::EffectSpan>,
    pub truth: Vec<NoteEvent>,
}

pub(super) fn trace_context(
    ctx: &NativeContext<'_>,
    extractor: &'static str,
) -> Result<TraceContext, NativeError> {
    let trace = emu::run_with_timing(ctx.header, ctx.bytes, ctx.subtune, ctx.frames, ctx.timing)
        .map_err(|error| NativeError::Emulation {
            driver: ctx.driver.to_owned(),
            stage: super::EmulationStage::Trace,
            reason: error.to_string(),
        })?;
    let timing = ctx.validation_timing(&trace, extractor)?;
    let states = analyze(&trace);
    let effects = detect_effects(&trace, &states, EffectThresholds::default());
    let truth = detect_notes(&states, ctx.timing.clock);
    Ok(TraceContext {
        trace,
        timing,
        states,
        effects,
        truth,
    })
}

/// Read a 16-bit pointer out of a split low/high table.
pub(super) fn table_pointer(ram: &[u8], lo: u16, hi: u16, index: u16) -> u16 {
    u16::from_le_bytes([
        ram[usize::from(lo.wrapping_add(index))],
        ram[usize::from(hi.wrapping_add(index))],
    ])
}

/// Walk the sampled per-voice cursors and emit one instance per pattern the
/// player actually entered, with the exact frame it started on. Both families
/// restart the pattern cursor at zero on entry, so a cursor that moves
/// backwards — or a change of order position or pattern number — marks a new
/// instance.
pub(super) fn runtime_instances(
    samples: &[NativeFrameSample],
    voice: crate::analysis::VoiceId,
    order_commands: &[RecoveredOrderCommand],
    patterns: &BTreeMap<PatternNumber, Vec<RecoveredPatternEvent>>,
) -> Vec<RecoveredPatternInstance> {
    let index = voice.to_index();
    let mut instances: Vec<RecoveredPatternInstance> = Vec::new();
    let mut previous: Option<(u8, u8, u8)> = None;
    let mut tick = 0u32;
    for (frame, sample) in samples.iter().enumerate() {
        let current = (
            sample.order_positions[index],
            sample.pattern_numbers[index],
            sample.pattern_positions[index],
        );
        let starts = previous.is_none_or(|before| {
            current.1 != before.1
                || current.0 != before.0
                || current.2 < before.2
                || (before.2 == 0 && current.2 != 0)
        });
        if starts && current.2 != 0 {
            let order_offset = OrderOffset(usize::from(current.0.saturating_sub(1)));
            let transpose = order_commands
                .iter()
                .filter_map(|command| match command {
                    RecoveredOrderCommand::SetTranspose {
                        order_offset: command_offset,
                        transpose,
                    } if command_offset.0 <= order_offset.0 => Some(*transpose),
                    _ => None,
                })
                .next_back()
                .unwrap_or(PatternTranspose(0));
            let repeat_ordinal = instances.last().map_or(RepeatOrdinal(0), |last| {
                if last.pattern == PatternNumber(current.1) && last.order_offset == order_offset {
                    RepeatOrdinal(last.repeat_ordinal.0.saturating_add(1))
                } else {
                    RepeatOrdinal(0)
                }
            });
            instances.push(RecoveredPatternInstance {
                pattern: PatternNumber(current.1),
                transpose,
                repeat_ordinal,
                order_offset,
                start_tick: NativeRowTick(tick),
                start_frame: FrameIndex(frame as u32),
            });
            tick =
                tick.saturating_add(patterns.get(&PatternNumber(current.1)).map_or(0, |events| {
                    events.iter().map(|event| u32::from(event.duration.0)).sum()
                }));
        }
        previous = Some(current);
    }
    instances
}

/// Lower recovered source instances to the render-oriented placement timeline
/// the synth exporter consumes.
pub(super) fn voice_placements(
    voice: crate::analysis::VoiceId,
    instances: &[RecoveredPatternInstance],
) -> VoicePlacements {
    VoicePlacements {
        voice,
        placements: instances
            .iter()
            .map(|instance| NativePlacement {
                pattern_number: instance.pattern,
                start_frame: instance.start_frame,
                transpose: instance.transpose,
                order_offset: Some(instance.order_offset),
                repeat_ordinal: Some(instance.repeat_ordinal),
            })
            .collect(),
    }
}

/// Turn accepted native pitches plus trace-measured articulation into the
/// owned song the synth exporter consumes.
pub(super) fn assemble(
    ctx: &NativeContext<'_>,
    extractor: &'static str,
    trace: TraceContext,
    notes: Vec<NoteEvent>,
    validation: NativeValidationReport,
    structure: Option<Vec<VoicePlacements>>,
    recovered_structure: Option<RecoveredStructure>,
) -> Result<NativeSong, NativeError> {
    if trace.truth.is_empty() {
        return Err(NativeError::UnsupportedConfiguration {
            driver: ctx.driver.to_owned(),
            extractor,
            reason: "selected subtune has no trace-detected pitched notes".to_owned(),
        });
    }
    if notes.is_empty() {
        return Err(NativeError::DecodeEmpty {
            driver: ctx.driver.to_owned(),
            extractor,
        });
    }
    if !validation.accepted {
        return Err(NativeError::DecodeUnreliable {
            driver: ctx.driver.to_owned(),
            extractor,
            reason: validation.reason_summary(),
        });
    }
    let TraceContext {
        trace,
        states,
        effects,
        truth: _,
        timing: _,
    } = trace;
    let voice3_reads = trace.voice3_reads_per_frame();
    let mut characteristics: Vec<_> = notes
        .iter()
        .map(|note| extract_characteristics(note, &states, &effects, ctx.timing.clock))
        .collect();
    apply_voice3_lfo_detection(&mut characteristics, &notes, &voice3_reads);
    let (patches, patch_assignments) = extract_patches(&notes, &characteristics);
    let mut provenance = vec![
        ProvenanceEvidence {
            field: "note.pitch".to_owned(),
            provenance: FieldProvenance::AuthoredPartial,
            samples: notes.len(),
            mismatches: validation.inserted.0,
        },
        ProvenanceEvidence {
            field: "note.articulation".to_owned(),
            provenance: FieldProvenance::TraceMeasured,
            samples: notes.len(),
            mismatches: 0,
        },
    ];
    if let Some(recovered) = &recovered_structure {
        provenance.push(ProvenanceEvidence {
            field: "song.structure".to_owned(),
            provenance: FieldProvenance::AuthoredDecoded,
            samples: recovered.patterns.len(),
            mismatches: 0,
        });
    }
    Ok(NativeSong {
        capture: trace.capture.clone(),
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
    })
}

#[cfg(test)]
mod tests {
    use super::{READER_HEAD, READER_TAIL, pointer_table_readers};

    #[test]
    fn pointer_reader_can_end_at_top_of_ram() {
        let mut ram = vec![0; 0x1_0000];
        let start = ram.len() - READER_HEAD - READER_TAIL;
        ram[start..start + READER_HEAD].copy_from_slice(&[
            0xBC, 0x10, 0x20, 0xB9, 0x30, 0x40, 0x85, 0xFB, 0xB9, 0x50, 0x60, 0x85, 0xFC,
        ]);
        ram[start + READER_HEAD..]
            .copy_from_slice(&[0xBC, 0x70, 0x80, 0xB1, 0xFB, 0xC9, 0xFF, 0xEA, 0xEA, 0xEA]);

        let readers = pointer_table_readers(&ram, 0xFF);
        assert_eq!(readers.len(), 1);
        assert_eq!(readers[0].code_address, start);
        assert_eq!(readers[0].table_lo, 0x4030);
        assert_eq!(readers[0].table_hi, 0x6050);
    }
}
