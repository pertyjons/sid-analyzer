/// Addresses of the Rob Hubbard player's data structures, recovered from the
/// post-load RAM image by [`locate`]. All are absolute 6502 addresses (raw
/// `u16`/`u8`, matching the [`crate::emu::bus::Bus`] address space) because
/// they are relocation-dependent and only meaningful against one RAM image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HubbardLayout {
    pub evidence: LocatorEvidence,
    /// Address of the `AND #imm` that masks a pattern status byte down to its
    /// duration field (see [`Self::dur_mask`] for the immediate).
    pub note_mask: u16,
    /// The status-byte duration mask, read from the `AND #imm` operand: `$1F`
    /// (5-bit duration, Commando and most variants) or `$3F` (6-bit duration,
    /// the Jeroen Tel relocation used by Ikari Union / Beginning). Tie (`$40`)
    /// and extra-byte (`$80`) flags sit above it in both cases.
    pub dur_mask: u8,
    /// Pattern pointer table, low bytes (`LDA pat_ptr_lo,Y`).
    pub pat_ptr_lo: u16,
    /// Pattern pointer table, high bytes (`LDA pat_ptr_hi,Y`).
    pub pat_ptr_hi: u16,
    /// Stride between a pattern's lo/hi pointer entries: `1` when the lo and hi
    /// bytes live in two *separate* tables indexed by the raw pattern number
    /// (Commando and most variants), `2` when they are *interleaved* into one
    /// `lo hi lo hi …` table indexed by `pattern * 2` (the Jeroen Tel relocation,
    /// whose play loop does `ASL A / TAY` before the pointer load and reads
    /// `pat_ptr_lo` / `pat_ptr_lo + 1` as the adjacent lo/hi bytes).
    pub pat_stride: u8,
    /// Zero-page pattern pointer; `(zp_ptr),Y` reads the pattern. The high byte
    /// lives at `zp_ptr + 1`.
    pub zp_ptr: u8,
    /// Address of the `LDA (zp_ptr),Y` pattern fetch.
    pub pat_read: u16,
    /// Frequency table base. With [`Self::freq_hi`] `None` this is a single
    /// *interleaved* 16-bit table read as `freq_table[idx*2]` / `freq_table[idx*2+1]`
    /// (Commando and most variants). With [`Self::freq_hi`] `Some(_)` this is the
    /// *low-byte* table of a split pair, read as `freq_table[idx]` (stride 1).
    pub freq_table: u16,
    /// High-byte frequency table base, set only when the variant uses two
    /// *separate* lo/hi tables (`LDA freq_lo,Y` / `LDA freq_hi,Y`, Y = the raw
    /// note index, the two bases differing by the table length rather than 1 —
    /// the Jeroen Tel relocation). `None` for the interleaved single-table layout,
    /// where the high byte is `freq_table + idx*2 + 1`.
    pub freq_hi: Option<u16>,
    /// Per-voice sequence (orderlist) pointer table, low bytes
    /// (`LDA seq_ptr_lo,X`). Indexed by voice (0/1/2).
    pub seq_ptr_lo: u16,
    /// Per-voice sequence (orderlist) pointer table, high bytes.
    pub seq_ptr_hi: u16,
    /// Tempo divider reload value (read from the located tempo cell post-init):
    /// the song advances one row-tick every time the divider underflows, i.e.
    /// once per `tempo + 1` divider *runs*. With no frame-skip gate that is one
    /// row every `tempo + 1` frames; a gate (see below) stretches it.
    pub tempo: u8,
    /// Prescale frame-skip gate reload, if the play loop gates the tempo divider
    /// behind a `DEC ctr / BPL <divider> / LDA #imm / STA ctr` counter that skips
    /// the divider on its underflow frame (Knucklebusters). With reload `R` the
    /// divider runs `R` of every `R + 1` frames, so the effective row period is
    /// `(tempo + 1) * (R + 1) / R` frames — *not* `(tempo + 1) * (R + 1)`. (The
    /// two coincide only at `R = 1`, which is why earlier work that modelled it as
    /// a plain multiplier fit Knucklebusters subtune 1 but not its faster
    /// subtunes.) `None` when the divider runs every frame.
    pub prescale_reload: Option<u8>,
    /// Whole-play stall gate reload, if the play routine opens with a
    /// `DEC ctr / BPL / LDA #imm / STA ctr / RTS` that returns early (playing
    /// nothing) on its underflow frame (Warhawk). With reload `R` the routine
    /// runs `R` of every `R + 1` frames; the dead frame advances no row, which is
    /// what makes Warhawk's effective tempo a non-integer number of frames per
    /// row. `None` when every frame runs the routine.
    pub stall_reload: Option<u8>,
    /// Number of voices this play routine drives (1..=3). The lo orderlist
    /// pointer table has one byte per voice and is immediately followed by the
    /// hi table, so `seq_ptr_hi - seq_ptr_lo` is the voice count — e.g. Commando
    /// drives 3, while Human Race's main routine drives only 2 (its third voice
    /// has a separate routine).
    pub voices: u8,
    /// Bytes consumed by a pattern's "effect" command (an extra byte whose
    /// bit 7 is set): 1 in the Commando variant, 2 in the Sigma Seven variant.
    /// Reading the wrong count desyncs the rest of the pattern stream.
    pub effect_bytes: u8,
    /// How an orderlist transpose command ([`ORDER_TRANSPOSE`]) carries its
    /// value. `None` (Auf Wiedersehen Monty): a bit-7 marker byte followed by a
    /// *separate* value byte (`INY / LDA (zp),Y / STA $EB81,X`). `Some(mask)`: the
    /// value is *embedded* in the marker byte itself — one byte, the driver masks
    /// off the bit-7 flag and stores the remainder per voice. The mask is `$1F`
    /// for the Jeroen Tel relocation (`AND #$80` to test then `AND #$1F` to
    /// extract, `STA $191B,X`) and `$7F` for the Magnar / Shape Music player
    /// (`AND #$7F / STA $E0E1,X`). Reading the wrong width desyncs the orderlist
    /// walk; reading the wrong mask repitches every note by a per-voice constant.
    pub embedded_transpose_mask: Option<u8>,
    /// Whether the orderlist interleaves a **repeat count** after each pattern
    /// number (the Delta / Shape Music family): the entries are `pattern, count,
    /// pattern, count, …`, and each pattern plays `count` times before the walk
    /// advances (the first pattern plays once). The driver tracks this with a
    /// repeat counter reloaded from the orderlist (`INC idx / LDA (ord),Y / STA
    /// counter,X / INC idx` at the pattern-end advance). `false` for the plain
    /// pattern-list variants (Commando, …); reading counts as patterns — or vice
    /// versa — desyncs the whole orderlist.
    pub order_repeat: bool,
    /// Instrument table, the static records the play loop reads to set up each
    /// note's voice (`LDA inst_table+f, idx`). Found from the ADSR register
    /// writes (`STA $D405`/`$D406`) fed by the table; see [`InstrumentTable`] and
    /// [`decode_instruments`] for the field layout. `None` when the instrument
    /// load could not be located (the note decode does not depend on it).
    pub inst_table: Option<InstrumentTable>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LocatorEvidence {
    pub pattern_pointer_anchor: u16,
    pub pattern_read_anchor: u16,
    pub sequence_pointer_tables: (u16, u16),
    pub frequency_table: u16,
    pub tempo_divider_anchor: Option<u16>,
}

