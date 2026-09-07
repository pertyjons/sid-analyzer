//! Native song-data extractor for the Ben Daglish / Gremlin playroutine
//! (`Ben_Daglish/Gremlin` — rep `720_Degrees.sid`).
//!
//! The format was reverse-engineered with the `sid-re taint` + `sid-re probe`
//! tools: taint gave every table address and the grammar constants, the probe
//! confirmed the `[note][dur]` row shape and the orderlist/pattern split, and
//! a focused disassembly of the decode loop ($C133 play, $C2BF row processor,
//! $C46F note decoder) pinned the exact byte grammar and timing. None of the
//! addresses are hard-coded — [`locate`] reads them from three relocation-
//! independent instruction signatures.
//!
//! # Format
//!
//! Three independent voices, each driven by a per-voice **orderlist** of
//! pattern numbers (with inline transpose / detune / loop / instrument
//! commands), and **patterns** of 2-byte-ish records:
//!
//! - `note < $80`  → `[note][dur]`: pitch index `note + transpose + $14` into
//!   the parallel freq tables (`$C8FA` lo, `$C95A` hi), sounding for `dur`
//!   frames. This is the only record that emits an onset.
//! - `$A0`         → `[A0][dur]`: a rest (gate off) for `dur` frames.
//! - `$A1` / `$A2` → filter-routing set / toggle (1 byte).
//! - `$80..$8F`    → instrument select (1 byte).
//! - `$90..$BF`    → a timbre/effect parameter (1 byte; `$A0..$A2` special-cased
//!   above).
//! - `$C0..$FE`    → a 4-byte slide/vibrato parameter block.
//! - `$FF`         → end of pattern: replay it while the loop counter is live,
//!   else advance the orderlist.
//!
//! Orderlist bytes: `< $80` = pattern number (looked up in the `$CB70`/`$CB8F`
//! pointer tables); `$FF` = end of song; `$E8` = filter value (2 bytes); other
//! `>= $80` = command whose bits 5-6 select transpose / loop-count / detune /
//! instrument-pointer patch and whose low 5 bits carry the parameter.
//!
//! [`decode_song`] is a faithful frame-synchronous simulation of that machine:
//! each frame, a voice with a live duration counter ticks it down, and a voice
//! whose counter has expired drains commands and advances the orderlist until
//! it lands on the next note/rest. The same walk retains exact order commands,
//! source pattern rows, repeat counts, transposes, and runtime placements.
//! Timbre and effect state is left to the shared trace-based characterisation.

use super::{
    CallResidual, DriverExtractor, FieldProvenance, NativeContext, NativeError, NativeSong,
    NativeValidationPolicy, ProvenanceEvidence, note_from_raw_freq, validate_native_notes,
};
#[cfg(all(test, feature = "asset-tests"))]
use super::{MIN_AGREEMENT, onset_agreement};
use crate::analysis::effects::{EffectThresholds, detect_effects};
use crate::analysis::note::{NoteEvent, detect_notes, hertz_to_midi};
use crate::analysis::timbre::{
    apply_voice3_lfo_detection, extract_characteristics, extract_patches,
};
use crate::analysis::{FrameState, SystemClock, VoiceId, analyze};
use crate::emu::{self, Emulator};
use crate::export::{
    FrequencyTableIndex, InstrumentNumber, NativeDriverOpcode, NativeEffectByte, NativeOperand,
    NativePlacement, NativeRowTick, OrderOffset, PatternByteOffset, PatternDuration, PatternNumber,
    PatternRepeatCount, PatternTranspose, RecoveredOrderCommand, RecoveredPatternEvent,
    RecoveredPatternInstance, RecoveredStructure, RecoveredVoiceStructure, RepeatOrdinal,
    VoicePlacements,
};
use crate::trace::FrameIndex;

/// Caps on the per-frame command drain and the per-fetch orderlist advance —
/// generous bounds that only trip on malformed / unsupported data, never on a
/// real song (a pattern row is at most a handful of commands).
const MAX_DRAIN: u32 = 256;
const MAX_ORDER_ADVANCE: u32 = 256;
const EFFECT_PITCH_EVIDENCE_RADIUS: CallResidual = CallResidual(2);

/// Addresses of the Gremlin player's data cells, read out of three matched
/// instruction signatures (raw `u16`, relocation-dependent — the same
/// convention as the other native layouts).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct GremlinLayout {
    /// Per-voice pattern-pointer zero-page base (`$F7`): voice *v* reads its
    /// pattern through `($F7 + 2v)`.
    pub stream_zp: u8,
    /// Parallel top-octave frequency tables, indexed by `note + transpose +
    /// note_base`: lo (`$C8FA`) and hi (`$C95A`).
    pub freq_lo: u16,
    pub freq_hi: u16,
    /// Per-voice transpose / detune cell bases (`$C0BB` / `$C0ED`, X-indexed
    /// 0..2), set by orderlist commands.
    pub transpose_cell: u16,
    pub detune_cell: u16,
    /// The `ADC #imm` note-index bias (`$14` in every known build), read from
    /// the note-decoder signature.
    pub note_base: u8,
    /// Pattern-pointer tables (`$CB70` lo / `$CB8F` hi), indexed by pattern
    /// number.
    pub pat_lo: u16,
    pub pat_hi: u16,
    /// The subtune-selector cell (`$C0C1`) and the per-subtune orderlist-base
    /// pointer table (`$CA39`): voice *v*'s orderlist base is the little-endian
    /// word at `orderlist_table + ram[subtune_cell] + 2v`.
    pub subtune_cell: u16,
    pub orderlist_table: u16,
    active_voices: [bool; 3],
}

/// Locate the player's data cells from the post-`init` RAM image. Keys on three
/// relocation-independent instruction shapes (operands wildcarded), demanding
/// exactly one of each, and reads every address out of the matched operands.
#[must_use]
pub(crate) fn locate(ram: &[u8]) -> Option<GremlinLayout> {
    if ram.len() < 0x10000 {
        return None;
    }
    let note = find_unique(ram, 0x0200, 0xFF00, match_note_decoder)
        .or_else(|| find_unique(ram, 0x0200, 0xFF00, match_zero_page_note_decoder))?;
    let pat = find_unique(ram, 0x0200, 0xFF00, match_pattern_lookup)?;
    let order = find_unique(ram, 0x0200, 0xFF00, match_orderlist_setup)
        .or_else(|| find_unique(ram, 0x0200, 0xFF00, match_strict_repeated_orderlist_setup))
        .or_else(|| find_unique(ram, 0x0200, 0xFF00, match_repeated_orderlist_setup))
        .or_else(|| find_unique(ram, 0x0200, 0xFF00, match_zero_page_orderlist_setup))?;
    Some(GremlinLayout {
        stream_zp: pat.stream_zp,
        freq_lo: note.freq_lo,
        freq_hi: note.freq_hi,
        transpose_cell: note.transpose_cell,
        detune_cell: note.detune_cell,
        note_base: note.note_base,
        pat_lo: pat.pat_lo,
        pat_hi: pat.pat_hi,
        subtune_cell: order.subtune_cell,
        orderlist_table: order.orderlist_table,
        active_voices: order.active_voices,
    })
}

/// Scan `[lo, hi)` for the single address where `f` matches, returning `None`
/// if there is not exactly one (zero = unsupported relocation, more than one =
/// ambiguous signature).
fn find_unique<T>(
    ram: &[u8],
    lo: u16,
    hi: u16,
    f: impl Fn(&[u8], usize) -> Option<T>,
) -> Option<T> {
    let mut found = None;
    for a in usize::from(lo)..usize::from(hi) {
        if let Some(hit) = f(ram, a) {
            if found.is_some() {
                return None;
            }
            found = Some(hit);
        }
    }
    found
}

