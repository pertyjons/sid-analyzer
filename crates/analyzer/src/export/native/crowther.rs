//! Native song-data extractor for the Antony Crowther V3 playroutine.
//!
//! This driver powers most of Ben Daglish's catalogue alongside Antony
//! Crowther's own — `Antony_Crowther_V3` identifies **88 tunes HVSC-wide**
//! (hubbard-class volume). The format is fully mapped in
//! `docs/drivers/crowther.md`; the short version:
//!
//! Three per-voice byte streams of **uniform 2-byte records**: zero or more
//! command pairs `[cmd][value]`, then one row pair `[note][dur]`. Note
//! bytes: `$00` rest, `< $80` tie, `>= $80` pitched (top-octave freq table,
//! folded down by halving). A porta-prefix duration marks a slide-target
//! pair (the *next* pair is the sounding row); duration `$00` is a gate-off
//! row. Structure comes from two nesting levels of repeat loops (count =
//! total plays, `DEC $00 -> $FF` wrap).
//!
//! Two sequencer **generations** share that grammar (see [`Dispatch`]):
//! the Chain one (Ark_Pandora: `CPY`-chain dispatch, commands `$01..$7E`,
//! prefix `$63`, divider-gated 8-bit durations, song end when voice 0
//! reaches voice 1's stream) and the Table one (Cobra: jump-table dispatch,
//! commands `$01..$21`, prefix `$FF`, frame-counted 16-bit durations, an
//! explicit stop command, additive transpose/duration commands). Table
//! command semantics are **classified from the handler code**, galway-style,
//! so reordered command tables still decode.
//!
//! [`decode_song`] is a faithful frame-synchronous simulation of the player's
//! own loop: each frame first drains command pairs for all three voices (the
//! real player's drain runs every frame, so commands land mid-row), then
//! ticks the global speed divider (Chain), and on a sequencer tick steps
//! each voice's duration counter, fetching the next row on expiry. The
//! stream data is read-only; the Chain generation's self-modified divider
//! immediate is modelled directly.
//!
//! The same pass recovers render structure from the stream's actual control
//! flow. Loop bodies, loop exits, and transpose boundaries delimit linear
//! sections; revisiting a section address emits another placement of the same
//! pattern. Crowther has no separate orderlist, so this preserves its native
//! composition mechanism without similarity-based phrase detection.
//!
//! [`locate`] keys on relocation-independent instruction shapes — the shared
//! note->freq octave fold plus one generation's drain head — demanding
//! exactly one of each, and reads every cell address out of the matched
//! operands.

use super::{
    DriverExtractor, FieldProvenance, NativeContext, NativeError, NativeSong,
    NativeValidationPolicy, ProvenanceEvidence, note_from_raw_freq, validate_native_notes,
};
#[cfg(all(test, feature = "asset-tests"))]
use super::{MIN_AGREEMENT, onset_agreement};
use crate::analysis::effects::{EffectThresholds, detect_effects};
use crate::analysis::note::{NoteEvent, detect_notes};
use crate::analysis::timbre::{
    apply_voice3_lfo_detection, extract_characteristics, extract_patches, extract_patches_grouped,
};
use crate::analysis::voice::Adsr;
use crate::analysis::{SystemClock, VoiceId, analyze};
use crate::emu::{self, Emulator};
use crate::export::{
    FrequencyTableIndex, NativeDriverOpcode, NativeEffectByte, NativePlacement, NativeRowTick,
    OrderOffset, PatternByteOffset, PatternDuration, PatternNumber, PatternTranspose,
    RecoveredOrderCommand, RecoveredPatternEvent, RecoveredPatternInstance, RecoveredStructure,
    RecoveredVoiceStructure, RepeatOrdinal, VoicePlacements,
};
use crate::trace::FrameIndex;

/// Addresses of the Crowther V3 player's data cells, read out of the matched
/// instruction operands (raw `u16`, relocation-dependent — the same convention
/// as the Hubbard and Galway layouts).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CrowtherLayout {
    /// Per-voice stream-pointer lo bytes (3 entries; Ark_Pandora `$A586`,
    /// Cobra `$FF67`).
    pub seq_lo: u16,
    /// Per-voice stream-pointer hi bytes (3 entries; `$A589` / `$FF64`).
    pub seq_hi: u16,
    /// Top-octave frequency table: 12 interleaved pairs, **hi byte first**
    /// (`$A5C2` / `$FF75`). `freq = pair[note] >> (7 - octave_folds)`.
    pub freq_table: u16,
    /// Address of the outer speed divider's `CMP` immediate (`$A07F`),
    /// self-modified by command `$0E`. `None` when the shape was not found —
    /// the divider then defaults to 1 (the Table generation has no divider:
    /// its 16-bit durations count play frames directly).
    pub divider_imm: Option<u16>,
    /// Which sequencer generation the player runs (command dispatch model).
    pub dispatch: Dispatch,
}

/// The two known sequencer generations behind the `Antony_Crowther_V3`
/// signature. They share the stream grammar (2-byte records, the same note
/// encoding, the same loop mechanics) but differ in scaffolding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Dispatch {
    /// `CPY`-chain dispatch with hardwired command numbers; commands
    /// `$01..$7E`, porta prefix dur `$63`, 8-bit durations gated by the
    /// global divider, song end when voice 0 reaches voice 1's stream
    /// (the Ark_Pandora generation).
    Chain,
    /// Jump-table dispatch (one word per command); commands `$01..=cmd_max`
    /// (Cobra: `$21`), porta prefix dur `$FF`, 16-bit frame-counted
    /// durations, an explicit stop command instead of the positional end
    /// (the Cobra generation). Command semantics are classified from the
    /// handler code, galway-style, so reordered tables still decode.
    Table(TableCmds),
}

/// Classified command semantics for the Table generation, indexed by
/// `cmd - 1`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TableCmds {
    kinds: [CmdKind; TABLE_CMD_MAX as usize],
    /// Highest valid command byte (the drain's `CMP #imm` bound).
    cmd_max: u8,
    /// The porta-prefix duration marker, read from the dur-fetch `CMP`
    /// immediate — `$63` in the early table revisions, `$FF` later.
    porta_prefix: u8,
    /// Whether the duration countdown is 16-bit (the late revision; a dur-0
    /// row then holds 65536 ticks instead of 256).
    dur16: bool,
    /// Whether rows are fetched when the countdown reaches 1 instead of 0
    /// (the 8-bit revisions): a dur byte of `$01` then wraps the countdown —
    /// 256 frames — and `$00` gives 255.
    fetch_at_one: bool,
    /// Whether a stop command was classified. Without one (early table
    /// revisions) the song still ends positionally, voice 0 reaching
    /// voice 1's stream.
    has_stop: bool,
    /// The per-voice ctrl/waveform cell (X-indexed, 0..2), when located —
    /// seeds each voice's initial ctrl for the gate check.
    ctrl_cell: Option<u16>,
}

impl TableCmds {
    fn kind(&self, cmd: u8) -> CmdKind {
        if cmd == 0 || cmd > self.cmd_max {
            return CmdKind::Other;
        }
        self.kinds[usize::from(cmd - 1)]
    }
}

/// What a Table-generation command handler does to the *sequencer* (all
/// other handlers only touch timbre/effect state).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum CmdKind {
    #[default]
    Other,
    /// Sets the per-voice SID control/waveform byte staged at note-on. A
    /// value with the gate bit clear (Gauntlet/Firelord lead-ins use `$00`)
    /// sequences the rows *silently* — no audible onsets until a later set.
    CtrlSet,
    /// Sets the per-voice transpose added to the note byte before the fold.
    Transpose,
    /// *Adds* to the transpose (Cobra cmd `$1A` — descending key-change
    /// sections accumulate it per loop pass).
    TransposeAdd,
    /// Adds `value * 256` to the current row's 16-bit duration countdown
    /// (Cobra cmd `$19` — how rows longer than 255 frames are authored).
    DurHiAdd,
    /// Repeat-loop end (level 0/1): arm with the count, jump back while > 0.
    LoopEnd(u8),
    /// Repeat-loop start (level 0/1): remember the return position.
    LoopStart(u8),
    /// Stops the music (clears the play flag).
    Stop,
}

/// Largest command-table size accepted (Cobra uses `$21` = 33 commands).
const TABLE_CMD_MAX: u8 = 0x40;