/// How a Hubbard driver lays its instrument table out in memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InstrumentTable {
    /// Packed array-of-structs: 8-byte records at `base + index*8`, fields at
    /// fixed offsets (`+0/1` PW, `+2` ctrl, `+3` AD, `+4` SR, `+5..7` effects).
    /// Six of the seven supported variants (Commando, Sigma Seven, …).
    Packed { base: u16 },
    /// Columnar struct-of-arrays: one field-table per field, each indexed by the
    /// raw instrument index at stride 1, the field-tables themselves spaced by
    /// the instrument count. The Jeroen Tel relocation (Ikari Union). Only ADSR
    /// is statically authored: pulse-width low is forced to 0 (so `pwhi` is the
    /// whole PW, kept for validation but unused by the patch), and the control
    /// byte / waveform come from a *separate* per-voice program pointer that is
    /// not recovered — so the waveform and effects stay trace-derived.
    Columnar { pwhi: u16, ad: u16, sr: u16 },
}

/// How far past the pattern fetch the `AND #$1F` note mask may sit.
pub(super) const NOTE_MASK_WINDOW: u16 = 32;

/// Maximum distance from the pattern-pointer load to the other instructions in
/// one pattern-step routine. Dispatch stubs may be far away, but the pointer
/// load, pattern fetch, duration mask, frequency lookup, and order-pointer load
/// are part of one compact relocated routine.
pub(super) const ROUTINE_WINDOW: u16 = 0x1000;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub(crate) enum HubbardLocateError {
    #[error("no coherent Hubbard pattern-step candidate was found")]
    NotFound,
    #[error("multiple equally strong Hubbard locator candidates remain: {evidence:?}")]
    Ambiguous { evidence: Vec<LocatorEvidence> },
}