fn match_count<T>(ram: &[u8], f: impl Fn(&[u8], usize) -> Option<T>) -> usize {
    (0x0200_usize..0xFF00)
        .filter(|address| f(ram, *address).is_some())
        .count()
}

fn location_failure(ram: &[u8]) -> String {
    format!(
        "required Gremlin signatures were not unique (note_decoder={}, zero_page_note_decoder={}, pattern_lookup={}, orderlist_setup={}, strict_repeated_orderlist_setup={}, repeated_orderlist_setup={}, zero_page_orderlist_setup={})",
        match_count(ram, match_note_decoder),
        match_count(ram, match_zero_page_note_decoder),
        match_count(ram, match_pattern_lookup),
        match_count(ram, match_orderlist_setup),
        match_count(ram, match_strict_repeated_orderlist_setup),
        match_count(ram, match_repeated_orderlist_setup),
        match_count(ram, match_zero_page_orderlist_setup)
    )
}

struct NoteMatch {
    freq_lo: u16,
    freq_hi: u16,
    transpose_cell: u16,
    detune_cell: u16,
    note_base: u8,
}

/// The note decoder ($C46F): `CLC / ADC transpose,X / CLC / ADC #base / TAY /
/// STA ??,X / LDA freq_lo,Y / CLC / ADC detune,X / PHA / LDA #0 / ADC freq_hi,Y`.
fn match_note_decoder(ram: &[u8], a: usize) -> Option<NoteMatch> {
    let b = ram.get(a..a + 24)?;
    let ok = b[0] == 0x18
        && b[1] == 0x7D
        && b[4] == 0x18
        && b[5] == 0x69
        && b[7] == 0xA8
        && b[8] == 0x9D
        && b[11] == 0xB9
        && b[14] == 0x18
        && b[15] == 0x7D
        && b[18] == 0x48
        && b[19] == 0xA9
        && b[20] == 0x00
        && b[21] == 0x79;
    if !ok {
        return None;
    }
    Some(NoteMatch {
        transpose_cell: u16::from_le_bytes([b[2], b[3]]),
        note_base: b[6],
        freq_lo: u16::from_le_bytes([b[12], b[13]]),
        detune_cell: u16::from_le_bytes([b[16], b[17]]),
        freq_hi: u16::from_le_bytes([b[22], b[23]]),
    })
}

fn match_zero_page_note_decoder(ram: &[u8], a: usize) -> Option<NoteMatch> {
    let b = ram.get(a..a + 21)?;
    let ok = b[0] == 0x18
        && b[1] == 0x75
        && b[3] == 0x18
        && b[4] == 0x69
        && b[6] == 0xA8
        && b[7] == 0x95
        && b[9] == 0xB9
        && b[12] == 0x18
        && b[13] == 0x75
        && b[15] == 0x48
        && b[16] == 0xA9
        && b[17] == 0x00
        && b[18] == 0x79;
    if !ok {
        return None;
    }
    Some(NoteMatch {
        transpose_cell: u16::from(b[2]),
        note_base: b[5],
        freq_lo: u16::from_le_bytes([b[10], b[11]]),
        detune_cell: u16::from(b[14]),
        freq_hi: u16::from_le_bytes([b[19], b[20]]),
    })
}

struct PatMatch {
    pat_lo: u16,
    pat_hi: u16,
    stream_zp: u8,
}

/// The pattern-pointer lookup ($C356): `TAY / TXA / ASL / TAX / LDA pat_lo,Y /
/// STA zp,X / LDA pat_hi,Y / STA zp+1,X`.
fn match_pattern_lookup(ram: &[u8], a: usize) -> Option<PatMatch> {
    let b = ram.get(a..a + 14)?;
    let ok = b[0] == 0xA8
        && b[1] == 0x8A
        && b[2] == 0x0A
        && b[3] == 0xAA
        && b[4] == 0xB9
        && b[7] == 0x95
        && b[9] == 0xB9
        && b[12] == 0x95
        && b[13] == b[8].wrapping_add(1);
    if !ok {
        return None;
    }
    Some(PatMatch {
        pat_lo: u16::from_le_bytes([b[5], b[6]]),
        pat_hi: u16::from_le_bytes([b[10], b[11]]),
        stream_zp: b[8],
    })
}

struct OrderMatch {
    subtune_cell: u16,
    orderlist_table: u16,
    active_voices: [bool; 3],
}

/// The orderlist-base setup in the play loop ($C168): `LDY subtune / STA ?? /
/// STA ?? / STA ?? / LDA orderlist_table,Y / STA $FD`.
fn match_orderlist_setup(ram: &[u8], a: usize) -> Option<OrderMatch> {
    let b = ram.get(a..a + 18)?;
    let ok = b[0] == 0xAC
        && b[3] == 0x8D
        && b[6] == 0x8D
        && b[9] == 0x8D
        && b[12] == 0xB9
        && b[15] == 0x85;
    if !ok {
        return None;
    }
    Some(OrderMatch {
        subtune_cell: u16::from_le_bytes([b[1], b[2]]),
        orderlist_table: u16::from_le_bytes([b[13], b[14]]),
        active_voices: [true; 3],
    })
}

struct SetupPointerMatch {
    table: u16,
    pointer_zp: u8,
    operand_at: usize,
}

fn pointer_pair_operand(ram: &[u8], start: usize, end: usize) -> Option<SetupPointerMatch> {
    let end = end.min(ram.len().saturating_sub(10));
    for at in start..=end {
        let b = ram.get(at..at + 10)?;
        if b[0] != 0xB9
            || b[3] != 0x85
            || b[5] != 0xB9
            || b[8] != 0x85
            || u16::from_le_bytes([b[6], b[7]]) != u16::from_le_bytes([b[1], b[2]]).wrapping_add(1)
            || b[9] != b[4].wrapping_add(1)
        {
            continue;
        }
        return Some(SetupPointerMatch {
            table: u16::from_le_bytes([b[1], b[2]]),
            pointer_zp: b[4],
            operand_at: at,
        });
    }
    None
}

fn setup_table_operand(ram: &[u8], a: usize, subtune_cell: u16) -> Option<u16> {
    let prefix = ram.get(a..a + 3)?;
    if prefix[0] != 0xAC || u16::from_le_bytes([prefix[1], prefix[2]]) != subtune_cell {
        return None;
    }
    let end = (a + 48).min(ram.len().saturating_sub(3));
    for at in a + 3..=end {
        if ram[at] == 0xB9 && matches!(ram.get(at + 3), Some(0x85 | 0x8D)) {
            return Some(u16::from_le_bytes([ram[at + 1], ram[at + 2]]));
        }
    }
    None
}

fn next_setup_with_table(ram: &[u8], start: usize, subtune_cell: u16, table: u16) -> Option<usize> {
    let end = (start + 0x90).min(ram.len().saturating_sub(3));
    (start..=end).find(|address| {
        setup_table_operand(ram, *address, subtune_cell).is_some_and(|found| found == table)
    })
}

fn third_voice_active(ram: &[u8], subtune_cell: u16, start: usize, end: usize) -> bool {
    let end = end.min(ram.len().saturating_sub(3));
    for at in start..=end {
        if ram[at] == 0xC0 && matches!(ram[at + 2], 0x90 | 0x91 | 0xB0 | 0xB1) {
            return ram[usize::from(subtune_cell)] < ram[at + 1];
        }
    }
    true
}