/// One match of the note->freq octave fold (see [`locate`]):
/// `CMP #$8C / BCC +6 / SBC #$0C / DEX / JMP self / [STY ??] / SBC #$7F /
/// ASL / TAY / LDA hi,Y / STA ?? / LDA lo,Y` with `lo == hi + 1`. The `STY`
/// is generation-dependent (Ark_Pandora has it, Cobra does not).
fn match_octave_fold(ram: &[u8], a: usize) -> Option<u16> {
    let b = &ram[a..a + 29];
    let head_ok = b[0] == 0xC9
        && b[1] == 0x8C
        && b[2] == 0x90
        && b[3] == 0x06
        && b[4] == 0xE9
        && b[5] == 0x0C
        && b[6] == 0xCA
        && b[7] == 0x4C;
    if !head_ok {
        return None;
    }
    // The JMP loops back to the CMP itself.
    let jmp = u16::from_le_bytes([b[8], b[9]]);
    if usize::from(jmp) != a {
        return None;
    }
    // Optional `STY abs` before the `SBC #$7F`.
    let t = if b[10] == 0x8C { &b[13..] } else { &b[10..] };
    let tail_ok = t[0] == 0xE9
        && t[1] == 0x7F
        && t[2] == 0x0A
        && t[3] == 0xA8
        && t[4] == 0xB9
        && t[7] == 0x8D
        && t[10] == 0xB9;
    if !tail_ok {
        return None;
    }
    let hi = u16::from_le_bytes([t[5], t[6]]);
    let lo = u16::from_le_bytes([t[11], t[12]]);
    (lo == hi.wrapping_add(1)).then_some(hi)
}

/// One match of the command-drain head (see [`locate`]):
/// `STX ?? / LDY #0 / LDA seq_lo,X / STA zp / LDA seq_hi,X / STA zp+1 /
/// LDA (zp),Y / BEQ / CMP #$7F / BCS`.
fn match_drain_head(ram: &[u8], a: usize) -> Option<(u16, u16)> {
    let b = &ram[a..a + 21];
    let shape_ok = b[0] == 0x8E
        && b[3] == 0xA0
        && b[4] == 0x00
        && b[5] == 0xBD
        && b[8] == 0x85
        && b[10] == 0xBD
        && b[13] == 0x85
        && b[15] == 0xB1
        && b[17] == 0xF0
        && b[19] == 0xC9
        && b[20] == 0x7F;
    if !shape_ok {
        return None;
    }
    let zp = b[9];
    if b[14] != zp.wrapping_add(1) || b[16] != zp {
        return None;
    }
    let seq_lo = u16::from_le_bytes([b[6], b[7]]);
    let seq_hi = u16::from_le_bytes([b[11], b[12]]);
    Some((seq_lo, seq_hi))
}

/// One match of the outer speed divider:
/// `INC ctr / LDA ctr / CMP #imm / BCC` with both `ctr` operands equal.
/// Returns the address of the immediate.
fn match_divider(ram: &[u8], a: usize) -> Option<u16> {
    let b = &ram[a..a + 10];
    let shape_ok = b[0] == 0xEE && b[3] == 0xAD && b[6] == 0xC9 && b[8] == 0x90;
    if !shape_ok {
        return None;
    }
    (b[1] == b[4] && b[2] == b[5]).then(|| (a + 7) as u16)
}

/// One match of the Table-generation drain head (Cobra `$F9B6`):
/// `LDA seq_lo,X / STA zp / LDA seq_hi,X / STA zp+1 / LDY #0 / LDA (zp),Y /
/// SEC / SBC #$01 / CMP #cmd_max / BCS`. Returns
/// `(seq_lo, seq_hi, cmd_max, dispatch_addr)` where `dispatch_addr` is the
/// byte after the `BCS` operand (the `ASL/TAY/jump-table` tail).
fn match_table_drain(ram: &[u8], a: usize) -> Option<(u16, u16, u8, usize)> {
    let b = &ram[a..a + 21];
    let shape_ok = b[0] == 0xBD
        && b[3] == 0x85
        && b[5] == 0xBD
        && b[8] == 0x85
        && b[10] == 0xA0
        && b[11] == 0x00
        && b[12] == 0xB1
        && b[14] == 0x38
        && b[15] == 0xE9
        && b[16] == 0x01
        && b[17] == 0xC9
        && b[19] == 0xB0;
    if !shape_ok {
        return None;
    }
    let zp = b[4];
    if b[9] != zp.wrapping_add(1) || b[13] != zp {
        return None;
    }
    let seq_lo = u16::from_le_bytes([b[1], b[2]]);
    let seq_hi = u16::from_le_bytes([b[6], b[7]]);
    let cmd_max = b[18];
    if cmd_max == 0 || cmd_max > TABLE_CMD_MAX {
        return None;
    }
    Some((seq_lo, seq_hi, cmd_max, a + 21))
}

/// The dispatch tail right after the Table drain head (Cobra `$F9CB`):
/// `ASL / TAY / LDA tbl,Y / STA jsr+1 / LDA tbl+1,Y / STA jsr+2`.
/// Returns the command word-table base.
fn match_dispatch_tail(ram: &[u8], a: usize) -> Option<u16> {
    let b = ram.get(a..a + 14)?;
    let shape_ok = b[0] == 0x0A
        && b[1] == 0xA8
        && b[2] == 0xB9
        && b[5] == 0x8D
        && b[8] == 0xB9
        && b[11] == 0x8D;
    if !shape_ok {
        return None;
    }
    let lo = u16::from_le_bytes([b[3], b[4]]);
    let hi = u16::from_le_bytes([b[9], b[10]]);
    (hi == lo.wrapping_add(1)).then_some(lo)
}

/// The Table-generation note path: `LDA (zp),Y`, a zero-branch of either
/// polarity (the rest handling is inline in some revisions, a branch in
/// others), then within a short window `CMP #$80 / BCC ?? / CLC /
/// ADC transpose,X`. Returns the per-voice transpose cell the note byte is
/// offset by — the cross-confirmation target for the Transpose handler.
fn match_table_note_path(ram: &[u8], a: usize, zp: u8) -> Option<u16> {
    let b = ram.get(a..a + 24)?;
    if b[0] != 0xB1 || b[1] != zp {
        return None;
    }
    for o in 2..=16 {
        if b[o] == 0xC9
            && b[o + 1] == 0x80
            && b[o + 2] == 0x90
            && b[o + 4] == 0x18
            && b[o + 5] == 0x7D
        {
            return Some(u16::from_le_bytes([b[o + 6], b[o + 7]]));
        }
    }
    None
}

/// The Table-generation duration fetch: `LDA (zp),Y / LDY ?? / STA dur,X /
/// JSR ptr+=2 / LDA dur,X / CMP #marker / BNE`. Yields the porta-prefix
/// duration marker — `$63` in early table revisions, `$FF` later — straight
/// from the comparison immediate, plus the duration-lo cell.
fn match_dur_fetch(ram: &[u8], a: usize, zp: u8) -> Option<(u8, u16)> {
    let b = ram.get(a..a + 17)?;
    let shape_ok = b[0] == 0xB1
        && b[1] == zp
        && b[2] == 0xA4
        && b[4] == 0x9D
        && b[7] == 0x20
        && b[10] == 0xBD
        && b[13] == 0xC9
        && b[15] == 0xD0;
    if !shape_ok || b[5..7] != b[11..13] {
        return None;
    }
    Some((b[14], u16::from_le_bytes([b[5], b[6]])))
}

/// The split-indexed revisions' duration **hi** borrow (Rolling_Stoned
/// `$67A5`): `LDA hi,Y / SEC / SBC #$01 / STA hi,Y / CMP #$FF / BNE` —
/// the lo byte is X-indexed but the hi byte Y-indexed. Yields the hi cell
/// (the [`CmdKind::DurHiAdd`] cross-confirmation target in those builds).
fn match_durhi_borrow(ram: &[u8], a: usize) -> Option<u16> {
    let b = ram.get(a..a + 12)?;
    let shape_ok = b[0] == 0xB9
        && b[3] == 0x38
        && b[4] == 0xE9
        && b[5] == 0x01
        && b[6] == 0x99
        && b[9] == 0xC9
        && b[10] == 0xFF
        && b[11] == 0xD0;
    if !shape_ok || b[1..3] != b[7..9] {
        return None;
    }
    Some(u16::from_le_bytes([b[1], b[2]]))
}

/// The per-voice control/waveform cell: the register-image blast
/// (`LDA image,X / STA $D400,X`) names the image base, and the note-on
/// staging (`LDA ctrl,X / STA image+4,Y`) names the cell the `CtrlSet`
/// command writes. Both exactly-one; `None` disables ctrl gating (decode
/// then emits every gated row, the pre-Gauntlet behaviour).
fn locate_ctrl_cell(ram: &[u8]) -> Option<u16> {
    let mut image = None;
    for a in 0x0200..ram.len() - 8 {
        let b = &ram[a..a + 6];
        if b[0] == 0xBD && b[3] == 0x9D && b[4] == 0x00 && b[5] == 0xD4 {
            let base = u16::from_le_bytes([b[1], b[2]]);
            if image.replace(base).is_some() {
                return None;
            }
        }
    }
    let gate = image?.wrapping_add(4).to_le_bytes();
    let mut ctrl = None;
    for a in 0x0200..ram.len() - 8 {
        let b = &ram[a..a + 6];
        if b[0] == 0xBD && b[3] == 0x99 && b[4] == gate[0] && b[5] == gate[1] {
            let cell = u16::from_le_bytes([b[1], b[2]]);
            if ctrl.replace(cell).is_some() {
                return None;
            }
        }
    }
    ctrl
}