#[derive(Debug, Clone, Copy)]
pub(super) struct CoreCandidate {
    ptr_anchor: u16,
    pat_ptr_lo: u16,
    pat_ptr_hi: u16,
    zp_ptr: u8,
    pat_read: u16,
    note_mask: u16,
    dur_mask: u8,
    seq_ptr_lo: u16,
    seq_ptr_hi: u16,
    freq_table: u16,
    score: u32,
}

/// Scan a RAM image for the Rob Hubbard play loop and recover its table
/// addresses. `read` returns the byte at any address; `lo..=hi` bounds the code
/// search. Returns `None` if the play loop cannot be found.
///
/// The signatures (see `docs/drivers/hubbard.md`). Operand offsets are exact —
/// the second `STA`'s zero-page operand is at `+9` (the bytes at `+7`/`+8` are
/// the second `LDA`'s high byte and the `STA` opcode):
/// - pattern pointer load: `B9 ll hh / 85 zz / B9 mm hh' / 85 zz+1`
///   (`LDA lo,Y / STA zp / LDA hi,Y / STA zp+1`) — yields both tables + the zp slot
/// - pattern fetch: `B1 zz` (`LDA (zp),Y`) confirming the recovered zp slot
/// - note mask: the `29 1F` (`AND #$1F`) closest after the pattern fetch
/// - frequency table: two interleaved 16-bit reads `B9 ll hh` … `B9 ll+1 hh`
///   (`LDA freq,Y` / `LDA freq+1,Y`), the base reaches the SID via a `$D400`/
///   `$D401` write. The driver reads the table once per voice, so the true base
///   is the one that appears in the most such pairs (coincidental consecutive
///   reads occur once each).
/// - sequence (orderlist) pointer load: `BD ll hh / 85 zz / BD mm nn / 85 zz+1`
///   (`LDA seq_lo,X / STA zp / LDA seq_hi,X / STA zp+1`) — the same shape as the
///   pattern pointer load but `,X`-indexed (`$BD`) and into a different zp slot.
/// - tempo divider: `CE a / 10 rr / AD t / 8D a` (`DEC frame_ctr / BPL / LDA
///   tempo / STA frame_ctr`), where the `DEC` and `STA` share the counter cell
///   and the `LDA` operand is the tempo cell; its post-init value is read.
pub(crate) fn locate(read: &impl Fn(u16) -> u8, lo: u16, hi: u16) -> Option<HubbardLayout> {
    locate_checked(read, lo, hi).ok()
}