/// Older builds repeat a longer per-voice setup block instead of using the
/// compact three-`STA` form. The three blocks reference consecutive words in
/// one interleaved orderlist-pointer table.
fn match_repeated_orderlist_setup(ram: &[u8], a: usize) -> Option<OrderMatch> {
    let prefix = ram.get(a..a + 3)?;
    if prefix[0] != 0xAC {
        return None;
    }
    let subtune_cell = u16::from_le_bytes([prefix[1], prefix[2]]);
    let orderlist_table = setup_table_operand(ram, a, subtune_cell)?;
    let second = next_setup_with_table(ram, a + 3, subtune_cell, orderlist_table.wrapping_add(2))?;
    let third = next_setup_with_table(
        ram,
        second + 3,
        subtune_cell,
        orderlist_table.wrapping_add(4),
    )?;
    Some(OrderMatch {
        subtune_cell,
        orderlist_table,
        active_voices: [
            true,
            true,
            third_voice_active(ram, subtune_cell, third + 3, third + 48),
        ],
    })
}

fn strict_setup_table_operand(
    ram: &[u8],
    a: usize,
    subtune_cell: u16,
) -> Option<SetupPointerMatch> {
    let prefix = ram.get(a..a + 3)?;
    if prefix[0] != 0xAC || u16::from_le_bytes([prefix[1], prefix[2]]) != subtune_cell {
        return None;
    }
    pointer_pair_operand(ram, a + 3, a + 48)
}

fn next_strict_setup_with_table(
    ram: &[u8],
    start: usize,
    subtune_cell: u16,
    table: u16,
    pointer_zp: u8,
) -> Option<(usize, SetupPointerMatch)> {
    let end = (start + 0x90).min(ram.len().saturating_sub(3));
    for address in start..=end {
        if let Some(found) = strict_setup_table_operand(ram, address, subtune_cell)
            && found.table == table
            && found.pointer_zp == pointer_zp
        {
            return Some((address, found));
        }
    }
    None
}

fn match_strict_repeated_orderlist_setup(ram: &[u8], a: usize) -> Option<OrderMatch> {
    let prefix = ram.get(a..a + 3)?;
    if prefix[0] != 0xAC {
        return None;
    }
    let subtune_cell = u16::from_le_bytes([prefix[1], prefix[2]]);
    let first = strict_setup_table_operand(ram, a, subtune_cell)?;
    let (second, _) = next_strict_setup_with_table(
        ram,
        a + 3,
        subtune_cell,
        first.table.wrapping_add(2),
        first.pointer_zp,
    )?;
    let (third, third_setup) = next_strict_setup_with_table(
        ram,
        second + 3,
        subtune_cell,
        first.table.wrapping_add(4),
        first.pointer_zp,
    )?;
    Some(OrderMatch {
        subtune_cell,
        orderlist_table: first.table,
        active_voices: [
            true,
            true,
            third_voice_active(ram, subtune_cell, third + 3, third_setup.operand_at),
        ],
    })
}

fn zero_page_setup_table_operand(
    ram: &[u8],
    a: usize,
    subtune_cell: u8,
) -> Option<SetupPointerMatch> {
    let prefix = ram.get(a..a + 2)?;
    if prefix[0] != 0xA4 || prefix[1] != subtune_cell {
        return None;
    }
    pointer_pair_operand(ram, a + 2, a + 48)
}

fn next_zero_page_setup_with_table(
    ram: &[u8],
    start: usize,
    subtune_cell: u8,
    table: u16,
    pointer_zp: u8,
) -> Option<(usize, SetupPointerMatch)> {
    let end = (start + 0x90).min(ram.len().saturating_sub(2));
    for address in start..=end {
        if let Some(found) = zero_page_setup_table_operand(ram, address, subtune_cell)
            && found.table == table
            && found.pointer_zp == pointer_zp
        {
            return Some((address, found));
        }
    }
    None
}

fn match_zero_page_orderlist_setup(ram: &[u8], a: usize) -> Option<OrderMatch> {
    let prefix = ram.get(a..a + 2)?;
    if prefix[0] != 0xA4 {
        return None;
    }
    let subtune_cell = prefix[1];
    let first = zero_page_setup_table_operand(ram, a, subtune_cell)?;
    let (second, _) = next_zero_page_setup_with_table(
        ram,
        a + 2,
        subtune_cell,
        first.table.wrapping_add(2),
        first.pointer_zp,
    )?;
    let (third, third_setup) = next_zero_page_setup_with_table(
        ram,
        second + 2,
        subtune_cell,
        first.table.wrapping_add(4),
        first.pointer_zp,
    )?;
    let subtune_address = u16::from(subtune_cell);
    Some(OrderMatch {
        subtune_cell: subtune_address,
        orderlist_table: first.table,
        active_voices: [
            true,
            true,
            third_voice_active(ram, subtune_address, third + 2, third_setup.operand_at),
        ],
    })
}

/// Per-voice simulation state, mirroring the player's per-voice cells.
struct VoiceSim {
    order_base: u16,
    order_idx: u16,
    pat_ptr: u16,
    stream_idx: u16,
    dur: u32,
    transpose: u8,
    detune: u8,
    loop_count: u8,
    pattern_number: Option<PatternNumber>,
    pattern_order_offset: OrderOffset,
    active: bool,
}

pub(crate) struct DecodedSong {
    notes: Vec<NoteEvent>,
    structure: Vec<VoicePlacements>,
    recovered_structure: RecoveredStructure,
}

struct StructureRecorder {
    patterns: std::collections::BTreeMap<PatternNumber, Vec<RecoveredPatternEvent>>,
    placements: [Vec<NativePlacement>; 3],
    instances: [Vec<RecoveredPatternInstance>; 3],
    order_commands: [Vec<RecoveredOrderCommand>; 3],
}

impl StructureRecorder {
    fn new() -> Self {
        Self {
            patterns: std::collections::BTreeMap::new(),
            placements: std::array::from_fn(|_| Vec::new()),
            instances: std::array::from_fn(|_| Vec::new()),
            order_commands: std::array::from_fn(|_| Vec::new()),
        }
    }

    fn event(&mut self, pattern: PatternNumber, event: RecoveredPatternEvent) {
        let events = self.patterns.entry(pattern).or_default();
        if !events.contains(&event) {
            events.push(event);
        }
    }

    fn command(&mut self, voice: usize, command: RecoveredOrderCommand) {
        if !self.order_commands[voice].contains(&command) {
            self.order_commands[voice].push(command);
        }
    }

    fn placement(
        &mut self,
        voice: usize,
        pattern: PatternNumber,
        transpose: u8,
        order_offset: OrderOffset,
        frame: u32,
    ) {
        let repeat_ordinal = RepeatOrdinal(
            self.instances[voice]
                .iter()
                .filter(|instance| instance.pattern == pattern)
                .count() as u32,
        );
        let transpose = PatternTranspose(i16::from(transpose as i8));
        self.placements[voice].push(NativePlacement {
            pattern_number: pattern,
            start_frame: FrameIndex(frame),
            transpose,
            order_offset: Some(order_offset),
            repeat_ordinal: Some(repeat_ordinal),
        });
        self.instances[voice].push(RecoveredPatternInstance {
            pattern,
            transpose,
            repeat_ordinal,
            order_offset,
            start_tick: NativeRowTick(frame),
            start_frame: FrameIndex(frame),
        });
    }

    fn finish(mut self) -> (Vec<VoicePlacements>, RecoveredStructure) {
        let mut structure = Vec::new();
        let mut voices = Vec::new();
        for voice in 0..3 {
            if self.instances[voice].is_empty() {
                continue;
            }
            structure.push(VoicePlacements {
                voice: VoiceId::from_index(voice),
                placements: std::mem::take(&mut self.placements[voice]),
            });
            voices.push(RecoveredVoiceStructure {
                voice: VoiceId::from_index(voice),
                order_loop_offset: None,
                order_commands: std::mem::take(&mut self.order_commands[voice]),
                instances: std::mem::take(&mut self.instances[voice]),
            });
        }
        (
            structure,
            RecoveredStructure {
                patterns: self.patterns,
                voices,
            },
        )
    }
}