/// Whether the 8-bit revisions fetch the next row when the countdown
/// reaches **1** (`DEC dur,X / LDA dur,X / CMP #$01 / BNE`) instead of 0
/// (Cobra's 16-bit check). Under fetch-at-one a duration byte of `$01`
/// wraps the countdown past the trigger — 256 frames — and `$00` gives 255.
fn fetch_at_one(ram: &[u8], durlo: u16) -> bool {
    let pat = durlo.to_le_bytes();
    ram.windows(9).any(|w| {
        w[0] == 0xDE
            && w[1..3] == pat
            && w[3] == 0xBD
            && w[4..6] == pat
            && w[6] == 0xC9
            && w[7] == 0x01
            && w[8] == 0xD0
    })
}

/// The Table-generation 16-bit duration countdown (Cobra `$FA37`):
/// `LDA lo,X / SEC / SBC #$01 / STA lo,X / LDA hi / SBC #$00 / STA hi /
/// BNE`. The hi half is X-indexed in some builds and Y-indexed in others
/// (Chicken_Song keeps lo per voice-index, hi per SID-offset). Returns the
/// duration **hi** cell — the cross-confirmation target for the
/// [`CmdKind::DurHiAdd`] handler.
fn match_table_durctr(ram: &[u8], a: usize) -> Option<u16> {
    let b = ram.get(a..a + 22)?;
    let hi_indexed = matches!((b[9], b[14]), (0xBD, 0x9D) | (0xB9, 0x99));
    let shape_ok = b[0] == 0xBD
        && b[3] == 0x38
        && b[4] == 0xE9
        && b[5] == 0x01
        && b[6] == 0x9D
        && hi_indexed
        && b[12] == 0xE9
        && b[13] == 0x00
        && b[17] == 0xD0;
    if !shape_ok {
        return None;
    }
    if b[1..3] != b[7..9] || b[10..12] != b[15..17] {
        return None;
    }
    Some(u16::from_le_bytes([b[10], b[11]]))
}

/// Classify one Table-generation command handler by its code shape. The
/// loop handlers return their pointer-save table addresses so ends can be
/// paired with starts.
enum HandlerShape {
    Other,
    Stop,
    /// `STA cell,X / RTS` — semantic depends on which cell (transpose?).
    SingleStore(u16),
    /// `CLC / ADC cell,X / STA cell,X / RTS` — accumulate into a cell
    /// (transpose add, duration-hi add).
    AddStore(u16),
    /// `LDA zp / STA ret,X / LDA zp+1 / STA ret2,X / RTS`.
    LoopStart(u16, u16),
    /// The arm/count/jump-back shape; carries the restore-table addresses.
    LoopEnd(u16, u16),
}

fn classify_handler(ram: &[u8], h: u16, zp: u8) -> HandlerShape {
    let Some(b) = ram.get(usize::from(h)..usize::from(h).saturating_add(40)) else {
        return HandlerShape::Other;
    };
    // Stop: `LDA #$00 / STA flag` at entry (Cobra $FB88 — falls into the
    // register-image reset).
    if b[0] == 0xA9 && b[1] == 0x00 && b[2] == 0x8D {
        return HandlerShape::Stop;
    }
    // Single absolute,X store: `STA cell,X / RTS`.
    if b[0] == 0x9D && b[3] == 0x60 {
        return HandlerShape::SingleStore(u16::from_le_bytes([b[1], b[2]]));
    }
    // Accumulating store: `CLC / ADC cell,X / STA cell,X / RTS`.
    if b[0] == 0x18 && b[1] == 0x7D && b[4] == 0x9D && b[2..4] == b[5..7] && b[7] == 0x60 {
        return HandlerShape::AddStore(u16::from_le_bytes([b[2], b[3]]));
    }
    // Y-indexed accumulating store, optionally mirrored into a second cell:
    // `CLC / ADC cell,Y / STA cell,Y / [STA ??,X] / RTS` (the split-indexed
    // revisions keep the duration hi byte Y-indexed).
    if b[0] == 0x18
        && b[1] == 0x79
        && b[4] == 0x99
        && b[2..4] == b[5..7]
        && (b[7] == 0x60 || (b[7] == 0x9D && b[10] == 0x60))
    {
        return HandlerShape::AddStore(u16::from_le_bytes([b[2], b[3]]));
    }
    // Loop start: `LDA zp / STA ret,X / LDA zp+1 / STA ret2,X / RTS`
    // (saves the marker position; the drain's +2 lands on the next record).
    if b[0] == 0xA5
        && b[1] == zp
        && b[2] == 0x9D
        && b[5] == 0xA5
        && b[6] == zp.wrapping_add(1)
        && b[7] == 0x9D
        && b[10] == 0x60
    {
        return HandlerShape::LoopStart(
            u16::from_le_bytes([b[3], b[4]]),
            u16::from_le_bytes([b[8], b[9]]),
        );
    }
    // Loop end: `STA save / LDA flag,X / BNE skip-arm / <arm> / LDA save /
    // STA count,X / DEC count,X / LDA count,X / BEQ done / LDA ret,X /
    // STA zp / LDA ret2,X / STA zp+1 / RTS`. The arm encoding varies by
    // revision: `INC flag,X` (Cobra) or `LDA #1 / STA flag,X` (Biggles).
    if b[0] == 0x8D && b[3] == 0xBD && b[6] == 0xD0 {
        let after_arm = match (b[8], b[10]) {
            (0xFE, _) if b[9..11] == b[4..6] => Some(11),
            (0xA9, 0x9D) if b[9] == 0x01 && b[11..13] == b[4..6] => Some(13),
            _ => None,
        };
        if let Some(p) = after_arm {
            let t = &b[p..];
            let shape_ok = t[0] == 0xAD
                && t[3] == 0x9D
                && t[6] == 0xDE
                && t[9] == 0xBD
                && t[12] == 0xF0
                && t[14] == 0xBD
                && t[17] == 0x85
                && t[18] == zp
                && t[19] == 0xBD
                && t[22] == 0x85
                && t[23] == zp.wrapping_add(1)
                && t[24] == 0x60;
            let operands_ok = t[1..3] == b[1..3] // save
                && t[4..6] == t[7..9] // count
                && t[7..9] == t[10..12];
            if shape_ok && operands_ok {
                return HandlerShape::LoopEnd(
                    u16::from_le_bytes([t[15], t[16]]),
                    u16::from_le_bytes([t[20], t[21]]),
                );
            }
        }
    }
    HandlerShape::Other
}

/// The cross-confirmation cells and revision facts the Table anchors
/// recovered, handed to the handler classification.
struct TableAnchors {
    /// Stream-pointer zero-page pair (lo; hi at +1).
    zp: u8,
    /// The note path's transpose cell (`ADC cell,X` operand).
    transpose_cell: u16,
    /// The duration countdown's hi cell, when the build has one (either the
    /// 16-bit `SBC` shape or the split-indexed borrow).
    durctr_hi: Option<u16>,
    /// The per-voice ctrl/waveform cell, when located ([`locate_ctrl_cell`]).
    ctrl_cell: Option<u16>,
    /// Porta-prefix duration marker from the dur-fetch `CMP` immediate.
    porta_prefix: u8,
    /// Whether rows fetch when the countdown reaches 1 instead of 0.
    fetch_at_one: bool,
}

/// Read the Table generation's command table and classify every handler.
/// Demands at least one paired repeat loop (the structure mechanism — its
/// absence means the table was misparsed); a stop command is optional
/// (early table revisions end positionally instead).
fn classify_table(
    ram: &[u8],
    cmd_table: u16,
    cmd_max: u8,
    anchors: &TableAnchors,
) -> Option<TableCmds> {
    let mut kinds = [CmdKind::Other; TABLE_CMD_MAX as usize];
    let mut starts: Vec<(usize, u16, u16)> = Vec::new();
    let mut ends: Vec<(usize, u16, u16)> = Vec::new();
    let mut has_stop = false;
    for (i, kind) in kinds.iter_mut().enumerate().take(usize::from(cmd_max)) {
        let entry = cmd_table.wrapping_add((i * 2) as u16);
        let h = u16::from_le_bytes([
            ram[usize::from(entry)],
            ram[usize::from(entry.wrapping_add(1))],
        ]);
        match classify_handler(ram, h, anchors.zp) {
            HandlerShape::Stop => {
                *kind = CmdKind::Stop;
                has_stop = true;
            }
            HandlerShape::SingleStore(cell) if cell == anchors.transpose_cell => {
                *kind = CmdKind::Transpose;
            }
            HandlerShape::SingleStore(cell) if Some(cell) == anchors.ctrl_cell => {
                *kind = CmdKind::CtrlSet;
            }
            HandlerShape::AddStore(cell) if cell == anchors.transpose_cell => {
                *kind = CmdKind::TransposeAdd;
            }
            HandlerShape::AddStore(cell) if Some(cell) == anchors.durctr_hi => {
                *kind = CmdKind::DurHiAdd;
            }
            HandlerShape::LoopStart(r, r2) => starts.push((i, r, r2)),
            HandlerShape::LoopEnd(r, r2) => ends.push((i, r, r2)),
            _ => {}
        }
    }
    // Pair loop ends with the starts saving the same pointer tables.
    let mut level = 0u8;
    for &(ei, r, r2) in &ends {
        if let Some(&(si, _, _)) = starts.iter().find(|&&(_, sr, sr2)| sr == r && sr2 == r2)
            && level < 2
        {
            kinds[ei] = CmdKind::LoopEnd(level);
            kinds[si] = CmdKind::LoopStart(level);
            level += 1;
        }
    }
    (level > 0).then_some(TableCmds {
        kinds,
        cmd_max,
        porta_prefix: anchors.porta_prefix,
        dur16: anchors.durctr_hi.is_some(),
        fetch_at_one: anchors.fetch_at_one,
        has_stop,
        ctrl_cell: anchors.ctrl_cell,
    })
}