pub(crate) fn locate_checked(
    read: &impl Fn(u16) -> u8,
    lo: u16,
    hi: u16,
) -> Result<HubbardLayout, HubbardLocateError> {
    let r = read;
    let at = |a: u16, off: u16| {
        a.checked_add(off)
            .filter(|address| *address <= hi)
            .map_or(0, r)
    };
    let w = |a: u16| u16::from(at(a, 0)) | (u16::from(at(a, 1)) << 8);

    let mut ptr_anchors = Vec::new();
    let mut seq_anchors = Vec::new();
    // Tempo cell address, recovered from the frame-divider reload, plus the
    // address of that divider's `DEC` (so a gating pre-divider can be matched).
    let mut tempo_candidates = Vec::new();
    let mut freq_reads = Vec::new();
    let mut freq_writes = Vec::new();

    for a in lo..=hi {
        if at(a, 0) == 0xB9
            && at(a, 3) == 0x85
            && at(a, 5) == 0xB9
            && at(a, 8) == 0x85
            && at(a, 9) == at(a, 4).wrapping_add(1)
        {
            ptr_anchors.push((a, w(a.wrapping_add(1)), w(a.wrapping_add(6)), at(a, 4)));
        }

        if at(a, 0) == 0xBD
            && at(a, 3) == 0x85
            && at(a, 5) == 0xBD
            && at(a, 8) == 0x85
            && at(a, 9) == at(a, 4).wrapping_add(1)
        {
            seq_anchors.push((a, w(a.wrapping_add(1)), w(a.wrapping_add(6))));
        }

        if at(a, 0) == 0xCE
            && at(a, 3) == 0x10
            && at(a, 5) == 0xAD
            && at(a, 8) == 0x8D
            && at(a, 9) == at(a, 1)
            && at(a, 10) == at(a, 2)
            // A trailing `RTS` after the self-targeting `STA` marks a *stall*
            // gate (reload + return-early, an absolute-reload sibling of the
            // Warhawk gate), not the tempo divider — a real divider falls
            // through into row processing. Skip it so the true divider matches.
            && at(a, 11) != 0x60
        {
            tempo_candidates.push((a, w(a.wrapping_add(6))));
        }

        // `STA $D400,Y` / `STA $D401,Y` (indexed) or absolute — confirms the
        // play loop writes voice frequency, so a freq table really exists.
        if (at(a, 0) == 0x99 || at(a, 0) == 0x8D) && (w(a.wrapping_add(1)) & 0xFFFE) == 0xD400 {
            freq_writes.push(a);
        }

        if at(a, 0) == 0xB9 {
            let base = w(a.wrapping_add(1));
            for j in 3..=12u16 {
                if at(a, j) == 0xB9
                    && w(a.wrapping_add(j + 1)) == base.wrapping_add(1)
                    && at(a, j + 2) == at(a, 2)
                {
                    freq_reads.push((a, base));
                    break;
                }
            }
        }

        if a == hi {
            break;
        }
    }

    let distance = |left: u16, right: u16| left.abs_diff(right);
    let in_routine = |anchor: u16, address: u16| distance(anchor, address) <= ROUTINE_WINDOW;
    let mut candidates = Vec::new();
    for &(ptr_anchor, pat_ptr_lo, pat_ptr_hi, zp_ptr) in &ptr_anchors {
        let Some(pat_read) = (lo..=hi)
            .filter(|address| in_routine(ptr_anchor, *address))
            .find(|address| at(*address, 0) == 0xB1 && at(*address, 1) == zp_ptr)
        else {
            continue;
        };
        let mask_hi = pat_read.saturating_add(NOTE_MASK_WINDOW).min(hi);
        let Some(note_mask) = (pat_read..=mask_hi)
            .find(|address| at(*address, 0) == 0x29 && matches!(at(*address, 1), 0x1F | 0x3F))
        else {
            continue;
        };
        let Some(&(_, seq_ptr_lo, seq_ptr_hi)) = seq_anchors
            .iter()
            .filter(|(anchor, _, _)| in_routine(ptr_anchor, *anchor))
            .filter(|(_, seq_lo, seq_hi)| {
                seq_hi
                    .checked_sub(*seq_lo)
                    .is_some_and(|voices| (1..=3).contains(&voices))
            })
            .min_by_key(|(anchor, _, _)| distance(ptr_anchor, *anchor))
        else {
            continue;
        };
        if !freq_writes
            .iter()
            .any(|address| in_routine(ptr_anchor, *address))
        {
            continue;
        }
        let mut local_votes: Vec<(u16, u32)> = Vec::new();
        for &(_, base) in freq_reads
            .iter()
            .filter(|(address, _)| in_routine(ptr_anchor, *address))
        {
            match local_votes
                .iter_mut()
                .find(|(candidate, _)| *candidate == base)
            {
                Some((_, count)) => *count += 1,
                None => local_votes.push((base, 1)),
            }
        }
        let Some(&(freq_table, votes)) = local_votes.iter().max_by_key(|(_, count)| *count) else {
            continue;
        };
        let locality =
            u32::from(distance(ptr_anchor, pat_read)) + u32::from(distance(ptr_anchor, note_mask));
        candidates.push(CoreCandidate {
            ptr_anchor,
            pat_ptr_lo,
            pat_ptr_hi,
            zp_ptr,
            pat_read,
            note_mask,
            dur_mask: at(note_mask, 1),
            seq_ptr_lo,
            seq_ptr_hi,
            freq_table,
            score: votes.saturating_mul(1_000).saturating_sub(locality),
        });
    }
    let Some(best_score) = candidates.iter().map(|candidate| candidate.score).max() else {
        return Err(HubbardLocateError::NotFound);
    };
    let winners: Vec<_> = candidates
        .iter()
        .copied()
        .filter(|candidate| candidate.score == best_score)
        .collect();
    if winners.len() != 1 {
        return Err(HubbardLocateError::Ambiguous {
            evidence: winners
                .iter()
                .map(|candidate| LocatorEvidence {
                    pattern_pointer_anchor: candidate.ptr_anchor,
                    pattern_read_anchor: candidate.pat_read,
                    sequence_pointer_tables: (candidate.seq_ptr_lo, candidate.seq_ptr_hi),
                    frequency_table: candidate.freq_table,
                    tempo_divider_anchor: None,
                })
                .collect(),
        });
    }
    let candidate = winners[0];
    let ptr_anchor = candidate.ptr_anchor;
    let pat_ptr_lo = candidate.pat_ptr_lo;
    let pat_ptr_hi = candidate.pat_ptr_hi;
    let zp_ptr = candidate.zp_ptr;
    let pat_read = candidate.pat_read;
    let note_mask = candidate.note_mask;
    let dur_mask = candidate.dur_mask;
    let seq_ptr_lo = candidate.seq_ptr_lo;
    let seq_ptr_hi = candidate.seq_ptr_hi;
    let freq_table = candidate.freq_table;
    let routine_lo = ptr_anchor.saturating_sub(ROUTINE_WINDOW).max(lo);
    let routine_hi = ptr_anchor.saturating_add(ROUTINE_WINDOW).min(hi);
    let (tempo_div_addr, tempo_addr) = tempo_candidates
        .iter()
        .filter(|(anchor, _)| in_routine(ptr_anchor, *anchor))
        .min_by_key(|(anchor, _)| distance(ptr_anchor, *anchor))
        .map_or((None, None), |(anchor, cell)| (Some(*anchor), Some(*cell)));

    // Interleaved pattern-pointer table: the Jeroen Tel relocation packs the lo
    // and hi bytes into one `lo hi lo hi …` table indexed by `pattern * 2`, so the
    // play loop does `ASL A / TAY` (`0A A8`) right before the pointer load and the
    // two pointer operands are adjacent (`pat_ptr_lo`, `pat_ptr_lo + 1`). Both
    // conditions together distinguish it from the separate-table layout, whose lo
    // and hi tables sit far apart and whose index is the raw pattern number.
    let pat_stride = if r(ptr_anchor.wrapping_sub(2)) == 0x0A
        && r(ptr_anchor.wrapping_sub(1)) == 0xA8
        && pat_ptr_hi == pat_ptr_lo.wrapping_add(1)
    {
        2
    } else {
        1
    };

    // A tune may carry no separate tempo divider: its tick is then one frame,
    // with timing shaped entirely by a frame-skip gate (the immediate-reload
    // `DEC / BPL / LDA #imm / STA / RTS` form below, which the tempo-divider
    // anchor deliberately rejects). Default the divider to 1 instead of
    // failing. Reload zero means the divider underflows on every eligible play
    // call, which is the driver's one-row-per-call behavior.
    let tempo = tempo_addr.map_or(0, r);
    let voices = seq_ptr_hi
        .checked_sub(seq_ptr_lo)
        .ok_or(HubbardLocateError::NotFound)?;
    if !(1..=3).contains(&voices) {
        return Err(HubbardLocateError::NotFound);
    }
    let voices = voices as u8;

    // Prescale frame-skip gate: a `DEC ctr / BPL <tempo divider> / LDA #imm / STA
    // ctr` counter that, on underflow, reloads from an *immediate* (LDA `$A9`, vs
    // the tempo divider's absolute `$AD`) and skips the tempo divider that frame.
    // Its `BPL` lands on the tempo divider's `DEC`, so the divider runs `imm` of
    // every `imm + 1` frames. The reload immediate drives the timing simulation.
    let mut prescale_reload = None;
    if let Some(div) = tempo_div_addr {
        for a in routine_lo..=routine_hi {
            let bpl_target = a.wrapping_add(5).wrapping_add((at(a, 4) as i8) as u16);
            if at(a, 0) == 0xCE
                && at(a, 3) == 0x10
                && bpl_target == div
                && at(a, 5) == 0xA9
                && at(a, 7) == 0x8D
                && at(a, 8) == at(a, 1)
                && at(a, 9) == at(a, 2)
            {
                prescale_reload = Some(at(a, 6));
                break;
            }
            if a == routine_hi {
                break;
            }
        }
    }

    // Whole-play frame-skip gate: a counter at the play routine's head that, on
    // its underflow frame, reloads and abandons the rest of the frame — playing
    // nothing and not advancing the song that frame. The routine then runs
    // `reload` of every `reload + 1` frames, stretching the effective tick by
    // `(reload + 1) / reload`. Seen across relocations in several shapes:
    //   DEC ctr / BPL fwd / LDA <reload> / STA ctr / {RTS | JMP}
    // where the counter is zero-page (`C6`/`85`) or absolute (`CE`/`8D`), the
    // reload is immediate (`A9`, Warhawk/Kentilla/Spellbound) or from a cell
    // (`AD`, Camel Riders), and the skip is `RTS` (return) or `JMP` (jump away).
    // The self-targeting `STA` plus the trailing `RTS`/`JMP` distinguish it from
    // a tempo divider (which falls through into row processing).
    //
    // Only sought when no prescale gate was found: the prescale form (a gate that
    // skips just the tempo divider, Knucklebusters) ends in `JMP` too and would
    // otherwise be double-counted here. The two produce the same tick stretch, so
    // a tune needs only one of them.
    let skip_gate_at = |a: u16| -> Option<u8> {
        let (ctr, after_dec): (u16, u16) = match r(a) {
            0xC6 => (u16::from(at(a, 1)), a.wrapping_add(2)),
            0xCE => (w(a.wrapping_add(1)), a.wrapping_add(3)),
            _ => return None,
        };
        if r(after_dec) != 0x10 {
            return None; // BPL
        }
        let after_bpl = after_dec.wrapping_add(2);
        let (reload, after_lda): (u8, u16) = match r(after_bpl) {
            0xA9 => (r(after_bpl.wrapping_add(1)), after_bpl.wrapping_add(2)),
            0xAD => (r(w(after_bpl.wrapping_add(1))), after_bpl.wrapping_add(3)),
            _ => return None,
        };
        let after_sta = match r(after_lda) {
            0x85 if u16::from(r(after_lda.wrapping_add(1))) == ctr => after_lda.wrapping_add(2),
            0x8D if w(after_lda.wrapping_add(1)) == ctr => after_lda.wrapping_add(3),
            _ => return None,
        };
        matches!(r(after_sta), 0x60 | 0x4C).then_some(reload)
    };
    let mut stall_reload = None;
    if prescale_reload.is_none() {
        for a in routine_lo..=routine_hi {
            if let Some(reload) = skip_gate_at(a) {
                stall_reload = Some(reload);
                break;
            }
            if a == routine_hi {
                break;
            }
        }
    }

    // Effect command width: the play loop fetches the extra byte with a second
    // `LDA (zp),Y` followed by `BPL` (instrument-vs-effect test) and a
    // `STA abs,X`. If a *second* `INY / LDA (zp),Y` follows that store, the
    // effect carries two bytes (Sigma Seven); otherwise one (Commando).
    let mut effect_bytes = 1u8;
    for a in routine_lo..=routine_hi {
        if at(a, 0) == 0xB1 && at(a, 1) == zp_ptr && at(a, 2) == 0x10 && at(a, 4) == 0x9D {
            if at(a, 7) == 0xC8 && at(a, 8) == 0xB1 && at(a, 9) == zp_ptr {
                effect_bytes = 2;
            }
            break;
        }
        if a == routine_hi {
            break;
        }
    }

    // Repeat-count orderlist (Delta / Shape Music): the pattern-end advance reads
    // the next orderlist byte as a repeat count and stores it to a per-voice
    // counter, incrementing the orderlist index twice — once to reach the count,
    // once past it. The idiom is `INC idx,X / LDY idx,X / LDA (ord),Y / BMI / STA
    // counter,X / INC idx,X`, the two `INC`s targeting the same index cell. The
    // plain pattern-list variants advance the index just once, so this precise
    // double-increment is what distinguishes the format.
    let mut order_repeat = false;
    for a in routine_lo..=routine_hi {
        if at(a, 0) == 0xFE
            && at(a, 3) == 0xBC
            && w(a.wrapping_add(4)) == w(a.wrapping_add(1))
            && at(a, 6) == 0xB1
            && at(a, 8) == 0x30
            && at(a, 10) == 0x9D
            && at(a, 13) == 0xFE
            && w(a.wrapping_add(14)) == w(a.wrapping_add(1))
        {
            order_repeat = true;
            break;
        }
        if a == routine_hi {
            break;
        }
    }

    // Separate lo/hi frequency tables: the play loop writes the note's high
    // frequency byte with `LDA freq_hi,Y / LDY voice / STA $D401,Y`, fed from a
    // `LDA freq_lo,Y / STA tmp / LDA freq_hi,Y` pair (`B9 .. / 8D .. / B9 .. /
    // AC .. / 99 01 D4`). The same shape exists in the interleaved variants, but
    // there the two bases differ by 1 (`freq` / `freq + 1`); when they differ by
    // more, the tables are split and the note index is used at stride 1.
    let mut freq_hi = None;
    let mut freq_table = freq_table;
    for a in routine_lo..=routine_hi {
        if at(a, 0) == 0xB9
            && at(a, 3) == 0x8D
            && at(a, 6) == 0xB9
            && at(a, 9) == 0xAC
            && at(a, 12) == 0x99
            && at(a, 13) == 0x01
            && at(a, 14) == 0xD4
        {
            let lo_base = w(a.wrapping_add(1));
            let hi_base = w(a.wrapping_add(7));
            if hi_base != lo_base.wrapping_add(1) {
                freq_table = lo_base;
                freq_hi = Some(hi_base);
            }
            break;
        }
        if a == routine_hi {
            break;
        }
    }

    // Embedded orderlist transpose: a bit-7 orderlist byte carries the transpose
    // in its own low bits (one byte), rather than Auf Wiedersehen Monty's separate
    // following value byte (`INY / LDA (zp),Y`, which never shows either shape
    // below). Two players do this with different masks:
    //   - Jeroen Tel relocation: `AND #$80 / BEQ / LDA abs / … / AND #$1F / STA
    //     abs,X` — the `29 80 … 29 1F 9D` shape, mask $1F.
    //   - Magnar / Shape Music: a `$FE`/transpose command dispatcher `CMP #$FE /
    //     BEQ / AND #$7F / STA abs,X` — mask $7F.
    //   - Nemesis: `LDA (order),Y / BPL pattern / CMP #$FF / BEQ loop /
    //     AND #$7F / STA abs,X / INC order_index,X`.
    let mut embedded_transpose_mask = None;
    for a in routine_lo..=routine_hi {
        if at(a, 0) == 0x29
            && at(a, 1) == 0x80
            && at(a, 2) == 0xF0
            && at(a, 4) == 0xAD
            && at(a, 7) == 0x29
            && at(a, 8) == 0x1F
            && at(a, 9) == 0x9D
        {
            embedded_transpose_mask = Some(0x1F);
            break;
        }
        let command_dispatch = at(a, 0) == 0xC9
            && at(a, 1) == 0xFE
            && at(a, 2) == 0xF0
            && at(a, 4) == 0x29
            && at(a, 5) == 0x7F
            && at(a, 6) == 0x9D;
        let negative_order_entry = at(a, 0) == 0xB1
            && at(a, 2) == 0x10
            && at(a, 4) == 0xC9
            && at(a, 5) == 0xFF
            && at(a, 6) == 0xF0
            && at(a, 8) == 0x29
            && at(a, 9) == 0x7F
            && at(a, 10) == 0x9D
            && at(a, 13) == 0xFE;
        if command_dispatch || negative_order_entry {
            embedded_transpose_mask = Some(0x7F);
            break;
        }
        if a == routine_hi {
            break;
        }
    }

    // Instrument table base: the play loop sets each voice's ADSR from the
    // table's +3 (AD) and +4 (SR) fields, so the attack/decay register write
    // `LDA inst+3, idx / STA $D405, idx` pins the base at `operand - 3`. Confirm
    // it with the sustain/release write `LDA inst+4, idx / STA $D406, idx` at the
    // same base nearby — the two `STA $D405`/`STA $D406` register operands make a
    // false match very unlikely. The load is `,X` (Commando) or `,Y` indexed; the
    // store is `,Y`/`,X`/absolute. `None` if the pair is not found.
    //
    // This matches the *packed* 8-byte-record layout (`SR == AD + 1`), found in 6
    // of 7 supported variants. The Jeroen Tel relocation (Ikari Union) instead
    // uses a *columnar* layout — one field-table per field, `SR_base − AD_base ==
    // instrument_count` — handled by `locate_columnar_instruments` when this
    // packed confirm fails (`docs/drivers/hubbard.md`).
    let mut inst_table = None;
    'inst: for a in routine_lo..=routine_hi {
        if (at(a, 0) == 0xBD || at(a, 0) == 0xB9)
            && matches!(at(a, 3), 0x99 | 0x9D | 0x8D)
            && w(a.wrapping_add(4)) == 0xD405
        {
            let base = w(a.wrapping_add(1)).wrapping_sub(3);
            for b in a.saturating_sub(16).max(routine_lo)..=a.saturating_add(16).min(routine_hi) {
                if (at(b, 0) == 0xBD || at(b, 0) == 0xB9)
                    && w(b.wrapping_add(1)) == base.wrapping_add(4)
                    && matches!(at(b, 3), 0x99 | 0x9D | 0x8D)
                    && w(b.wrapping_add(4)) == 0xD406
                {
                    inst_table = Some(InstrumentTable::Packed { base });
                    break 'inst;
                }
            }
        }
        if a == routine_hi {
            break;
        }
    }
    let inst_table = inst_table.or_else(|| locate_columnar_instruments(r, routine_lo, routine_hi));

    Ok(HubbardLayout {
        evidence: LocatorEvidence {
            pattern_pointer_anchor: ptr_anchor,
            pattern_read_anchor: pat_read,
            sequence_pointer_tables: (seq_ptr_lo, seq_ptr_hi),
            frequency_table: freq_table,
            tempo_divider_anchor: tempo_div_addr,
        },
        note_mask,
        dur_mask,
        pat_ptr_lo,
        pat_ptr_hi,
        pat_stride,
        zp_ptr,
        pat_read,
        freq_table,
        freq_hi,
        seq_ptr_lo,
        seq_ptr_hi,
        tempo,
        prescale_reload,
        stall_reload,
        voices,
        effect_bytes,
        embedded_transpose_mask,
        order_repeat,
        inst_table,
    })
}