fn initial_pitch_state(ram: &[u8], layout: &GremlinLayout, voice: usize) -> (u8, u8) {
    let offset = u16::try_from(voice).unwrap_or(0);
    (
        ram[usize::from(layout.transpose_cell.wrapping_add(offset))],
        ram[usize::from(layout.detune_cell.wrapping_add(offset))],
    )
}

/// Faithfully simulate the player for `frames` frames, emitting a [`NoteEvent`]
/// at every note-row fetch. The timbre/effect state the real player also keeps
/// is not modelled — only onsets, voice, and pitch, which is what the
/// agreement gate scores.
#[must_use]
pub(crate) fn decode_song(
    ram: &[u8],
    layout: &GremlinLayout,
    clock: SystemClock,
    frames: u32,
) -> DecodedSong {
    let rd = |addr: u16| ram[usize::from(addr)];
    let rd16 = |addr: u16| u16::from_le_bytes([rd(addr), rd(addr.wrapping_add(1))]);

    let subtune = u16::from(rd(layout.subtune_cell));
    let mut voices: Vec<VoiceSim> = (0..3u16)
        .map(|v| {
            let (transpose, detune) = initial_pitch_state(ram, layout, usize::from(v));
            VoiceSim {
                order_base: rd16(
                    layout
                        .orderlist_table
                        .wrapping_add(subtune)
                        .wrapping_add(2 * v),
                ),
                order_idx: 0,
                // The player seeds each voice's pattern pointer (in zero page) to
                // an `$FF` sentinel during init, so the first row fetch advances
                // the orderlist to the real first pattern. Read that seed from the
                // post-init zero page rather than re-deriving it.
                pat_ptr: rd16(u16::from(layout.stream_zp).wrapping_add(2 * v)),
                stream_idx: 0,
                dur: 0,
                transpose,
                detune,
                loop_count: 0,
                pattern_number: None,
                pattern_order_offset: OrderOffset(0),
                active: layout.active_voices[usize::from(v)],
            }
        })
        .collect();

    let mut notes: Vec<NoteEvent> = Vec::new();
    let mut recorder = StructureRecorder::new();
    // Open onsets per voice (index into `notes`) so a fetch can close the
    // previous note's `end_frame` at the exact frame the next row begins.
    let mut open: [Option<usize>; 3] = [None, None, None];

    for frame in 0..frames {
        for (vi, voice) in voices.iter_mut().enumerate() {
            if !voice.active {
                continue;
            }
            // The player's pre-scan zeroes a counter that reaches 1 and fetches
            // the next row that same frame, so a row of `dur` occupies exactly
            // `dur` frames — expire at 1, not 0.
            if voice.dur > 1 {
                voice.dur -= 1;
                continue;
            }
            // Counter expired: drain to the next note/rest row.
            if let Some(idx) = open[vi].take() {
                notes[idx].end_frame = Some(crate::trace::FrameIndex(frame));
            }
            match fetch_row(ram, layout, voice, vi, clock, frame, &mut recorder) {
                Row::Note(note) => {
                    open[vi] = Some(notes.len());
                    notes.push(note);
                }
                Row::Rest | Row::Ended => {}
            }
        }
    }
    // Close any still-open notes at the end of the simulated window.
    for (vi, idx) in open.iter().enumerate() {
        if let Some(i) = idx {
            let end = notes[*i].start_frame.0.saturating_add(voices[vi].dur);
            notes[*i].end_frame = Some(crate::trace::FrameIndex(end.min(frames)));
        }
    }
    let (structure, recovered_structure) = recorder.finish();
    DecodedSong {
        notes,
        structure,
        recovered_structure,
    }
}

/// Outcome of fetching one voice's next row.
enum Row {
    Note(NoteEvent),
    Rest,
    Ended,
}

/// Drain commands and advance the orderlist for one voice until it lands on a
/// note or rest (which sets the duration counter), the song ends, or the caps
/// trip. Mutates `voice` exactly as the player's row processor does.
fn fetch_row(
    ram: &[u8],
    layout: &GremlinLayout,
    voice: &mut VoiceSim,
    voice_index: usize,
    clock: SystemClock,
    frame: u32,
    recorder: &mut StructureRecorder,
) -> Row {
    let rd = |addr: u16| ram[usize::from(addr)];
    for _ in 0..MAX_DRAIN {
        let byte = rd(voice.pat_ptr.wrapping_add(voice.stream_idx));
        if byte == 0xFF {
            if voice.loop_count > 0 {
                voice.loop_count -= 1;
                voice.stream_idx = 0;
                if let Some(pattern) = voice.pattern_number {
                    recorder.placement(
                        voice_index,
                        pattern,
                        voice.transpose,
                        voice.pattern_order_offset,
                        frame,
                    );
                }
                continue;
            }
            if !advance_orderlist(ram, layout, voice, voice_index, frame, recorder) {
                voice.active = false;
                return Row::Ended;
            }
            continue;
        }
        if byte < 0x80 {
            let offset = voice.stream_idx;
            let dur = rd(voice.pat_ptr.wrapping_add(voice.stream_idx).wrapping_add(1));
            voice.stream_idx = voice.stream_idx.wrapping_add(2);
            voice.dur = u32::from(dur);
            if let Some(pattern) = voice.pattern_number {
                recorder.event(
                    pattern,
                    RecoveredPatternEvent {
                        offset: PatternByteOffset(offset),
                        duration: PatternDuration(u16::from(dur)),
                        frequency_index: Some(FrequencyTableIndex(byte)),
                        instrument: None,
                        hold: false,
                        slide: None,
                        command: None,
                        command_data: None,
                        duration_index: Some(NativeEffectByte(dur)),
                        operand: None,
                    },
                );
            }
            let raw = note_raw(layout, ram, byte, voice.transpose, voice.detune);
            let end = frame.saturating_add(u32::from(dur));
            return match note_from_raw_freq(
                raw,
                clock,
                VoiceId::from_index(voice_index),
                frame,
                end,
            ) {
                Some(n) => Row::Note(n),
                None => Row::Rest,
            };
        }
        match byte {
            0xA0 => {
                let offset = voice.stream_idx;
                let dur = rd(voice.pat_ptr.wrapping_add(voice.stream_idx).wrapping_add(1));
                voice.stream_idx = voice.stream_idx.wrapping_add(2);
                voice.dur = u32::from(dur);
                if let Some(pattern) = voice.pattern_number {
                    recorder.event(
                        pattern,
                        RecoveredPatternEvent {
                            offset: PatternByteOffset(offset),
                            duration: PatternDuration(u16::from(dur)),
                            frequency_index: None,
                            instrument: None,
                            hold: false,
                            slide: None,
                            command: Some(NativeDriverOpcode(byte)),
                            command_data: Some(NativeEffectByte(dur)),
                            duration_index: Some(NativeEffectByte(dur)),
                            operand: None,
                        },
                    );
                }
                return Row::Rest;
            }
            0xC0..=0xFE => {
                let offset = voice.stream_idx;
                let data = rd(voice.pat_ptr.wrapping_add(offset).wrapping_add(1));
                let operand = u16::from_le_bytes([
                    rd(voice.pat_ptr.wrapping_add(offset).wrapping_add(2)),
                    rd(voice.pat_ptr.wrapping_add(offset).wrapping_add(3)),
                ]);
                voice.stream_idx = voice.stream_idx.wrapping_add(4);
                if let Some(pattern) = voice.pattern_number {
                    recorder.event(
                        pattern,
                        RecoveredPatternEvent {
                            offset: PatternByteOffset(offset),
                            duration: PatternDuration(0),
                            frequency_index: None,
                            instrument: None,
                            hold: false,
                            slide: Some(NativeEffectByte(byte)),
                            command: Some(NativeDriverOpcode(byte)),
                            command_data: Some(NativeEffectByte(data)),
                            duration_index: None,
                            operand: Some(NativeOperand(operand)),
                        },
                    );
                }
            }
            // $A1/$A2 filter, $80-$8F instrument, $90-$BF parameter: one byte.
            _ => {
                let offset = voice.stream_idx;
                voice.stream_idx = voice.stream_idx.wrapping_add(1);
                if let Some(pattern) = voice.pattern_number {
                    recorder.event(
                        pattern,
                        RecoveredPatternEvent {
                            offset: PatternByteOffset(offset),
                            duration: PatternDuration(0),
                            frequency_index: None,
                            instrument: (byte <= 0x8F).then_some(InstrumentNumber(byte & 0x0F)),
                            hold: false,
                            slide: None,
                            command: Some(NativeDriverOpcode(byte)),
                            command_data: None,
                            duration_index: None,
                            operand: None,
                        },
                    );
                }
            }
        }
    }
    Row::Rest
}

