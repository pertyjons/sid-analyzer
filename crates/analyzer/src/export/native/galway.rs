//! Native song-data extractor for Martin Galway's playroutine.
//!
//! Galway wrote his own player (reportedly after disassembling Rob Hubbard's),
//! and it is structurally quite different: hard-restart gating, table-driven
//! vibrato/arpeggio, and — in several Ocean tunes — the famous sampled "Galway
//! noise" percussion. So this is a separate reverse-engineering effort from
//! [`super::hubbard`], not a variant of it.
//!
//! # Status: locate + decode_song implemented and gated
//!
//! The driver is *identified* ([`crate::playerid`] reports `Martin_Galway` for
//! all 40 tunes in HVSC's `MUSICIANS/G/Galway_Martin/`) and the per-voice
//! sequencer grammar is fully mapped for the supported generations (see
//! `docs/drivers/galway.md`): three identical per-voice sequencers; events are
//! 2-byte notes or table-dispatched commands, with threshold/mask, duration,
//! and inline-repeat variants recovered from the player code; note->pitch uses
//! semitone tables and structure is procedural over return/repeat stacks.
//!
//! [`decode_song`] is a faithful player *simulation* over a mutable copy of the
//! post-`init` RAM image: the pattern pointers, duration counters, gosub stack,
//! register images and duration tables all live at their real addresses and the
//! command handlers mutate them exactly as the 6502 code does. Because the
//! three voices' jump tables map opcodes to *different* handlers (voice 1 even
//! dispatches odd opcodes through misaligned word reads), commands are resolved
//! by classifying the handler *code* ([`classify_handler`]) rather than by
//! opcode number — which also recovers the per-voice stack/transpose/reg-image
//! addresses straight from the instructions that use them. That covers the
//! `$95xx` generation (Neverending_Story), the later Ocean-loader generation
//! (range-checked stack ops, an end-voice `$80`, a 2-byte standalone transpose,
//! no tie byte), and Rambo's `$C0` command / `$3F` dispatch-mask revision with
//! folded high-note rows and inline repeats.
//!
//! Comic Bakery uses a separate older `$C0` command generation.
//! [`locate_comic`] finds its three sequencers, and [`decode_comic`] lets the
//! real 6502 player execute its procedural command graph while recovering
//! authored note bytes, durations, transposes, and frequency-table pitches
//! from the located cells.
//! Street Hawk and Yie Ar Kung Fu II use a related legacy `$C0` generation.
//! [`locate_legacy_c0`] strictly recovers its consecutive pointer, duration,
//! enable, transpose, frequency, and duration-table cells across all three
//! sequencers. [`decode_legacy_c0`] uses the live player for control flow and
//! retains the two activation forms without weakening the validation gate.
//!
//! All supported simulations emit [`RecoveredStructure`]. Maximal linear
//! pointer runs become patterns; note rows retain their raw duration-table
//! index, and commands retain their opcode, operand, and optional transpose
//! byte. The procedural arrangement is preserved as explicit call, jump, return,
//! transpose, native-command, and stop operations alongside runtime instances.
//! Instrument block snapshots are decoded at note time into authored pitch and
//! pulse-width programs, envelope/control, filter preset, and duration table.
//!
//! [`locate`], [`locate_comic`], and [`locate_legacy_c0`] find the three
//! per-voice sequencers by their instruction shapes and read every cell address
//! out of the code. The supported family is Comic_Bakery, Neverending_Story,
//! Ocean_Loader_1, Helikopter_Jagd, Hyper_Sports, Rambo_First_Blood_Part_II,
//! Street_Hawk_Prototype, Street_Hawk, and Yie_Ar_Kung_Fu_II. The remaining
//! Galway tunes run structurally different engines (Kong's 1984 one,
//! Highlander's workspace-swapping one, …) and fall back cleanly via
//! [`NativeError::LocateFailed`]; a decode that locates but disagrees with the
//! trace is rejected by the shared monotonic one-to-one validation report.
//!
//! Triage tests (`#[ignore]`, CI-safe, no-op without `SID_HVSC_ROOT`):
//! `dbg_galway` dumps the post-`init` RAM image for disassembly
//! (`/tmp/dis6502.py`), `dbg_galway_trace` logs a voice's pattern pointer per
//! play-frame, `dbg_galway_micro` single-steps `play` to log *every* individual
//! pointer move, and `dbg_galway_decode` validates [`decode_song`] against the
//! emulator trace with the Hubbard onset-agreement gate.

#[cfg(test)]
use super::MIN_AGREEMENT;
use super::{
    DriverExtractor, FieldProvenance, NativeContext, NativeError, NativeSong,
    NativeValidationPolicy, ProvenanceEvidence, note_from_raw_freq, validate_native_notes,
};
use crate::analysis::effects::{EffectThresholds, detect_effects};
use crate::analysis::filter::FilterState;
use crate::analysis::note::{NoteEvent, detect_notes, hertz_to_midi};
use crate::analysis::timbre::{
    AuthoredEffects, AuthoredFilterPreset, AuthoredInstrumentDefinition, AuthoredLoopMode,
    AuthoredModulationProgram, AuthoredModulationStage, FilterRoutingMask, ModulationDelta,
    NoteCharacteristics, Patch, PatchId, ProgramFrames, SidVolume, apply_voice3_lfo_detection,
    extract_characteristics, extract_patches, extract_patches_grouped,
};
use crate::analysis::voice::{Adsr, PulseWidth};
use crate::analysis::{SystemClock, VoiceId, analyze};
use crate::emu::{self, Emulator};
use crate::export::{
    FrequencyTableIndex, NativeDriverOpcode, NativeEffectByte, NativeOperand, NativePlacement,
    NativeRowTick, OrderOffset, PatternByteOffset, PatternDuration, PatternNumber,
    PatternRepeatCount, PatternTranspose, RecoveredOrderCommand, RecoveredPatternEvent,
    RecoveredPatternInstance, RecoveredStructure, RecoveredVoiceStructure, RepeatOrdinal,
    VoicePlacements,
};
use crate::trace::FrameIndex;

/// Addresses of one voice's sequencer cells. Raw `u16` absolute addresses (and
/// a raw mask bit), matching the [`crate::emu::bus::Bus`] address space — they
/// are relocation-dependent and only meaningful against one RAM image, the same
/// convention as [`super::hubbard::HubbardLayout`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct GalwayVoice {
    /// Zero-page cell holding the pattern pointer (lo at `ptr_zp`, hi at +1).
    pub ptr_zp: u16,
    /// Zero-page duration counter, `DEC`'d once per play-frame.
    pub durctr_zp: u16,
    /// This voice's bit in the active-voice mask.
    pub active_bit: u8,
    /// Base of the command word table indexed by `(cmd & $7F)` as a *byte*
    /// offset — odd opcodes read misaligned words, which is why dispatch
    /// classifies the handler code instead of the opcode number.
    pub jump_table: u16,
    /// Note-duration table: `frames = dur_table[dur-idx]` (the register image
    /// at offset `$22`, populated at runtime by block-load/poke commands).
    pub dur_table: u16,
    /// Per-voice transpose cell added to every note index.
    pub transpose: u16,
}

/// Addresses of the Galway player's data structures (see [`GalwayVoice`] for
/// the raw-`u16` convention). Currently built by hand for the proven tune;
/// a `locate` stage will recover it from the code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct GalwayLayout {
    pub voices: [GalwayVoice; 3],
    /// Semitone frequency tables, stride 1, indexed by `note + transpose`.
    pub freq_lo: u16,
    pub freq_hi: u16,
    /// Zero-page active-voice mask (one [`GalwayVoice::active_bit`] per voice).
    pub active_mask_zp: u16,
    /// Lowest byte dispatched as a command. The original engine uses bit 7
    /// (`$80`); Rambo's revision reserves `$80..$BF` for folded note rows and
    /// starts commands at `$C0`.
    command_threshold: GalwayCommandThreshold,
    /// Mask applied before indexing the byte-addressed command word table.
    command_mask: GalwayCommandMask,
    /// Whether note bytes `$60..$BF` fold down by `$60`, with their second byte
    /// used as a literal duration instead of a duration-table index.
    fold_high_notes: bool,
    /// Whether the note path treats `$5F` as a tie (the `$95xx` generation
    /// does; the later Ocean-loader generation has no tie — only `$60` = rest,
    /// and `$5F` is an ordinary note index).
    pub has_tie: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