/// How far the divider shape may sit from the drain head and still be
/// trusted. The shape (`INC abs / LDA same / CMP # / BCC`) is a generic 6502
/// counter idiom — the driver itself has a *second* divider of the same shape
/// (the `$0C` effect divider) and resident game code can contain more — so a
/// match only counts inside the player: the outer divider and the drain
/// routine sit a few hundred bytes apart in every observed build.
const DIVIDER_WINDOW: u16 = 0x1000;

/// Find the Crowther V3 player in a post-`init` RAM image. Demands exactly
/// one octave-fold and exactly one drain-head match of either generation
/// (a second hit, or one of each, means a false positive or a different
/// engine — fail closed). The Chain divider shape is optional (default
/// speed 1) and must sit within [`DIVIDER_WINDOW`] of the drain head (first
/// match wins there — the outer divider precedes the effect divider in the
/// player layout).
pub(crate) fn locate(ram: &[u8]) -> Option<CrowtherLayout> {
    if ram.len() < 0x10000 {
        return None;
    }
    let mut fold = None;
    let mut chain_drain = None;
    let mut table_drains: Vec<(u16, u16, u8, usize)> = Vec::new();
    let mut dividers: Vec<u16> = Vec::new();
    for a in 0x0200..ram.len() - 0x30 {
        if let Some(freq) = match_octave_fold(ram, a)
            && fold.replace(freq).is_some()
        {
            return None;
        }
        if let Some(tables) = match_drain_head(ram, a)
            && chain_drain.replace((a as u16, tables)).is_some()
        {
            return None;
        }
        if table_drains.len() < 8
            && let Some(t) = match_table_drain(ram, a)
        {
            table_drains.push(t);
        }
        if dividers.len() < 64
            && let Some(imm) = match_divider(ram, a)
        {
            dividers.push(imm);
        }
    }
    let freq_table = fold?;
    // The table-drain shape can match more than once (a second player copy,
    // similar resident code); a candidate only counts when its dispatch tail
    // also parses. Demand exactly one *validated* candidate.
    let mut validated =
        table_drains
            .into_iter()
            .filter_map(|(seq_lo, seq_hi, cmd_max, dispatch_addr)| {
                let cmd_table = match_dispatch_tail(ram, dispatch_addr)?;
                Some((seq_lo, seq_hi, cmd_max, dispatch_addr, cmd_table))
            });
    let table_drain = validated.next();
    if validated.next().is_some() {
        return None;
    }
    match (chain_drain, table_drain) {
        (Some((drain_addr, (seq_lo, seq_hi))), None) => {
            let divider_imm = dividers
                .into_iter()
                .find(|&imm| imm.abs_diff(drain_addr) <= DIVIDER_WINDOW);
            Some(CrowtherLayout {
                seq_lo,
                seq_hi,
                freq_table,
                divider_imm,
                dispatch: Dispatch::Chain,
            })
        }
        (None, Some((seq_lo, seq_hi, cmd_max, dispatch_addr, cmd_table))) => {
            // The drain reads through a zp pair; recover it from the head.
            let zp = ram[dispatch_addr - 21 + 4];
            // Find the note path (transpose cell), the duration fetch (the
            // porta-prefix marker), and — in the late revision only — the
            // 16-bit duration countdown's hi cell. Each exactly-one.
            let mut note_path = None;
            let mut porta = None;
            let mut durctr_hi = None;
            let mut durhi_borrow = None;
            for a in 0x0200..ram.len() - 0x30 {
                if let Some(cell) = match_table_note_path(ram, a, zp)
                    && note_path.replace(cell).is_some()
                {
                    return None;
                }
                if let Some(m) = match_dur_fetch(ram, a, zp)
                    && porta.replace(m).is_some()
                {
                    return None;
                }
                if let Some(cell) = match_table_durctr(ram, a)
                    && durctr_hi.replace(cell).is_some()
                {
                    return None;
                }
                if let Some(cell) = match_durhi_borrow(ram, a)
                    && durhi_borrow.replace(cell).is_some()
                {
                    return None;
                }
            }
            let (porta_marker, durlo) = porta?;
            let anchors = TableAnchors {
                zp,
                transpose_cell: note_path?,
                durctr_hi: durctr_hi.or(durhi_borrow),
                ctrl_cell: locate_ctrl_cell(ram),
                porta_prefix: porta_marker,
                fetch_at_one: fetch_at_one(ram, durlo),
            };
            let cmds = classify_table(ram, cmd_table, cmd_max, &anchors)?;
            Some(CrowtherLayout {
                seq_lo,
                seq_hi,
                freq_table,
                divider_imm: None,
                dispatch: Dispatch::Table(cmds),
            })
        }
        _ => None,
    }
}

/// Duration byte marking a portamento-target prefix pair in the Chain
/// generation: the pair's note sets the slide target, the *next* pair is
/// the sounding row. (Table revisions carry their marker in
/// [`TableCmds::porta_prefix`], read from the dur-fetch `CMP` immediate.)
const PORTA_PREFIX_CHAIN: u8 = 0x63;
/// Largest octave fold the player's `LSR` countdown supports.
const MAX_FOLDS: u8 = 7;
/// Drain-iteration bound per voice per frame — a garbage stream must not
/// hang the decode. On overrun the drain merely yields until the next frame
/// (progress is kept); a legitimate over-long drain (huge command-only
/// loops) resumes instead of silencing the voice.
const MAX_DRAIN: u32 = 4096;
/// Bound on chained porta-prefix pairs per fetch. The player has no limit
/// and authored data uses one; this is a garbage-stream hang guard.
const MAX_PORTA_PREFIXES: u32 = 8;

// The Chain generation's hardwired command numbers (CPY-chain dispatch; the
// Table generation classifies its handlers instead). The rest of the chain
// (docs/drivers/crowther.md) is timbre/effect state with no bearing on note
// onsets; only the loop-end commands mutate the stream pointer.
const CMD_SPEED: u8 = 0x0E;
const CMD_NOTE_TRANSPOSE: u8 = 0x14;
const CMD_LOOP1_END: u8 = 0x10;
const CMD_LOOP1_START: u8 = 0x11;
const CMD_LOOP2_END: u8 = 0x12;
const CMD_LOOP2_START: u8 = 0x13;

/// Resolve one command byte to its sequencer semantic for either generation.
fn cmd_kind(dispatch: &Dispatch, cmd: u8) -> CmdKind {
    match dispatch {
        Dispatch::Chain => match cmd {
            CMD_NOTE_TRANSPOSE => CmdKind::Transpose,
            CMD_LOOP1_END => CmdKind::LoopEnd(0),
            CMD_LOOP2_END => CmdKind::LoopEnd(1),
            CMD_LOOP1_START => CmdKind::LoopStart(0),
            CMD_LOOP2_START => CmdKind::LoopStart(1),
            // CMD_SPEED is handled separately (it mutates the global
            // divider, which only the Chain generation has).
            _ => CmdKind::Other,
        },
        Dispatch::Table(t) => t.kind(cmd),
    }
}

/// Per-voice simulation state (mirrors the player's cells).
struct Voice {
    ptr: u16,
    /// Sequencer-tick countdown to the next row fetch (init sets 1).
    durctr: u32,
    /// Command `$14`: semitones added to the note byte before the fold.
    transpose: u8,
    /// The staged SID control byte: rows only sound while the gate bit is
    /// set (silent lead-ins sequence with ctrl `$00`). `0x01` when the
    /// build's ctrl cell is unknown — every row then counts, the
    /// pre-ctrl-tracking behaviour.
    ctrl: u8,
    /// Repeat loops, two nesting levels: (armed, count, return ptr).
    loops: [(bool, u8, u16); 2],
    /// Index into the output notes of the currently sounding note.
    open: Option<usize>,
    /// Authored timbre/effect command values currently in force. The command
    /// stream is itself Crowther's instrument definition; rows inherit this
    /// state until a later command changes it.
    instrument: CrowtherInstrument,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct CrowtherInstrument {
    commands: [u8; TABLE_CMD_MAX as usize],
}

impl Default for CrowtherInstrument {
    fn default() -> Self {
        Self {
            commands: [0; TABLE_CMD_MAX as usize],
        }
    }
}

impl CrowtherInstrument {
    fn set(&mut self, cmd: u8, value: u8) {
        if let Some(slot) = cmd
            .checked_sub(1)
            .and_then(|i| self.commands.get_mut(usize::from(i)))
        {
            *slot = value;
        }
    }