/// Process orderlist commands until a pattern number is reached (sets the
/// pattern pointer, returns `true`) or the song ends (`false`).
fn advance_orderlist(
    ram: &[u8],
    layout: &GremlinLayout,
    voice: &mut VoiceSim,
    voice_index: usize,
    frame: u32,
    recorder: &mut StructureRecorder,
) -> bool {
    let rd = |addr: u16| ram[usize::from(addr)];
    let next = |voice: &mut VoiceSim| {
        let b = rd(voice.order_base.wrapping_add(voice.order_idx));
        voice.order_idx = voice.order_idx.wrapping_add(1);
        b
    };
    for _ in 0..MAX_ORDER_ADVANCE {
        let order_offset = OrderOffset(usize::from(voice.order_idx));
        let b = next(voice);
        if b == 0xFF {
            recorder.command(voice_index, RecoveredOrderCommand::Stop { order_offset });
            return false;
        }
        if b == 0xE8 {
            let _ = next(voice); // filter value
            recorder.command(
                voice_index,
                RecoveredOrderCommand::DriverCommand {
                    order_offset,
                    opcode: NativeDriverOpcode(b),
                },
            );
            continue;
        }
        if b >= 0x80 {
            let low5 = b & 0x1F;
            match (b >> 5) & 3 {
                0 => {
                    voice.transpose = next(voice);
                    recorder.command(
                        voice_index,
                        RecoveredOrderCommand::SetTranspose {
                            order_offset,
                            transpose: PatternTranspose(i16::from(voice.transpose as i8)),
                        },
                    );
                }
                1 => voice.loop_count = low5.wrapping_sub(1),
                2 => voice.detune = next(voice),
                _ => {
                    let _ = next(voice); // instrument-pointer patch value
                }
            }
            if (b >> 5) & 3 != 0 {
                recorder.command(
                    voice_index,
                    RecoveredOrderCommand::DriverCommand {
                        order_offset,
                        opcode: NativeDriverOpcode(b),
                    },
                );
            }
            continue;
        }
        // Pattern number.
        let pattern = PatternNumber(b);
        voice.pat_ptr = u16::from_le_bytes([
            rd(layout.pat_lo.wrapping_add(u16::from(b))),
            rd(layout.pat_hi.wrapping_add(u16::from(b))),
        ]);
        voice.stream_idx = 0;
        voice.pattern_number = Some(pattern);
        voice.pattern_order_offset = order_offset;
        recorder.command(
            voice_index,
            RecoveredOrderCommand::Pattern {
                order_offset,
                pattern,
                repeat: PatternRepeatCount(u32::from(voice.loop_count) + 1),
            },
        );
        recorder.placement(voice_index, pattern, voice.transpose, order_offset, frame);
        return true;
    }
    false
}

/// The 16-bit SID frequency for a note byte: index the parallel freq tables by
/// `note + transpose + note_base`, then add the per-voice detune into the lo
/// byte with carry — exactly the note decoder's arithmetic.
fn note_raw(layout: &GremlinLayout, ram: &[u8], note: u8, transpose: u8, detune: u8) -> u32 {
    let sum = u16::from(note) + u16::from(transpose) + u16::from(layout.note_base);
    let idx = (sum & 0xFF) as u8;
    let carry_in = u16::from(sum > 0xFF);
    let cell = |base: u16| ram[usize::from(base.wrapping_add(u16::from(idx)))];
    let lo_sum = u16::from(cell(layout.freq_lo)) + u16::from(detune) + carry_in;
    let lo = (lo_sum & 0xFF) as u8;
    let hi = cell(layout.freq_hi).wrapping_add((lo_sum >> 8) as u8);
    (u32::from(hi) << 8) | u32::from(lo)
}

fn unseeded_pattern_streams(ram: &[u8], layout: &GremlinLayout) -> bool {
    (0..3u16).all(|voice| {
        let address = u16::from(layout.stream_zp).wrapping_add(2 * voice);
        ram[usize::from(address)] == 0 && ram[usize::from(address.wrapping_add(1))] == 0
    })
}

fn pitch_cents(note: &NoteEvent) -> f32 {
    f32::from(note.midi.0) * 100.0 + note.cents.0
}

fn trace_contains_authored_pitch(
    note: &NoteEvent,
    states: &[FrameState],
    clock: SystemClock,
    policy: NativeValidationPolicy,
) -> bool {
    if states.is_empty() {
        return false;
    }
    let last = states.len().saturating_sub(1);
    let voice = note.voice.to_index();
    (0..=last).any(|frame| {
        let pitched = hertz_to_midi(states[frame].voices[voice].freq.to_hertz(clock)).is_some_and(
            |(midi, cents)| {
                let observed = f32::from(midi.0) * 100.0 + cents.0;
                (observed - pitch_cents(note)).abs() <= policy.maximum_pitch_cents.0
            },
        );
        pitched
            && (frame.saturating_sub(1)..=(frame + 1).min(last))
                .any(|adjacent| states[adjacent].voices[voice].control.gate)
    })
}

/// Gremlin instruments can write the authored base pitch while the gate is
/// off, then move the oscillator before raising the gate. Preserve the
/// authored note in output, but validate it against the nearest gate event
/// when the SID state independently proves that base pitch elsewhere in the
/// capture.
fn instrument_effect_validation_notes(
    native: &[NoteEvent],
    truth: &[NoteEvent],
    states: &[FrameState],
    clock: SystemClock,
    policy: NativeValidationPolicy,
) -> Vec<NoteEvent> {
    let radius = u32::try_from(EFFECT_PITCH_EVIDENCE_RADIUS.0).unwrap_or(0);
    native
        .iter()
        .map(|authored| {
            let traced = truth
                .iter()
                .filter(|traced| {
                    traced.voice == authored.voice
                        && traced.start_frame.0.abs_diff(authored.start_frame.0) <= radius
                })
                .min_by_key(|traced| traced.start_frame.0.abs_diff(authored.start_frame.0));
            if let Some(traced) = traced
                && (pitch_cents(traced) - pitch_cents(authored)).abs()
                    > policy.maximum_pitch_cents.0
                && trace_contains_authored_pitch(authored, states, clock, policy)
            {
                return NoteEvent {
                    midi: traced.midi,
                    cents: traced.cents,
                    ..*authored
                };
            }
            *authored
        })
        .collect()
}

/// The `Ben_Daglish/Gremlin` driver extractor.
pub struct GremlinExtractor;