/// Locate a *columnar* (struct-of-arrays) instrument table — the Jeroen Tel
/// relocation's layout, where each field lives in its own table indexed by the
/// raw instrument index at stride 1, and the field-tables are spaced by the
/// instrument count.
///
/// Anchored on the attack/decay write `LDA ad_table,X / STA $D405,Y`; the
/// sustain/release write `LDA sr_table,X / STA $D406,Y` nearby gives the
/// instrument count (`sr − ad`), and the pulse-width-high write `LDA pwhi,X /
/// STA $D403,Y` is the third confirming register. Requiring all three of the
/// $D403/$D405/$D406 field-table writes with a sane count keeps false matches
/// down. Returns `None` otherwise.
pub(super) fn locate_columnar_instruments(
    read: &impl Fn(u16) -> u8,
    lo: u16,
    hi: u16,
) -> Option<InstrumentTable> {
    let r = read;
    let at = |a: u16, off: u16| {
        a.checked_add(off)
            .filter(|address| *address <= hi)
            .map_or(0, r)
    };
    let w = |a: u16| u16::from(at(a, 0)) | (u16::from(at(a, 1)) << 8);
    // `LDA abs,X | abs,Y` feeding `STA reg,Y | ,X | abs`; yields the table base.
    let table_write = |a: u16, reg: u16| -> Option<u16> {
        ((at(a, 0) == 0xBD || at(a, 0) == 0xB9)
            && matches!(at(a, 3), 0x99 | 0x9D | 0x8D)
            && w(a.wrapping_add(4)) == reg)
            .then(|| w(a.wrapping_add(1)))
    };

    /// The field-tables sit within this many bytes of the AD anchor.
    const WINDOW: u16 = 48;
    /// A plausible instrument count (the field-table stride).
    const MAX_COUNT: u16 = 64;

    for a in lo..=hi {
        if let Some(ad) = table_write(a, 0xD405) {
            let win_lo = a.saturating_sub(WINDOW);
            let win_hi = a.saturating_add(WINDOW).min(hi);
            let mut sr = None;
            let mut pwhi = None;
            for b in win_lo..=win_hi {
                sr = sr.or_else(|| table_write(b, 0xD406));
                pwhi = pwhi.or_else(|| table_write(b, 0xD403));
            }
            if let (Some(sr), Some(pwhi)) = (sr, pwhi) {
                let count = sr.wrapping_sub(ad);
                if (1..=MAX_COUNT).contains(&count) {
                    return Some(InstrumentTable::Columnar { pwhi, ad, sr });
                }
            }
        }
        if a == hi {
            break;
        }
    }
    None
}

/// Bounds for the code search: above the zero page/stack, up to the top of RAM.
/// Hubbard players are frequently relocated high — several HVSC tunes (Nemesis,
/// Auf Wiedersehen Monty) load at `$E000`, RAM under the KERNAL ROM — so the
/// search must cover `$E000..$FFFF`. The emulator is flat RAM (no ROM/I/O
/// banking), so reading `$D000..$DFFF` just returns loaded data, not registers.
pub(super) const SCAN_LO: u16 = 0x0200;
pub(super) const SCAN_HI: u16 = 0xFFFF;