    fn value(&self, cmd: u8) -> u8 {
        cmd.checked_sub(1)
            .and_then(|i| self.commands.get(usize::from(i)))
            .copied()
            .unwrap_or(0)
    }
}

pub(crate) struct DecodedSong {
    notes: Vec<NoteEvent>,
    groups: Vec<Option<u8>>,
    instruments: Vec<CrowtherInstrument>,
    structure: Vec<VoicePlacements>,
    recovered_structure: RecoveredStructure,
}

struct SectionRecorder {
    pending_break: [bool; 3],
    numbers: std::collections::HashMap<u16, u8>,
    starts: [Option<u16>; 3],
    placements: [Vec<NativePlacement>; 3],
    instances: [Vec<RecoveredPatternInstance>; 3],
    patterns: std::collections::BTreeMap<PatternNumber, Vec<RecoveredPatternEvent>>,
    commands: [Vec<RecoveredOrderCommand>; 3],
    ticks: [u32; 3],
}

impl SectionRecorder {
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
        }
    }

    fn number_for(&mut self, address: u16) -> PatternNumber {
        let next = self.numbers.len() as u8;
        PatternNumber(*self.numbers.entry(address).or_insert(next))
    }

    fn begin(&mut self, voice: usize, address: u16, frame: u32, transpose: u8) {
        if !self.pending_break[voice] {
            return;
        }
        self.pending_break[voice] = false;
        self.starts[voice] = Some(address);
        let pattern = self.number_for(address);
        let repeat_ordinal = RepeatOrdinal(
            self.instances[voice]
                .iter()
                .filter(|instance| instance.pattern == pattern)
                .count() as u32,
        );
        let order_offset = OrderOffset(self.commands[voice].len());
        let transpose = PatternTranspose(i16::from(transpose as i8));
        let placement = NativePlacement {
            pattern_number: pattern,
            start_frame: FrameIndex(frame),
            transpose,
            order_offset: Some(order_offset),
            repeat_ordinal: Some(repeat_ordinal),
        };
        self.placements[voice].push(placement);
        self.instances[voice].push(RecoveredPatternInstance {
            pattern,
            transpose,
            repeat_ordinal,
            order_offset,
            start_tick: NativeRowTick(self.ticks[voice]),
            start_frame: FrameIndex(frame),
        });
    }

    fn event(&mut self, voice: usize, address: u16, event: RecoveredPatternEvent) {
        let Some(start) = self.starts[voice] else {
            return;
        };
        let pattern = self.number_for(start);
        let event = RecoveredPatternEvent {
            offset: PatternByteOffset(address.wrapping_sub(start)),
            ..event
        };
        let events = self.patterns.entry(pattern).or_default();
        if !events.contains(&event) {
            events.push(event);
        }
    }

    fn command_offset(&self, voice: usize) -> OrderOffset {
        OrderOffset(self.commands[voice].len())
    }

    fn command(&mut self, voice: usize, command: RecoveredOrderCommand) {
        self.commands[voice].push(command);
    }

    fn split(&mut self, voice: usize) {
        self.pending_break[voice] = true;
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
                order_commands: std::mem::take(&mut self.commands[voice]),
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

fn is_instrument_command(dispatch: &Dispatch, cmd: u8) -> bool {
    match dispatch {
        // $02-$0D are the complete per-voice synthesis program: staged
        // parameters, control, effect rate/type, pulse and pitch behaviour.
        Dispatch::Chain => (0x02..=0x0D).contains(&cmd),
        // Cobra table generation: per-voice stores and packed synthesis
        // controls. Exclude globals ($01/$17/$18), duration ($16/$19),
        // structure/transpose ($10-$14/$1A/$1B), and stop ($1E).
        Dispatch::Table(_) => matches!(cmd, 0x02..=0x0F | 0x15 | 0x1C..=0x1D | 0x1F..=0x21),
    }
}

fn duration_countdown(
    duration: u8,
    sixteen_bit: bool,
    fetch_at_one: bool,
    wrapped_hold: u32,
) -> u32 {
    if fetch_at_one && !sixteen_bit {
        let held = duration.wrapping_sub(1);
        return if held == 0 { 256 } else { u32::from(held) };
    }
    if duration == 0 {
        wrapped_hold
    } else {
        u32::from(duration)
    }
}

/// Decode the song by simulating the player's own frame loop over the
/// (read-only) post-`init` RAM image. See the module docs for the model.
pub(crate) fn decode_song(
    ram: &[u8],
    layout: &CrowtherLayout,
    clock: SystemClock,
    frames: u32,
) -> DecodedSong {
    let read = |a: u16| ram[usize::from(a)];
    let stream_ptr = |v: usize| {
        u16::from_le_bytes([
            read(layout.seq_lo.wrapping_add(v as u16)),
            read(layout.seq_hi.wrapping_add(v as u16)),
        ])
    };
    let starts = [stream_ptr(0), stream_ptr(1), stream_ptr(2)];
    // Initial per-voice ctrl from the located cell's post-init value; 0x01
    // ("audible") when the build's ctrl cell is unknown.
    let ctrl_cell = match &layout.dispatch {
        Dispatch::Table(t) => t.ctrl_cell,
        Dispatch::Chain => None,
    };
    let mut voices: Vec<Voice> = starts
        .iter()
        .enumerate()
        .map(|(v, &ptr)| Voice {
            ptr,
            durctr: 1,
            transpose: 0,
            ctrl: ctrl_cell.map_or(1, |c| read(c.wrapping_add(v as u16))),
            loops: [(false, 0, 0); 2],
            open: None,
            instrument: CrowtherInstrument::default(),
        })
        .collect();
    // The only end condition the player has ($A1AE): the song stops when
    // voice 0's pointer reaches voice 1's stream start. Voices 1/2 have no
    // end of their own — authored data idles them on endless rest loops, and
    // a stream that falls through simply reads on into the next voice's
    // bytes, exactly like the hardware.
    let song_end = starts[1];

    // Per-generation parameters: command range, porta-prefix marker, how
    // long a dur-0 row holds (8-bit vs 16-bit countdown wrap), and whether
    // the song ends positionally (Chain, and Table revisions without a stop
    // command) or via the stop command.
    let (cmd_max, porta_prefix, rest_hold, positional_end) = match &layout.dispatch {
        Dispatch::Chain => (0x7Eu8, PORTA_PREFIX_CHAIN, 256u32, true),
        Dispatch::Table(t) => (
            t.cmd_max,
            t.porta_prefix,
            if t.dur16 { 65_536u32 } else { 256 },
            !t.has_stop,
        ),
    };

    let mut divider = layout
        .divider_imm
        .map_or(1u32, |a| u32::from(read(a)).max(1));
    let mut tick_ctr = 0u32;
    // The note->freq conversion output cell (`$A5DB/$A5DC` / `$FF5F/$FF60`)
    // is a single GLOBAL register in the player: every fetched row rewrites
    // it — a rest stages zero, a gate-off row stages its pitch with the gate
    // cleared — and a tie row re-gates whatever it holds, even across voices.
    let mut staged: u32 = 0;
    let mut notes: Vec<NoteEvent> = Vec::new();
    let mut note_instruments: Vec<CrowtherInstrument> = Vec::new();
    let mut stopped_at: Option<u32> = None;
    let mut sections = SectionRecorder::new();

    'frames: for frame in 0..frames {
        // 1. Command drain, every frame, all voices (matches the player).
        for (voice_index, voice) in voices.iter_mut().enumerate() {
            let mut steps = 0u32;
            loop {
                let cmd = read(voice.ptr);
                if cmd == 0 || cmd > cmd_max {
                    break;
                }
                steps += 1;
                if steps > MAX_DRAIN {
                    break;
                }
                let command_ptr = voice.ptr;
                let value = read(voice.ptr.wrapping_add(1));
                sections.begin(voice_index, command_ptr, frame, voice.transpose);
                sections.event(
                    voice_index,
                    command_ptr,
                    RecoveredPatternEvent {
                        offset: PatternByteOffset(0),
                        duration: PatternDuration(0),
                        frequency_index: None,
                        instrument: None,
                        hold: false,
                        slide: None,
                        command: Some(NativeDriverOpcode(cmd)),
                        command_data: Some(NativeEffectByte(value)),
                        duration_index: None,
                        operand: None,
                    },
                );
                if is_instrument_command(&layout.dispatch, cmd) {
                    voice.instrument.set(cmd, value);
                }
                let mut jumped = false;
                match cmd_kind(&layout.dispatch, cmd) {
                    CmdKind::CtrlSet => voice.ctrl = value,
                    CmdKind::Transpose => voice.transpose = value,
                    CmdKind::TransposeAdd => {
                        voice.transpose = voice.transpose.wrapping_add(value);
                    }
                    // Extends the *current* row: the fetch zeroes the hi
                    // byte, this adds N*256 ticks on top.
                    CmdKind::DurHiAdd => {
                        voice.durctr = voice.durctr.saturating_add(u32::from(value) << 8);
                    }
                    // Loop end: arm with the count, DEC, jump back while
                    // non-zero — `DEC $00 -> $FF` on the 6502, so a count
                    // byte of 0 means 256 total plays.
                    CmdKind::LoopEnd(lvl) => {
                        let (armed, count, ret) = &mut voice.loops[usize::from(lvl)];
                        if !*armed {
                            *armed = true;
                            *count = value;
                        }
                        *count = count.wrapping_sub(1);
                        if *count != 0 {
                            voice.ptr = *ret;
                            jumped = true;
                            let target = sections.number_for(*ret);
                            let order_offset = sections.command_offset(voice_index);
                            sections.command(
                                voice_index,
                                RecoveredOrderCommand::Jump {
                                    order_offset,
                                    target,
                                    transpose: Some(PatternTranspose(i16::from(
                                        voice.transpose as i8,
                                    ))),
                                },
                            );
                        } else {
                            *armed = false;
                            let order_offset = sections.command_offset(voice_index);
                            sections.command(
                                voice_index,
                                RecoveredOrderCommand::DriverCommand {
                                    order_offset,
                                    opcode: NativeDriverOpcode(cmd),
                                },
                            );
                        }
                        sections.split(voice_index);
                    }
                    // Loop start: remember the record after the marker.
                    CmdKind::LoopStart(lvl) => {
                        voice.loops[usize::from(lvl)].2 = voice.ptr.wrapping_add(2);
                        let order_offset = sections.command_offset(voice_index);
                        sections.command(
                            voice_index,
                            RecoveredOrderCommand::DriverCommand {
                                order_offset,
                                opcode: NativeDriverOpcode(cmd),
                            },
                        );
                        sections.split(voice_index);
                    }
                    CmdKind::Stop => {
                        let order_offset = sections.command_offset(voice_index);
                        sections.command(voice_index, RecoveredOrderCommand::Stop { order_offset });
                        stopped_at = Some(frame);
                        break 'frames;
                    }
                    CmdKind::Other => {
                        // The Chain generation's speed command self-modifies
                        // the global divider immediate.
                        if matches!(layout.dispatch, Dispatch::Chain) && cmd == CMD_SPEED {
                            divider = u32::from(value).max(1);
                        }
                        // Everything else is timbre/effect state —
                        // irrelevant to note onsets, consumed as a pair.
                    }
                }
                if !jumped {
                    voice.ptr = voice.ptr.wrapping_add(2);
                }
                if matches!(
                    cmd_kind(&layout.dispatch, cmd),
                    CmdKind::Transpose | CmdKind::TransposeAdd
                ) {
                    let order_offset = sections.command_offset(voice_index);
                    sections.command(
                        voice_index,
                        RecoveredOrderCommand::SetTranspose {
                            order_offset,
                            transpose: PatternTranspose(i16::from(voice.transpose as i8)),
                        },
                    );
                    sections.split(voice_index);
                }
            }
        }

        // 2. Outer speed divider gates the sequencer tick (Chain only; the
        // Table generation counts durations in play frames — divider 1).
        tick_ctr += 1;
        if tick_ctr >= divider {
            tick_ctr = 0;

            // 3. Sequencer tick: step each voice's duration counter.
            for (v, voice) in voices.iter_mut().enumerate() {
                voice.durctr = voice.durctr.saturating_sub(1);
                if voice.durctr > 0 {
                    continue;
                }
                // Fetch the next row; a porta-prefix pair only sets the
                // slide target, the sounding row follows (bounded chain —
                // on a garbage overrun the current pair is taken as the row).
                let mut row_ptr = voice.ptr;
                let mut note = read(row_ptr);
                let mut dur = read(row_ptr.wrapping_add(1));
                let mut prefixes = 0;
                while dur == porta_prefix && prefixes < MAX_PORTA_PREFIXES {
                    sections.begin(v, row_ptr, frame, voice.transpose);
                    sections.event(
                        v,
                        row_ptr,
                        RecoveredPatternEvent {
                            offset: PatternByteOffset(0),
                            duration: PatternDuration(0),
                            frequency_index: (note >= 0x80)
                                .then_some(FrequencyTableIndex(note.wrapping_sub(0x80))),
                            instrument: None,
                            hold: false,
                            slide: Some(NativeEffectByte(note)),
                            command: None,
                            command_data: None,
                            duration_index: Some(NativeEffectByte(dur)),
                            operand: None,
                        },
                    );
                    voice.ptr = voice.ptr.wrapping_add(2);
                    row_ptr = voice.ptr;
                    note = read(row_ptr);
                    dur = read(row_ptr.wrapping_add(1));
                    prefixes += 1;
                }
                voice.ptr = voice.ptr.wrapping_add(2);
                sections.begin(v, row_ptr, frame, voice.transpose);
                let recovered_duration = match (&layout.dispatch, dur) {
                    (Dispatch::Table(t), 0) if t.dur16 => u16::MAX,
                    (_, 0) => 256,
                    (_, value) => u16::from(value),
                };
                sections.event(
                    v,
                    row_ptr,
                    RecoveredPatternEvent {
                        offset: PatternByteOffset(0),
                        duration: PatternDuration(recovered_duration),
                        frequency_index: (note >= 0x80)
                            .then_some(FrequencyTableIndex(note.wrapping_sub(0x80))),
                        instrument: None,
                        hold: note < 0x80 && dur != 0,
                        slide: None,
                        command: None,
                        command_data: None,
                        duration_index: Some(NativeEffectByte(dur)),
                        operand: None,
                    },
                );

                // Close the previous note: it sounded up to (not including)
                // this fetch frame — `end_frame` is exclusive.
                if let Some(i) = voice.open.take() {
                    let end = frame.max(notes[i].start_frame.0);
                    notes[i].end_frame = Some(FrameIndex(end));
                }

                // Stage the conversion cell; bytes below $80 (ties) leave it
                // untouched — the `CMP #$80 / BCC -> RTS` path.
                if note == 0 {
                    staged = 0;
                } else if note >= 0x80 {
                    staged = note_raw_freq(ram, layout, note, voice.transpose).unwrap_or(0);
                }
                // The row gates on unless it is a rest (staged zero), a
                // gate-off row (dur 0: pitch staged, gate bit cleared), or
                // the voice's staged ctrl has the gate bit clear (silent
                // lead-in sequencing).
                if dur != 0
                    && staged != 0
                    && voice.ctrl & 1 == 1
                    && let Some(ev) =
                        note_from_raw_freq(staged, clock, VoiceId::from_index(v), frame, frame)
                {
                    voice.open = Some(notes.len());
                    notes.push(ev);
                    note_instruments.push(voice.instrument);
                }
                // A duration byte of 0 wraps the countdown — 256 ticks
                // (8-bit, Chain) or 65536 (16-bit, Table). The 8-bit
                // fetch-at-one revisions `DEC dur,X / CMP #$01 / BNE` fetch
                // one tick earlier, so a dur byte `d` holds `d - 1` ticks
                // (`$01` -> 256, `$00` -> 255). A split 16-bit countdown can
                // contain the same low-byte shape but still borrows through
                // the high byte and fetches on the full counter reaching zero.
                let (dur16, fetch_at_one) = match &layout.dispatch {
                    Dispatch::Chain => (false, false),
                    Dispatch::Table(table) => (table.dur16, table.fetch_at_one),
                };
                voice.durctr = duration_countdown(dur, dur16, fetch_at_one, rest_hold);
                sections.ticks[v] = sections.ticks[v].saturating_add(voice.durctr);
            }
        }

        // 4. Positional song-end check (Chain: the player's $A1AE ordering,
        // frame level after the tick — the drain or a fetch may have crossed
        // this frame). The Table generation ends via the stop command.
        if positional_end && voices[0].ptr >= song_end {
            stopped_at = Some(frame);
            break 'frames;
        }
    }

    // Close anything still sounding where the music actually stopped (or at
    // the simulation horizon when it never did). The stop frame itself still
    // sounded (the positional check runs after the tick), so the exclusive
    // end is one past it.
    let end_excl = stopped_at.map_or(frames, |f| (f + 1).min(frames));
    for voice in &voices {
        if let Some(i) = voice.open {
            let end = end_excl.max(notes[i].start_frame.0);
            notes[i].end_frame = Some(FrameIndex(end));
        }
    }
    let mut paired: Vec<_> = notes.into_iter().zip(note_instruments).collect();
    paired.sort_by_key(|(note, _)| (note.start_frame.0, note.voice.0));

    let mut notes = Vec::with_capacity(paired.len());
    let mut groups = Vec::with_capacity(paired.len());
    let mut instruments = Vec::new();
    for (note, instrument) in paired {
        let group = instruments
            .iter()
            .position(|known| known == &instrument)
            .or_else(|| {
                (instruments.len() < usize::from(u8::MAX)).then(|| {
                    instruments.push(instrument);
                    instruments.len() - 1
                })
            });
        notes.push(note);
        groups.push(group.map(|index| index as u8));
    }
    let (structure, recovered_structure) = sections.finish();
    DecodedSong {
        notes,
        groups,
        instruments,
        structure,
        recovered_structure,
    }
}