impl DriverExtractor for GremlinExtractor {
    fn name(&self) -> &'static str {
        "gremlin"
    }

    fn handles(&self, driver: &str) -> bool {
        driver == "Ben_Daglish/Gremlin"
    }

    fn extract(&self, ctx: &NativeContext<'_>) -> Result<NativeSong, NativeError> {
        let emu_err = |e: crate::emu::EmuError| NativeError::Emulation {
            driver: ctx.driver.to_string(),
            stage: super::EmulationStage::ExtractorSetup,
            reason: e.to_string(),
        };

        let mut img = Emulator::with_timing(ctx.timing);
        img.load(ctx.header, ctx.bytes).map_err(emu_err)?;
        img.call_init(ctx.header.init_address, ctx.subtune, ctx.header.songs)
            .map_err(emu_err)?;
        let ram = img.ram_image();
        let layout = locate(&ram).ok_or(NativeError::LocateFailed {
            driver: ctx.driver.to_string(),
            extractor: self.name(),
            reason: location_failure(&ram),
        })?;
        if unseeded_pattern_streams(&ram, &layout) {
            return Err(NativeError::UnsupportedConfiguration {
                driver: ctx.driver.to_string(),
                extractor: self.name(),
                reason: "post-init pattern pointers are all zero; the first-play stream-control variant is not supported"
                    .to_owned(),
            });
        }

        let trace =
            emu::run_with_timing(ctx.header, ctx.bytes, ctx.subtune, ctx.frames, ctx.timing)
                .map_err(emu_err)?;
        let validation_timing = ctx.validation_timing(&trace, self.name())?;
        let states = analyze(&trace);
        let frame_count = states.len();
        let effects = detect_effects(&trace, &states, EffectThresholds::default());

        let decoded = decode_song(&ram, &layout, ctx.timing.clock, frame_count as u32);
        let notes = decoded.notes;
        if notes.is_empty() {
            return Err(NativeError::DecodeEmpty {
                driver: ctx.driver.to_string(),
                extractor: self.name(),
            });
        }

        let truth = detect_notes(&states, ctx.timing.clock);
        let validation_policy = NativeValidationPolicy::default();
        let validation_notes = instrument_effect_validation_notes(
            &notes,
            &truth,
            &states,
            ctx.timing.clock,
            validation_policy,
        );
        let validation = validate_native_notes(
            &validation_notes,
            &truth,
            validation_timing,
            validation_policy,
        );
        if !validation.accepted {
            return Err(NativeError::DecodeUnreliable {
                driver: ctx.driver.to_string(),
                extractor: self.name(),
                reason: validation.reason_summary(),
            });
        }

        let voice3_reads = trace.voice3_reads_per_frame();
        let mut characteristics: Vec<_> = notes
            .iter()
            .map(|n| extract_characteristics(n, &states, &effects, ctx.timing.clock))
            .collect();
        apply_voice3_lfo_detection(&mut characteristics, &notes, &voice3_reads);

        let (patches, patch_assignments) = extract_patches(&notes, &characteristics);
        let structure_patterns = decoded.recovered_structure.patterns.len();

        Ok(NativeSong {
            capture: trace.capture.clone(),
            states,
            notes,
            patches,
            patch_assignments,
            characteristics,
            effects,
            structure: Some(decoded.structure),
            recovered_structure: Some(decoded.recovered_structure),
            validation,
            provenance: vec![ProvenanceEvidence {
                field: "song.structure".to_owned(),
                provenance: FieldProvenance::AuthoredDecoded,
                samples: structure_patterns,
                mismatches: 0,
            }],
        })
    }
}

#[cfg(all(test, feature = "asset-tests"))]
mod tests {
    use super::*;
    use crate::header::SubtuneIndex;

    fn post_init_ram(asset: &str, song: u16) -> (Vec<u8>, crate::header::Header) {
        let bytes = std::fs::read(format!("../../assets/music/{asset}")).unwrap();
        let header = crate::header::parse(&bytes).unwrap();
        let mut img = Emulator::new();
        img.load(&header, &bytes).unwrap();
        img.call_init(header.init_address, SubtuneIndex(song), header.songs)
            .unwrap();
        (img.ram_image(), header)
    }

    /// Ground-truth check: locate reads the exact cell addresses the hand
    /// disassembly of 720_Degrees found, with no driver knowledge baked in.
    #[test]
    fn locates_720_degrees_layout() {
        let (ram, _) = post_init_ram("720_Degrees.sid", 2);
        let l = locate(&ram).expect("Gremlin layout located");
        assert_eq!(l.stream_zp, 0xF7, "pattern-pointer zp base");
        assert_eq!(l.freq_lo, 0xC8FA, "freq_lo table");
        assert_eq!(l.freq_hi, 0xC95A, "freq_hi table");
        assert_eq!(l.note_base, 0x14, "note-index bias");
        assert_eq!(l.pat_lo, 0xCB70, "pattern-pointer lo table");
        assert_eq!(l.pat_hi, 0xCB8F, "pattern-pointer hi table");
        assert_eq!(l.subtune_cell, 0xC0C1, "subtune selector");
        assert_eq!(l.orderlist_table, 0xCA39, "orderlist-base table");
    }

    #[test]
    fn zero_page_note_decoder_recovers_older_cell_layout() {
        let mut ram = vec![0; 0x1_0000];
        ram[0x1572..0x1587].copy_from_slice(&[
            0x18, 0x75, 0x34, 0x18, 0x69, 0x14, 0xA8, 0x95, 0x8E, 0xB9, 0xF6, 0x18, 0x18, 0x75,
            0x4C, 0x48, 0xA9, 0x00, 0x79, 0x56, 0x19,
        ]);

        let match_ = match_zero_page_note_decoder(&ram, 0x1572).unwrap();
        assert_eq!(match_.transpose_cell, 0x34);
        assert_eq!(match_.detune_cell, 0x4C);
        assert_eq!(match_.note_base, 0x14);
        assert_eq!(match_.freq_lo, 0x18F6);
        assert_eq!(match_.freq_hi, 0x1956);
    }

    #[test]
    fn repeated_order_setup_recovers_two_voice_guard() {
        let mut ram = vec![0; 0x1_0000];
        let subtune_cell = 0x1000_u16;
        ram[usize::from(subtune_cell)] = 0x20;
        for (address, table) in [(0x2000, 0x3000_u16), (0x2040, 0x3002), (0x2080, 0x3004)] {
            ram[address..address + 3].copy_from_slice(&[
                0xAC,
                subtune_cell as u8,
                (subtune_cell >> 8) as u8,
            ]);
            let operand = table.to_le_bytes();
            let next_operand = table.wrapping_add(1).to_le_bytes();
            ram[address + 10..address + 20].copy_from_slice(&[
                0xB9,
                operand[0],
                operand[1],
                0x85,
                0x09,
                0xB9,
                next_operand[0],
                next_operand[1],
                0x85,
                0x0A,
            ]);
        }
        ram[0x2083..0x2086].copy_from_slice(&[0xC0, 0x1D, 0xB0]);

        let match_ = match_repeated_orderlist_setup(&ram, 0x2000).unwrap();
        assert_eq!(match_.subtune_cell, subtune_cell);
        assert_eq!(match_.orderlist_table, 0x3000);
        assert_eq!(match_.active_voices, [true, true, false]);
        assert_eq!(
            match_strict_repeated_orderlist_setup(&ram, 0x2000)
                .unwrap()
                .orderlist_table,
            0x3000
        );

        ram[usize::from(subtune_cell)] = 0x10;
        assert_eq!(
            match_repeated_orderlist_setup(&ram, 0x2000)
                .unwrap()
                .active_voices,
            [true; 3]
        );
    }