struct GalwayCommandThreshold(u8);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
struct GalwayCommandMask(u8);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
struct TraceCorrectedNoteCount(usize);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
struct ComicCell(u16);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
struct ComicMask(u8);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ComicVoice {
    ptr_zp: ComicCell,
    durctr_zp: ComicCell,
    active_bit: ComicMask,
    gate_bit: ComicMask,
    transpose: ComicCell,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ComicLayout {
    voices: [ComicVoice; 3],
    freq_lo: ComicCell,
    freq_hi: ComicCell,
    active_mask_zp: ComicCell,
    note_enable_mask: ComicCell,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
struct LegacyC0Cell(u16);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
struct LegacyC0Mask(u8);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LegacyC0Voice {
    ptr_zp: LegacyC0Cell,
    durctr_zp: LegacyC0Cell,
    sequence_enable: LegacyC0Cell,
    note_enable: LegacyC0Cell,
    transpose: LegacyC0Cell,
    active_bit: Option<LegacyC0Mask>,
    has_rest: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LegacyC0Layout {
    voices: [LegacyC0Voice; 3],
    freq_lo: LegacyC0Cell,
    freq_hi: LegacyC0Cell,
    duration_table: LegacyC0Cell,
    active_mask: Option<LegacyC0Cell>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LegacyC0Site {
    voice: LegacyC0Voice,
    freq_lo: LegacyC0Cell,
    freq_hi: LegacyC0Cell,
    duration_table: LegacyC0Cell,
    active_mask: Option<LegacyC0Cell>,
}

#[derive(Clone, Copy)]
struct C0Observation {
    voice: usize,
    address: u16,
    value: u8,
    data: u8,
    duration: PatternDuration,
    transpose: u8,
    has_rest: bool,
    frame: FrameIndex,
}

/// Note-index byte meaning "extend the previous note" (no new gate).
const NOTE_TIE: u8 = 0x5F;
/// Note-index byte meaning "silence for the duration".
const NOTE_REST: u8 = 0x60;
const COMIC_REST: u8 = 0x5E;
const ARPEGGIO_ONSET_TOLERANCE: u32 = 8;

/// One classified command handler. Handlers embed the per-voice cell addresses
/// in their own instructions, so classification recovers them as a side effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GalwayCmd {
    /// Pop the return-pointer stack: `idx += 1`, reload the pattern pointer
    /// from `stack_lo[idx]/stack_hi[idx]`, re-read.
    Return {
        stack_idx_zp: u16,
        stack_lo: u16,
        stack_hi: u16,
    },
    /// Copy `last_src + 1` bytes from the operand address into the register
    /// image ending at offset `last_dst` (instrument / duration-table loads).
    BlockCopy {
        last_src: u8,
        last_dst: u8,
        reg_image: u16,
    },
    /// Jump the pattern pointer to the operand address.
    Goto,
    /// 4-byte form: 4th byte -> transpose cell, then goto.
    GotoTranspose { transpose: u16 },
    /// Push `ptr + 3` on the return stack (`idx -= 1` after), then goto.
    Gosub {
        stack_idx_zp: u16,
        stack_lo: u16,
        stack_hi: u16,
    },
    /// 4-byte form: 4th byte -> transpose cell, push `ptr + 4`, then goto.
    GosubTranspose {
        transpose: u16,
        stack_idx_zp: u16,
        stack_lo: u16,
        stack_hi: u16,
    },
    /// Push `ptr + 2` plus a repeat counter, then enter the inline loop body.
    LoopStart {
        stack_idx_zp: u16,
        stack_lo: u16,
        stack_hi: u16,
        repeat_counts: u16,
    },
    /// Decrement the innermost repeat counter and jump back while non-zero.
    LoopNext {
        stack_idx_zp: u16,
        stack_lo: u16,
        stack_hi: u16,
        repeat_counts: u16,
    },
    /// `reg_image[operand-lo] = operand-hi`.
    Poke { reg_image: u16 },
    /// 2-byte form `[cmd][value]`: set the transpose cell, advance +2 (the
    /// Ocean-loader generation's standalone transpose command).
    TransposeSet { transpose: u16 },
    /// `JMP (operand)` into tune-specific native code; returns to `ptr + 3`.
    /// Opaque to the note timeline — skipped.
    NativeCall,
    /// Silence everything: stores a mask with all voice bits clear.
    Stop { mask_zp: u16, mask_value: u8 },
    /// The Ocean-loader generation's `$80`: end this voice when the return
    /// stack is back at its initial index (`AND` the voice's bit out of the
    /// mask), anything else is the player's error trap.
    StopVoice {
        stack_idx_zp: u16,
        limit: u8,
        mask_zp: u16,
        and_mask: u8,
    },
    /// Unrecognised handler code (e.g. a garbage entry behind an opcode the
    /// tune never uses). Dispatching one stops the whole decode.
    Unknown,
}

#[inline]
fn rd16(ram: &[u8], a: u16) -> u16 {
    u16::from(ram[a as usize]) | (u16::from(ram[a.wrapping_add(1) as usize]) << 8)
}

#[inline]
fn wr16(ram: &mut [u8], a: u16, v: u16) {
    ram[a as usize] = (v & 0xFF) as u8;
    ram[a.wrapping_add(1) as usize] = (v >> 8) as u8;
}

/// Resolve a relative-branch operand at `at` (the offset byte itself).
fn branch_target(ram: &[u8], at: u16) -> u16 {
    let off = ram[at as usize] as i8;
    at.wrapping_add(1).wrapping_add(off as u16)
}

/// Parse the gosub push body — `LDX idx / [BMI err] / CLC / ADC ptr /
/// STA lo,X / LDA ptr+1 / ADC #0 / STA hi,X / DEC idx` — starting at the `LDX`
/// (`addr` points at `A6`). The Ocean-loader generation inserts a stack-
/// overflow `BMI` after the `LDX`. Returns `(stack_idx_zp, stack_lo, stack_hi)`.
fn parse_gosub_body(ram: &[u8], addr: u16) -> Option<(u16, u16, u16)> {
    let b = |i: u16| ram[addr.wrapping_add(i) as usize];
    let w = |i: u16| rd16(ram, addr.wrapping_add(i));
    let store = match b(0) {
        0xA6 => 0x9D,
        0xA4 => 0x99,
        _ => return None,
    };
    let p = if b(2) == 0x30 { 4u16 } else { 2 };
    (b(p) == 0x18 // CLC
        && b(p + 1) == 0x65 // ADC zp (ptr lo)
        && b(p + 3) == store // STA abs,X/Y (stack lo)
        && b(p + 6) == 0xA5 // LDA zp (ptr hi)
        && b(p + 8) == 0x69 // ADC #0
        && b(p + 10) == store // STA abs,X/Y (stack hi)
        && b(p + 13) == 0xC6) // DEC zp (stack index)
        .then(|| (u16::from(b(1)), w(p + 4), w(p + 11)))
}

/// Parse a return / orderlist-pop handler — `LDX idx / [BMI ok] / [CPX #limit /
/// BCS err] / INX / STX idx / LDA lo,X -> ptr / LDA hi,X -> ptr+1` — starting
/// at the `LDX`. The Ocean-loader generation range-checks the index before
/// popping. Returns `(stack_idx_zp, stack_lo, stack_hi)`.
fn parse_return(ram: &[u8], addr: u16) -> Option<(u16, u16, u16)> {
    let b = |i: u16| ram[addr.wrapping_add(i) as usize];
    let w = |i: u16| rd16(ram, addr.wrapping_add(i));
    if b(0) != 0xA6 {
        return None;
    }
    let mut p = 2u16;
    if b(p) == 0x30 {
        p += 2;
    }
    if b(p) == 0xE0 && b(p + 2) == 0xB0 {
        p += 4;
    }
    (b(p) == 0xE8 // INX
        && b(p + 1) == 0x86 // STX zp (stack index)
        && b(p + 2) == b(1)
        && b(p + 3) == 0xBD // LDA lo,X
        && b(p + 6) == 0x85 // STA ptr
        && b(p + 8) == 0xBD // LDA hi,X
        && b(p + 11) == 0x85) // STA ptr+1
        .then(|| (u16::from(b(1)), w(p + 4), w(p + 9)))
}

/// Parse Rambo's increment-first return handler: `INC idx / LDY idx / CPY
/// #limit / BEQ stop / LDX lo,Y / LDA hi,Y / JMP reload`.
fn parse_increment_return(ram: &[u8], addr: u16) -> Option<(u16, u16, u16)> {
    let b = |i: u16| ram[addr.wrapping_add(i) as usize];
    let w = |i: u16| rd16(ram, addr.wrapping_add(i));
    (b(0) == 0xE6
        && b(2) == 0xA4
        && b(3) == b(1)
        && b(4) == 0xC0
        && (b(6) == 0xF0 || b(6) == 0xB0)
        && b(8) == 0xBE
        && b(11) == 0xB9)
        .then(|| (u16::from(b(1)), w(9), w(12)))
}

/// Parse Rambo's inline repeat start. The high return byte is formed either as
/// `LDA ptr_hi / ADC #0` or `LDA #0 / ADC ptr_hi`; both are emitted by the same
/// player revision for different voices.
fn parse_loop_start(ram: &[u8], addr: u16) -> Option<(u16, u16, u16, u16)> {
    let b = |i: u16| ram[addr.wrapping_add(i) as usize];
    let w = |i: u16| rd16(ram, addr.wrapping_add(i));
    if b(0) != 0xA6
        || b(2) != 0x18
        || b(3) != 0x98
        || b(4) != 0x65
        || b(6) != 0x9D
        || b(13) != 0x9D
        || b(16) != 0xA5
        || b(18) != 0x9D
        || b(21) != 0xC6
        || b(22) != b(1)
        || b(23) != 0x98
    {
        return None;
    }
    let ptr_lo = b(5);
    let high_ok = (b(9) == 0xA5 && b(10) == ptr_lo.wrapping_add(1) && b(11) == 0x69 && b(12) == 0)
        || (b(9) == 0xA9 && b(10) == 0 && b(11) == 0x65 && b(12) == ptr_lo.wrapping_add(1));
    high_ok.then(|| (u16::from(b(1)), w(7), w(14), w(19)))
}

/// Parse Rambo's inline repeat drain. The active stack index points one slot
/// below the stored counter, so the handler decrements `repeat_counts + 1,X`
/// before either popping or restoring the saved pointer through `Y = X + 1`.
fn parse_loop_next(ram: &[u8], addr: u16) -> Option<(u16, u16, u16, u16)> {
    let b = |i: u16| ram[addr.wrapping_add(i) as usize];
    let w = |i: u16| rd16(ram, addr.wrapping_add(i));
    if b(0) != 0xA6
        || b(2) != 0xDE
        || b(5) != 0xF0
        || b(7) != 0xE8
        || b(8) != 0x8A
        || b(9) != 0xA8
        || b(10) != 0x10
        || b(12) != 0xE6
        || b(13) != b(1)
    {
        return None;
    }
    let restore = branch_target(ram, addr.wrapping_add(11));
    let r = |i: u16| ram[restore.wrapping_add(i) as usize];
    if r(0) != 0xBE || r(3) != 0xB9 {
        return None;
    }
    Some((
        u16::from(b(1)),
        rd16(ram, restore.wrapping_add(1)),
        rd16(ram, restore.wrapping_add(4)),
        w(3).wrapping_sub(1),
    ))
}

/// Classify a command handler by its code. The three voices' jump tables map
/// opcodes to different handlers (and voice 1 reads misaligned words for odd
/// opcodes), but the handler *code shapes* are shared — and they embed the
/// per-voice cell addresses, which classification extracts as a side effect.
fn classify_handler(ram: &[u8], addr: u16) -> GalwayCmd {
    let b = |i: u16| ram[addr.wrapping_add(i) as usize];
    let w = |i: u16| rd16(ram, addr.wrapping_add(i));

    // Return / orderlist-pop (plain or range-checked).
    if b(0) == 0xA6
        && let Some((stack_idx_zp, stack_lo, stack_hi)) = parse_return(ram, addr)
    {
        return GalwayCmd::Return {
            stack_idx_zp,
            stack_lo,
            stack_hi,
        };
    }
    if let Some((stack_idx_zp, stack_lo, stack_hi)) = parse_increment_return(ram, addr) {
        return GalwayCmd::Return {
            stack_idx_zp,
            stack_lo,
            stack_hi,
        };
    }
    if let Some((stack_idx_zp, stack_lo, stack_hi, repeat_counts)) = parse_loop_start(ram, addr) {
        return GalwayCmd::LoopStart {
            stack_idx_zp,
            stack_lo,
            stack_hi,
            repeat_counts,
        };
    }
    if let Some((stack_idx_zp, stack_lo, stack_hi, repeat_counts)) = parse_loop_next(ram, addr) {
        return GalwayCmd::LoopNext {
            stack_idx_zp,
            stack_lo,
            stack_hi,
            repeat_counts,
        };
    }
    // Goto: LDX op-lo / STX ptr / LDX op-hi / STX ptr+1 / JMP re-read.
    if b(0) == 0xA6 && b(1) == 0x16 && b(2) == 0x86 && b(4) == 0xA6 && b(5) == 0x17 && b(6) == 0x86
    {
        return GalwayCmd::Goto;
    }
    // End-voice (the Ocean-loader generation's `$80`): LDY idx / CPY #limit /
    // BNE err / LDA mask / AND #bit-clear / STA mask / JMP silence.
    if b(0) == 0xA4
        && b(2) == 0xC0
        && b(4) == 0xD0
        && b(6) == 0xA5
        && b(8) == 0x29
        && b(10) == 0x85
        && b(11) == b(7)
    {
        return GalwayCmd::StopVoice {
            stack_idx_zp: u16::from(b(1)),
            limit: b(3),
            mask_zp: u16::from(b(7)),
            and_mask: b(9),
        };
    }
    // Block copy: LDY #last_src / LDX #last_dst, then the canonical descending
    // copy loop (LDA (op),Y / STA regimg,X / DEX / DEY / BPL) — entered inline,
    // via BNE ($95xx generation) or via JMP (Ocean-loader generation).
    if b(0) == 0xA0 && b(2) == 0xA2 {
        let (last_src, last_dst) = (b(1), b(3));
        let loop_at = match b(4) {
            0xB1 => Some(addr.wrapping_add(4)),
            0xD0 => Some(branch_target(ram, addr.wrapping_add(5))),
            0x4C => Some(w(5)),
            _ => None,
        };
        if let Some(loop_at) = loop_at
            && ram[loop_at as usize] == 0xB1
            && ram[loop_at.wrapping_add(2) as usize] == 0x9D
        {
            return GalwayCmd::BlockCopy {
                last_src,
                last_dst,
                reg_image: rd16(ram, loop_at.wrapping_add(3)),
            };
        }
    }
    // Transpose-carrying forms: LDY #3 / LDA (ptr),Y / STA transpose, then
    // either a goto (LDX $16 …) or LDA #4 into the gosub push body.
    if b(0) == 0xA0 && b(1) == 0x03 && b(2) == 0xB1 && b(4) == 0x8D {
        let transpose = w(5);
        if b(7) == 0xA6 {
            return GalwayCmd::GotoTranspose { transpose };
        }
        if b(7) == 0xA9 && b(8) == 0x04 {
            // The push body follows via BNE/JMP, or behind a BIT-skip trick
            // that hides the plain-gosub `LDA #3` entry inside `BIT $03A9`.
            let body = match b(9) {
                0xD0 => branch_target(ram, addr.wrapping_add(10)),
                0x4C => w(10),
                0x2C => addr.wrapping_add(12),
                _ => return GalwayCmd::Unknown,
            };
            if let Some((stack_idx_zp, stack_lo, stack_hi)) = parse_gosub_body(ram, body) {
                return GalwayCmd::GosubTranspose {
                    transpose,
                    stack_idx_zp,
                    stack_lo,
                    stack_hi,
                };
            }
            return GalwayCmd::Unknown;
        }
    }
    // Rambo's shared dispatch enters the 4-byte gosub+transpose handler with
    // Y already at 2: `INY / LDA (ptr),Y / STA transpose / LDA #4 / BNE body`.
    if b(0) == 0xC8 && b(1) == 0xB1 && b(3) == 0x8D && b(6) == 0xA9 && b(7) == 0x04 {
        let body = match b(8) {
            0xD0 => branch_target(ram, addr.wrapping_add(9)),
            0x4C => w(9),
            _ => return GalwayCmd::Unknown,
        };
        if let Some((stack_idx_zp, stack_lo, stack_hi)) = parse_gosub_body(ram, body) {
            return GalwayCmd::GosubTranspose {
                transpose: w(4),
                stack_idx_zp,
                stack_lo,
                stack_hi,
            };
        }
        return GalwayCmd::Unknown;
    }
    if b(0) == 0xA0 && b(1) == 0x01 && b(2) == 0xB1 {
        // Poke: LDY #1 / LDA (ptr),Y / TAX / INY / LDA (ptr),Y / STA regimg,X.
        // The `INY` at +5 distinguishes it from the TAX-first checked variant
        // below (`TAX / CPX #limit`), whose branch-offset byte could otherwise
        // fake the `9D` at +8.
        if b(4) == 0xAA && b(5) == 0xC8 && b(8) == 0x9D {
            return GalwayCmd::Poke { reg_image: w(9) };
        }
        // Range-checked poke (Ocean-loader generation): LDY #1 / LDA (ptr),Y /
        // CMP #limit / BCC ok / LDX #err / JMP trap / TAX / INY / LDA (ptr),Y /
        // STA regimg,X — or the TAX-first variant (`TAX / CPX #limit / …`).
        if b(4) == 0xC9 && b(6) == 0x90 && b(13) == 0xAA && b(17) == 0x9D {
            return GalwayCmd::Poke { reg_image: w(18) };
        }
        if b(4) == 0xAA && b(5) == 0xE0 && b(7) == 0x90 && b(14) == 0xAA && b(18) == 0x9D {
            return GalwayCmd::Poke { reg_image: w(19) };
        }
        // Standalone transpose, 2 bytes (Ocean-loader generation): LDY #1 /
        // LDA (ptr),Y / STA transpose / LDA #2 / JMP advance.
        if b(4) == 0x8D && b(7) == 0xA9 && b(8) == 0x02 {
            return GalwayCmd::TransposeSet { transpose: w(5) };
        }
    }
    if b(0) == 0xA9 {
        // Native call: push a return address, JMP (operand).
        if b(2) == 0x48 && b(3) == 0xA9 && b(5) == 0x48 && b(6) == 0x6C {
            return GalwayCmd::NativeCall;
        }
        // Plain gosub: LDA #3 straight into the push body.
        if b(1) == 0x03
            && let Some((stack_idx_zp, stack_lo, stack_hi)) =
                parse_gosub_body(ram, addr.wrapping_add(2))
        {
            return GalwayCmd::Gosub {
                stack_idx_zp,
                stack_lo,
                stack_hi,
            };
        }
        // Stop: LDA #mask / STA mask-zp / LDA #0 / STA … (silence everything).
        if b(2) == 0x85 && b(4) == 0xA9 && b(5) == 0x00 && b(6) == 0x8D {
            return GalwayCmd::Stop {
                mask_zp: u16::from(b(3)),
                mask_value: b(1),
            };
        }
    }
    // Rambo's dispatcher has already loaded the operand into X/A before
    // entering these compact poke tails.
    if b(0) == 0x9D && b(3) == 0x4C {
        return GalwayCmd::Poke { reg_image: w(1) };
    }
    GalwayCmd::Unknown
}

/// Build a [`NoteEvent`] from a frequency-table index (transpose already
/// applied): `freq_lo[idx] | freq_hi[idx] << 8` -> Hz -> MIDI, the same pitch
/// model as the Hubbard decoder.
fn note_event(
    ram: &[u8],
    layout: &GalwayLayout,
    clock: SystemClock,
    voice: VoiceId,
    idx: u8,
    start: u32,
    end: u32,
) -> Option<NoteEvent> {
    let i = u16::from(idx);
    let raw = u32::from(ram[layout.freq_lo.wrapping_add(i) as usize])
        | (u32::from(ram[layout.freq_hi.wrapping_add(i) as usize]) << 8);
    note_from_raw_freq(raw, clock, voice, start, end)
}

/// Commands a voice may execute within one frame before it is declared wedged
/// (a goto self-loop would otherwise hang the simulation). Real streams run a
/// handful of setup commands between notes.
const MAX_CMDS_PER_FRAME: u32 = 4096;
const INSTRUMENT_BYTES: usize = 0x33;

#[derive(Debug, Clone, PartialEq, Eq)]
struct GalwayInstrument {
    bytes: [u8; INSTRUMENT_BYTES],
}

impl GalwayInstrument {
    fn capture(ram: &[u8], base: u16) -> Self {
        let mut bytes = [0; INSTRUMENT_BYTES];
        for (offset, byte) in bytes.iter_mut().enumerate() {
            *byte = ram[base.wrapping_add(offset as u16) as usize];
        }
        Self { bytes }
    }

    fn modulation(
        &self,
        delta_offset: usize,
        count_offset: usize,
        stages: usize,
        flags_offset: usize,
        delay_offset: usize,
        period_offset: usize,
    ) -> AuthoredModulationProgram {
        let mut decoded = Vec::new();
        for stage in 0..stages {
            let at = delta_offset + stage * 2;
            decoded.push(AuthoredModulationStage {
                delta: ModulationDelta(i16::from_le_bytes([self.bytes[at], self.bytes[at + 1]])),
                frames: ProgramFrames(self.bytes[count_offset + stage]),
            });
        }
        let flags = self.bytes[flags_offset];
        AuthoredModulationProgram {
            stages: decoded,
            initial_delay_frames: ProgramFrames(self.bytes[delay_offset]),
            step_period_frames: ProgramFrames(self.bytes[period_offset]),
            enabled: flags & 0x04 != 0,
            apply_during_delay: flags & 0x02 != 0,
            loop_mode: if flags & 0x80 != 0 {
                AuthoredLoopMode::RestartFromCurrent
            } else if flags & 0x01 != 0 {
                AuthoredLoopMode::RestartFromInitial
            } else {
                AuthoredLoopMode::None
            },
        }
    }

    fn adsr(&self) -> Adsr {
        Adsr::from_bytes(self.bytes[0x1B], self.bytes[0x1A])
    }

    fn definition(&self) -> AuthoredInstrumentDefinition {
        let filter_regs = [
            self.bytes[0x1F],
            self.bytes[0x20],
            self.bytes[0x21],
            self.bytes[0x22],
        ];
        let filter = FilterState::from_regs(&filter_regs);
        AuthoredInstrumentDefinition {
            pitch: self.modulation(0x00, 0x08, 4, 0x0E, 0x0C, 0x0D),
            pulse_width: self.modulation(0x0F, 0x13, 2, 0x17, 0x15, 0x16),
            initial_pulse_width: PulseWidth(
                u16::from_le_bytes([self.bytes[0x18], self.bytes[0x19]]) & 0x0FFF,
            ),
            gate_frames: ProgramFrames(self.bytes[0x1E]),
            release_frames: ProgramFrames(self.bytes[0x1D]),
            filter: AuthoredFilterPreset {
                cutoff: filter.cutoff,
                resonance: filter.resonance,
                routing: FilterRoutingMask(self.bytes[0x21] & 0x0F),
                mode: filter.mode,
                volume: SidVolume(self.bytes[0x22] & 0x0F),
            },
            duration_table: self.bytes[0x22..=0x32]
                .iter()
                .copied()
                .map(ProgramFrames)
                .collect(),
        }
    }

    fn effects(&self) -> AuthoredEffects {
        AuthoredEffects {
            vibrato: None,
            pwm: None,
            pw_offset: None,
            pw_init: Some(self.definition().initial_pulse_width),
            chirp_up: false,
            arp: false,
        }
    }
}

/// Per-voice arrangement recorder built up during [`decode_song_structured`].
///
/// A *section* is a maximal linear run of the pattern pointer: it begins at a
/// control-flow target (the first event after a `Goto`/`Gosub`/`Return`, or the
/// voice's first event) and continues while events advance the pointer in place.
/// Inline commands (`Poke`/`BlockCopy`/`TransposeSet`/`NativeCall`) stay inside
/// the section. Each section start is one [`NativePlacement`]; a target address
/// recurring gets the same `pattern_number`, so `build_song_structured` can
/// collapse byte-identical replays into one Pertylizer pattern. The recovery is
/// the driver's own — gosub/goto reuse, not analyser similarity matching.
struct SectionRec {
    /// Whether the next event on this voice starts a new section.
    pending_break: [bool; 3],
    /// Global map from a section's start address to its pattern number, minted
    /// in first-seen order so shared sections keep one identity across voices.
    numbers: std::collections::HashMap<u16, u8>,
    starts: [Option<u16>; 3],
    placements: [Vec<NativePlacement>; 3],
    instances: [Vec<RecoveredPatternInstance>; 3],
    patterns: std::collections::BTreeMap<PatternNumber, Vec<RecoveredPatternEvent>>,
    commands: [Vec<RecoveredOrderCommand>; 3],
    ticks: [u32; 3],
    native_calls: [bool; 3],
}

impl SectionRec {
    fn new() -> Self {
        Self {
            pending_break: [true; 3],
            numbers: std::collections::HashMap::new(),
            starts: [None; 3],
            placements: std::array::from_fn(|_| Vec::new()),
            instances: std::array::from_fn(|_| Vec::new()),
            patterns: std::collections::BTreeMap::new(),
            commands: std::array::from_fn(|_| Vec::new()),
            ticks: [0; 3],
            native_calls: [false; 3],
        }
    }

    fn number_for(&mut self, ptr: u16) -> PatternNumber {
        let next = self.numbers.len() as u8;
        PatternNumber(*self.numbers.entry(ptr).or_insert(next))
    }

    /// Record a section start at address `ptr` on `vi`, beginning at `frame`
    /// with the active `transpose` (signed semitones, the chromatic freq-table
    /// index offset). A no-op unless a control jump armed `pending_break`.
    fn note_section(&mut self, vi: usize, ptr: u16, frame: u32, transpose: i16) {
        if !self.pending_break[vi] {
            return;
        }
        self.pending_break[vi] = false;
        self.starts[vi] = Some(ptr);
        let number = self.number_for(ptr);
        let ordinal = self.instances[vi]
            .iter()
            .filter(|instance| instance.pattern == number)
            .count() as u32;
        self.placements[vi].push(NativePlacement {
            pattern_number: number,
            start_frame: FrameIndex(frame),
            transpose: PatternTranspose(transpose),
            order_offset: Some(OrderOffset(self.commands[vi].len())),
            repeat_ordinal: Some(RepeatOrdinal(ordinal)),
        });
        self.instances[vi].push(RecoveredPatternInstance {
            pattern: number,
            transpose: PatternTranspose(transpose),
            repeat_ordinal: RepeatOrdinal(ordinal),
            order_offset: OrderOffset(self.commands[vi].len()),
            start_tick: NativeRowTick(self.ticks[vi]),
            start_frame: FrameIndex(frame),
        });
    }

    fn record_event(&mut self, vi: usize, ptr: u16, event: RecoveredPatternEvent) {
        let Some(start) = self.starts[vi] else {
            return;
        };
        let pattern = self.number_for(start);
        let events = self.patterns.entry(pattern).or_default();
        let offset = PatternByteOffset(ptr.wrapping_sub(start));
        let event = RecoveredPatternEvent { offset, ..event };
        if !events.contains(&event) {
            events.push(event);
        }
    }

    fn command_offset(&self, vi: usize) -> OrderOffset {
        OrderOffset(self.commands[vi].len())
    }

    fn push_command(&mut self, vi: usize, command: RecoveredOrderCommand) {
        self.commands[vi].push(command);
    }
}

/// Notes-only convenience over [`decode_song_structured`], used by the decode
/// validation / debug tests that don't exercise placement recovery.
#[cfg(test)]
pub(crate) fn decode_song(
    ram: Vec<u8>,
    layout: &GalwayLayout,
    clock: SystemClock,
    frames: u32,
) -> Vec<NoteEvent> {
    decode_song_structured(ram, layout, clock, frames).notes
}

struct DecodedGalwaySong {
    notes: Vec<NoteEvent>,
    structure: Vec<VoicePlacements>,
    note_instruments: Vec<Option<GalwayInstrument>>,
    recovered: RecoveredStructure,
    native_calls: [bool; 3],
}

/// Decode the song by *simulating the player* over a mutable copy of the
/// post-`init` RAM image, one play-frame at a time. All sequencer state — the
/// zero-page pattern pointers and duration counters, the active-voice mask, the
/// gosub stacks, the register images (including the lazily-loaded duration
/// tables) — lives at its real address in the copy, and commands mutate it
/// exactly as the handlers do, so dynamic data dependencies come out right by
/// construction.
///
/// Recovers both the flat note timeline and the per-voice pattern placements
/// (the authored gosub/goto arrangement). The placement walk rides the same
/// single simulation pass as the notes — there is no second traversal that could
/// disagree.
fn decode_song_structured(
    mut ram: Vec<u8>,
    layout: &GalwayLayout,
    clock: SystemClock,
    frames: u32,
) -> DecodedGalwaySong {
    let mut notes: Vec<NoteEvent> = Vec::new();
    let mut note_instruments: Vec<Option<GalwayInstrument>> = Vec::new();
    let mut reg_images: [Option<u16>; 3] = [None; 3];
    // Per-voice index into `notes` of the still-extending (tie-able) note.
    let mut open: [Option<usize>; 3] = [None; 3];
    let mut rec = SectionRec::new();

    for frame in 0..frames {
        for (vi, v) in layout.voices.iter().enumerate() {
            if ram[layout.active_mask_zp as usize] & v.active_bit == 0 {
                continue;
            }
            let ctr = ram[v.durctr_zp as usize].wrapping_sub(1);
            ram[v.durctr_zp as usize] = ctr;
            if ctr != 0 {
                continue;
            }
            step_voice(
                ram.as_mut_slice(),
                layout,
                vi,
                clock,
                frame,
                frames,
                &mut notes,
                &mut note_instruments,
                &mut reg_images[vi],
                &mut open[vi],
                &mut rec,
            );
        }
    }
    // The simulation interleaves voices frame-by-frame; downstream consumers
    // (and the Hubbard decoder this mirrors) expect per-voice runs. Reorder the
    // parallel instrument-definition vector in lockstep.
    let mut order: Vec<usize> = (0..notes.len()).collect();
    order.sort_by_key(|&i| (notes[i].voice, notes[i].start_frame.0));
    let sorted_notes: Vec<NoteEvent> = order.iter().map(|&i| notes[i]).collect();
    let sorted_instruments: Vec<Option<GalwayInstrument>> =
        order.iter().map(|&i| note_instruments[i].clone()).collect();
    notes = sorted_notes;

    // Per-voice placements are pushed in play order (frames ascending), exactly
    // what `build_song_structured` walks; drop voices with no recovered run.
    let structure: Vec<VoicePlacements> = rec
        .placements
        .iter_mut()
        .enumerate()
        .filter(|(_, p)| !p.is_empty())
        .map(|(vi, p)| VoicePlacements {
            voice: VoiceId::from_index(vi),
            placements: std::mem::take(p),
        })
        .collect();
    let mut recovered_voices = Vec::new();
    for vi in 0..3 {
        if rec.instances[vi].is_empty() {
            continue;
        }
        recovered_voices.push(RecoveredVoiceStructure {
            voice: VoiceId::from_index(vi),
            order_loop_offset: None,
            order_commands: std::mem::take(&mut rec.commands[vi]),
            instances: std::mem::take(&mut rec.instances[vi]),
        });
    }
    let recovered = RecoveredStructure {
        patterns: rec.patterns,
        voices: recovered_voices,
    };
    DecodedGalwaySong {
        notes,
        structure,
        note_instruments: sorted_instruments,
        recovered,
        native_calls: rec.native_calls,
    }
}

// Structure-spike recorder: when `Some`, `step_voice` logs every 2-byte
// event as `(voice, ptr, event_byte, dur, transpose)`. Lets the Galway
// structure spike measure pattern-pointer reuse and content divergence
// straight off the real decoder. Inert (and absent) outside test builds.
// A recorded 2-byte event: `(voice, ptr, event_byte, dur, transpose)`.
#[cfg(test)]
type GalwayRecEvent = (u8, u16, u8, u8, u8);

#[cfg(test)]
thread_local! {
    static EVENT_REC: std::cell::RefCell<Option<Vec<GalwayRecEvent>>> =
        const { std::cell::RefCell::new(None) };
}

/// Run one voice's event loop at a frame where its duration counter expired:
/// execute commands until a note/tie/rest reloads the counter (or the stream
/// stops). Mirrors the sequencer's read loop, including executing several
/// commands within the same frame.
#[allow(clippy::too_many_arguments)]
fn step_voice(
    ram: &mut [u8],
    layout: &GalwayLayout,
    vi: usize,
    clock: SystemClock,
    frame: u32,
    frames: u32,
    notes: &mut Vec<NoteEvent>,
    note_instruments: &mut Vec<Option<GalwayInstrument>>,
    reg_image_base: &mut Option<u16>,
    open: &mut Option<usize>,
    rec: &mut SectionRec,
) {
    let v = &layout.voices[vi];
    let mut cmds = 0u32;
    loop {
        let ptr = rd16(ram, v.ptr_zp);
        let event = ram[ptr as usize];

        if event < layout.command_threshold.0 {
            // NOTE / tie / rest: 2 bytes [idx][dur-idx]. The duration table is
            // read *live* from the register image — block loads and pokes have
            // already updated it in `ram`.
            rec.note_section(vi, ptr, frame, i16::from(ram[v.transpose as usize] as i8));
            let dur_idx = ram[ptr.wrapping_add(1) as usize];
            let folded = layout.fold_high_notes && event >= NOTE_REST;
            let note_idx = if folded {
                event.wrapping_sub(NOTE_REST)
            } else {
                event
            };
            let dur = if folded {
                dur_idx
            } else {
                ram[v.dur_table.wrapping_add(u16::from(dur_idx)) as usize]
            };
            ram[v.durctr_zp as usize] = dur;
            #[cfg(test)]
            EVENT_REC.with(|r| {
                if let Some(log) = r.borrow_mut().as_mut() {
                    log.push((vi as u8, ptr, event, dur, ram[v.transpose as usize]));
                }
            });
            wr16(ram, v.ptr_zp, ptr.wrapping_add(2));
            // A zero table entry wraps the 8-bit counter: 256 frames.
            let dur_frames = if dur == 0 { 256 } else { u32::from(dur) };
            // Exclusive end: the frame the next row begins.
            let end = (frame + dur_frames).min(frames);
            rec.record_event(
                vi,
                ptr,
                RecoveredPatternEvent {
                    offset: PatternByteOffset(0),
                    duration: PatternDuration(if dur == 0 { 256 } else { u16::from(dur) }),
                    frequency_index: (note_idx < NOTE_TIE).then_some(FrequencyTableIndex(note_idx)),
                    instrument: None,
                    hold: note_idx == NOTE_TIE && layout.has_tie,
                    slide: None,
                    command: None,
                    command_data: None,
                    duration_index: (!folded).then_some(NativeEffectByte(dur_idx)),
                    operand: None,
                },
            );
            rec.ticks[vi] = rec.ticks[vi].saturating_add(dur_frames);
            match note_idx {
                NOTE_REST => *open = None,
                NOTE_TIE if layout.has_tie => {
                    if let Some(i) = *open {
                        notes[i].end_frame = Some(FrameIndex(end));
                    }
                }
                idx => {
                    let pitch_idx = idx.wrapping_add(ram[v.transpose as usize]);
                    let voice = VoiceId::from_index(vi);
                    *open =
                        note_event(ram, layout, clock, voice, pitch_idx, frame, end).map(|ne| {
                            notes.push(ne);
                            note_instruments.push(
                                reg_image_base.map(|base| GalwayInstrument::capture(ram, base)),
                            );
                            notes.len() - 1
                        });
                }
            }
            return;
        }

        cmds += 1;
        if cmds > MAX_CMDS_PER_FRAME {
            // Wedged stream (e.g. a goto self-loop): silence this voice.
            ram[layout.active_mask_zp as usize] &= !v.active_bit;
            return;
        }
        let handler = rd16(
            ram,
            v.jump_table
                .wrapping_add(u16::from(event & layout.command_mask.0)),
        );
        let operand = rd16(ram, ptr.wrapping_add(1));
        let command = classify_handler(ram, handler);
        rec.note_section(vi, ptr, frame, i16::from(ram[v.transpose as usize] as i8));
        let command_data = match command {
            GalwayCmd::GotoTranspose { .. } | GalwayCmd::GosubTranspose { .. } => {
                Some(NativeEffectByte(ram[ptr.wrapping_add(3) as usize]))
            }
            GalwayCmd::TransposeSet { .. } | GalwayCmd::LoopStart { .. } => {
                Some(NativeEffectByte(ram[ptr.wrapping_add(1) as usize]))
            }
            _ => None,
        };
        rec.record_event(
            vi,
            ptr,
            RecoveredPatternEvent {
                offset: PatternByteOffset(0),
                duration: PatternDuration(0),
                frequency_index: None,
                instrument: None,
                hold: false,
                slide: None,
                command: Some(NativeDriverOpcode(event)),
                command_data,
                duration_index: None,
                operand: (!matches!(
                    command,
                    GalwayCmd::TransposeSet { .. }
                        | GalwayCmd::LoopStart { .. }
                        | GalwayCmd::LoopNext { .. }
                ))
                .then_some(NativeOperand(operand)),
            },
        );
        let order_offset = rec.command_offset(vi);
        match command {
            GalwayCmd::Return {
                stack_idx_zp,
                stack_lo,
                stack_hi,
            } => {
                let idx = ram[stack_idx_zp as usize].wrapping_add(1);
                ram[stack_idx_zp as usize] = idx;
                let lo = ram[stack_lo.wrapping_add(u16::from(idx)) as usize];
                let hi = ram[stack_hi.wrapping_add(u16::from(idx)) as usize];
                wr16(ram, v.ptr_zp, u16::from(lo) | (u16::from(hi) << 8));
                rec.push_command(vi, RecoveredOrderCommand::Return { order_offset });
                rec.pending_break[vi] = true;
            }
            GalwayCmd::BlockCopy {
                last_src,
                last_dst,
                reg_image,
            } => {
                *reg_image_base = Some(reg_image);
                // Mirror the hardware loop exactly: Y counts last_src..0,
                // X counts down from last_dst in 8-bit space (so an exotic
                // last_dst < last_src wraps X just like the chip would), and
                // reads/writes interleave in the same descending order.
                for k in 0..=last_src {
                    let y = last_src - k;
                    let x = last_dst.wrapping_sub(k);
                    ram[reg_image.wrapping_add(u16::from(x)) as usize] =
                        ram[operand.wrapping_add(u16::from(y)) as usize];
                }
                rec.push_command(
                    vi,
                    RecoveredOrderCommand::DriverCommand {
                        order_offset,
                        opcode: NativeDriverOpcode(event),
                    },
                );
                wr16(ram, v.ptr_zp, ptr.wrapping_add(3));
            }
            GalwayCmd::Goto => {
                let target = rec.number_for(operand);
                rec.push_command(
                    vi,
                    RecoveredOrderCommand::Jump {
                        order_offset,
                        target,
                        transpose: None,
                    },
                );
                wr16(ram, v.ptr_zp, operand);
                rec.pending_break[vi] = true;
            }
            GalwayCmd::GotoTranspose { transpose } => {
                let value = ram[ptr.wrapping_add(3) as usize];
                let target = rec.number_for(operand);
                rec.push_command(
                    vi,
                    RecoveredOrderCommand::Jump {
                        order_offset,
                        target,
                        transpose: Some(PatternTranspose(i16::from(value as i8))),
                    },
                );
                ram[transpose as usize] = value;
                wr16(ram, v.ptr_zp, operand);
                rec.pending_break[vi] = true;
            }
            GalwayCmd::Gosub {
                stack_idx_zp,
                stack_lo,
                stack_hi,
            } => {
                let target = rec.number_for(operand);
                rec.push_command(
                    vi,
                    RecoveredOrderCommand::Call {
                        order_offset,
                        target,
                        transpose: None,
                    },
                );
                push_return(ram, ptr.wrapping_add(3), stack_idx_zp, stack_lo, stack_hi);
                wr16(ram, v.ptr_zp, operand);
                rec.pending_break[vi] = true;
            }
            GalwayCmd::GosubTranspose {
                transpose,
                stack_idx_zp,
                stack_lo,
                stack_hi,
            } => {
                let value = ram[ptr.wrapping_add(3) as usize];
                let target = rec.number_for(operand);
                rec.push_command(
                    vi,
                    RecoveredOrderCommand::Call {
                        order_offset,
                        target,
                        transpose: Some(PatternTranspose(i16::from(value as i8))),
                    },
                );
                ram[transpose as usize] = value;
                push_return(ram, ptr.wrapping_add(4), stack_idx_zp, stack_lo, stack_hi);
                wr16(ram, v.ptr_zp, operand);
                rec.pending_break[vi] = true;
            }
            GalwayCmd::LoopStart {
                stack_idx_zp,
                stack_lo,
                stack_hi,
                repeat_counts,
            } => {
                rec.push_command(
                    vi,
                    RecoveredOrderCommand::DriverCommand {
                        order_offset,
                        opcode: NativeDriverOpcode(event),
                    },
                );
                let slot = ram[stack_idx_zp as usize];
                let target = ptr.wrapping_add(2);
                ram[stack_lo.wrapping_add(u16::from(slot)) as usize] = (target & 0xFF) as u8;
                ram[stack_hi.wrapping_add(u16::from(slot)) as usize] = (target >> 8) as u8;
                ram[repeat_counts.wrapping_add(u16::from(slot)) as usize] =
                    ram[ptr.wrapping_add(1) as usize];
                ram[stack_idx_zp as usize] = slot.wrapping_sub(1);
                wr16(ram, v.ptr_zp, target);
            }
            GalwayCmd::LoopNext {
                stack_idx_zp,
                stack_lo,
                stack_hi,
                repeat_counts,
            } => {
                rec.push_command(
                    vi,
                    RecoveredOrderCommand::DriverCommand {
                        order_offset,
                        opcode: NativeDriverOpcode(event),
                    },
                );
                let slot = ram[stack_idx_zp as usize].wrapping_add(1);
                let count_addr = repeat_counts.wrapping_add(u16::from(slot));
                let count = ram[count_addr as usize].wrapping_sub(1);
                ram[count_addr as usize] = count;
                if count == 0 {
                    ram[stack_idx_zp as usize] = slot;
                    wr16(ram, v.ptr_zp, ptr.wrapping_add(1));
                } else {
                    let lo = ram[stack_lo.wrapping_add(u16::from(slot)) as usize];
                    let hi = ram[stack_hi.wrapping_add(u16::from(slot)) as usize];
                    wr16(ram, v.ptr_zp, u16::from(lo) | (u16::from(hi) << 8));
                    rec.pending_break[vi] = true;
                }
            }
            GalwayCmd::Poke { reg_image } => {
                *reg_image_base = Some(reg_image);
                let off = ram[ptr.wrapping_add(1) as usize];
                ram[reg_image.wrapping_add(u16::from(off)) as usize] =
                    ram[ptr.wrapping_add(2) as usize];
                rec.push_command(
                    vi,
                    RecoveredOrderCommand::DriverCommand {
                        order_offset,
                        opcode: NativeDriverOpcode(event),
                    },
                );
                wr16(ram, v.ptr_zp, ptr.wrapping_add(3));
            }
            GalwayCmd::TransposeSet { transpose } => {
                let value = ram[ptr.wrapping_add(1) as usize];
                rec.push_command(
                    vi,
                    RecoveredOrderCommand::SetTranspose {
                        order_offset,
                        transpose: PatternTranspose(i16::from(value as i8)),
                    },
                );
                ram[transpose as usize] = value;
                wr16(ram, v.ptr_zp, ptr.wrapping_add(2));
            }
            GalwayCmd::NativeCall => {
                rec.native_calls[vi] = true;
                rec.push_command(
                    vi,
                    RecoveredOrderCommand::DriverCommand {
                        order_offset,
                        opcode: NativeDriverOpcode(event),
                    },
                );
                wr16(ram, v.ptr_zp, ptr.wrapping_add(3));
            }
            GalwayCmd::Stop {
                mask_zp,
                mask_value,
            } => {
                rec.push_command(vi, RecoveredOrderCommand::Stop { order_offset });
                ram[mask_zp as usize] = mask_value;
                return;
            }
            GalwayCmd::StopVoice {
                stack_idx_zp,
                limit,
                mask_zp,
                and_mask,
            } => {
                rec.push_command(vi, RecoveredOrderCommand::Stop { order_offset });
                if ram[stack_idx_zp as usize] == limit {
                    // Stack back at its initial index: this voice is done.
                    ram[mask_zp as usize] &= and_mask;
                } else {
                    // Anything else hits the player's error trap, which
                    // silences everything.
                    ram[layout.active_mask_zp as usize] = 0;
                }
                return;
            }
            GalwayCmd::Unknown => {
                rec.push_command(
                    vi,
                    RecoveredOrderCommand::DriverCommand {
                        order_offset,
                        opcode: NativeDriverOpcode(event),
                    },
                );
                // A dispatched-but-unclassifiable handler means either a
                // garbage layout or a classifier gap — both invalidate the
                // whole decode, so stop every voice (a lone silenced voice
                // could otherwise leave a truncated timeline that still
                // passes the onset gate).
                ram[layout.active_mask_zp as usize] = 0;
                return;
            }
        }
    }
}

/// A chord-style arpeggio loop: short cycle with a real chord interval in it.
/// The interval floor keeps vibrato jitter (±1 semitone cycles) from ever
/// counting as an arpeggio.
fn is_chord_arp(body: &[i8]) -> bool {
    (2..=4).contains(&body.len()) && body.iter().any(|&o| o.abs() >= 3)
}

/// Split each trace-clustered patch by chord-arpeggio body. The heuristic
/// clustering keys on ADSR + waveform + role tags, which can't tell a plain
/// stab from an arpeggiating one (same registers — the cycling is reg-image
/// data), so one patch ends up mixing flat notes with one or more chord
/// shapes, and the patch-level `arpeggio_loop` (taken from the *first* member)
/// flattens or mis-chords everything. Give every distinct chord body its own
/// patch — that is what they are: different authored instruments.
///
/// Notes that never clustered (`None` assignments, singleton fallback) keep
/// rendering flat even if they carry a chord loop — a one-shot arp fill is
/// rare enough to live with until native instrument decode lands.
fn split_arpeggio_patches(
    patches: &mut Vec<Patch>,
    assignments: &mut [Option<PatchId>],
    characteristics: &[NoteCharacteristics],
) {
    let chord_body = |i: usize| {
        characteristics[i]
            .pitch_relative_loop
            .as_ref()
            .filter(|(body, _)| is_chord_arp(body))
            .map(|(body, _)| body.clone())
    };

    for pid in 0..patches.len() {
        let members: Vec<usize> = assignments
            .iter()
            .enumerate()
            .filter(|&(_, a)| *a == Some(patches[pid].id))
            .map(|(i, _)| i)
            .collect();
        // Partition by chord body, preserving first-seen (note-index) order.
        let mut plain = 0u16;
        let mut bodies: Vec<(Vec<i8>, Vec<usize>)> = Vec::new();
        for &i in &members {
            match chord_body(i) {
                None => plain += 1,
                Some(b) => match bodies.iter_mut().find(|(body, _)| *body == b) {
                    Some((_, idxs)) => idxs.push(i),
                    None => bodies.push((b, vec![i])),
                },
            }
        }
        if bodies.is_empty() {
            continue;
        }
        // Flat members keep the base patch (now explicitly loop-free); each
        // chord body gets its own derived patch. When the cluster is pure and
        // single-bodied, just stamp the loop in place.
        // A pure single-body cluster just gets the loop stamped in place; a
        // mixed one keeps the flat members on the (now explicitly loop-free)
        // base patch and derives one new patch per chord body.
        if plain == 0 && bodies.len() == 1 {
            let (body, _) = &bodies[0];
            for vp in &mut patches[pid].voices {
                vp.arpeggio_loop = Some(body.clone());
            }
            continue;
        }
        patches[pid].member_count = plain;
        for vp in &mut patches[pid].voices {
            vp.arpeggio_loop = None;
        }
        for (body, idxs) in bodies {
            let mut p = patches[pid].clone();
            p.id = PatchId(patches.len() as u16);
            p.member_count = idxs.len() as u16;
            for vp in &mut p.voices {
                vp.arpeggio_loop = Some(body.clone());
            }
            let id = p.id;
            patches.push(p);
            for i in idxs {
                assignments[i] = Some(id);
            }
        }
    }
}

/// Onset agreement that tolerates **instrument arpeggios** — the
/// Galway-specific twist on [`super::onset_agreement`]. The pattern stream
/// holds one base note while the instrument's per-frame effect cycles the chip
/// through chord steps (e.g. Ocean_Loader_1 V2: base 67, chip cycles 72/75/67
/// every frame), so [`detect_notes`] reports the *gate-time* step and a strict
/// pitch comparison miscounts a correct decode as a miss. The detected-effect
/// arpeggio spans can't arbitrate (these gates are only 3-6 frames long, below
/// the detector's thresholds), so fall back to the per-frame trace itself: a
/// native note that fails the strict voice+pitch+frame match still counts if
/// the chip *actually played its pitch*, gated, on its voice within the onset
/// window — for a real arpeggio the base recurs every cycle, while a wrong
/// decode has to coincide with a chip frame pitch on the right voice at the
/// right time to be miscounted.
#[cfg(test)]
fn arp_aware_agreement(
    native: &[NoteEvent],
    truth: &[NoteEvent],
    states: &[crate::analysis::FrameState],
    clock: SystemClock,
) -> f64 {
    if native.is_empty() {
        return 0.0;
    }
    let hits = native
        .iter()
        .filter(|n| note_matches(n, truth, states, clock))
        .count();
    hits as f64 / native.len() as f64
}

/// One native note's match predicate for [`arp_aware_agreement`]: the strict
/// voice+pitch+frame check against the detected truth notes, with the
/// per-frame-trace fallback for arpeggio bases.
fn note_matches(
    n: &NoteEvent,
    truth: &[NoteEvent],
    states: &[crate::analysis::FrameState],
    clock: SystemClock,
) -> bool {
    if truth.iter().any(|candidate| {
        candidate.voice == n.voice
            && candidate.midi == n.midi
            && candidate.start_frame.0.abs_diff(n.start_frame.0) <= ARPEGGIO_ONSET_TOLERANCE
    }) {
        return true;
    }
    if states.is_empty() {
        return false;
    }
    let vi = usize::from(n.voice.0.saturating_sub(1)).min(2);
    let hi = (n.start_frame.0 as usize + ARPEGGIO_ONSET_TOLERANCE as usize).min(states.len() - 1);
    let lo = (n.start_frame.0.saturating_sub(ARPEGGIO_ONSET_TOLERANCE) as usize).min(hi);
    // The pitch itself may sit on a gate-off frame (these arpeggios drop the
    // gate on their closing step, which is exactly where the base pitch
    // lands) — but the voice must be gated right next to that frame, so an
    // unrelated released tail ringing at the pitch can't count.
    (lo..=hi).any(|f| {
        let pitched = hertz_to_midi(states[f].voices[vi].freq.to_hertz(clock))
            .is_some_and(|(m, _)| m == n.midi);
        pitched
            && (f.saturating_sub(1)..=(f + 1).min(states.len() - 1))
                .any(|g| states[g].voices[vi].control.gate)
    })
}

fn arpeggio_validation_notes(
    native: &[NoteEvent],
    truth: &[NoteEvent],
    states: &[crate::analysis::FrameState],
    clock: SystemClock,
) -> Vec<NoteEvent> {
    native
        .iter()
        .map(|note| {
            if truth.iter().any(|candidate| {
                candidate.voice == note.voice
                    && candidate.midi == note.midi
                    && candidate.start_frame.0.abs_diff(note.start_frame.0)
                        <= ARPEGGIO_ONSET_TOLERANCE
            }) {
                return *note;
            }
            let candidate = truth
                .iter()
                .filter(|candidate| {
                    candidate.voice == note.voice
                        && candidate.start_frame.0.abs_diff(note.start_frame.0)
                            <= ARPEGGIO_ONSET_TOLERANCE
                })
                .min_by_key(|candidate| candidate.start_frame.0.abs_diff(note.start_frame.0));
            if note_matches(note, truth, states, clock)
                && let Some(candidate) = candidate
            {
                return NoteEvent {
                    midi: candidate.midi,
                    cents: candidate.cents,
                    ..*note
                };
            }
            *note
        })
        .collect()
}

/// Cheap deterministic check that a RAM image really runs `layout`: every
/// voice's jump-table entry at byte offset 0 (opcode `$80`) must classify as
/// [`GalwayCmd::Return`] ($95xx generation) or [`GalwayCmd::StopVoice`]
/// (Ocean-loader generation) — true for the real player by construction, and
/// essentially never true for arbitrary bytes behind a mislocated table.
/// Turns most layout mismatches into an up-front [`NativeError::LocateFailed`]
/// instead of leaning on the probabilistic onset gate.
fn layout_matches(ram: &[u8], layout: &GalwayLayout) -> bool {
    layout.voices.iter().all(|v| {
        matches!(
            classify_handler(ram, rd16(ram, v.jump_table)),
            GalwayCmd::Return { .. } | GalwayCmd::StopVoice { .. }
        )
    })
}

/// Push a gosub return pointer: `stack[idx] = ret`, then `idx -= 1`.
fn push_return(ram: &mut [u8], ret: u16, stack_idx_zp: u16, stack_lo: u16, stack_hi: u16) {
    let idx = ram[stack_idx_zp as usize];
    ram[stack_lo.wrapping_add(u16::from(idx)) as usize] = (ret & 0xFF) as u8;
    ram[stack_hi.wrapping_add(u16::from(idx)) as usize] = (ret >> 8) as u8;
    ram[stack_idx_zp as usize] = idx.wrapping_sub(1);
}

/// One voice's sequencer found by [`locate`]: the recovered cells plus the
/// shared values that must agree across all three voices.
#[derive(Clone, Copy)]
struct DispatchSite {
    voice: GalwayVoice,
    freq_lo: u16,
    freq_hi: u16,
    active_mask_zp: u16,
    command_threshold: GalwayCommandThreshold,
    command_mask: GalwayCommandMask,
    fold_high_notes: bool,
    has_tie: bool,
}

/// Parse one candidate command-dispatch site at `a` — the sequencer's
/// `AND #mask / TAX / [STX zp] / LDA table,X / STA … / LDA table+1,X / STA … /
/// INY / LDA (ptr),Y / STA $16` shape — then recover the rest of the voice's
/// cells from the code around it:
/// - duration reload `LDA (ptr),Y / TAX / LDA dur,X / STA durctr` (forward),
/// - transpose add `ADC abs / STA $18` (forward),
/// - pitch lookup `LDX $18 / LDY hi,X / LDA lo,X` (forward),
/// - the routine head `LDA mask / LSR | AND #bit` (backward).
fn parse_dispatch(ram: &[u8], a: usize) -> Option<DispatchSite> {
    let word = |at: usize| u16::from(ram[at]) | (u16::from(ram[at + 1]) << 8);
    if ram[a] != 0x29 || !matches!(ram[a + 1], 0x3F | 0x7F) || ram[a + 2] != 0xAA {
        return None;
    }
    let command_mask = GalwayCommandMask(ram[a + 1]);
    let command_threshold = if ram[a - 2] == 0x10 {
        GalwayCommandThreshold(0x80)
    } else if ram[a - 4] == 0xC9 && ram[a - 2] == 0x90 {
        GalwayCommandThreshold(ram[a - 3])
    } else {
        return None;
    };
    // Voice 2's instance omits the `STX $20` the other two carry.
    let p = if ram[a + 3] == 0x86 { a + 5 } else { a + 3 };
    if ram[p] != 0xBD || ram[p + 3] != 0x8D {
        return None;
    }
    let jump_table = word(p + 1);
    let p2 = p + 6;
    if ram[p2] != 0xBD || word(p2 + 1) != jump_table.wrapping_add(1) || ram[p2 + 3] != 0x8D {
        return None;
    }
    let p3 = p2 + 6;
    if ram[p3] != 0xC8 || ram[p3 + 1] != 0xB1 {
        return None;
    }
    let ptr_zp = u16::from(ram[p3 + 2]);
    let operand_store_ok = (ram[p3 + 3] == 0x85 && ram[p3 + 4] == 0x16)
        || (ram[p3 + 3] == 0xAA && ram[p3 + 4] == 0x85 && ram[p3 + 5] == 0x16);
    if !operand_store_ok {
        return None;
    }

    // First match wins for every cell: the voice's own note path directly
    // follows its dispatch, while the *next* voice's identically-shaped code
    // starts ~0x2C0 bytes later — keeping the first hit means a window that
    // overruns into a neighbouring sequencer can't overwrite the right answer.
    let mut dur = None;
    let mut transpose = None;
    let mut freq = None;
    let mut has_tie = false;
    let mut fold_high_notes = false;
    for q in a..(a + 0x250).min(ram.len().saturating_sub(16)) {
        if dur.is_none()
            && ram[q] == 0xB1
            && u16::from(ram[q + 1]) == ptr_zp
            && ram[q + 2] == 0xAA
            && ram[q + 3] == 0xBD
            && ram[q + 6] == 0x85
        {
            dur = Some((word(q + 4), u16::from(ram[q + 7])));
        }
        if dur.is_none()
            && ram[q] == 0xB1
            && u16::from(ram[q + 1]) == ptr_zp
            && ram[q + 2] == 0xA6
            && ram[q + 4] == 0xE0
            && ram[q + 5] == 0x60
            && ram[q + 6] == 0xB0
            && ram[q + 8] == 0xAA
            && ram[q + 9] == 0xBD
            && ram[q + 12] == 0x85
        {
            dur = Some((word(q + 10), u16::from(ram[q + 13])));
        }
        if transpose.is_none()
            && ram[q] == 0x6D
            && ((ram[q + 3] == 0x85 && ram[q + 4] == 0x18) || ram[q + 3] == 0xAA)
        {
            transpose = Some(word(q + 1));
        }
        if freq.is_none()
            && ram[q] == 0xA6
            && ram[q + 1] == 0x18
            && ram[q + 2] == 0xBC
            && ram[q + 5] == 0xBD
        {
            freq = Some((word(q + 6), word(q + 3)));
        }
        if freq.is_none() && ram[q] == 0xBC && ram[q + 3] == 0xBD {
            freq = Some((word(q + 4), word(q + 1)));
        }
        // The $95xx-generation note path checks `CMP #$5F / BEQ` for a tie;
        // the Ocean-loader generation has no tie at all.
        if ram[q] == 0xC9 && ram[q + 1] == 0x5F && ram[q + 2] == 0xF0 {
            has_tie = true;
        }
        if ram[q..q + 6] == [0xC9, 0x60, 0x90, 0x02, 0xE9, 0x60] {
            fold_high_notes = true;
        }
    }
    let (dur_table, durctr_zp) = dur?;
    let (freq_lo, freq_hi) = freq?;

    // Routine head: `LDA mask` then `LSR A` (voice 1, bit 0) or `AND #bit`.
    let mut head = None;
    for q in (a.saturating_sub(0x40)..a).rev() {
        if ram[q] == 0xA5 && (ram[q + 2] == 0x4A || ram[q + 2] == 0x29) {
            let bit = if ram[q + 2] == 0x4A { 1 } else { ram[q + 3] };
            head = Some((u16::from(ram[q + 1]), bit));
            break;
        }
    }
    let (active_mask_zp, active_bit) = head?;

    Some(DispatchSite {
        voice: GalwayVoice {
            ptr_zp,
            durctr_zp,
            active_bit,
            jump_table,
            dur_table,
            transpose: transpose?,
        },
        freq_lo,
        freq_hi,
        active_mask_zp,
        command_threshold,
        command_mask,
        fold_high_notes,
        has_tie,
    })
}

/// Find the player's three per-voice sequencers in a post-`init` RAM image and
/// recover a [`GalwayLayout`] from their code. The scan keys on the
/// command-dispatch shape (see [`parse_dispatch`]) and demands exactly three
/// hits that agree on the shared cells: one frequency-table pair, one
/// active-mask zero-page cell, distinct pattern pointers, and the voice bits
/// `{1, 2, 4}` (in ascending code order, voice 1 first — how the player lays
/// its three sequencer copies out). Returns `None` for the other Galway player
/// generations (e.g. Kong_Strikes_Back's 1984 engine or Highlander's later
/// workspace-swapping one), which need their own reverse engineering.
pub(crate) fn locate(ram: &[u8]) -> Option<GalwayLayout> {
    let mut sites: Vec<DispatchSite> = Vec::new();
    for a in 0x0200..ram.len().saturating_sub(0x20) {
        if let Some(site) = parse_dispatch(ram, a) {
            sites.push(site);
        }
    }
    let mut candidates = Vec::new();
    for v1 in sites.iter().filter(|site| site.voice.active_bit == 1) {
        for v2 in sites.iter().filter(|site| site.voice.active_bit == 2) {
            for v3 in sites.iter().filter(|site| site.voice.active_bit == 4) {
                let shared_ok = v1.freq_lo == v2.freq_lo
                    && v2.freq_lo == v3.freq_lo
                    && v1.freq_hi == v2.freq_hi
                    && v2.freq_hi == v3.freq_hi
                    && v1.active_mask_zp == v2.active_mask_zp
                    && v2.active_mask_zp == v3.active_mask_zp
                    && v1.command_threshold == v2.command_threshold
                    && v2.command_threshold == v3.command_threshold
                    && v1.command_mask == v2.command_mask
                    && v2.command_mask == v3.command_mask
                    && v1.fold_high_notes == v2.fold_high_notes
                    && v2.fold_high_notes == v3.fold_high_notes;
                let ptrs_ok = v1.voice.ptr_zp != v2.voice.ptr_zp
                    && v2.voice.ptr_zp != v3.voice.ptr_zp
                    && v1.voice.ptr_zp != v3.voice.ptr_zp;
                let tie_ok = v1.has_tie == v2.has_tie && v2.has_tie == v3.has_tie;
                if shared_ok && ptrs_ok && tie_ok {
                    let candidate = GalwayLayout {
                        voices: [v1.voice, v2.voice, v3.voice],
                        freq_lo: v1.freq_lo,
                        freq_hi: v1.freq_hi,
                        active_mask_zp: v1.active_mask_zp,
                        command_threshold: v1.command_threshold,
                        command_mask: v1.command_mask,
                        fold_high_notes: v1.fold_high_notes,
                        has_tie: v1.has_tie,
                    };
                    if !candidates.contains(&candidate) {
                        candidates.push(candidate);
                    }
                }
            }
        }
    }
    if candidates.len() == 1 {
        return candidates.pop();
    }

    // Multi-song containers such as Rambo retain several complete relocated
    // player copies in RAM. The selected subtune initializes the shared ZP
    // stream pointers into exactly one copy's nearby data. Rank only otherwise
    // strict three-voice candidates by that relocation distance and fail
    // closed if the live copy is not unique.
    candidates.sort_by_key(|layout| {
        layout
            .voices
            .iter()
            .map(|voice| rd16(ram, voice.ptr_zp).abs_diff(voice.jump_table) as u64)
            .sum::<u64>()
    });
    let first = candidates.first().copied()?;
    let first_score = first
        .voices
        .iter()
        .map(|voice| rd16(ram, voice.ptr_zp).abs_diff(voice.jump_table) as u64)
        .sum::<u64>();
    let tied = candidates.get(1).is_some_and(|second| {
        second
            .voices
            .iter()
            .map(|voice| rd16(ram, voice.ptr_zp).abs_diff(voice.jump_table) as u64)
            .sum::<u64>()
            == first_score
    });
    (!tied).then_some(first)
}

fn parse_comic_dispatch(
    ram: &[u8],
    at: usize,
) -> Option<(ComicVoice, ComicCell, ComicCell, ComicCell, ComicCell)> {
    let word = |offset: usize| u16::from(ram[offset]) | (u16::from(ram[offset + 1]) << 8);
    if ram.get(at) != Some(&0xB1)
        || ram.get(at + 2) != Some(&0xC9)
        || ram.get(at + 3) != Some(&0xC0)
    {
        return None;
    }
    let ptr_zp = ComicCell(u16::from(ram[at + 1]));
    let mut transpose = None;
    let mut note_enable_mask = None;
    let mut freq = None;
    let mut duration_counter = None;
    for q in at..(at + 0x110).min(ram.len().saturating_sub(16)) {
        if transpose.is_none() && ram[q] == 0x6D && ram[q + 3] == 0xAA && ram[q + 4] == 0xAD {
            transpose = Some(ComicCell(word(q + 1)));
            note_enable_mask = Some(ComicCell(word(q + 5)));
        }
        if freq.is_none() && ram[q] == 0xBC && ram[q + 3] == 0xBD {
            freq = Some((ComicCell(word(q + 4)), ComicCell(word(q + 1))));
        }
        if duration_counter.is_none()
            && ram[q] == 0xB1
            && u16::from(ram[q + 1]) == ptr_zp.0
            && ram[q + 2] == 0xA6
            && ram[q + 4] == 0xE0
            && ram[q + 5] == 0x60
            && ram[q + 8] == 0xAA
            && ram[q + 9] == 0xBD
            && ram[q + 12] == 0x85
        {
            duration_counter = Some(ComicCell(u16::from(ram[q + 13])));
        }
    }
    let mut head = None;
    for q in (at.saturating_sub(0x30)..at).rev() {
        if ram[q] != 0xA5 {
            continue;
        }
        let (active_bit, width) = match ram[q + 2] {
            0x4A => (1, 1),
            0x29 => (ram[q + 3], 2),
            _ => continue,
        };
        let branch = q + 2 + width;
        if ram[branch] == 0xF0 || (active_bit == 1 && ram[branch] == 0x90) {
            head = Some((ComicCell(u16::from(ram[q + 1])), ComicMask(active_bit)));
            break;
        }
    }
    let (active_mask_zp, active_bit) = head?;
    let (freq_lo, freq_hi) = freq?;
    Some((
        ComicVoice {
            ptr_zp,
            durctr_zp: duration_counter?,
            active_bit,
            gate_bit: ComicMask(active_bit.0 << 3),
            transpose: transpose?,
        },
        freq_lo,
        freq_hi,
        note_enable_mask?,
        active_mask_zp,
    ))
}

fn locate_comic(ram: &[u8]) -> Option<ComicLayout> {
    let mut sites = Vec::new();
    for at in 0x0200..ram.len().saturating_sub(0x120) {
        if let Some(site) = parse_comic_dispatch(ram, at) {
            sites.push(site);
        }
    }
    sites.sort_by_key(|site| site.0.active_bit.0);
    let [v1, v2, v3] = sites.as_slice() else {
        return None;
    };
    let shared = v1.1 == v2.1
        && v2.1 == v3.1
        && v1.2 == v2.2
        && v2.2 == v3.2
        && v1.3 == v2.3
        && v2.3 == v3.3
        && v1.4 == v2.4
        && v2.4 == v3.4;
    let bits = v1.0.active_bit.0 == 1 && v2.0.active_bit.0 == 2 && v3.0.active_bit.0 == 4;
    if !shared || !bits {
        return None;
    }
    Some(ComicLayout {
        voices: [v1.0, v2.0, v3.0],
        freq_lo: v1.1,
        freq_hi: v1.2,
        active_mask_zp: v1.4,
        note_enable_mask: v1.3,
    })
}

fn parse_legacy_c0_dispatch(ram: &[u8], at: usize) -> Option<LegacyC0Site> {
    let word = |offset: usize| u16::from(ram[offset]) | (u16::from(ram[offset + 1]) << 8);
    if ram.get(at.wrapping_sub(2)) != Some(&0xA0)
        || ram.get(at.wrapping_sub(1)) != Some(&0x00)
        || ram.get(at) != Some(&0xB1)
        || ram.get(at + 2) != Some(&0xC9)
        || ram.get(at + 3) != Some(&0xC0)
        || ram.get(at + 4) != Some(&0x90)
    {
        return None;
    }
    let ptr_zp = LegacyC0Cell(u16::from(ram[at + 1]));
    let fetch = u16::try_from(at).ok()?;
    let mut head = None;
    for q in (at.saturating_sub(0x60)..at).rev() {
        if ram[q] == 0xAD
            && ram[q + 3] == 0xF0
            && ram[q + 4] == 0x04
            && ram[q + 5] == 0xC6
            && ram[q + 7] == 0xF0
            && branch_target(ram, u16::try_from(q + 8).ok()?) == fetch.wrapping_sub(2)
        {
            head = Some((
                LegacyC0Cell(word(q + 1)),
                LegacyC0Cell(u16::from(ram[q + 6])),
            ));
            break;
        }
    }
    let (sequence_enable, durctr_zp) = head?;

    let note_at = usize::from(branch_target(ram, u16::try_from(at + 5).ok()?));
    if note_at + 24 >= ram.len()
        || ram[note_at] != 0x85
        || ram[note_at + 2] != 0xC9
        || ram[note_at + 3] != 0x60
        || ram[note_at + 4] != 0x90
        || ram[note_at + 5] != 0x02
        || ram[note_at + 6] != 0xE9
        || ram[note_at + 7] != 0x60
        || ram[note_at + 8] != 0xC9
        || ram[note_at + 9] != NOTE_TIE
        || ram[note_at + 10] != 0xF0
    {
        return None;
    }
    let scratch = ram[note_at + 1];
    let has_rest = (note_at + 12..note_at + 22).any(|q| ram[q] == 0xC9 && ram[q + 1] == COMIC_REST);
    let transpose_at =
        (note_at + 12..note_at + 22).find(|&q| ram[q] == 0x65 && ram[q + 2] == 0xAA)?;
    let transpose = LegacyC0Cell(u16::from(ram[transpose_at + 1]));
    let condition_at = transpose_at + 3;
    let (active_mask, active_bit, note_enable) = if ram[condition_at] == 0xAD
        && ram[condition_at + 3] == 0x29
        && ram[condition_at + 5] == 0xF0
        && ram[condition_at + 7] == 0xAD
        && ram[condition_at + 10] == 0xF0
    {
        (
            Some(LegacyC0Cell(word(condition_at + 1))),
            Some(LegacyC0Mask(ram[condition_at + 4])),
            LegacyC0Cell(word(condition_at + 8)),
        )
    } else if ram[condition_at] == 0xAD && ram[condition_at + 3] == 0xF0 {
        (None, None, LegacyC0Cell(word(condition_at + 1)))
    } else {
        return None;
    };

    let mut frequency_tables = None;
    for q in condition_at..(at + 0x130).min(ram.len().saturating_sub(8)) {
        if ram[q] == 0xBC && ram[q + 3] == 0xBD && ram[q + 6] == 0x8D {
            frequency_tables = Some((LegacyC0Cell(word(q + 4)), LegacyC0Cell(word(q + 1))));
            break;
        }
    }
    let (freq_lo, freq_hi) = frequency_tables?;

    let mut duration_table = None;
    for q in at..(at + 0x180).min(ram.len().saturating_sub(24)) {
        if ram[q] == 0xA0
            && ram[q + 1] == 0x01
            && ram[q + 2] == 0xB1
            && u16::from(ram[q + 3]) == ptr_zp.0
            && ram[q + 4] == 0xA6
            && ram[q + 5] == scratch
            && ram[q + 6] == 0xE0
            && ram[q + 7] == 0x60
            && ram[q + 8] == 0xB0
            && ram[q + 9] == 0x04
            && ram[q + 10] == 0xAA
            && ram[q + 11] == 0xBD
            && ram[q + 14] == 0x85
            && u16::from(ram[q + 15]) == durctr_zp.0
            && ram[q + 16] == 0xA9
            && ram[q + 17] == 0x02
            && ram[q + 18] == 0x18
            && ram[q + 19] == 0x65
            && u16::from(ram[q + 20]) == ptr_zp.0
            && ram[q + 21] == 0x85
            && u16::from(ram[q + 22]) == ptr_zp.0
        {
            duration_table = Some(LegacyC0Cell(word(q + 12)));
            break;
        }
    }

    Some(LegacyC0Site {
        voice: LegacyC0Voice {
            ptr_zp,
            durctr_zp,
            sequence_enable,
            note_enable,
            transpose,
            active_bit,
            has_rest,
        },
        freq_lo,
        freq_hi,
        duration_table: duration_table?,
        active_mask,
    })
}

fn locate_legacy_c0(ram: &[u8]) -> Option<LegacyC0Layout> {
    let mut sites = Vec::new();
    for at in 0x0202..ram.len().saturating_sub(0x180) {
        if let Some(site) = parse_legacy_c0_dispatch(ram, at) {
            sites.push(site);
        }
    }
    sites.sort_by_key(|site| site.voice.ptr_zp.0);
    let [v1, v2, v3] = sites.as_slice() else {
        return None;
    };
    let shared = v1.freq_lo == v2.freq_lo
        && v2.freq_lo == v3.freq_lo
        && v1.freq_hi == v2.freq_hi
        && v2.freq_hi == v3.freq_hi
        && v1.duration_table == v2.duration_table
        && v2.duration_table == v3.duration_table;
    let consecutive_cells = v2.voice.ptr_zp.0 == v1.voice.ptr_zp.0.wrapping_add(2)
        && v3.voice.ptr_zp.0 == v2.voice.ptr_zp.0.wrapping_add(2)
        && v2.voice.durctr_zp.0 == v1.voice.durctr_zp.0.wrapping_add(1)
        && v3.voice.durctr_zp.0 == v2.voice.durctr_zp.0.wrapping_add(1)
        && v2.voice.sequence_enable.0 == v1.voice.sequence_enable.0.wrapping_add(1)
        && v3.voice.sequence_enable.0 == v2.voice.sequence_enable.0.wrapping_add(1)
        && v2.voice.note_enable.0 == v1.voice.note_enable.0.wrapping_add(1)
        && v3.voice.note_enable.0 == v2.voice.note_enable.0.wrapping_add(1)
        && v2.voice.transpose.0 == v1.voice.transpose.0.wrapping_add(1)
        && v3.voice.transpose.0 == v2.voice.transpose.0.wrapping_add(1);
    let active_mask = match (v1.active_mask, v2.active_mask, v3.active_mask) {
        (None, None, None)
            if [
                v1.voice.active_bit,
                v2.voice.active_bit,
                v3.voice.active_bit,
            ] == [None; 3] =>
        {
            None
        }
        (Some(a), Some(b), Some(c))
            if a == b
                && b == c
                && [
                    v1.voice.active_bit,
                    v2.voice.active_bit,
                    v3.voice.active_bit,
                ] == [
                    Some(LegacyC0Mask(1)),
                    Some(LegacyC0Mask(2)),
                    Some(LegacyC0Mask(4)),
                ] =>
        {
            Some(a)
        }
        _ => return None,
    };
    if !shared || !consecutive_cells {
        return None;
    }
    Some(LegacyC0Layout {
        voices: [v1.voice, v2.voice, v3.voice],
        freq_lo: v1.freq_lo,
        freq_hi: v1.freq_hi,
        duration_table: v1.duration_table,
        active_mask,
    })
}

fn recover_c0_structure(
    observations: &[C0Observation],
) -> Option<(Vec<VoicePlacements>, RecoveredStructure)> {
    let mut patterns = std::collections::BTreeMap::new();
    let mut address_owner = std::collections::HashMap::new();
    let mut pattern_starts = std::collections::HashMap::new();
    let mut placements: [Vec<NativePlacement>; 3] = std::array::from_fn(|_| Vec::new());
    let mut instances: [Vec<RecoveredPatternInstance>; 3] = std::array::from_fn(|_| Vec::new());
    let mut commands: [Vec<RecoveredOrderCommand>; 3] = std::array::from_fn(|_| Vec::new());

    for voice in 0..3 {
        let mut active = None;
        let mut previous_address = None;
        for observation in observations.iter().filter(|event| event.voice == voice) {
            let contiguous = previous_address
                .is_some_and(|previous: u16| observation.address == previous.wrapping_add(2));
            if active.is_none() || !contiguous {
                let pattern = if let Some(pattern) = address_owner.get(&observation.address) {
                    *pattern
                } else {
                    let number = u8::try_from(pattern_starts.len()).ok()?;
                    let pattern = PatternNumber(number);
                    pattern_starts.insert(pattern, observation.address);
                    pattern
                };
                active = Some(pattern);
                let order_offset = OrderOffset(commands[voice].len());
                let repeat_ordinal = RepeatOrdinal(
                    instances[voice]
                        .iter()
                        .filter(|instance| instance.pattern == pattern)
                        .count() as u32,
                );
                let transpose = PatternTranspose(i16::from(observation.transpose as i8));
                commands[voice].push(RecoveredOrderCommand::Pattern {
                    order_offset,
                    pattern,
                    repeat: PatternRepeatCount(1),
                });
                placements[voice].push(NativePlacement {
                    pattern_number: pattern,
                    start_frame: observation.frame,
                    transpose,
                    order_offset: Some(order_offset),
                    repeat_ordinal: Some(repeat_ordinal),
                });
                instances[voice].push(RecoveredPatternInstance {
                    pattern,
                    transpose,
                    repeat_ordinal,
                    order_offset,
                    start_tick: NativeRowTick(observation.frame.0),
                    start_frame: observation.frame,
                });
            }

            let pattern = active?;
            let start = *pattern_starts.get(&pattern)?;
            address_owner.entry(observation.address).or_insert(pattern);
            let command = observation.value >= 0xC0;
            let index = if observation.value >= 0x60 && !command {
                observation.value.wrapping_sub(0x60)
            } else {
                observation.value
            };
            let rest = !command && observation.has_rest && index == COMIC_REST;
            let hold = !command && index == NOTE_TIE;
            let event = RecoveredPatternEvent {
                offset: PatternByteOffset(observation.address.wrapping_sub(start)),
                duration: if command {
                    PatternDuration(0)
                } else {
                    observation.duration
                },
                frequency_index: (!command && !rest && !hold).then_some(FrequencyTableIndex(index)),
                instrument: None,
                hold,
                slide: None,
                command: command.then_some(NativeDriverOpcode(observation.value)),
                command_data: command.then_some(NativeEffectByte(observation.data)),
                duration_index: (!command).then_some(NativeEffectByte(observation.data)),
                operand: None,
            };
            let events = patterns.entry(pattern).or_insert_with(Vec::new);
            if !events.contains(&event) {
                events.push(event);
            }
            previous_address = Some(observation.address);
        }
        if !commands[voice].is_empty() {
            let order_offset = OrderOffset(commands[voice].len());
            commands[voice].push(RecoveredOrderCommand::Stop { order_offset });
        }
    }

    let mut structure = Vec::new();
    let mut voices = Vec::new();
    for voice in 0..3 {
        if instances[voice].is_empty() {
            continue;
        }
        structure.push(VoicePlacements {
            voice: VoiceId::from_index(voice),
            placements: std::mem::take(&mut placements[voice]),
        });
        voices.push(RecoveredVoiceStructure {
            voice: VoiceId::from_index(voice),
            order_loop_offset: None,
            order_commands: std::mem::take(&mut commands[voice]),
            instances: std::mem::take(&mut instances[voice]),
        });
    }
    Some((structure, RecoveredStructure { patterns, voices }))
}

fn decode_comic(
    emu: &mut Emulator,
    play: crate::header::PlayAddress,
    layout: &ComicLayout,
    clock: SystemClock,
    frames: u32,
) -> Result<(Vec<NoteEvent>, Vec<VoicePlacements>, RecoveredStructure), crate::emu::EmuError> {
    let mut notes: Vec<NoteEvent> = Vec::new();
    let mut observations = Vec::new();
    let mut open: [Option<usize>; 3] = [None; 3];
    for frame in 0..frames {
        let active = emu.read_ram(layout.active_mask_zp.0);
        let expired = layout
            .voices
            .map(|voice| active & voice.active_bit.0 != 0 && emu.read_ram(voice.durctr_zp.0) == 1);
        emu.run_play_frame(play, FrameIndex(frame))?;
        for (voice_index, voice) in layout.voices.iter().enumerate() {
            if !expired[voice_index] {
                continue;
            }
            let ptr = u16::from(emu.read_ram(voice.ptr_zp.0))
                | (u16::from(emu.read_ram(voice.ptr_zp.0.wrapping_add(1))) << 8);
            let event_addr = ptr.wrapping_sub(2);
            let event = emu.read_ram(event_addr);
            let duration = emu.read_ram(voice.durctr_zp.0);
            observations.push(C0Observation {
                voice: voice_index,
                address: event_addr,
                value: event,
                data: emu.read_ram(event_addr.wrapping_add(1)),
                duration: PatternDuration(if duration == 0 {
                    256
                } else {
                    u16::from(duration)
                }),
                transpose: emu.read_ram(voice.transpose.0),
                has_rest: true,
                frame: FrameIndex(frame),
            });
            if event >= 0xC0 {
                continue;
            }
            let duration_frames = if duration == 0 {
                256
            } else {
                u32::from(duration)
            };
            let end = frame.saturating_add(duration_frames).min(frames);
            let index = if event >= 0x60 {
                event.wrapping_sub(0x60)
            } else {
                event
            };
            match index {
                COMIC_REST => open[voice_index] = None,
                NOTE_TIE => {
                    if let Some(note_index) = open[voice_index] {
                        notes[note_index].end_frame = Some(FrameIndex(end));
                    }
                }
                _ => {
                    let enabled = emu.read_ram(layout.note_enable_mask.0) & voice.active_bit.0 != 0
                        && emu.read_ram(layout.active_mask_zp.0) & voice.gate_bit.0 != 0;
                    if !enabled {
                        open[voice_index] = None;
                        continue;
                    }
                    let pitch_index = index.wrapping_add(emu.read_ram(voice.transpose.0));
                    let raw = u32::from(
                        emu.read_ram(layout.freq_lo.0.wrapping_add(u16::from(pitch_index))),
                    ) | (u32::from(
                        emu.read_ram(layout.freq_hi.0.wrapping_add(u16::from(pitch_index))),
                    ) << 8);
                    let physical_voice = VoiceId::from_index(voice_index);
                    open[voice_index] = note_from_raw_freq(raw, clock, physical_voice, frame, end)
                        .map(|note| {
                            notes.push(note);
                            notes.len() - 1
                        });
                }
            }
        }
    }
    notes.sort_by_key(|note| (note.voice, note.start_frame.0));
    let (structure, recovered) = recover_c0_structure(&observations).unwrap_or_else(|| {
        (
            Vec::new(),
            RecoveredStructure {
                patterns: std::collections::BTreeMap::new(),
                voices: Vec::new(),
            },
        )
    });
    Ok((notes, structure, recovered))
}

fn decode_legacy_c0(
    emu: &mut Emulator,
    play: crate::header::PlayAddress,
    layout: &LegacyC0Layout,
    clock: SystemClock,
    frames: u32,
) -> Result<(Vec<NoteEvent>, Vec<VoicePlacements>, RecoveredStructure), crate::emu::EmuError> {
    let mut notes: Vec<NoteEvent> = Vec::new();
    let mut observations = Vec::new();
    let mut open: [Option<usize>; 3] = [None; 3];
    for frame in 0..frames {
        let expired = layout.voices.map(|voice| {
            emu.read_ram(voice.sequence_enable.0) != 0 && emu.read_ram(voice.durctr_zp.0) == 1
        });
        emu.run_play_frame(play, FrameIndex(frame))?;
        for (voice_index, voice) in layout.voices.iter().enumerate() {
            if !expired[voice_index] {
                continue;
            }
            let ptr = u16::from(emu.read_ram(voice.ptr_zp.0))
                | (u16::from(emu.read_ram(voice.ptr_zp.0.wrapping_add(1))) << 8);
            let event_addr = ptr.wrapping_sub(2);
            let event = emu.read_ram(event_addr);
            let data = emu.read_ram(event_addr.wrapping_add(1));
            let duration_byte = emu.read_ram(voice.durctr_zp.0);
            let duration = PatternDuration(if duration_byte == 0 {
                256
            } else {
                u16::from(duration_byte)
            });
            observations.push(C0Observation {
                voice: voice_index,
                address: event_addr,
                value: event,
                data,
                duration,
                transpose: emu.read_ram(voice.transpose.0),
                has_rest: voice.has_rest,
                frame: FrameIndex(frame),
            });
            if event >= 0xC0 {
                continue;
            }
            let end = frame.saturating_add(u32::from(duration.0)).min(frames);
            let index = if event >= 0x60 {
                event.wrapping_sub(0x60)
            } else {
                event
            };
            if index == NOTE_TIE {
                if let Some(note_index) = open[voice_index] {
                    notes[note_index].end_frame = Some(FrameIndex(end));
                }
                continue;
            }
            if voice.has_rest && index == COMIC_REST {
                open[voice_index] = None;
                continue;
            }
            let active = match (layout.active_mask, voice.active_bit) {
                (None, None) => true,
                (Some(cell), Some(bit)) => emu.read_ram(cell.0) & bit.0 != 0,
                _ => false,
            };
            if !active || emu.read_ram(voice.note_enable.0) == 0 {
                open[voice_index] = None;
                continue;
            }
            let pitch_index = index.wrapping_add(emu.read_ram(voice.transpose.0));
            let raw =
                u32::from(emu.read_ram(layout.freq_lo.0.wrapping_add(u16::from(pitch_index))))
                    | (u32::from(
                        emu.read_ram(layout.freq_hi.0.wrapping_add(u16::from(pitch_index))),
                    ) << 8);
            let physical_voice = VoiceId::from_index(voice_index);
            open[voice_index] =
                note_from_raw_freq(raw, clock, physical_voice, frame, end).map(|note| {
                    notes.push(note);
                    notes.len() - 1
                });
        }
    }
    notes.sort_by_key(|note| (note.voice, note.start_frame.0));
    let (structure, recovered) = recover_c0_structure(&observations).unwrap_or_else(|| {
        (
            Vec::new(),
            RecoveredStructure {
                patterns: std::collections::BTreeMap::new(),
                voices: Vec::new(),
            },
        )
    });
    Ok((notes, structure, recovered))
}

fn supplement_native_call_voices(
    notes: &mut Vec<NoteEvent>,
    note_instruments: &mut Vec<Option<GalwayInstrument>>,
    truth: &[NoteEvent],
    native_calls: [bool; 3],
) -> TraceCorrectedNoteCount {
    let mut corrected = 0;
    for (voice_index, native_call) in native_calls.into_iter().enumerate() {
        let voice = VoiceId::from_index(voice_index);
        if !native_call || notes.iter().any(|note| note.voice == voice) {
            continue;
        }
        for note in truth.iter().filter(|note| note.voice == voice) {
            notes.push(*note);
            note_instruments.push(None);
            corrected += 1;
        }
    }
    if corrected == 0 {
        return TraceCorrectedNoteCount(0);
    }
    let mut order: Vec<usize> = (0..notes.len()).collect();
    order.sort_by_key(|&index| (notes[index].voice, notes[index].start_frame.0));
    *notes = order.iter().map(|&index| notes[index]).collect();
    *note_instruments = order
        .iter()
        .map(|&index| note_instruments[index].clone())
        .collect();
    TraceCorrectedNoteCount(corrected)
}

/// The `Martin_Galway` driver extractor (see the module docs for status).
pub struct GalwayExtractor;

impl DriverExtractor for GalwayExtractor {
    fn name(&self) -> &'static str {
        "galway"
    }

    fn handles(&self, driver: &str) -> bool {
        driver == "Martin_Galway"
    }

    fn extract(&self, ctx: &NativeContext<'_>) -> Result<NativeSong, NativeError> {
        let emu_err = |e: crate::emu::EmuError| NativeError::Emulation {
            driver: ctx.driver.to_string(),
            stage: super::EmulationStage::ExtractorSetup,
            reason: e.to_string(),
        };

        // Post-`init` RAM image the simulation runs over.
        let mut img = Emulator::with_timing(ctx.timing);
        img.load(ctx.header, ctx.bytes).map_err(emu_err)?;
        img.call_init(ctx.header.init_address, ctx.subtune, ctx.header.songs)
            .map_err(emu_err)?;
        let read = |a| img.read_ram(a);
        let ram: Vec<u8> = (0..=u16::MAX).map(read).collect();
        let locate_failed = || NativeError::LocateFailed {
            driver: ctx.driver.to_string(),
            extractor: self.name(),
            reason: "required Galway signatures were not unique".to_owned(),
        };
        let layout = locate(&ram).filter(|layout| layout_matches(&ram, layout));
        let comic_layout = layout.is_none().then(|| locate_comic(&ram)).flatten();
        let legacy_c0_layout = (layout.is_none() && comic_layout.is_none())
            .then(|| locate_legacy_c0(&ram))
            .flatten();
        if layout.is_none() && comic_layout.is_none() && legacy_c0_layout.is_none() {
            return Err(locate_failed());
        }

        // Per-frame voice state and effects come from a faithful trace (the
        // emulator is ground truth for what the chip plays); the note timeline
        // comes from simulating the driver's own sequencer.
        let trace =
            emu::run_with_timing(ctx.header, ctx.bytes, ctx.subtune, ctx.frames, ctx.timing)
                .map_err(emu_err)?;
        let validation_timing = ctx.validation_timing(&trace, self.name())?;
        let states = analyze(&trace);
        let frame_count = states.len();
        let effects = detect_effects(&trace, &states, EffectThresholds::default());

        let (mut notes, structure, mut note_instruments, recovered_structure, native_calls) =
            if let Some(layout) = layout {
                let decoded =
                    decode_song_structured(ram, &layout, ctx.timing.clock, frame_count as u32);
                (
                    decoded.notes,
                    decoded.structure,
                    decoded.note_instruments,
                    Some(decoded.recovered),
                    decoded.native_calls,
                )
            } else if let Some(layout) = comic_layout {
                let (notes, structure, recovered) = decode_comic(
                    &mut img,
                    ctx.header.play_address,
                    &layout,
                    ctx.timing.clock,
                    frame_count as u32,
                )
                .map_err(emu_err)?;
                let instruments = vec![None; notes.len()];
                (notes, structure, instruments, Some(recovered), [false; 3])
            } else if let Some(layout) = legacy_c0_layout {
                let (notes, structure, recovered) = decode_legacy_c0(
                    &mut img,
                    ctx.header.play_address,
                    &layout,
                    ctx.timing.clock,
                    frame_count as u32,
                )
                .map_err(emu_err)?;
                let instruments = vec![None; notes.len()];
                (notes, structure, instruments, Some(recovered), [false; 3])
            } else {
                return Err(locate_failed());
            };
        if notes.is_empty() {
            return Err(NativeError::DecodeEmpty {
                driver: ctx.driver.to_string(),
                extractor: self.name(),
            });
        }

        // Self-validation against the trace, exactly like the Hubbard
        // extractor: a relocated instance of the player decodes to garbage
        // against the fixed layout, so refuse — cleanly falling back to
        // `--format synth` — when the onsets disagree.
        let truth = detect_notes(&states, ctx.timing.clock);
        let validation_notes = arpeggio_validation_notes(&notes, &truth, &states, ctx.timing.clock);
        let mut validation = validate_native_notes(
            &validation_notes,
            &truth,
            validation_timing,
            NativeValidationPolicy::default(),
        );
        let mut trace_corrected_notes = TraceCorrectedNoteCount(0);
        if !validation.accepted {
            trace_corrected_notes = supplement_native_call_voices(
                &mut notes,
                &mut note_instruments,
                &truth,
                native_calls,
            );
            if trace_corrected_notes.0 > 0 {
                let validation_notes =
                    arpeggio_validation_notes(&notes, &truth, &states, ctx.timing.clock);
                validation = validate_native_notes(
                    &validation_notes,
                    &truth,
                    validation_timing,
                    NativeValidationPolicy::default(),
                );
            }
        }
        if !validation.accepted {
            return Err(NativeError::DecodeUnreliable {
                driver: ctx.driver.to_string(),
                extractor: self.name(),
                reason: validation.reason_summary(),
            });
        }

        // Per-note timbre fingerprints (trace-derived; the per-voice profile,
        // filter, arpeggio-loop etc. stay trace-measured).
        let voice3_reads = trace.voice3_reads_per_frame();
        let mut characteristics: Vec<_> = notes
            .iter()
            .map(|n| extract_characteristics(n, &states, &effects, ctx.timing.clock))
            .collect();
        apply_voice3_lfo_detection(&mut characteristics, &notes, &voice3_reads);

        let mut instruments: Vec<GalwayInstrument> = Vec::new();
        let group: Vec<Option<u8>> = note_instruments
            .iter()
            .map(|instrument| {
                instrument.as_ref().and_then(|instrument| {
                    let idx = instruments
                        .iter()
                        .position(|existing| existing == instrument)
                        .unwrap_or_else(|| {
                            instruments.push(instrument.clone());
                            instruments.len() - 1
                        });
                    u8::try_from(idx).ok()
                })
            })
            .collect();
        let (patches, patch_assignments) = if instruments.is_empty() {
            let (mut p, mut a) = extract_patches(&notes, &characteristics);
            split_arpeggio_patches(&mut p, &mut a, &characteristics);
            (p, a)
        } else {
            let authored = |idx: u8| {
                let instrument = &instruments[usize::from(idx)];
                (
                    instrument.adsr(),
                    // The simulated register image does not execute the live
                    // waveform program, so the trace remains waveform truth.
                    None,
                    false,
                    Some(instrument.effects()),
                    Some(instrument.definition()),
                )
            };
            extract_patches_grouped(&notes, &characteristics, &group, authored)
        };

        let provenance = vec![
            ProvenanceEvidence {
                field: "note.pitch".to_owned(),
                provenance: if trace_corrected_notes.0 == 0 {
                    FieldProvenance::AuthoredDecoded
                } else {
                    FieldProvenance::TraceCorrected
                },
                samples: notes.len(),
                mismatches: validation.inserted.0,
            },
            ProvenanceEvidence {
                field: "note.articulation".to_owned(),
                provenance: if trace_corrected_notes.0 == 0 {
                    FieldProvenance::AuthoredDecoded
                } else {
                    FieldProvenance::TraceCorrected
                },
                samples: notes.len(),
                mismatches: validation.deleted.0,
            },
            ProvenanceEvidence {
                field: "song.structure".to_owned(),
                provenance: if recovered_structure.is_some() {
                    FieldProvenance::AuthoredDecoded
                } else {
                    FieldProvenance::Unsupported
                },
                samples: recovered_structure
                    .as_ref()
                    .map_or(0, |structure| structure.patterns.len()),
                mismatches: 0,
            },
            ProvenanceEvidence {
                field: "instrument.identity".to_owned(),
                provenance: FieldProvenance::AuthoredDecoded,
                samples: instruments.len(),
                mismatches: 0,
            },
            ProvenanceEvidence {
                field: "instrument.definition".to_owned(),
                provenance: FieldProvenance::AuthoredDecoded,
                samples: instruments.len(),
                mismatches: 0,
            },
        ];

        Ok(NativeSong {
            capture: trace.capture.clone(),
            states,
            notes,
            patches,
            patch_assignments,
            characteristics,
            effects,
            structure: (!structure.is_empty()).then_some(structure),
            recovered_structure,
            validation,
            provenance,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::emu::Emulator;
    use crate::header::SubtuneIndex;
    use crate::trace::FrameIndex;

    #[test]
    fn decodes_complete_instrument_register_image() {
        let mut bytes = [0u8; INSTRUMENT_BYTES];
        bytes[0x00..=0x07].copy_from_slice(&[1, 0, 2, 0, 3, 0, 4, 0]);
        bytes[0x08..=0x0B].copy_from_slice(&[5, 6, 7, 8]);
        bytes[0x0C..=0x0E].copy_from_slice(&[9, 10, 0x87]);
        bytes[0x0F..=0x12].copy_from_slice(&[0xFE, 0xFF, 3, 0]);
        bytes[0x13..=0x17].copy_from_slice(&[11, 12, 13, 14, 0x05]);
        bytes[0x18..=0x1E].copy_from_slice(&[0x34, 0x02, 0xA7, 0xB8, 0x41, 16, 17]);
        bytes[0x1F..=0x22].copy_from_slice(&[5, 6, 0xC3, 0x5F]);
        for (index, byte) in bytes[0x23..=0x32].iter_mut().enumerate() {
            *byte = index as u8 + 1;
        }
        let definition = GalwayInstrument { bytes }.definition();

        assert_eq!(definition.pitch.stages.len(), 4);
        assert_eq!(definition.pitch.stages[3].delta, ModulationDelta(4));
        assert_eq!(definition.pitch.stages[3].frames, ProgramFrames(8));
        assert_eq!(
            definition.pitch.loop_mode,
            AuthoredLoopMode::RestartFromCurrent
        );
        assert!(definition.pitch.apply_during_delay);
        assert_eq!(definition.pulse_width.stages[0].delta, ModulationDelta(-2));
        assert_eq!(
            definition.pulse_width.loop_mode,
            AuthoredLoopMode::RestartFromInitial
        );
        assert_eq!(definition.initial_pulse_width, PulseWidth(0x234));
        assert_eq!(definition.gate_frames, ProgramFrames(17));
        assert_eq!(definition.release_frames, ProgramFrames(16));
        assert_eq!(definition.filter.routing, FilterRoutingMask(3));
        assert_eq!(definition.filter.volume, SidVolume(15));
        assert_eq!(definition.duration_table.len(), 17);
        assert_eq!(definition.duration_table[16], ProgramFrames(16));
    }

    /// 64 KiB RAM with the Neverending_Story command handlers pasted at their
    /// real addresses (bytes lifted verbatim from the post-`init` dump), so
    /// [`classify_handler`] is exercised against the actual code shapes.
    fn handler_ram() -> Vec<u8> {
        let mut ram = vec![0u8; 0x1_0000];
        // $9B6B..: V2 return, block-copy loop, the four BNE block-copy heads,
        // goto+transpose, goto, native call, gosub, poke, gosub+transpose,
        // then the start of the V2 jump table.
        let v2: &[u8] = &[
            0xA6, 0x1E, 0xE8, 0x86, 0x1E, 0xBD, 0x7A, 0x9F, 0x85, 0x12, 0xBD, 0x8A, 0x9F, 0x85,
            0x13, 0x4C, 0xA4, 0x9A, 0xA0, 0x04, 0xA2, 0x1E, 0xB1, 0x16, 0x9D, 0x47, 0x9F, 0xCA,
            0x88, 0x10, 0xF7, 0x4C, 0x99, 0x9A, 0xA0, 0x22, 0xA2, 0x22, 0xD0, 0xEE, 0xA0, 0x32,
            0xA2, 0x32, 0xD0, 0xE8, 0xA0, 0x0E, 0xA2, 0x0E, 0xD0, 0xE2, 0xA0, 0x0A, 0xA2, 0x19,
            0xD0, 0xDC, 0xA0, 0x03, 0xB1, 0x12, 0x8D, 0x9A, 0x9F, 0xA6, 0x16, 0x86, 0x12, 0xA6,
            0x17, 0x86, 0x13, 0x4C, 0xA4, 0x9A, 0xA9, 0x9A, 0x48, 0xA9, 0x98, 0x48, 0x6C, 0x16,
            0x00, 0xA9, 0x03, 0xA6, 0x1E, 0x18, 0x65, 0x12, 0x9D, 0x7A, 0x9F, 0xA5, 0x13, 0x69,
            0x00, 0x9D, 0x8A, 0x9F, 0xC6, 0x1E, 0x4C, 0xAC, 0x9B, 0xA0, 0x01, 0xB1, 0x12, 0xAA,
            0xC8, 0xB1, 0x12, 0x9D, 0x47, 0x9F, 0x4C, 0x99, 0x9A, 0xA0, 0x03, 0xB1, 0x12, 0x8D,
            0x9A, 0x9F, 0xA9, 0x04, 0xD0, 0xD3,
        ];
        ram[0x9B6B..0x9B6B + v2.len()].copy_from_slice(v2);
        // $96A1..: the shared stop/silence handler.
        let stop: &[u8] = &[
            0xA9, 0x38, 0x85, 0x19, 0xA9, 0x00, 0x8D, 0x83, 0xF0, 0x8D, 0x84, 0xF0,
        ];
        ram[0x96A1..0x96A1 + stop.len()].copy_from_slice(stop);
        // $98E5..: V1 gosub+transpose, whose `LDA #4` falls through a
        // `BIT $03A9` skip-trick hiding the plain-gosub `LDA #3` entry ($98EF).
        let v1: &[u8] = &[
            0xA0, 0x03, 0xB1, 0x10, 0x8D, 0x46, 0x9F, 0xA9, 0x04, 0x2C, 0xA9, 0x03, 0xA6, 0x1D,
            0x18, 0x65, 0x10, 0x9D, 0x26, 0x9F, 0xA5, 0x11, 0x69, 0x00, 0x9D, 0x36, 0x9F, 0xC6,
            0x1D, 0x4C, 0xD1, 0x98,
        ];
        ram[0x98E5..0x98E5 + v1.len()].copy_from_slice(v1);
        ram
    }

    fn rambo_handler_ram() -> Vec<u8> {
        let mut ram = vec![0u8; 0x1_0000];
        let voice_1: &[u8] = &[
            0xE6, 0x1D, 0xA4, 0x1D, 0xC0, 0x08, 0xF0, 0x09, 0xBE, 0x05, 0x29, 0xB9, 0x0D, 0x29,
            0x4C, 0xED, 0x21, 0xA5, 0x19, 0x29, 0xFE, 0x85, 0x19, 0x60, 0xA6, 0x1D, 0x18, 0x98,
            0x65, 0x10, 0x9D, 0x05, 0x29, 0xA9, 0x00, 0x65, 0x11, 0x9D, 0x0D, 0x29, 0xA5, 0x16,
            0x9D, 0x15, 0x29, 0xC6, 0x1D, 0x98, 0x4C, 0xAD, 0x20, 0xA6, 0x1D, 0xDE, 0x16, 0x29,
            0xF0, 0x05, 0xE8, 0x8A, 0xA8, 0x10, 0xC9, 0xE6, 0x1D, 0xA9, 0x01, 0x4C, 0xAD, 0x20,
        ];
        ram[0x217D..0x217D + voice_1.len()].copy_from_slice(voice_1);
        let voice_2: &[u8] = &[
            0xE6, 0x1E, 0xA4, 0x1E, 0xC0, 0x08, 0xF0, 0x09, 0xBE, 0x4F, 0x29, 0xB9, 0x57, 0x29,
            0x4C, 0xAC, 0x24, 0xA5, 0x19, 0x29, 0xFD, 0x85, 0x19, 0x60, 0xA6, 0x1E, 0x18, 0x98,
            0x65, 0x12, 0x9D, 0x4F, 0x29, 0xA5, 0x13, 0x69, 0x00, 0x9D, 0x57, 0x29, 0xA5, 0x16,
            0x9D, 0x5F, 0x29, 0xC6, 0x1E, 0x98, 0x4C, 0x83, 0x23, 0xA6, 0x1E, 0xDE, 0x60, 0x29,
            0xF0, 0x05, 0xE8, 0x8A, 0xA8, 0x10, 0xC9, 0xE6, 0x1E, 0xA9, 0x01, 0x4C, 0x83, 0x23,
            0xA0, 0x04, 0xA2, 0x1C, 0xB1, 0x16, 0x9D, 0x1E, 0x29, 0xCA, 0x88, 0x10, 0xF7, 0x4C,
            0x81, 0x23, 0xA0, 0x0D, 0xA2, 0x0D, 0xD0, 0xEE, 0xA0, 0x09, 0xA2, 0x17, 0xD0, 0xE8,
            0xA0, 0x1C, 0xA2, 0x1C, 0xD0, 0xE2, 0xA5, 0x17, 0x86, 0x12, 0x85, 0x13, 0x4C, 0x8C,
            0x23, 0xA9, 0x03, 0xA4, 0x1E, 0x18, 0x65, 0x12, 0x99, 0x4F, 0x29, 0xA5, 0x13, 0x69,
            0x00, 0x99, 0x57, 0x29, 0xC6, 0x1E, 0x4C, 0xAA, 0x24, 0xC8, 0xB1, 0x12, 0x8D, 0x67,
            0x29, 0xA9, 0x04, 0xD0, 0xE2, 0x9D, 0x1E, 0x29, 0x4C, 0x81, 0x23, 0x9D, 0xD9, 0x29,
            0x4C, 0x81, 0x23, 0x60,
        ];
        ram[0x2442..0x2442 + voice_2.len()].copy_from_slice(voice_2);
        ram
    }

    /// Post-init RAM image for the Neverending_Story asset fixture.
    fn neverending_ram() -> Vec<u8> {
        let bytes = std::fs::read("../../assets/music/Neverending_Story.sid").unwrap();
        let header = crate::header::parse(&bytes).unwrap();
        let mut emu = Emulator::new();
        emu.load(&header, &bytes).unwrap();
        emu.call_init(header.init_address, SubtuneIndex(1), header.songs)
            .unwrap();
        emu.ram_image()
    }

    fn comic_ram() -> Vec<u8> {
        let bytes = std::fs::read("../../assets/music/Comic_Bakery.sid").unwrap();
        let header = crate::header::parse(&bytes).unwrap();
        let mut emu = Emulator::new();
        emu.load(&header, &bytes).unwrap();
        emu.call_init(header.init_address, SubtuneIndex(1), header.songs)
            .unwrap();
        emu.ram_image()
    }

    fn legacy_c0_fixture_ram(active_mask: Option<LegacyC0Cell>) -> Vec<u8> {
        fn write_word(ram: &mut [u8], at: usize, value: u16) {
            ram[at..at + 2].copy_from_slice(&value.to_le_bytes());
        }

        let mut ram = vec![0u8; 0x1_0000];
        for voice_index in 0..3usize {
            let voice_offset = u16::try_from(voice_index).unwrap();
            let at = 0x1000 + voice_index * 0x0400;
            let ptr_zp = 0x40 + voice_offset * 2;
            let duration_counter = 0x50 + voice_offset;
            let sequence_enable = 0x9000 + voice_offset;
            let note_enable = 0x9010 + voice_offset;
            let transpose = 0x60 + voice_offset;

            let head = at - 0x10;
            ram[head] = 0xAD;
            write_word(&mut ram, head + 1, sequence_enable);
            ram[head + 3] = 0xF0;
            ram[head + 4] = 0x04;
            ram[head + 5] = 0xC6;
            ram[head + 6] = duration_counter as u8;
            ram[head + 7] = 0xF0;
            ram[head + 8] = 0x05;

            ram[at - 2..at + 6].copy_from_slice(&[
                0xA0,
                0x00,
                0xB1,
                ptr_zp as u8,
                0xC9,
                0xC0,
                0x90,
                0x1A,
            ]);

            let note = at + 0x20;
            ram[note..note + 12].copy_from_slice(&[
                0x85, 0x70, 0xC9, 0x60, 0x90, 0x02, 0xE9, 0x60, 0xC9, NOTE_TIE, 0xF0, 0x00,
            ]);
            let transpose_at = if voice_index == 1 {
                note + 12
            } else {
                ram[note + 12..note + 14].copy_from_slice(&[0xC9, COMIC_REST]);
                note + 14
            };
            ram[transpose_at..transpose_at + 3].copy_from_slice(&[0x65, transpose as u8, 0xAA]);
            let condition = transpose_at + 3;
            if let Some(active_mask) = active_mask {
                ram[condition] = 0xAD;
                write_word(&mut ram, condition + 1, active_mask.0);
                ram[condition + 3] = 0x29;
                ram[condition + 4] = 1 << voice_index;
                ram[condition + 5] = 0xF0;
                ram[condition + 7] = 0xAD;
                write_word(&mut ram, condition + 8, note_enable);
                ram[condition + 10] = 0xF0;
            } else {
                ram[condition] = 0xAD;
                write_word(&mut ram, condition + 1, note_enable);
                ram[condition + 3] = 0xF0;
            }

            let frequency = at + 0x50;
            ram[frequency] = 0xBC;
            write_word(&mut ram, frequency + 1, 0x9200);
            ram[frequency + 3] = 0xBD;
            write_word(&mut ram, frequency + 4, 0x9100);
            ram[frequency + 6] = 0x8D;

            let duration = at + 0x70;
            ram[duration..duration + 12].copy_from_slice(&[
                0xA0,
                0x01,
                0xB1,
                ptr_zp as u8,
                0xA6,
                0x70,
                0xE0,
                0x60,
                0xB0,
                0x04,
                0xAA,
                0xBD,
            ]);
            write_word(&mut ram, duration + 12, 0x9300);
            ram[duration + 14..duration + 23].copy_from_slice(&[
                0x85,
                duration_counter as u8,
                0xA9,
                0x02,
                0x18,
                0x65,
                ptr_zp as u8,
                0x85,
                ptr_zp as u8,
            ]);
        }
        ram
    }

    #[test]
    #[cfg_attr(
        not(feature = "asset-tests"),
        ignore = "requires the optional assets/music corpus"
    )]
    fn locates_the_neverending_story_layout() {
        // Every address verified against the full disassembly
        // (docs/drivers/galway.md).
        let ram = neverending_ram();
        let layout = locate(&ram).unwrap();
        assert_eq!(layout.freq_lo, 0xF0EA);
        assert_eq!(layout.freq_hi, 0xF08E);
        assert_eq!(layout.active_mask_zp, 0x19);
        assert!(layout.has_tie);
        let expected: [(u16, u16, u8, u16, u16, u16); 3] = [
            (0x10, 0x1A, 0x01, 0x9913, 0x9F15, 0x9F46),
            (0x12, 0x1B, 0x02, 0x9BEF, 0x9F69, 0x9F9A),
            (0x14, 0x1C, 0x04, 0x9E31, 0x9FBD, 0x9FEE),
        ];
        for (v, (ptr, dur_ctr, bit, table, dur, transpose)) in layout.voices.iter().zip(expected) {
            assert_eq!(v.ptr_zp, ptr);
            assert_eq!(v.durctr_zp, dur_ctr);
            assert_eq!(v.active_bit, bit);
            assert_eq!(v.jump_table, table);
            assert_eq!(v.dur_table, dur);
            assert_eq!(v.transpose, transpose);
        }
        assert!(layout_matches(&ram, &layout));
    }

    #[test]
    #[cfg_attr(
        not(feature = "asset-tests"),
        ignore = "requires the optional assets/music corpus"
    )]
    fn locates_the_comic_bakery_generation() {
        let layout = locate_comic(&comic_ram()).unwrap();
        assert_eq!(layout.freq_lo, ComicCell(0x8DF1));
        assert_eq!(layout.freq_hi, ComicCell(0x8D92));
        assert_eq!(layout.active_mask_zp, ComicCell(0xF9));
        assert_eq!(layout.note_enable_mask, ComicCell(0x8D7D));
        let expected = [
            (0xF0, 0xFA, 0x01, 0x08, 0x8D7A),
            (0xF2, 0xFB, 0x02, 0x10, 0x8D7B),
            (0xF4, 0xFC, 0x04, 0x20, 0x8D7C),
        ];
        for (voice, expected) in layout.voices.iter().zip(expected) {
            assert_eq!(
                (
                    voice.ptr_zp.0,
                    voice.durctr_zp.0,
                    voice.active_bit.0,
                    voice.gate_bit.0,
                    voice.transpose.0,
                ),
                expected
            );
        }
    }

    #[test]
    fn locates_synthetic_legacy_c0_dispatches_with_an_active_mask() {
        let layout = locate_legacy_c0(&legacy_c0_fixture_ram(Some(LegacyC0Cell(0x9020)))).unwrap();
        assert_eq!(layout.freq_lo, LegacyC0Cell(0x9100));
        assert_eq!(layout.freq_hi, LegacyC0Cell(0x9200));
        assert_eq!(layout.duration_table, LegacyC0Cell(0x9300));
        assert_eq!(layout.active_mask, Some(LegacyC0Cell(0x9020)));
        let expected = [
            (0x40, 0x50, 0x9000, 0x9010, 0x60, Some(1), true),
            (0x42, 0x51, 0x9001, 0x9011, 0x61, Some(2), false),
            (0x44, 0x52, 0x9002, 0x9012, 0x62, Some(4), true),
        ];
        for (voice, expected) in layout.voices.iter().zip(expected) {
            assert_eq!(
                (
                    voice.ptr_zp.0,
                    voice.durctr_zp.0,
                    voice.sequence_enable.0,
                    voice.note_enable.0,
                    voice.transpose.0,
                    voice.active_bit.map(|bit| bit.0),
                    voice.has_rest,
                ),
                expected,
            );
        }
    }

    #[test]
    fn locates_synthetic_legacy_c0_dispatches_without_an_active_mask() {
        let layout = locate_legacy_c0(&legacy_c0_fixture_ram(None)).unwrap();
        assert_eq!(layout.freq_lo, LegacyC0Cell(0x9100));
        assert_eq!(layout.freq_hi, LegacyC0Cell(0x9200));
        assert_eq!(layout.duration_table, LegacyC0Cell(0x9300));
        assert_eq!(layout.active_mask, None);
        let expected = [
            (0x40, 0x50, 0x9000, 0x9010, 0x60, true),
            (0x42, 0x51, 0x9001, 0x9011, 0x61, false),
            (0x44, 0x52, 0x9002, 0x9012, 0x62, true),
        ];
        for (voice, expected) in layout.voices.iter().zip(expected) {
            assert_eq!(
                (
                    voice.ptr_zp.0,
                    voice.durctr_zp.0,
                    voice.sequence_enable.0,
                    voice.note_enable.0,
                    voice.transpose.0,
                    voice.has_rest,
                ),
                expected,
            );
            assert_eq!(voice.active_bit, None);
        }
    }

    #[test]
    #[cfg_attr(
        not(feature = "asset-tests"),
        ignore = "requires the optional assets/music corpus"
    )]
    fn full_length_named_variants_pass_native_validation() {
        let db = crate::playerid::PlayerDb::embedded();
        for (file, frames, exact_count) in [
            ("Comic_Bakery.sid", 9_474, false),
            ("Ocean_Loader_1.sid", 10_126, true),
        ] {
            let bytes = std::fs::read(format!("../../assets/music/{file}")).unwrap();
            let header = crate::header::parse(&bytes).unwrap();
            let timing = crate::emu::PlaybackTiming::for_subtune(&header, SubtuneIndex(1));
            let (_, extractor, song) = crate::export::native::extract_native_song(
                &db,
                &header,
                &bytes,
                SubtuneIndex(1),
                timing,
                frames,
            )
            .unwrap_or_else(|error| panic!("{file} full-length native extraction: {error}"));
            assert_eq!(extractor, "galway");
            assert!(song.validation.accepted);
            assert!(
                song.structure
                    .as_ref()
                    .is_some_and(|voices| !voices.is_empty())
            );
            assert!(
                song.recovered_structure
                    .as_ref()
                    .is_some_and(|structure| !structure.patterns.is_empty())
            );
            if exact_count {
                assert_eq!(song.validation.native, song.validation.truth);
            }
        }
    }

    #[test]
    #[cfg_attr(
        not(feature = "asset-tests"),
        ignore = "requires the optional assets/music corpus"
    )]
    fn recovers_neverending_story_placements() {
        let ram = neverending_ram();
        let layout = locate(&ram).unwrap();
        let decoded = decode_song_structured(ram, &layout, SystemClock::Pal, 3000);
        assert!(!decoded.notes.is_empty());
        assert!(
            !decoded.structure.is_empty(),
            "expected recovered per-voice placements"
        );
        for vp in &decoded.structure {
            // Placements are emitted in play order: start frames never go back.
            assert!(
                vp.placements
                    .windows(2)
                    .all(|w| w[0].start_frame <= w[1].start_frame),
                "V{} placements not frame-ordered",
                vp.voice.0
            );
        }
        // The driver's gosub/goto reuse surfaces as a pattern number recurring
        // within a voice (fewer distinct numbers than placements somewhere).
        let reused = decoded.structure.iter().any(|vp| {
            let distinct: std::collections::HashSet<u8> =
                vp.placements.iter().map(|p| p.pattern_number.0).collect();
            distinct.len() < vp.placements.len()
        });
        assert!(reused, "expected at least one reused (recurring) pattern");
    }

    /// A note + default characteristics at `midi` on voice 1, active on
    /// frames `[start, end)` — `end` exclusive, like `NoteEvent.end_frame`.
    fn arp_note(start: u32, end: u32, midi: u8) -> (NoteEvent, NoteCharacteristics) {
        use crate::analysis::note::{Cents, GmProgram, MidiNote, Velocity};
        (
            NoteEvent {
                voice: VoiceId(1),
                start_frame: FrameIndex(start),
                end_frame: Some(FrameIndex(end)),
                midi: MidiNote(midi),
                cents: Cents(0.0),
                program: GmProgram::SQUARE_LEAD,
                velocity: Velocity(100),
            },
            NoteCharacteristics::default(),
        )
    }

    #[test]
    fn standard_characteristics_detect_two_cycle_chord_loops() {
        use crate::analysis::voice::SidFreq;
        // The Ocean_Loader_1 V2 stab: chip cycles 74/77/70 per frame over a
        // 6-frame note whose base is 70 (raw register values verified from
        // the trace: $267E -> 74, $2DC6 -> 77, $1E8D -> 70). The standard
        // per-note pipeline sees the full [0, 6) span, so the two-cycle loop
        // IS detected — [`split_arpeggio_patches`] only has to fix the patch
        // clustering, not the detection.
        let cycle = [0x267Eu16, 0x2DC6, 0x1E8D];
        let mut states = vec![crate::analysis::FrameState::default(); 6];
        for (f, s) in states.iter_mut().enumerate() {
            s.voices[0].freq = SidFreq(cycle[f % 3]);
        }
        let (note, _) = arp_note(0, 6, 70);
        let c = extract_characteristics(&note, &states, &[], SystemClock::Pal);
        assert_eq!(c.pitch_relative_loop, Some((vec![4, 7, 0], 0)));
        assert!(is_chord_arp(&c.pitch_relative_loop.unwrap().0));
    }

    #[test]
    fn vibrato_jitter_is_not_a_chord_arp() {
        assert!(!is_chord_arp(&[0, 1]));
        assert!(!is_chord_arp(&[0, -1, 1]));
        assert!(is_chord_arp(&[4, 7, 0]));
        assert!(is_chord_arp(&[0, 3]));
        // Too long to be a chord cycle.
        assert!(!is_chord_arp(&[0, 3, 5, 7, 9]));
    }

    #[test]
    fn splits_mixed_patches_by_chord_body() {
        // One trace cluster mixing a flat stab with two different chord
        // shapes — what the ADSR+waveform key cannot tell apart.
        let (notes, mut chars): (Vec<_>, Vec<_>) =
            (0..4u32).map(|i| arp_note(i * 10, i * 10 + 5, 70)).unzip();
        chars[1].pitch_relative_loop = Some((vec![4, 7, 0], 0));
        chars[2].pitch_relative_loop = Some((vec![4, 7, 0], 0));
        chars[3].pitch_relative_loop = Some((vec![3, 7, 0], 0));
        let (mut patches, mut assignments) = extract_patches(&notes, &chars);
        assert_eq!(patches.len(), 1);

        split_arpeggio_patches(&mut patches, &mut assignments, &chars);
        assert_eq!(patches.len(), 3);
        // Flat member keeps the loop-free base patch.
        assert_eq!(assignments[0], Some(patches[0].id));
        assert!(patches[0].voices.iter().all(|v| v.arpeggio_loop.is_none()));
        // Each chord body gets its own patch, members reassigned.
        assert_eq!(assignments[1], assignments[2]);
        assert_ne!(assignments[1], assignments[0]);
        assert_ne!(assignments[3], assignments[1]);
        let patch_of = |a: Option<PatchId>| {
            patches
                .iter()
                .find(|p| Some(p.id) == a)
                .expect("assigned patch")
        };
        assert_eq!(
            patch_of(assignments[1]).voices[0].arpeggio_loop,
            Some(vec![4, 7, 0])
        );
        assert_eq!(
            patch_of(assignments[3]).voices[0].arpeggio_loop,
            Some(vec![3, 7, 0])
        );
        assert_eq!(patch_of(assignments[1]).member_count, 2);
        assert_eq!(patch_of(assignments[3]).member_count, 1);
        assert_eq!(patches[0].member_count, 1);
    }

    #[test]
    fn pure_single_body_cluster_is_stamped_in_place() {
        let (notes, mut chars): (Vec<_>, Vec<_>) =
            (0..2u32).map(|i| arp_note(i * 10, i * 10 + 5, 70)).unzip();
        chars[0].pitch_relative_loop = Some((vec![4, 7, 0], 0));
        chars[1].pitch_relative_loop = Some((vec![4, 7, 0], 0));
        let (mut patches, mut assignments) = extract_patches(&notes, &chars);
        assert_eq!(patches.len(), 1);
        split_arpeggio_patches(&mut patches, &mut assignments, &chars);
        assert_eq!(patches.len(), 1);
        assert_eq!(patches[0].voices[0].arpeggio_loop, Some(vec![4, 7, 0]));
        assert_eq!(patches[0].member_count, 2);
    }

    #[test]
    fn classifies_the_real_handler_shapes() {
        let ram = handler_ram();
        assert_eq!(
            classify_handler(&ram, 0x9B6B),
            GalwayCmd::Return {
                stack_idx_zp: 0x1E,
                stack_lo: 0x9F7A,
                stack_hi: 0x9F8A,
            }
        );
        // Direct copy loop and a BNE head sharing it.
        assert_eq!(
            classify_handler(&ram, 0x9B7D),
            GalwayCmd::BlockCopy {
                last_src: 0x04,
                last_dst: 0x1E,
                reg_image: 0x9F47,
            }
        );
        assert_eq!(
            classify_handler(&ram, 0x9B8D),
            GalwayCmd::BlockCopy {
                last_src: 0x22,
                last_dst: 0x22,
                reg_image: 0x9F47,
            }
        );
        assert_eq!(classify_handler(&ram, 0x9BAC), GalwayCmd::Goto);
        assert_eq!(
            classify_handler(&ram, 0x9BA5),
            GalwayCmd::GotoTranspose { transpose: 0x9F9A }
        );
        assert_eq!(classify_handler(&ram, 0x9BB7), GalwayCmd::NativeCall);
        assert_eq!(
            classify_handler(&ram, 0x9BC0),
            GalwayCmd::Gosub {
                stack_idx_zp: 0x1E,
                stack_lo: 0x9F7A,
                stack_hi: 0x9F8A,
            }
        );
        assert_eq!(
            classify_handler(&ram, 0x9BD6),
            GalwayCmd::Poke { reg_image: 0x9F47 }
        );
        // The 4-byte gosub+transpose reaches the push body via BNE.
        assert_eq!(
            classify_handler(&ram, 0x9BE4),
            GalwayCmd::GosubTranspose {
                transpose: 0x9F9A,
                stack_idx_zp: 0x1E,
                stack_lo: 0x9F7A,
                stack_hi: 0x9F8A,
            }
        );
        assert_eq!(
            classify_handler(&ram, 0x96A1),
            GalwayCmd::Stop {
                mask_zp: 0x19,
                mask_value: 0x38,
            }
        );
        // V1's BIT-skip-trick pair: the 4-byte form at $98E5 and the plain
        // gosub entry hidden one byte into the BIT operand at $98EF.
        assert_eq!(
            classify_handler(&ram, 0x98E5),
            GalwayCmd::GosubTranspose {
                transpose: 0x9F46,
                stack_idx_zp: 0x1D,
                stack_lo: 0x9F26,
                stack_hi: 0x9F36,
            }
        );
        assert_eq!(
            classify_handler(&ram, 0x98EF),
            GalwayCmd::Gosub {
                stack_idx_zp: 0x1D,
                stack_lo: 0x9F26,
                stack_hi: 0x9F36,
            }
        );
        assert_eq!(classify_handler(&ram, 0x4242), GalwayCmd::Unknown);
    }

    #[test]
    fn classifies_rambo_repeat_and_indexed_stack_handlers() {
        let ram = rambo_handler_ram();
        assert_eq!(
            classify_handler(&ram, 0x217D),
            GalwayCmd::Return {
                stack_idx_zp: 0x1D,
                stack_lo: 0x2905,
                stack_hi: 0x290D,
            }
        );
        assert_eq!(
            classify_handler(&ram, 0x2195),
            GalwayCmd::LoopStart {
                stack_idx_zp: 0x1D,
                stack_lo: 0x2905,
                stack_hi: 0x290D,
                repeat_counts: 0x2915,
            }
        );
        assert_eq!(
            classify_handler(&ram, 0x21B0),
            GalwayCmd::LoopNext {
                stack_idx_zp: 0x1D,
                stack_lo: 0x2905,
                stack_hi: 0x290D,
                repeat_counts: 0x2915,
            }
        );
        assert_eq!(
            classify_handler(&ram, 0x245A),
            GalwayCmd::LoopStart {
                stack_idx_zp: 0x1E,
                stack_lo: 0x294F,
                stack_hi: 0x2957,
                repeat_counts: 0x295F,
            }
        );
        assert_eq!(
            classify_handler(&ram, 0x24B3),
            GalwayCmd::Gosub {
                stack_idx_zp: 0x1E,
                stack_lo: 0x294F,
                stack_hi: 0x2957,
            }
        );
        assert_eq!(
            classify_handler(&ram, 0x24C9),
            GalwayCmd::GosubTranspose {
                transpose: 0x2967,
                stack_idx_zp: 0x1E,
                stack_lo: 0x294F,
                stack_hi: 0x2957,
            }
        );
        assert_eq!(
            classify_handler(&ram, 0x24D3),
            GalwayCmd::Poke { reg_image: 0x291E }
        );
    }

    #[test]
    fn decodes_rambo_inline_repeat_count() {
        let mut ram = rambo_handler_ram();
        ram[0x301A] = 0x95;
        ram[0x301B] = 0x21;
        ram[0x301C] = 0xB0;
        ram[0x301D] = 0x21;
        ram[0x4000..0x4005].copy_from_slice(&[0xDA, 0x03, 0x01, 0x01, 0xDC]);
        wr16(&mut ram, 0x10, 0x4000);
        ram[0x19] = 0x01;
        ram[0x1A] = 0x01;
        ram[0x1D] = 0x07;
        ram[0x5001] = 0x01;
        ram[0x6001] = 0x12;
        ram[0x6101] = 0x01;
        let layout = GalwayLayout {
            voices: [
                GalwayVoice {
                    ptr_zp: 0x10,
                    durctr_zp: 0x1A,
                    active_bit: 0x01,
                    jump_table: 0x3000,
                    dur_table: 0x5000,
                    transpose: 0x6200,
                },
                GalwayVoice {
                    ptr_zp: 0x12,
                    durctr_zp: 0x1B,
                    active_bit: 0x02,
                    jump_table: 0x3100,
                    dur_table: 0x5100,
                    transpose: 0x6201,
                },
                GalwayVoice {
                    ptr_zp: 0x14,
                    durctr_zp: 0x1C,
                    active_bit: 0x04,
                    jump_table: 0x3200,
                    dur_table: 0x5200,
                    transpose: 0x6202,
                },
            ],
            freq_lo: 0x6000,
            freq_hi: 0x6100,
            active_mask_zp: 0x19,
            command_threshold: GalwayCommandThreshold(0xC0),
            command_mask: GalwayCommandMask(0x3F),
            fold_high_notes: true,
            has_tie: true,
        };

        let decoded = decode_song_structured(ram, &layout, SystemClock::Pal, 5);
        assert_eq!(decoded.notes.len(), 3);
        assert_eq!(
            decoded
                .notes
                .iter()
                .map(|note| note.start_frame.0)
                .collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
    }

    #[test]
    fn supplements_only_empty_native_call_voices() {
        let (native, _) = arp_note(10, 20, 60);
        let (preserved, _) = arp_note(30, 40, 62);
        let (corrected, _) = arp_note(15, 25, 64);
        let mut notes = vec![native, preserved];
        notes[0].voice = VoiceId(1);
        notes[1].voice = VoiceId(3);
        let mut instruments = vec![None, None];
        let mut truth = notes.clone();
        let mut corrected = corrected;
        corrected.voice = VoiceId(2);
        truth.push(corrected);
        truth.sort_by_key(|note| (note.voice, note.start_frame.0));

        assert_eq!(
            supplement_native_call_voices(
                &mut notes,
                &mut instruments,
                &truth,
                [true, true, false],
            ),
            TraceCorrectedNoteCount(1)
        );
        assert_eq!(notes, truth);
        assert_eq!(instruments.len(), notes.len());
    }

    /// Validate [`decode_song`] against the emulator trace for one Galway tune
    /// (default Neverending_Story): print onset agreement plus the first
    /// decoded-vs-truth notes per voice. CI-safe: no-ops without
    /// `SID_HVSC_ROOT`.
    ///
    /// Run: `SID_HVSC_ROOT=… cargo test -p sid-analyzer --lib dbg_galway_decode -- \
    /// --ignored --nocapture`.
    #[test]
    #[ignore]
    fn dbg_galway_decode() {
        let Ok(root) = std::env::var("SID_HVSC_ROOT") else {
            eprintln!("SID_HVSC_ROOT unset; skipping");
            return;
        };
        let rel = std::env::var("SID_DBG_TUNES")
            .unwrap_or_else(|_| "MUSICIANS/G/Galway_Martin/Neverending_Story.sid".into());
        let frames: u32 = std::env::var("SID_DBG_FRAMES")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(1500);
        let Ok(bytes) = std::fs::read(format!("{root}/{rel}")) else {
            eprintln!("{rel}: missing");
            return;
        };
        let header = crate::header::parse(&bytes).unwrap();
        let mut emu = Emulator::new();
        emu.load(&header, &bytes).unwrap();
        emu.call_init(header.init_address, header.start_song, header.songs)
            .unwrap();
        let ram = emu.ram_image();
        let Some(layout) = locate(&ram) else {
            eprintln!("=== {rel}: locate failed (other player generation?) ===");
            return;
        };
        let clock = crate::analysis::SystemClock::Pal;
        eprintln!("  layout: {layout:04X?}");
        eprintln!("  layout_matches: {}", layout_matches(&ram, &layout));
        for (vi, v) in layout.voices.iter().enumerate() {
            let cmds: Vec<String> = (0..30u16)
                .step_by(2)
                .map(|off| {
                    let h = rd16(&ram, v.jump_table.wrapping_add(off));
                    format!("${:02X}:{:?}", 0x80 + off, classify_handler(&ram, h))
                })
                .collect();
            eprintln!("  V{} cmds: {}", vi + 1, cmds.join(" "));
        }

        let native = decode_song(ram, &layout, clock, frames);
        let trace = match emu::run(&header, &bytes, header.start_song, frames) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("=== {rel}: trace emulation failed ({e}) ===");
                return;
            }
        };
        let states = analyze(&trace);
        let truth = detect_notes(&states, clock);
        let agreement = arp_aware_agreement(&native, &truth, &states, clock);
        eprintln!(
            "=== {rel}: onset={agreement:.3}  native={} truth={} ({frames}f) ===",
            native.len(),
            truth.len()
        );
        for vi in 1..=3u8 {
            let voice = crate::analysis::VoiceId(vi);
            let n: Vec<String> = native
                .iter()
                .filter(|n| n.voice == voice)
                .take(12)
                .map(|n| format!("{}@{}", n.midi.0, n.start_frame.0))
                .collect();
            let t: Vec<String> = truth
                .iter()
                .filter(|n| n.voice == voice)
                .take(12)
                .map(|n| format!("{}@{}", n.midi.0, n.start_frame.0))
                .collect();
            eprintln!("  V{vi} native: {}", n.join(" "));
            eprintln!("  V{vi} truth:  {}", t.join(" "));
        }
        // The first decoded notes the (arp-aware) gate counts as misses —
        // where a partially-agreeing decode starts to diverge.
        let misses: Vec<String> = native
            .iter()
            .filter(|n| !note_matches(n, &truth, &states, clock))
            .take(15)
            .map(|n| format!("V{} {}@{}", n.voice.0, n.midi.0, n.start_frame.0))
            .collect();
        if !misses.is_empty() {
            eprintln!("  first misses: {}", misses.join("  "));
        }
        // SID_DBG_AT=<frame> prints both streams in a window around a frame,
        // to inspect a divergence found via "first misses".
        if let Some(at) = std::env::var("SID_DBG_AT")
            .ok()
            .and_then(|s| s.parse::<i64>().ok())
        {
            let effects = detect_effects(&trace, &states, EffectThresholds::default());
            let win = |s: &[crate::analysis::note::NoteEvent], name: &str| {
                let xs: Vec<String> = s
                    .iter()
                    .filter(|n| (i64::from(n.start_frame.0) - at).abs() <= 60)
                    .map(|n| format!("V{} {}@{}", n.voice.0, n.midi.0, n.start_frame.0))
                    .collect();
                eprintln!("  {name} @{at}: {}", xs.join("  "));
            };
            win(&native, "native");
            win(&truth, "truth ");
            let es: Vec<String> = effects
                .iter()
                .filter(|e| {
                    i64::from(e.start_frame.0) <= at + 60 && i64::from(e.end_frame.0) >= at - 60
                })
                .map(|e| {
                    format!(
                        "{:?} V{:?} {}..{}",
                        e.effect,
                        e.voice.map(|v| v.0),
                        e.start_frame.0,
                        e.end_frame.0
                    )
                })
                .collect();
            eprintln!("  effects @{at}: {}", es.join("  "));
            let at_i = (at.max(0) as usize).min(states.len());
            for s in &states[at_i.saturating_sub(2)..(at_i + 10).min(states.len())] {
                let v: Vec<String> = s
                    .voices
                    .iter()
                    .map(|v| {
                        let m = hertz_to_midi(v.freq.to_hertz(clock))
                            .map_or("--".into(), |(m, _)| m.0.to_string());
                        format!("f={:04X} m={m} g={}", v.freq.0, u8::from(v.control.gate))
                    })
                    .collect();
                eprintln!("    s{} {}", s.frame.0, v.join(" | "));
            }
        }
        // SID_DBG_CELL=<hex addr>: re-run the emulator frame by frame and log
        // every change of that RAM cell — ground truth for a sim'd cell (e.g.
        // a voice's transpose) when the decode diverges.
        if let Some(cell) = std::env::var("SID_DBG_CELL")
            .ok()
            .and_then(|s| u16::from_str_radix(&s, 16).ok())
        {
            let mut emu2 = Emulator::new();
            emu2.load(&header, &bytes).unwrap();
            emu2.call_init(header.init_address, header.start_song, header.songs)
                .unwrap();
            let mut last = emu2.read_ram(cell);
            eprintln!("  cell ${cell:04X} init={last:02X}");
            for f in 0..frames {
                if emu2
                    .run_play_frame(header.play_address, FrameIndex(f))
                    .is_err()
                {
                    break;
                }
                let v = emu2.read_ram(cell);
                if v != last {
                    eprintln!("  cell ${cell:04X} f{f}: {last:02X} -> {v:02X}");
                    last = v;
                }
            }
        }
    }

    /// Frame-by-frame pointer trace for one Galway tune: run `play` per frame and,
    /// whenever voice 1's pattern pointer (`$10/$11`) moves, log the frame, the new
    /// pointer, the duration counter `$1A`, and the bytes at the new pointer. Pins
    /// the real event boundaries + durations the static parse can't (see
    /// `docs/drivers/galway.md`). CI-safe: no-ops without `SID_HVSC_ROOT`.
    ///
    /// Run: `SID_HVSC_ROOT=… cargo test -p sid-analyzer --lib dbg_galway_trace -- \
    /// --ignored --nocapture`.
    #[test]
    #[ignore]
    fn dbg_galway_trace() {
        let Ok(root) = std::env::var("SID_HVSC_ROOT") else {
            eprintln!("SID_HVSC_ROOT unset; skipping");
            return;
        };
        let rel = std::env::var("SID_DBG_TUNES")
            .unwrap_or_else(|_| "MUSICIANS/G/Galway_Martin/Neverending_Story.sid".into());
        let frames: u32 = std::env::var("SID_DBG_FRAMES")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(80);
        let Ok(bytes) = std::fs::read(format!("{root}/{rel}")) else {
            eprintln!("{rel}: missing");
            return;
        };
        let header = crate::header::parse(&bytes).unwrap();
        let mut emu = Emulator::new();
        emu.load(&header, &bytes).unwrap();
        emu.call_init(header.init_address, SubtuneIndex(1), header.songs)
            .unwrap();
        let ptr = |emu: &Emulator, lo: u16| {
            u16::from(emu.read_ram(lo)) | (u16::from(emu.read_ram(lo + 1)) << 8)
        };
        // (zp pointer lo, duration-counter cell) per voice.
        let voices = [(0x10u16, 0x1Au16), (0x12, 0x1B), (0x14, 0x1C)];
        let vi: usize = std::env::var("SID_DBG_VOICE")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0)
            .min(2);
        let (plo, dur) = voices[vi];
        let mut last = ptr(&emu, plo);
        eprintln!(
            "=== V{} trace ({rel})  start ptr=${:04X} dur={} ===",
            vi + 1,
            last,
            emu.read_ram(dur)
        );
        for f in 0..frames {
            emu.run_play_frame(header.play_address, FrameIndex(f))
                .unwrap();
            let p = ptr(&emu, plo);
            if p != last {
                let b: Vec<u8> = (0..6).map(|i| emu.read_ram(p.wrapping_add(i))).collect();
                let bytes_s: Vec<String> = b.iter().map(|x| format!("{x:02X}")).collect();
                eprintln!(
                    "  f{:<4} ptr ${:04X}->${:04X}  dur={:<3}  @bytes: {}",
                    f,
                    last,
                    p,
                    emu.read_ram(dur),
                    bytes_s.join(" ")
                );
                last = p;
            }
        }
    }

    /// Within-frame micro-trace (CI-safe: no-ops without `SID_HVSC_ROOT`).
    /// Single-steps each `play` call and logs *every* individual change of a
    /// voice's pattern pointer (`$10/$11`), not just the net per-frame move:
    /// the frame, the program counter of the store, the old/new pointer + delta,
    /// `Y`, and the bytes at the old pointer (the event just consumed). This
    /// resolves the setup/boundary command operand-lengths the per-frame
    /// `dbg_galway_trace` only sees in aggregate (see `docs/drivers/galway.md`).
    ///
    /// Run: `SID_HVSC_ROOT=… cargo test -p sid-analyzer --lib dbg_galway_micro -- \
    /// --ignored --nocapture`.
    #[test]
    #[ignore]
    fn dbg_galway_micro() {
        let Ok(root) = std::env::var("SID_HVSC_ROOT") else {
            eprintln!("SID_HVSC_ROOT unset; skipping");
            return;
        };
        let rel = std::env::var("SID_DBG_TUNES")
            .unwrap_or_else(|_| "MUSICIANS/G/Galway_Martin/Neverending_Story.sid".into());
        let frames: u32 = std::env::var("SID_DBG_FRAMES")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(40);
        // Voices are 0/1/2; clamp so an out-of-range `SID_DBG_VOICE` can't panic
        // the array indexing below.
        let vi: usize = std::env::var("SID_DBG_VOICE")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0)
            .min(2);
        let Ok(bytes) = std::fs::read(format!("{root}/{rel}")) else {
            eprintln!("{rel}: missing");
            return;
        };
        let header = crate::header::parse(&bytes).unwrap();
        let mut emu = Emulator::new();
        emu.load(&header, &bytes).unwrap();
        emu.call_init(header.init_address, SubtuneIndex(1), header.songs)
            .unwrap();
        // (zp pointer lo cell) per voice.
        let plo = [0x10u16, 0x12, 0x14][vi];
        let read16 = |ram: &[u8], lo: u16| -> u16 {
            u16::from(ram[lo as usize]) | (u16::from(ram[(lo + 1) as usize]) << 8)
        };
        eprintln!("=== V{} micro-trace ({rel}) ===", vi + 1);
        for f in 0..frames {
            // `last` tracks the pointer across single-steps within this frame;
            // seeded from the value at frame entry.
            let mut last = u16::from(emu.read_ram(plo)) | (u16::from(emu.read_ram(plo + 1)) << 8);
            emu.run_play_frame_stepwise(header.play_address, |cpu| {
                let p = read16(&cpu.memory.ram[..], plo);
                if p != last {
                    let consumed: Vec<String> = (0..6)
                        .map(|i| format!("{:02X}", cpu.memory.ram[last.wrapping_add(i) as usize]))
                        .collect();
                    eprintln!(
                        "  f{:<4} pc=${:04X}  ${:04X}->${:04X} (+{})  Y={:02X}  consumed@old: {}",
                        f,
                        cpu.registers.program_counter,
                        last,
                        p,
                        p.wrapping_sub(last) as i16,
                        cpu.registers.index_y,
                        consumed.join(" ")
                    );
                    last = p;
                }
            });
        }
        // Dump the per-voice duration table (reg-image + $22) after playback, to
        // see whether it is populated lazily (post-init it reads [15,0,..]).
        let regimg = [0x9EF3u16, 0x9F47, 0x9F9B][vi];
        let dur_base = regimg + 0x22;
        let durs: Vec<String> = (0..12)
            .map(|i| format!("{:02X}", emu.read_ram(dur_base + i)))
            .collect();
        eprintln!(
            "  dur-table @ ${:04X} (regimg+$22) after {frames} frames: {}",
            dur_base,
            durs.join(" ")
        );
    }

    /// Structure spike: measure pattern-pointer reuse and content divergence
    /// across the Galway_Martin corpus, to decide whether a real
    /// `resolve_placements` (recovering the authored gosub/goto arrangement) is
    /// worth building. Runs locate + decode_song with the event recorder on;
    /// the emulator trace / onset gate is skipped (irrelevant here).
    ///
    /// Each voice's 2-byte-event stream is split into pattern-pointer *runs*
    /// (contiguous `ptr += 2` spans), keyed by start address:
    ///  - **reuse** = fraction of run instances that re-enter an earlier start
    ///    address (the raw signal that authored structure exists to recover).
    ///  - **clean** = of the addresses entered ≥2×, the fraction whose every
    ///    instance has an identical `(event_byte, dur)` sequence. Transpose is
    ///    excluded — it maps to a Pertylizer placement transpose. The rest are
    ///    content/duration-divergent (runtime block-loads/pokes changed the
    ///    table), which a placement cannot express → reuse breaks there.
    ///
    /// Run: `SID_HVSC_ROOT=… cargo test -p sid-analyzer --lib dbg_galway_structure_spike \
    /// -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn dbg_galway_structure_spike() {
        use std::collections::HashMap;

        // A run's content signature: its `(event_byte, dur)` sequence.
        type Sig = Vec<(u8, u8)>;

        let Ok(root) = std::env::var("SID_HVSC_ROOT") else {
            eprintln!("SID_HVSC_ROOT unset; skipping");
            return;
        };
        let frames: u32 = std::env::var("SID_DBG_FRAMES")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(6000);
        let dir = format!("{root}/MUSICIANS/G/Galway_Martin");
        let mut paths: Vec<std::path::PathBuf> = std::fs::read_dir(&dir)
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.path())
                    .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("sid"))
                    .collect()
            })
            .unwrap_or_default();
        paths.sort();
        eprintln!("Galway_Martin: {} tunes, {frames}f\n", paths.len());

        let mut tot_runs = 0usize;
        let mut tot_repeat = 0usize;
        let mut tot_recurring = 0usize;
        let mut tot_clean = 0usize;
        let mut tot_div = 0usize;
        let mut decoded = 0usize;
        let mut located = 0usize;

        for path in &paths {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("?");
            let Ok(bytes) = std::fs::read(path) else {
                continue;
            };
            let Ok(header) = crate::header::parse(&bytes) else {
                continue;
            };
            let mut emu = Emulator::new();
            if emu.load(&header, &bytes).is_err()
                || emu
                    .call_init(header.init_address, header.start_song, header.songs)
                    .is_err()
            {
                continue;
            }
            let ram = emu.ram_image();
            let Some(layout) = locate(&ram) else {
                eprintln!("{name:34} locate-fail");
                continue;
            };
            if !layout_matches(&ram, &layout) {
                eprintln!("{name:34} locate-fail (mismatch)");
                continue;
            }
            located += 1;

            EVENT_REC.with(|r| *r.borrow_mut() = Some(Vec::new()));
            let _ = decode_song(ram, &layout, SystemClock::Pal, frames);
            let log = EVENT_REC.with(|r| r.borrow_mut().take().unwrap_or_default());
            if log.is_empty() {
                eprintln!("{name:34} no events");
                continue;
            }
            decoded += 1;

            // start address -> the content signatures of every run starting there
            let mut runs: HashMap<u16, Vec<Sig>> = HashMap::new();
            for v in 0..3u8 {
                let vlog: Vec<&GalwayRecEvent> = log.iter().filter(|e| e.0 == v).collect();
                let mut i = 0;
                while i < vlog.len() {
                    let start = vlog[i].1;
                    let mut prev = start;
                    let mut sig = vec![(vlog[i].2, vlog[i].3)];
                    i += 1;
                    while i < vlog.len() && vlog[i].1 == prev.wrapping_add(2) {
                        prev = vlog[i].1;
                        sig.push((vlog[i].2, vlog[i].3));
                        i += 1;
                    }
                    runs.entry(start).or_default().push(sig);
                }
            }

            let n_runs: usize = runs.values().map(Vec::len).sum();
            let n_addrs = runs.len();
            let n_repeat = n_runs - n_addrs;
            let recurring: Vec<&Vec<Sig>> = runs.values().filter(|v| v.len() >= 2).collect();
            let n_recurring = recurring.len();
            let n_clean = recurring
                .iter()
                .filter(|insts| insts.iter().all(|s| *s == insts[0]))
                .count();
            let n_div = n_recurring - n_clean;

            tot_runs += n_runs;
            tot_repeat += n_repeat;
            tot_recurring += n_recurring;
            tot_clean += n_clean;
            tot_div += n_div;

            let reuse = 100.0 * n_repeat as f64 / n_runs as f64;
            let clean = if n_recurring > 0 {
                100.0 * n_clean as f64 / n_recurring as f64
            } else {
                0.0
            };
            eprintln!(
                "{name:34} runs={n_runs:4} addrs={n_addrs:3} reuse={reuse:3.0}%  recurring={n_recurring:3} clean={n_clean:3} div={n_div:3} ({clean:3.0}% clean)"
            );
        }

        let reuse = if tot_runs > 0 {
            100.0 * tot_repeat as f64 / tot_runs as f64
        } else {
            0.0
        };
        let clean = if tot_recurring > 0 {
            100.0 * tot_clean as f64 / tot_recurring as f64
        } else {
            0.0
        };
        eprintln!("\n=== Galway structure spike ===");
        eprintln!("located={located}/{} decoded={decoded}", paths.len());
        eprintln!("runs={tot_runs} repeats={tot_repeat} ({reuse:.0}% reuse)");
        eprintln!(
            "recurring addrs={tot_recurring} clean={tot_clean} divergent={tot_div} ({clean:.0}% clean)"
        );
        eprintln!(
            "\nverdict: high reuse + high clean => authored resolve_placements is worth building;\nlow clean => runtime table-swaps make the reuse inexpressible as placements."
        );
    }

    /// HVSC-wide sweep for the Galway engine (CI-safe: no-ops without
    /// `SID_HVSC_ROOT`). Unlike the Hubbard sweep this does NOT pre-filter on
    /// the playerid signature: the raw file bytes are pre-scanned for at least
    /// three `AND #$7F / TAX` dispatch heads (the sequencer code ships in the
    /// file as loaded — init only relocates *data*), so engine instances that
    /// sidid's signature misses are still found. Candidates then run the real
    /// pipeline (init → locate → decode → arp-aware gate) under a per-tune
    /// deadline, and every located tune is printed with its playerid name,
    /// onset agreement and path.
    ///
    /// Run: `SID_HVSC_ROOT=… cargo test -p sid-analyzer --lib dbg_galway_hvsc_sweep \
    /// -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn dbg_galway_hvsc_sweep() {
        use rayon::prelude::*;

        let Ok(root) = std::env::var("SID_HVSC_ROOT") else {
            eprintln!("SID_HVSC_ROOT unset; skipping");
            return;
        };
        let frames: u32 = std::env::var("SID_DBG_FRAMES")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(400);

        let mut paths = Vec::new();
        let mut stack = vec![std::path::PathBuf::from(&root)];
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
        eprintln!("scanning {} files…", paths.len());

        enum Cat {
            EmuFail(String, String),
            LocateFail(String, String),
            Timeout(String, String),
            Decoded(f64, usize, usize, String, String),
        }

        fn analyze_one(bytes: Vec<u8>, rel: String, driver: String, frames: u32) -> Cat {
            let Ok(header) = crate::header::parse(&bytes) else {
                return Cat::EmuFail(rel, driver);
            };
            let sub = header.start_song;
            let mut img = Emulator::new();
            if img.load(&header, &bytes).is_err()
                || img
                    .call_init(header.init_address, sub, header.songs)
                    .is_err()
            {
                return Cat::EmuFail(rel, driver);
            }
            let ram = img.ram_image();
            let Some(layout) = locate(&ram) else {
                return Cat::LocateFail(rel, driver);
            };
            if !layout_matches(&ram, &layout) {
                return Cat::LocateFail(rel, driver);
            }
            let native = decode_song(ram, &layout, SystemClock::Pal, frames);
            let Ok(trace) = emu::run(&header, &bytes, sub, frames) else {
                return Cat::EmuFail(rel, driver);
            };
            let states = analyze(&trace);
            let truth = detect_notes(&states, SystemClock::Pal);
            let onset = arp_aware_agreement(&native, &truth, &states, SystemClock::Pal);
            Cat::Decoded(onset, native.len(), truth.len(), rel, driver)
        }

        let deadline = std::time::Duration::from_secs(5);
        let dispatch_head: &[u8] = &[0x29, 0x7F, 0xAA];

        let outcomes: Vec<Cat> = paths
            .par_iter()
            .filter_map(|path| {
                let bytes = std::fs::read(path).ok()?;
                // Cheap raw-bytes prescan: three sequencers => at least three
                // dispatch heads somewhere in the load image.
                let heads = bytes.windows(3).filter(|w| *w == dispatch_head).count();
                if heads < 3 {
                    return None;
                }
                let rel = path
                    .strip_prefix(&root)
                    .unwrap_or(path)
                    .to_string_lossy()
                    .into_owned();
                let driver = crate::playerid::PlayerDb::embedded()
                    .identify(&bytes)
                    .unwrap_or("?")
                    .to_string();
                let (tx, rx) = std::sync::mpsc::channel();
                {
                    let rel = rel.clone();
                    let driver = driver.clone();
                    std::thread::spawn(move || {
                        let _ = tx.send(analyze_one(bytes, rel, driver, frames));
                    });
                }
                Some(
                    rx.recv_timeout(deadline)
                        .unwrap_or(Cat::Timeout(rel, driver)),
                )
            })
            .collect();

        let mut pass = 0;
        let mut rows: Vec<String> = Vec::new();
        let (mut emu_fail, mut locate_fail, mut timeout, mut decoded) = (0, 0, 0, 0);
        for c in &outcomes {
            match c {
                Cat::EmuFail(rel, drv) => {
                    emu_fail += 1;
                    rows.push(format!("  emu_fail              {drv:24} {rel}"));
                }
                Cat::LocateFail(rel, drv) => {
                    locate_fail += 1;
                    rows.push(format!("  locate_fail           {drv:24} {rel}"));
                }
                Cat::Timeout(rel, drv) => {
                    timeout += 1;
                    rows.push(format!("  timeout               {drv:24} {rel}"));
                }
                Cat::Decoded(onset, nn, nt, rel, drv) => {
                    decoded += 1;
                    if *onset >= MIN_AGREEMENT {
                        pass += 1;
                    }
                    rows.push(format!("  o{onset:.3} {nn:5}/{nt:<5}  {drv:24} {rel}"));
                }
            }
        }
        rows.sort();
        eprintln!(
            "\n=== HVSC Galway-engine sweep (start_song, {frames}f) ===\n\
             candidates={} emu_fail={emu_fail} locate_fail={locate_fail} \
             timeout={timeout} decoded={decoded} PASS={pass}\n",
            outcomes.len()
        );
        for r in rows {
            eprintln!("{r}");
        }
    }

    /// Triage tool (CI-safe: no-ops without `SID_HVSC_ROOT`). For each tune in
    /// `SID_DBG_TUNES` (comma-separated paths relative to the HVSC root; defaults
    /// to a clean single-subtune PSID set), emulate past `init`, print the header
    /// addresses, and write the full post-`init` RAM image to `/tmp/galway_<name>.bin`
    /// for disassembly with `/tmp/dis6502.py`.
    ///
    /// Run: `SID_HVSC_ROOT=… SID_DBG_TUNES=MUSICIANS/G/Galway_Martin/Neverending_Story.sid \
    /// cargo test -p sid-analyzer --lib dbg_galway -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn dbg_galway() {
        let Ok(root) = std::env::var("SID_HVSC_ROOT") else {
            eprintln!("SID_HVSC_ROOT unset; skipping");
            return;
        };
        let default_tunes = "MUSICIANS/G/Galway_Martin/Neverending_Story.sid,\
             MUSICIANS/G/Galway_Martin/Highlander.sid,\
             MUSICIANS/G/Galway_Martin/Kong_Strikes_Back.sid"
            .to_string();
        let tunes_var = std::env::var("SID_DBG_TUNES").unwrap_or(default_tunes);
        for rel in tunes_var.split(',').map(str::trim) {
            let path = format!("{root}/{rel}");
            let Ok(bytes) = std::fs::read(&path) else {
                eprintln!("{rel}: missing");
                continue;
            };
            let Ok(header) = crate::header::parse(&bytes) else {
                eprintln!("{rel}: bad header");
                continue;
            };
            let mut emu = Emulator::new();
            if emu.load(&header, &bytes).is_err()
                || emu
                    .call_init(header.init_address, SubtuneIndex(1), header.songs)
                    .is_err()
            {
                eprintln!("{rel}: emulation failed");
                continue;
            }
            let name = rel.rsplit('/').next().unwrap_or(rel);
            eprintln!(
                "=== {name}  load=${:04X} init=${:04X} play=${:04X} songs={} speed=${:08X} clock={:?} ===",
                header.load_address.0,
                header.init_address.0,
                header.play_address.0,
                header.songs.0,
                header.speed.0,
                header.flags.clock,
            );
            let ram = emu.ram_image();
            let out = format!("/tmp/galway_{name}.bin");
            if std::fs::write(&out, &ram).is_ok() {
                eprintln!("  wrote {out}");
            }
        }
    }
}