/// The driver's note->frequency conversion: add the `$14` transpose, fold
/// into the top-octave window `$80..$8C`, look up the hi-first pair, halve
/// per remaining octave. Returns `None` for out-of-range bytes.
fn note_raw_freq(ram: &[u8], layout: &CrowtherLayout, note: u8, transpose: u8) -> Option<u32> {
    let mut n = note.wrapping_add(transpose);
    let mut folds = 0u8;
    while n >= 0x8C {
        n -= 12;
        folds += 1;
        if folds > MAX_FOLDS {
            return None;
        }
    }
    if n < 0x80 {
        return None;
    }
    let idx = u16::from(n - 0x80) * 2;
    // Absolute,Y addressing wraps at $FFFF on the 6502 — index the same way
    // so a freq table matched near the top of memory cannot panic.
    let cell = |off: u16| ram[usize::from(layout.freq_table.wrapping_add(off))];
    let raw = ((u32::from(cell(idx)) << 8) | u32::from(cell(idx + 1))) >> (MAX_FOLDS - folds);
    (raw > 0).then_some(raw)
}

/// The `Antony_Crowther_V3` driver extractor.
pub struct CrowtherExtractor;

impl DriverExtractor for CrowtherExtractor {
    fn name(&self) -> &'static str {
        "crowther"
    }

    fn handles(&self, driver: &str) -> bool {
        driver == "Antony_Crowther_V3"
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
            reason: "required Crowther signatures were not unique".to_owned(),
        })?;

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

        let validation = validate_native_notes(
            &notes,
            &detect_notes(&states, ctx.timing.clock),
            validation_timing,
            NativeValidationPolicy::default(),
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

        // Bind each note to its authored instrument via taint's per-voice
        // instrument timeline (the index feeding the `ad`/`sr` tables), with
        // the ADSR read from those tables. Falls back to heuristic clustering
        // when the instrument tables are not recovered, so this never produces
        // a worse grouping than before. Two guards back that claim up: the
        // recovered base must come from an indexed table read (a shadow cell
        // that out-writes the table would otherwise win the histogram), and
        // at least one note must actually bind to the timeline (sinks alone
        // do not imply a usable timeline — non-indexed AD writes never stamp
        // it, and an all-`None` group would zero out the patch table).
        // The base is voice 1's; per-voice ad/sr blocks living at different
        // addresses (the Chain generation does this) cannot be cross-checked
        // against it — reading the shared instrument id through voice 1's
        // block is the hand-validated grouping (Ark: 4 authored instruments).
        let taint = img.run_taint(ctx.header.play_address, frame_count as u32);
        let top_src = |reg: u8| {
            taint
                .sinks
                .iter()
                .find(|s| s.reg == reg)
                .and_then(|s| s.sources.first())
                .filter(|(_, stat)| stat.indexed)
                .map(|(b, _)| *b)
        };
        let taint_group: Vec<Option<u8>> = notes
            .iter()
            .map(|n| taint.instrument_at(n.voice.to_index(), n.start_frame.0))
            .collect();
        let bases = match (top_src(5), top_src(6)) {
            (Some(ad_base), Some(sr_base)) if taint_group.iter().any(Option::is_some) => {
                Some((ad_base, sr_base))
            }
            _ => None,
        };
        let (patches, patch_assignments) = if decoded.groups.iter().any(Option::is_some) {
            let authored = |idx: u8| {
                let instrument = decoded
                    .instruments
                    .get(usize::from(idx))
                    .copied()
                    .unwrap_or_default();
                let (adsr, waveform) = match layout.dispatch {
                    Dispatch::Table(_) => (
                        Adsr::from_bytes(instrument.value(0x02), instrument.value(0x0A)),
                        Some(instrument.value(0x03)),
                    ),
                    Dispatch::Chain => {
                        let adsr = bases.map_or_else(
                            || Adsr::from_bytes(0, 0),
                            |(ad_base, sr_base)| {
                                let taint_idx = taint_group
                                    .iter()
                                    .zip(&decoded.groups)
                                    .find_map(|(taint, group)| {
                                        (*group == Some(idx)).then_some(*taint).flatten()
                                    })
                                    .unwrap_or(0);
                                let cell = |base: u16| {
                                    ram[usize::from(base.wrapping_add(u16::from(taint_idx)))]
                                };
                                Adsr::from_bytes(cell(ad_base), cell(sr_base))
                            },
                        );
                        (adsr, Some(instrument.value(0x03)))
                    }
                };
                (adsr, waveform, false, None, None)
            };
            extract_patches_grouped(&notes, &characteristics, &decoded.groups, authored)
        } else {
            extract_patches(&notes, &characteristics)
        };

        let provenance = vec![
            ProvenanceEvidence {
                field: "note.pitch".to_owned(),
                provenance: FieldProvenance::AuthoredDecoded,
                samples: notes.len(),
                mismatches: validation.inserted.0,
            },
            ProvenanceEvidence {
                field: "note.articulation".to_owned(),
                provenance: FieldProvenance::AuthoredDecoded,
                samples: notes.len(),
                mismatches: validation.deleted.0,
            },
            ProvenanceEvidence {
                field: "instrument.identity".to_owned(),
                provenance: FieldProvenance::AuthoredDecoded,
                samples: decoded.instruments.len(),
                mismatches: 0,
            },
            ProvenanceEvidence {
                field: "instrument.definition".to_owned(),
                provenance: FieldProvenance::AuthoredPartial,
                samples: decoded.instruments.len(),
                mismatches: 0,
            },
            ProvenanceEvidence {
                field: "song.structure".to_owned(),
                provenance: FieldProvenance::AuthoredDecoded,
                samples: decoded.recovered_structure.patterns.len(),
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
            structure: Some(decoded.structure),
            recovered_structure: Some(decoded.recovered_structure),
            validation,
            provenance,
        })
    }
}