    #[test]
    fn zero_page_order_setup_recovers_interleaved_pointer_table() {
        let mut ram = vec![0; 0x1_0000];
        let subtune_cell = 0x92_u8;
        ram[usize::from(subtune_cell)] = 0x0B;
        for (address, table) in [(0x2000, 0x3000_u16), (0x2040, 0x3002), (0x2080, 0x3004)] {
            ram[address..address + 2].copy_from_slice(&[0xA4, subtune_cell]);
            let operand = table.to_le_bytes();
            let next_operand = table.wrapping_add(1).to_le_bytes();
            ram[address + 10..address + 20].copy_from_slice(&[
                0xB9,
                operand[0],
                operand[1],
                0x85,
                0x09,
                0xB9,
                next_operand[0],
                next_operand[1],
                0x85,
                0x0A,
            ]);
        }
        ram[0x2082..0x2085].copy_from_slice(&[0xC0, 0x0B, 0x90]);

        let match_ = match_zero_page_orderlist_setup(&ram, 0x2000).unwrap();
        assert_eq!(match_.subtune_cell, u16::from(subtune_cell));
        assert_eq!(match_.orderlist_table, 0x3000);
        assert_eq!(match_.active_voices, [true, true, false]);

        ram[usize::from(subtune_cell)] = 0x0A;
        assert_eq!(
            match_zero_page_orderlist_setup(&ram, 0x2000)
                .unwrap()
                .active_voices,
            [true; 3]
        );
    }

    #[test]
    fn decoder_seeds_pitch_state_from_post_init_cells() {
        let mut ram = vec![0; 0x1_0000];
        let layout = GremlinLayout {
            stream_zp: 0,
            freq_lo: 0,
            freq_hi: 0,
            transpose_cell: 0x2000,
            detune_cell: 0x3000,
            note_base: 0,
            pat_lo: 0,
            pat_hi: 0,
            subtune_cell: 0,
            orderlist_table: 0,
            active_voices: [true; 3],
        };
        ram[0x2001] = 0xF4;
        ram[0x3001] = 7;
        assert_eq!(initial_pitch_state(&ram, &layout, 1), (0xF4, 7));
    }

    #[test]
    fn recognizes_unseeded_first_play_stream_variant() {
        let mut ram = vec![0; 0x1_0000];
        let layout = GremlinLayout {
            stream_zp: 0xF7,
            freq_lo: 0,
            freq_hi: 0,
            transpose_cell: 0,
            detune_cell: 0,
            note_base: 0,
            pat_lo: 0,
            pat_hi: 0,
            subtune_cell: 0,
            orderlist_table: 0,
            active_voices: [true; 3],
        };
        assert!(unseeded_pattern_streams(&ram, &layout));
        ram[0xF8] = 0x80;
        assert!(!unseeded_pattern_streams(&ram, &layout));
    }

    #[test]
    fn validation_accepts_authored_pitch_proven_by_gate_adjacent_state() {
        let authored = note_from_raw_freq(0x1000, SystemClock::Pal, VoiceId::V1, 1, 5)
            .expect("authored pitch");
        let traced =
            note_from_raw_freq(0x0800, SystemClock::Pal, VoiceId::V1, 2, 4).expect("traced pitch");
        let mut states = vec![FrameState::default(); 4];
        states[1].voices[0] =
            crate::analysis::voice::VoiceState::from_regs(&[0x00, 0x10, 0, 0, 0x10, 0, 0]);
        states[2].voices[0] =
            crate::analysis::voice::VoiceState::from_regs(&[0x00, 0x08, 0, 0, 0x11, 0, 0]);

        let checked = instrument_effect_validation_notes(
            &[authored],
            &[traced],
            &states,
            SystemClock::Pal,
            NativeValidationPolicy::default(),
        );

        assert_eq!(checked[0].midi, traced.midi);
        assert_eq!(checked[0].cents, traced.cents);
        assert_eq!(authored.start_frame, checked[0].start_frame);
        assert_ne!(authored.midi, checked[0].midi);

        let unproven = note_from_raw_freq(0x2000, SystemClock::Pal, VoiceId::V1, 1, 5)
            .expect("unproven authored pitch");
        let unchecked = instrument_effect_validation_notes(
            &[unproven],
            &[traced],
            &states,
            SystemClock::Pal,
            NativeValidationPolicy::default(),
        );
        assert_eq!(unchecked[0].midi, unproven.midi);
        assert_eq!(unchecked[0].cents, unproven.cents);
    }

    /// The faithful simulation must agree with the emulated register trace:
    /// same voice + pitch + onset frame for the bulk of decoded notes.
    #[test]
    fn decodes_720_degrees_above_the_gate() {
        let (ram, header) = post_init_ram("720_Degrees.sid", 2);
        let layout = locate(&ram).unwrap();
        let frames = 1500u32;
        let decoded = decode_song(&ram, &layout, SystemClock::Pal, frames);
        assert!(decoded.recovered_structure.patterns.len() > 8);
        assert!(
            decoded
                .recovered_structure
                .voices
                .iter()
                .any(|voice| voice.instances.len() > 8)
        );
        assert_eq!(
            decoded
                .recovered_structure
                .voices
                .iter()
                .map(|voice| voice.instances.len())
                .sum::<usize>(),
            decoded
                .structure
                .iter()
                .map(|voice| voice.placements.len())
                .sum::<usize>()
        );
        let notes = decoded.notes;
        assert!(!notes.is_empty(), "decoded some notes");

        let trace = emu::run(
            &header,
            &std::fs::read("../../assets/music/720_Degrees.sid").unwrap(),
            SubtuneIndex(2),
            frames,
        )
        .unwrap();
        let states = analyze(&trace);
        let truth = detect_notes(&states, SystemClock::Pal);
        let agreement = onset_agreement(&notes, &truth);
        assert!(
            agreement >= MIN_AGREEMENT,
            "onset agreement {agreement:.3} >= {MIN_AGREEMENT} ({} native vs {} trace notes)",
            notes.len(),
            truth.len()
        );
    }

    #[test]
    fn full_length_720_degrees_passes_native_validation() {
        let bytes = std::fs::read("../../assets/music/720_Degrees.sid").unwrap();
        let header = crate::header::parse(&bytes).unwrap();
        let subtune = SubtuneIndex(2);
        let timing = crate::emu::PlaybackTiming::for_subtune(&header, subtune);
        let db = crate::playerid::PlayerDb::embedded();
        let (_, extractor, song) =
            super::super::extract_native_song(&db, &header, &bytes, subtune, timing, 5_163)
                .unwrap_or_else(|error| {
                    panic!("720 Degrees full-length native extraction: {error}")
                });

        assert_eq!(extractor, "gremlin");
        assert!(song.validation.accepted);
        assert!(
            song.recovered_structure
                .as_ref()
                .is_some_and(|structure| !structure.patterns.is_empty())
        );
        assert!(
            song.structure
                .as_ref()
                .is_some_and(|voices| { voices.iter().any(|voice| voice.placements.len() > 8) })
        );
    }