#[cfg(all(test, feature = "asset-tests"))]
mod tests {
    use super::*;
    use crate::header::SubtuneIndex;

    fn ark_pandora_ram() -> Vec<u8> {
        let bytes = std::fs::read("../../assets/music/Ark_Pandora.sid").unwrap();
        let header = crate::header::parse(&bytes).unwrap();
        let mut img = Emulator::new();
        img.load(&header, &bytes).unwrap();
        img.call_init(header.init_address, SubtuneIndex(1), header.songs)
            .unwrap();
        img.ram_image()
    }

    fn post_init_ram(asset: &str) -> Vec<u8> {
        let bytes = std::fs::read(format!("../../assets/music/{asset}")).unwrap();
        let header = crate::header::parse(&bytes).unwrap();
        let mut img = Emulator::new();
        img.load(&header, &bytes).unwrap();
        img.call_init(header.init_address, SubtuneIndex(1), header.songs)
            .unwrap();
        img.ram_image()
    }

    #[test]
    fn locates_ark_pandora_layout() {
        let ram = ark_pandora_ram();
        let layout = locate(&ram).expect("Ark_Pandora must locate");
        assert_eq!(layout.seq_lo, 0xA586);
        assert_eq!(layout.seq_hi, 0xA589);
        assert_eq!(layout.freq_table, 0xA5C2);
        assert_eq!(layout.divider_imm, Some(0xA07F));
        assert_eq!(layout.dispatch, Dispatch::Chain);
    }

    #[test]
    fn locates_cobra_table_layout() {
        let ram = post_init_ram("Cobra.sid");
        let layout = locate(&ram).expect("Cobra must locate");
        assert_eq!(layout.seq_lo, 0xFF67);
        assert_eq!(layout.seq_hi, 0xFF64);
        assert_eq!(layout.freq_table, 0xFF75);
        assert_eq!(layout.divider_imm, None);
        let Dispatch::Table(cmds) = layout.dispatch else {
            panic!("Cobra must classify as the Table generation");
        };
        assert_eq!(cmds.cmd_max, 0x21);
        // The known Cobra command numbers: $10/$11 loop, $12/$13 loop,
        // $14 transpose, $1E stop.
        assert_eq!(cmds.kind(0x10), CmdKind::LoopEnd(0));
        assert_eq!(cmds.kind(0x11), CmdKind::LoopStart(0));
        assert_eq!(cmds.kind(0x12), CmdKind::LoopEnd(1));
        assert_eq!(cmds.kind(0x13), CmdKind::LoopStart(1));
        assert_eq!(cmds.kind(0x14), CmdKind::Transpose);
        assert_eq!(cmds.kind(0x1E), CmdKind::Stop);
    }

    #[test]
    fn split_sixteen_bit_countdown_does_not_use_low_byte_fetch_at_one() {
        assert_eq!(duration_countdown(10, true, true, 65_536), 10);
        assert_eq!(duration_countdown(10, false, true, 256), 9);
        assert_eq!(duration_countdown(1, false, true, 256), 256);
    }

    #[test]
    fn decodes_cobra_above_the_gate() {
        let bytes = std::fs::read("../../assets/music/Cobra.sid").unwrap();
        let header = crate::header::parse(&bytes).unwrap();
        let frames = 1500;
        let ram = post_init_ram("Cobra.sid");
        let layout = locate(&ram).unwrap();
        let clock = SystemClock::Pal;

        let decoded = decode_song(&ram, &layout, clock, frames);
        assert_eq!(decoded.groups.len(), decoded.notes.len());
        assert!(
            decoded.instruments.len() >= 6,
            "Cobra must retain its authored timbre changes"
        );
        assert!(decoded.recovered_structure.patterns.len() > 3);
        assert!(decoded.recovered_structure.voices.iter().any(|voice| {
            voice.instances.iter().enumerate().any(|(index, instance)| {
                voice.instances[..index]
                    .iter()
                    .any(|previous| previous.pattern == instance.pattern)
            })
        }));
        let notes = decoded.notes;
        assert!(!notes.is_empty(), "decode must produce notes");

        let trace = emu::run(&header, &bytes, SubtuneIndex(1), frames).unwrap();
        let states = analyze(&trace);
        let truth = detect_notes(&states, clock);
        let agreement = onset_agreement(&notes, &truth);
        eprintln!(
            "cobra: agreement {agreement:.3}, {} native vs {} trace notes",
            notes.len(),
            truth.len()
        );
        assert!(
            agreement >= MIN_AGREEMENT,
            "onset agreement {agreement:.3} below gate ({} native vs {} trace notes)",
            notes.len(),
            truth.len()
        );
    }

    #[test]
    fn decodes_ark_pandora_above_the_gate() {
        let bytes = std::fs::read("../../assets/music/Ark_Pandora.sid").unwrap();
        let header = crate::header::parse(&bytes).unwrap();
        let frames = 1500;
        let ram = ark_pandora_ram();
        let layout = locate(&ram).unwrap();
        let clock = SystemClock::Pal;

        let decoded = decode_song(&ram, &layout, clock, frames);
        assert!(decoded.recovered_structure.patterns.len() > 3);
        assert!(
            decoded
                .structure
                .iter()
                .any(|voice| voice.placements.len() > 3)
        );
        let notes = decoded.notes;
        assert!(!notes.is_empty(), "decode must produce notes");

        let trace = emu::run(&header, &bytes, SubtuneIndex(1), frames).unwrap();
        let states = analyze(&trace);
        let truth = detect_notes(&states, clock);
        let agreement = onset_agreement(&notes, &truth);
        eprintln!(
            "ark_pandora: agreement {agreement:.3}, {} native vs {} trace notes",
            notes.len(),
            truth.len()
        );
        assert!(
            agreement >= MIN_AGREEMENT,
            "onset agreement {agreement:.3} below gate ({} native vs {} trace notes)",
            notes.len(),
            truth.len()
        );
    }

    /// Per-tune triage: decode one tune (path in `SID_DBG_TUNE`) and dump
    /// the layout, the first rows, and every native-vs-trace mismatch.
    /// CI-safe: no-op without the env var. Run:
    /// `SID_DBG_TUNE=path/to.sid cargo test -p sid-analyzer --lib crowther::tests::dbg_crowther_tune -- --ignored --nocapture`
    #[test]
    #[ignore = "manual triage tool; needs SID_DBG_TUNE"]
    fn dbg_crowther_tune() {
        let Ok(path) = std::env::var("SID_DBG_TUNE") else {
            eprintln!("SID_DBG_TUNE unset; skipping");
            return;
        };
        let frames: u32 = std::env::var("SID_DBG_FRAMES")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(400);
        let bytes = std::fs::read(&path).unwrap();
        let header = crate::header::parse(&bytes).unwrap();
        let sub = header.start_song;
        let mut img = Emulator::new();
        img.load(&header, &bytes).unwrap();
        img.call_init(header.init_address, sub, header.songs)
            .unwrap();
        let ram = img.ram_image();
        let Some(layout) = locate(&ram) else {
            eprintln!("locate: FAILED");
            return;
        };
        eprintln!("layout: {layout:#?}");
        let decoded = decode_song(&ram, &layout, SystemClock::Pal, frames);
        let notes = decoded.notes;
        let trace = emu::run(&header, &bytes, sub, frames).unwrap();
        let states = analyze(&trace);
        let truth = detect_notes(&states, SystemClock::Pal);
        let agreement = onset_agreement(&notes, &truth);
        eprintln!(
            "agreement {agreement:.3}, {} native vs {} trace notes",
            notes.len(),
            truth.len()
        );
        for n in &notes {
            if !truth.iter().any(|t| super::super::onset_match(n, t)) {
                let nearest = truth.iter().filter(|t| t.voice == n.voice).min_by_key(|t| {
                    (i64::from(t.start_frame.0) - i64::from(n.start_frame.0)).abs()
                });
                eprintln!(
                    "  MISS v{} f{:5} midi{:3} | nearest same-voice: f{:?} midi{:?}",
                    n.voice.0,
                    n.start_frame.0,
                    n.midi.0,
                    nearest.map(|t| t.start_frame.0),
                    nearest.map(|t| t.midi.0)
                );
            }
        }
    }

    /// HVSC-wide triage sweep over every `Antony_Crowther_V3`-identified tune
    /// (the hubbard `dbg_hvsc_sweep` pattern). CI-safe: no-op without
    /// `SID_HVSC_ROOT`. Run:
    /// `SID_HVSC_ROOT=… cargo test -p sid-analyzer --lib crowther::tests::dbg_crowther_hvsc_sweep -- --ignored --nocapture`
    #[test]
    #[ignore = "manual triage tool; needs SID_HVSC_ROOT"]
    fn dbg_crowther_hvsc_sweep() {
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
            // (onset, accepted, pitchset, name, n_native, n_truth)
            Decoded(f64, bool, f64, String, usize, usize),
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
            let decoded = decode_song(&ram, &layout, SystemClock::Pal, frames);
            let native = decoded.notes;
            if native.is_empty() {
                return Cat::Empty;
            }
            let Ok(trace) = emu::run(&header, &bytes, sub, frames) else {
                return Cat::EmuFail;
            };
            let states = analyze(&trace);
            let truth = detect_notes(&states, SystemClock::Pal);
            let onset = onset_agreement(&native, &truth);
            let timing =
                crate::emu::PlaybackTiming::for_subtune(&header, sub).resolved_from_trace(&trace);
            let validation =
                validate_native_notes(&native, &truth, timing, NativeValidationPolicy::default());
            let truth_set: std::collections::HashSet<(VoiceId, u8)> =
                truth.iter().map(|t| (t.voice, t.midi.0)).collect();
            let pitch_hits = native
                .iter()
                .filter(|n| truth_set.contains(&(n.voice, n.midi.0)))
                .count();
            let pitchset = pitch_hits as f64 / native.len() as f64;
            Cat::Decoded(
                onset,
                validation.accepted,
                pitchset,
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
                if db.identify(&bytes) != Some("Antony_Crowther_V3") {
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
        let (mut located, mut locate_fail, mut emu_fail, mut empty, mut decoded, mut timeout) =
            (0, 0, 0, 0, 0, 0);
        let mut buckets = [0u32; 11];
        let mut pass = 0;
        let mut clump: Vec<(f64, f64, String, usize, usize)> = Vec::new();
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
                Cat::Decoded(onset, accepted, pitchset, name, nn, nt) => {
                    located += 1;
                    decoded += 1;
                    let b = ((onset * 10.0).round() as usize).min(10);
                    buckets[b] += 1;
                    if accepted {
                        pass += 1;
                    } else {
                        clump.push((onset, pitchset, name, nn, nt));
                    }
                }
            }
        }

        eprintln!(
            "\n=== HVSC Antony_Crowther_V3 sweep (start_song, {frames}f) ===\n\
             total={total} located={located} locate_fail={locate_fail} emu_fail={emu_fail} \
             timeout={timeout} empty={empty} decoded={decoded} PASS={pass}\n\
             buckets[0.0..1.0]={buckets:?}\n"
        );
        clump.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        eprintln!("--- clump rows (onset / pitchset / nN / nT  name) ---");
        for (onset, pitchset, name, nn, nt) in &clump {
            eprintln!("  o{onset:.2} p{pitchset:.2}  {nn:4}/{nt:<4}  {name}");
        }
        for name in &timed_out {
            eprintln!("  timeout {name}");
        }
    }
}