    #[test]
    #[ignore = "manual triage tool; needs SID_DBG_TUNE"]
    fn dbg_gremlin_tune() {
        let Ok(path) = std::env::var("SID_DBG_TUNE") else {
            eprintln!("SID_DBG_TUNE unset; skipping");
            return;
        };
        let frames = std::env::var("SID_DBG_FRAMES")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(400);
        let bytes = std::fs::read(path).unwrap();
        let header = crate::header::parse(&bytes).unwrap();
        let mut emulator = Emulator::new();
        emulator.load(&header, &bytes).unwrap();
        emulator
            .call_init(header.init_address, header.start_song, header.songs)
            .unwrap();
        let ram = emulator.ram_image();
        let Some(layout) = locate(&ram) else {
            eprintln!("locate: FAILED");
            return;
        };
        eprintln!("layout: {layout:#?}");
        let native = decode_song(&ram, &layout, SystemClock::Pal, frames).notes;
        let trace = emu::run(&header, &bytes, header.start_song, frames).unwrap();
        let states = analyze(&trace);
        let truth = detect_notes(&states, SystemClock::Pal);
        let effects = detect_effects(&trace, &states, EffectThresholds::default());
        let timing = crate::emu::PlaybackTiming::for_subtune(&header, header.start_song)
            .resolved_from_trace(&trace);
        let validation =
            validate_native_notes(&native, &truth, timing, NativeValidationPolicy::default());
        eprintln!(
            "agreement {:.3}, {} native vs {} trace notes",
            onset_agreement(&native, &truth),
            native.len(),
            truth.len()
        );
        eprintln!("validation: {validation:#?}");
        eprintln!("effects: {effects:#?}");
        for voice in [VoiceId::V1, VoiceId::V2, VoiceId::V3] {
            let native_voice = native.iter().filter(|note| note.voice == voice);
            let truth_voice = truth.iter().filter(|note| note.voice == voice);
            for (decoded, traced) in native_voice.zip(truth_voice) {
                let (onset, pitch, duration) = super::super::pair_residuals(decoded, traced);
                if onset.0 > 1 || pitch.0 > 75.0 || duration.is_some_and(|value| value.0 > 8) {
                    eprintln!(
                        "PAIR v{} native f{}..{:?} midi{} {:+.1}c | trace f{}..{:?} midi{} {:+.1}c | residual onset{} pitch{:.1}c dur{:?}",
                        voice.0,
                        decoded.start_frame.0,
                        decoded.end_frame.map(|frame| frame.0),
                        decoded.midi.0,
                        decoded.cents.0,
                        traced.start_frame.0,
                        traced.end_frame.map(|frame| frame.0),
                        traced.midi.0,
                        traced.cents.0,
                        onset.0,
                        pitch.0,
                        duration.map(|value| value.0),
                    );
                }
            }
        }
        for note in &native {
            if !truth
                .iter()
                .any(|traced| super::super::onset_match(note, traced))
            {
                let nearest = truth
                    .iter()
                    .filter(|traced| traced.voice == note.voice)
                    .min_by_key(|traced| {
                        (i64::from(traced.start_frame.0) - i64::from(note.start_frame.0)).abs()
                    });
                eprintln!(
                    "MISS v{} f{:5}..{:?} midi{:3} {:+.1}c | nearest same-voice: f{:?}..{:?} midi{:?} cents{:?}",
                    note.voice.0,
                    note.start_frame.0,
                    note.end_frame.map(|frame| frame.0),
                    note.midi.0,
                    note.cents.0,
                    nearest.map(|traced| traced.start_frame.0),
                    nearest.and_then(|traced| traced.end_frame.map(|frame| frame.0)),
                    nearest.map(|traced| traced.midi.0),
                    nearest.map(|traced| traced.cents.0)
                );
            }
        }
    }

    /// HVSC-wide family-coverage measure: how many `Ben_Daglish/Gremlin` tunes
    /// the extractor locates and decodes above the gate. Manual tool, needs
    /// `SID_HVSC_ROOT`. Each tune runs on a detached thread with a wall-clock
    /// timeout so a pathological player can never hang the sweep.
    #[test]
    #[ignore = "manual triage tool; needs SID_HVSC_ROOT"]
    fn dbg_gremlin_hvsc_sweep() {
        use rayon::prelude::*;

        let Ok(root) = std::env::var("SID_HVSC_ROOT") else {
            eprintln!("SID_HVSC_ROOT unset; skipping HVSC sweep");
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

        let frames = 400u32;

        enum Cat {
            EmuFail,
            LocateFail,
            Empty,
            Timeout(String),
            Decoded(f64, bool, String, usize, usize),
        }

        fn analyze_one(bytes: Vec<u8>, name: String, frames: u32) -> Cat {
            let Ok(header) = crate::header::parse(&bytes) else {
                return Cat::EmuFail;
            };
            let sub = header.start_song;
            let mut img = Emulator::new();
            if img.load(&header, &bytes).is_err()
                || img
                    .call_init(header.init_address, sub, header.songs)
                    .is_err()
            {
                return Cat::EmuFail;
            }
            let ram = img.ram_image();
            let Some(layout) = locate(&ram) else {
                return Cat::LocateFail;
            };
            let native = decode_song(&ram, &layout, SystemClock::Pal, frames).notes;
            if native.is_empty() {
                return Cat::Empty;
            }
            let Ok(trace) = emu::run(&header, &bytes, sub, frames) else {
                return Cat::EmuFail;
            };
            let truth = detect_notes(&analyze(&trace), SystemClock::Pal);
            let timing =
                crate::emu::PlaybackTiming::for_subtune(&header, sub).resolved_from_trace(&trace);
            let validation =
                validate_native_notes(&native, &truth, timing, NativeValidationPolicy::default());
            Cat::Decoded(
                onset_agreement(&native, &truth),
                validation.accepted,
                name,
                native.len(),
                truth.len(),
            )
        }

        let deadline = std::time::Duration::from_secs(30);
        let db = crate::playerid::PlayerDb::embedded();
        let outcomes: Vec<Cat> = paths
            .par_iter()
            .filter_map(|path| {
                let bytes = std::fs::read(path).ok()?;
                if db.identify(&bytes) != Some("Ben_Daglish/Gremlin") {
                    return None;
                }
                let name = path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("?")
                    .to_string();
                let timeout_name = name.clone();
                let (tx, rx) = std::sync::mpsc::channel();
                std::thread::spawn(move || {
                    let _ = tx.send(analyze_one(bytes, name, frames));
                });
                Some(
                    rx.recv_timeout(deadline)
                        .unwrap_or(Cat::Timeout(timeout_name)),
                )
            })
            .collect();

        let total = outcomes.len();
        let (mut located, mut locate_fail, mut emu_fail, mut empty, mut timeout, mut pass) =
            (0, 0, 0, 0, 0, 0);
        let mut buckets = [0u32; 11];
        let mut clump: Vec<(f64, String, usize, usize)> = Vec::new();
        let mut timed_out = Vec::new();
        for c in outcomes {
            match c {
                Cat::Timeout(name) => {
                    timeout += 1;
                    timed_out.push(name);
                }
                Cat::EmuFail => emu_fail += 1,
                Cat::LocateFail => locate_fail += 1,
                Cat::Empty => {
                    located += 1;
                    empty += 1;
                }
                Cat::Decoded(onset, accepted, name, nn, nt) => {
                    located += 1;
                    buckets[((onset * 10.0).round() as usize).min(10)] += 1;
                    if accepted {
                        pass += 1;
                    } else {
                        clump.push((onset, name, nn, nt));
                    }
                }
            }
        }
        eprintln!(
            "\n=== HVSC Ben_Daglish/Gremlin sweep (start_song, {frames}f) ===\n\
             total={total} located={located} locate_fail={locate_fail} emu_fail={emu_fail} \
             timeout={timeout} empty={empty} PASS={pass}\n\
             buckets[0.0..1.0]={buckets:?}\n"
        );
        clump.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        eprintln!("--- below-gate (onset / nN / nT  name) ---");
        for (onset, name, nn, nt) in &clump {
            eprintln!("  o{onset:.2}  {nn:4}/{nt:<4}  {name}");
        }
        for name in &timed_out {
            eprintln!("  timeout {name}");
        }
    }
}
