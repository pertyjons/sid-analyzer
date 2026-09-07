//! Dynamic data-flow (taint) provenance tracker for the playroutine — the
//! engine behind `sid-re taint`.
//!
//! The goal is to recover, driver-agnostically, the cell addresses every
//! per-driver `locate` finds by hand: the frequency table feeding the SID
//! pitch registers, and the zero-page pointers the sequence streams are read
//! through. Instead of treating RAM as flat bytes, we attach a shadow
//! [`Prov`] tag to every byte and to the CPU's `A`/`X`/`Y`, propagate it as
//! the player runs, and read it off at the SID-register "sinks".
//!
//! The `mos6502` crate exposes no instruction-level hooks, so we do not
//! instrument the CPU. We drive it through [`Emulator::run_play_frame_stepwise`]
//! (a pre-instruction probe giving PC + registers + RAM), decode each
//! instruction with [`crate::emu::dis`], compute its effective address from
//! the probed register state, and apply the propagation rules ourselves. The
//! SID write is itself a `STA $D40x` we see in the probe, so even the sink
//! needs no bus trap.
//!
//! Provenance is a **chain of source addresses**, not a value+offset: a
//! `LDA tbl,Y / STA conv / LDA conv / STA $D400` staged write propagates the
//! origin `tbl` through the staging cell, and the octave-fold `LSR` chain
//! marks the value `transformed` without losing the address. The arithmetic
//! itself is not modelled — only the address hops, which is what reveals the
//! tables.

use crate::emu::dis::{Insn, Mode, decode};
use crate::emu::runner::Cpu;

/// The SID register window the player writes (mirrors [`crate::emu::bus`]).
const SID_BASE: u16 = 0xD400;
const SID_LAST: u16 = 0xD41C;

/// Forward read-address step (through one stream pointer) above which the
/// advance is treated as a pattern/orderlist jump rather than sequential
/// stream consumption. A backward step is always a boundary. Small enough to
/// not split a row's multi-byte read, large enough to catch a pattern change.
const STREAM_JUMP: u16 = 16;

/// Provenance of a byte: where in RAM it ultimately came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Prov {
    /// An immediate literal, or an origin we could not track (stack traffic,
    /// arithmetic from a constant base). Carries no address.
    Const,
    /// Sourced from RAM. `base` is the operand base — the **table base** for
    /// an indexed load, which is the address `locate` would recover. `eff`
    /// is the actual byte read. `via` is the zero-page pointer cell for a
    /// `(zp),Y` / `(zp,X)` read. `index_via` is the stream pointer the *index*
    /// register derived from when this (or an upstream) value was read from an
    /// indexed table — it links a freq/instrument table entry back to the
    /// sequence stream that selected it. `transformed` is set once the value
    /// passed through arithmetic or a shift, so a consumer knows the written
    /// byte no longer equals `ram[base]`.
    Sourced {
        base: u16,
        eff: u16,
        indexed: bool,
        via: Option<u16>,
        index_via: Option<u16>,
        /// The index register's *value* at the indexed read — the table row.
        /// For an instrument table that is the instrument id; for the freq
        /// table the (top-octave) note index (B5).
        index_val: Option<u8>,
        transformed: bool,
        /// The first *memory* operand arithmetically/logically combined into
        /// this value (`ADC/SBC/AND/ORA/EOR mem`) — the modulation-delta /
        /// transpose cell, or the table base for an indexed combine (B6).
        /// Immediate-mode arithmetic (octave folds, constant offsets) never
        /// sets this — those constants are A1's grammar tests — so a combine
        /// is the signature that separates vibrato/porta/transpose from the
        /// fold.
        combined: Option<u16>,
    },
}

impl Prov {
    /// Mark a value as having passed through arithmetic/shift: the address
    /// chain survives, the byte-equality does not.
    fn transformed(self) -> Self {
        match self {
            Self::Const => Self::Const,
            Self::Sourced {
                base,
                eff,
                indexed,
                via,
                index_via,
                index_val,
                combined,
                ..
            } => Self::Sourced {
                base,
                eff,
                indexed,
                via,
                index_via,
                index_val,
                transformed: true,
                combined,
            },
        }
    }

    /// Mark a value as combined with a memory operand at `src` (B6): keep the
    /// address chain, set `transformed`, and record the first combine source.
    fn combine(self, src: u16) -> Self {
        let mut p = self.transformed();
        if let Self::Sourced { combined, .. } = &mut p {
            *combined = combined.or(Some(src));
        }
        p
    }

    /// The stream pointer the index register derived from, if any.
    fn via(self) -> Option<u16> {
        match self {
            Self::Sourced { via, .. } => via,
            Self::Const => None,
        }
    }
}

/// What feeds one SID register, accumulated over the run: per origin table
/// base, how many writes traced to it and whether they were indexed-table
/// reads and/or arithmetic-transformed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SinkStat {
    pub writes: u64,
    pub indexed: bool,
    pub transformed: bool,
}

/// The shadow-provenance tracker. One instance per tune; [`Self::step`] is
/// fed every instruction, [`Self::report`] read off at the end.
pub struct Taint {
    a: Prov,
    x: Prov,
    y: Prov,
    ram: Box<[Prov; 0x1_0000]>,
    /// Per SID-register offset (`0..=0x1C`): origin-base histogram of the
    /// values written to it.
    sinks: Box<[Vec<(u16, SinkStat)>; 0x1D]>,
    /// SID-register writes whose value had no trackable origin (immediates,
    /// stack traffic), per register offset.
    const_sinks: [u64; 0x1D],
    /// Per SID-register offset: histogram of the stream pointer whose bytes
    /// indexed the table feeding this register — the per-voice stream→note
    /// link (A2).
    sink_via: Box<[Vec<(u16, u64)>; 0x1D]>,
    /// Per SID-register offset: histogram of the table-row index feeding this
    /// register — the instrument id (for ad/sr/pw/ctrl) or note index (for
    /// freq) the tune actually uses (B5).
    sink_idx: Box<[Vec<(u8, u64)>; 0x1D]>,
    /// Per SID-register offset: histogram of the combine-source addresses —
    /// the memory operands arithmetically folded into the written value (B6):
    /// vibrato/porta delta cells, transpose cells, modulation tables.
    sink_combine: Box<[Vec<(u16, u64)>; 0x1D]>,
    /// Zero-page pointer cells used in `(zp),Y` / `(zp,X)` reads, with the
    /// number of reads through each — the stream-pointer candidates.
    stream_ptrs: Vec<(u16, u64)>,
    /// Immediate-mode tests applied to bytes that came through an indirect
    /// (stream) read: `(mnemonic, immediate) -> count`. The dominant entries
    /// are the sequence grammar's own constants — the note/command boundary,
    /// duration masks, tie/porta markers, the command-range bound — read off
    /// without disassembling the decode routine.
    cmp_tests: Vec<((&'static str, u8), u64)>,
    /// Per stream pointer, where the read address jumps discontinuously — a
    /// pattern/orderlist boundary. A re-visited segment start is a reused
    /// pattern, so this recovers song structure (pattern reuse, loops) from
    /// the read trace alone.
    structure: Vec<StreamStruct>,
    /// The play frame currently being stepped (set by [`Self::enter_frame`]).
    frame: u32,
    /// Per voice (0..2): the `(frame, instrument id)` changes over the run,
    /// read from the index feeding that voice's `ad` register. A note's
    /// authored instrument is the id in effect at its onset frame (B5 → export).
    inst_timeline: [Vec<(u32, u8)>; 3],
    /// First-fetch log: the first time each address is read through a stream
    /// pointer, in fetch order — `(zp, addr, value)`. The probe targets come
    /// from here (capped; the head of the song reaches every byte role).
    fetch_log: Vec<(u16, u16, u8)>,
    /// Dedup map for `fetch_log`.
    fetched: Box<[bool; 0x1_0000]>,
    /// Bytes that were executed as part of an instruction (opcode or operand).
    executed: Box<[bool; 0x1_0000]>,
    /// How many times each address was the target of a store. Its
    /// intersection with `executed` is the self-modified code — the
    /// live-patched immediates (tempo dividers, dispatch operands) that are
    /// painful to find by hand.
    store_counts: Box<[u32; 0x1_0000]>,
}

/// Structure tracking for one stream pointer.
struct StreamStruct {
    zp: u16,
    last: u16,
    /// Distinct segment-start addresses (pattern entry points) and how many
    /// times each was (re)visited.
    starts: Vec<(u16, u64)>,
    /// Total discontinuous jumps — the orderlist length (segment visits).
    segments: u64,
}

impl Default for Taint {
    fn default() -> Self {
        Self::new()
    }
}

impl Taint {
    #[must_use]
    pub fn new() -> Self {
        Self {
            a: Prov::Const,
            x: Prov::Const,
            y: Prov::Const,
            ram: Box::new([Prov::Const; 0x1_0000]),
            sinks: Box::new(std::array::from_fn(|_| Vec::new())),
            const_sinks: [0; 0x1D],
            sink_via: Box::new(std::array::from_fn(|_| Vec::new())),
            sink_idx: Box::new(std::array::from_fn(|_| Vec::new())),
            sink_combine: Box::new(std::array::from_fn(|_| Vec::new())),
            stream_ptrs: Vec::new(),
            cmp_tests: Vec::new(),
            structure: Vec::new(),
            frame: 0,
            inst_timeline: [Vec::new(), Vec::new(), Vec::new()],
            fetch_log: Vec::new(),
            fetched: Box::new([false; 0x1_0000]),
            executed: Box::new([false; 0x1_0000]),
            store_counts: Box::new([0; 0x1_0000]),
        }
    }

    /// Reset the register provenance at the start of a `play` frame: the
    /// runner enters `play` with `A`/`X`/`Y` = 0 (literals). RAM provenance
    /// persists across frames — that is the player's evolving state. `frame`
    /// stamps the instrument timeline.
    pub fn enter_frame(&mut self, frame: u32) {
        self.a = Prov::Const;
        self.x = Prov::Const;
        self.y = Prov::Const;
        self.frame = frame;
    }

    /// Process one instruction, given the CPU state **before** it executes.
    /// We compute provenance from the pre-instruction registers exactly as
    /// the CPU computes effective addresses from them.
    pub fn step(&mut self, cpu: &Cpu) {
        let pc = cpu.registers.program_counter;
        let ram: &[u8] = cpu.memory.ram.as_ref();
        let insn = decode(ram, pc);
        let x = cpu.registers.index_x;
        let y = cpu.registers.index_y;

        // Mark this instruction's bytes as executed code (opcode + operands),
        // so a later store landing here is recognised as self-modification.
        for off in 0..insn.size() {
            self.executed[usize::from(pc.wrapping_add(off))] = true;
        }

        // Log grammar tests before the propagation match consumes the operand:
        // a `CMP/AND/…  #imm` on a stream-tainted register reads the byte's
        // pre-instruction provenance.
        self.log_stream_test(&insn);

        match insn.mnemonic {
            "LDA" => self.a = self.read_prov(insn.mode, insn.target, x, y, ram, true),
            "LDX" => self.x = self.read_prov(insn.mode, insn.target, x, y, ram, true),
            "LDY" => self.y = self.read_prov(insn.mode, insn.target, x, y, ram, true),

            "STA" => self.store(insn.mode, insn.target, x, y, ram, self.a),
            "STX" => self.store(insn.mode, insn.target, x, y, ram, self.x),
            "STY" => self.store(insn.mode, insn.target, x, y, ram, self.y),

            "TAX" => self.x = self.a,
            "TAY" => self.y = self.a,
            "TXA" => self.a = self.x,
            "TYA" => self.a = self.y,

            // Arithmetic / logic on A: keep the address chain, mark it
            // transformed (the review's chain model — do not track the
            // arithmetic itself). A *memory* operand is additionally recorded
            // as a combine source (B6): its address is the modulation-delta /
            // transpose cell (or table) folded into the value. Immediate
            // operands (octave folds, constant offsets) stay pure transforms.
            "ADC" | "SBC" | "AND" | "ORA" | "EOR" => {
                if insn.mode == Mode::Immediate {
                    self.a = self.a.transformed();
                } else {
                    // `log_fetch: false` — an arithmetic operand read is not a
                    // sequence fetch, so it must not feed the stream-pointer
                    // counts, B4 structure, or the probe's fetch log (those
                    // report surfaces were corpus-validated on loads only).
                    let operand = self.read_prov(insn.mode, insn.target, x, y, ram, false);
                    self.a = match (self.a, operand) {
                        // A constant accumulator inherits the operand's chain:
                        // `LDA #0 / ADC tbl,Y` keeps `tbl` as the origin.
                        (Prov::Const, op @ Prov::Sourced { .. }) => op.transformed(),
                        (a, Prov::Sourced { base, .. }) => a.combine(base),
                        // Const operand means the effective address was
                        // unresolvable — nothing to record.
                        (a, Prov::Const) => a.transformed(),
                    };
                }
            }
            "ASL" | "LSR" | "ROL" | "ROR" => {
                if insn.mode == Mode::Accumulator {
                    self.a = self.a.transformed();
                } else if let Some((eff, ..)) = effective(insn.mode, insn.target, x, y, ram) {
                    self.ram[eff as usize] = self.ram[eff as usize].transformed();
                }
            }
            "INC" | "DEC" => {
                if let Some((eff, ..)) = effective(insn.mode, insn.target, x, y, ram) {
                    self.ram[eff as usize] = self.ram[eff as usize].transformed();
                }
            }
            "INX" | "DEX" => self.x = self.x.transformed(),
            "INY" | "DEY" => self.y = self.y.transformed(),

            // Anything routing data through the stack or an untracked unit
            // loses provenance for the destination register, conservatively.
            "PLA" => self.a = Prov::Const,
            "PLX" => self.x = Prov::Const,
            "PLY" => self.y = Prov::Const,

            // CMP/CPX/CPY/BIT only set flags; branches/JSR/RTS/JMP are control
            // flow; PHA/PHP/NOP/flag ops touch no tracked register's origin.
            _ => {}
        }
    }

    /// Provenance of the value a memory read yields, chaining through any
    /// code-written origin still recorded in the shadow RAM. `log_fetch`
    /// marks the read as a data load (LDA/LDX/LDY): only those count as
    /// sequence fetches for the stream histograms, B4 structure, and the
    /// probe's fetch log — arithmetic operand reads track provenance only.
    fn read_prov(
        &mut self,
        mode: Mode,
        target: Option<u16>,
        x: u8,
        y: u8,
        ram: &[u8],
        log_fetch: bool,
    ) -> Prov {
        if mode == Mode::Immediate {
            return Prov::Const;
        }
        let Some((eff, base, indexed, via)) = effective(mode, target, x, y, ram) else {
            return Prov::Const;
        };
        if let Some(zp) = via
            && log_fetch
        {
            self.note_stream_ptr(zp);
            self.note_stream_read(zp, eff);
            if !self.fetched[usize::from(eff)] && self.fetch_log.len() < 8192 {
                self.fetched[usize::from(eff)] = true;
                self.fetch_log.push((zp, eff, ram[usize::from(eff)]));
            }
        }
        // For an indexed table read, the index register's stream origin links
        // this table entry to the sequence stream that selected it (A2), and
        // its value is the table row — the instrument id / note index (B5).
        let (index_via, index_val) = match mode {
            Mode::AbsoluteX | Mode::ZeroPageX | Mode::IndexedIndirect => (self.x.via(), Some(x)),
            Mode::AbsoluteY | Mode::ZeroPageY | Mode::IndirectIndexed => (self.y.via(), Some(y)),
            _ => (None, None),
        };
        // Chain: if this cell carries a tracked origin (it was written by the
        // player from a table), inherit that deeper origin; otherwise the
        // cell itself is the source (file/table data).
        match self.ram[eff as usize] {
            Prov::Sourced { .. } => self.ram[eff as usize],
            Prov::Const => Prov::Sourced {
                base,
                eff,
                indexed,
                via,
                index_via,
                index_val,
                transformed: false,
                combined: None,
            },
        }
    }

    /// Apply a store: record the SID sink if it targets the register window,
    /// then propagate the source register's provenance into the shadow RAM.
    fn store(&mut self, mode: Mode, target: Option<u16>, x: u8, y: u8, ram: &[u8], src: Prov) {
        let Some((eff, ..)) = effective(mode, target, x, y, ram) else {
            return;
        };
        if (SID_BASE..=SID_LAST).contains(&eff) {
            self.record_sink((eff - SID_BASE) as u8, src);
        }
        self.store_counts[eff as usize] = self.store_counts[eff as usize].saturating_add(1);
        self.ram[eff as usize] = src;
    }

    fn record_sink(&mut self, reg: u8, src: Prov) {
        let Prov::Sourced {
            base,
            indexed,
            transformed,
            index_via,
            index_val,
            combined,
            ..
        } = src
        else {
            self.const_sinks[usize::from(reg)] += 1;
            return;
        };
        if let Some(c) = combined {
            bump(&mut self.sink_combine[usize::from(reg)], c);
        }
        if let Some(p) = index_via {
            bump(&mut self.sink_via[usize::from(reg)], p);
        }
        if let Some(v) = index_val {
            bump(&mut self.sink_idx[usize::from(reg)], v);
            // The `ad` register (offset 5 within each voice) is read indexed by
            // the current instrument; log a change so a note can bind to the
            // instrument in effect at its onset.
            if reg % 7 == 5 && reg < 21 {
                let voice = usize::from(reg / 7);
                let line = &mut self.inst_timeline[voice];
                if line.last().map(|&(_, id)| id) != Some(v) {
                    line.push((self.frame, v));
                }
            }
        }
        let bucket = &mut self.sinks[usize::from(reg)];
        if let Some((_, stat)) = bucket.iter_mut().find(|(b, _)| *b == base) {
            stat.writes += 1;
            stat.indexed |= indexed;
            stat.transformed |= transformed;
        } else {
            bucket.push((
                base,
                SinkStat {
                    writes: 1,
                    indexed,
                    transformed,
                },
            ));
        }
    }

    fn note_stream_ptr(&mut self, zp: u16) {
        bump(&mut self.stream_ptrs, zp);
    }

    /// Track the read address through a stream pointer: a discontinuity (a
    /// backward step, or a forward skip past [`STREAM_JUMP`]) is a pattern /
    /// orderlist boundary, and the address it lands on is a segment start.
    fn note_stream_read(&mut self, zp: u16, eff: u16) {
        let Some(i) = self.structure.iter().position(|s| s.zp == zp) else {
            self.structure.push(StreamStruct {
                zp,
                last: eff,
                starts: vec![(eff, 1)],
                segments: 1,
            });
            return;
        };
        let s = &mut self.structure[i];
        let prev = s.last;
        s.last = eff;
        if eff < prev || eff > prev.wrapping_add(STREAM_JUMP) {
            s.segments += 1;
            if let Some((_, n)) = s.starts.iter_mut().find(|(a, _)| *a == eff) {
                *n += 1;
            } else if s.starts.len() < 4096 {
                s.starts.push((eff, 1));
            }
        }
    }

    /// Record an immediate-mode test/mask applied to a stream-tainted register.
    /// A register is stream-tainted when its provenance still carries the `via`
    /// pointer of an indirect read — i.e. it holds a sequence byte (or one
    /// masked/offset from it). The immediate is a grammar constant.
    fn log_stream_test(&mut self, insn: &Insn) {
        if insn.mode != Mode::Immediate {
            return;
        }
        let reg = match insn.mnemonic {
            "CMP" | "AND" | "ORA" | "EOR" | "ADC" | "SBC" => self.a,
            "CPX" => self.x,
            "CPY" => self.y,
            _ => return,
        };
        if !matches!(reg, Prov::Sourced { via: Some(_), .. }) {
            return;
        }
        bump(&mut self.cmp_tests, (insn.mnemonic, insn.bytes[1]));
    }

    /// The accumulated discovery report (sinks and stream pointers sorted by
    /// write/read count, most-used first).
    #[must_use]
    pub fn report(&self) -> TaintReport {
        let sinks = self
            .sinks
            .iter()
            .enumerate()
            .filter(|(_, b)| !b.is_empty())
            .map(|(reg, bucket)| {
                let mut sources = bucket.clone();
                sources.sort_by_key(|(_, s)| std::cmp::Reverse(s.writes));
                let index_via = self.sink_via[reg]
                    .iter()
                    .max_by_key(|(_, n)| *n)
                    .map(|(p, _)| *p);
                let mut index_vals = self.sink_idx[reg].clone();
                index_vals.sort_by_key(|&(_, n)| std::cmp::Reverse(n));
                let mut combine_srcs = self.sink_combine[reg].clone();
                combine_srcs.sort_by_key(|&(_, n)| std::cmp::Reverse(n));
                SinkReport {
                    reg: reg as u8,
                    sources,
                    const_writes: self.const_sinks[reg],
                    index_via,
                    index_vals,
                    combine_srcs,
                }
            })
            .collect();
        let mut stream_ptrs = self.stream_ptrs.clone();
        stream_ptrs.sort_by_key(|&(_, n)| std::cmp::Reverse(n));
        let mut cmp_tests = self.cmp_tests.clone();
        cmp_tests.sort_by_key(|&(_, n)| std::cmp::Reverse(n));
        let mut structure: Vec<StructReport> = self
            .structure
            .iter()
            .map(|s| {
                let mut starts = s.starts.clone();
                starts.sort_by_key(|&(_, n)| std::cmp::Reverse(n));
                StructReport {
                    zp: s.zp,
                    patterns: s.starts.len(),
                    segments: s.segments,
                    starts,
                }
            })
            .collect();
        structure.sort_by_key(|s| std::cmp::Reverse(s.segments));
        let mut self_mod: Vec<(u16, u32)> = (0..=0xFFFF)
            .filter(|&a| self.executed[usize::from(a)] && self.store_counts[usize::from(a)] > 0)
            .map(|a| (a, self.store_counts[usize::from(a)]))
            .collect();
        self_mod.sort_by_key(|&(_, n)| std::cmp::Reverse(n));
        TaintReport {
            sinks,
            stream_ptrs,
            cmp_tests,
            structure,
            self_mod,
            instruments: self.inst_timeline.clone(),
            fetches: self.fetch_log.clone(),
        }
    }
}

/// Bump `key`'s count in a small linear-scan histogram.
fn bump<K: PartialEq + Copy>(bucket: &mut Vec<(K, u64)>, key: K) {
    if let Some((_, n)) = bucket.iter_mut().find(|(k, _)| *k == key) {
        *n += 1;
    } else {
        bucket.push((key, 1));
    }
}

/// Compute `(effective_addr, base, indexed, via)` for a memory-touching
/// instruction, exactly as the 6502 would from the same registers. Returns
/// `None` for modes that read no data address (implied/accumulator/immediate/
/// relative/JMP-indirect).
fn effective(
    mode: Mode,
    target: Option<u16>,
    x: u8,
    y: u8,
    ram: &[u8],
) -> Option<(u16, u16, bool, Option<u16>)> {
    let base = target?;
    let read16 = |zp: u8| {
        let lo = ram[usize::from(zp)];
        let hi = ram[usize::from(zp.wrapping_add(1))];
        u16::from_le_bytes([lo, hi])
    };
    match mode {
        Mode::Absolute | Mode::ZeroPage => Some((base, base, false, None)),
        Mode::AbsoluteX => Some((base.wrapping_add(u16::from(x)), base, true, None)),
        Mode::AbsoluteY => Some((base.wrapping_add(u16::from(y)), base, true, None)),
        Mode::ZeroPageX => {
            let eff = u16::from((base as u8).wrapping_add(x));
            Some((eff, base, true, None))
        }
        Mode::ZeroPageY => {
            let eff = u16::from((base as u8).wrapping_add(y));
            Some((eff, base, true, None))
        }
        // (zp),Y: pointer at the zp cell, then + Y.
        Mode::IndirectIndexed => {
            let ptr = read16(base as u8);
            Some((ptr.wrapping_add(u16::from(y)), ptr, true, Some(base)))
        }
        // (zp,X): X picks the pointer, no post-index.
        Mode::IndexedIndirect => {
            let ptr = read16((base as u8).wrapping_add(x));
            Some((ptr, ptr, false, Some(base)))
        }
        Mode::Immediate
        | Mode::Implied
        | Mode::Accumulator
        | Mode::Relative
        | Mode::Indirect
        | Mode::Unknown => None,
    }
}

/// Discovery output of a taint run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaintReport {
    /// One entry per SID register written, with its origin-base histogram.
    pub sinks: Vec<SinkReport>,
    /// Zero-page pointer cells used in indirect reads, most-read first.
    pub stream_ptrs: Vec<(u16, u64)>,
    /// Immediate-mode tests applied to stream-tainted bytes — the recovered
    /// sequence-grammar constants — most-tested first.
    pub cmp_tests: Vec<((&'static str, u8), u64)>,
    /// Per-stream-pointer song structure: distinct pattern starts and how
    /// often each is revisited, most-active pointer first.
    pub structure: Vec<StructReport>,
    /// Self-modified code cells (executed bytes that were also stored to),
    /// with patch counts, most-patched first — the live-patched immediates
    /// (tempo dividers, dispatch operands, gated note-on cells).
    pub self_mod: Vec<(u16, u32)>,
    /// Per voice (0..2): the `(frame, instrument id)` changes over the run —
    /// the instrument feeding each voice's `ad` register, for binding a note
    /// to its authored instrument (B5 → export).
    pub instruments: [Vec<(u32, u8)>; 3],
    /// First fetch of each stream-byte address, in fetch order:
    /// `(zp pointer, address, byte)`. The probe's default target list.
    pub fetches: Vec<(u16, u16, u8)>,
}

impl TaintReport {
    /// The instrument id in effect for `voice` at `frame` — the last change at
    /// or before that frame. `None` if the voice took no instrument before it.
    #[must_use]
    pub fn instrument_at(&self, voice: usize, frame: u32) -> Option<u8> {
        let line = self.instruments.get(voice)?;
        line.iter()
            .rev()
            .find(|&&(f, _)| f <= frame)
            .map(|&(_, id)| id)
    }
}

/// Recovered song structure for one stream pointer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructReport {
    /// The zero-page pointer cell.
    pub zp: u16,
    /// Distinct segment-start addresses seen (pattern count).
    pub patterns: usize,
    /// Total segment visits (orderlist length); `segments > patterns` means
    /// patterns are reused.
    pub segments: u64,
    /// Segment-start addresses with visit counts, most-visited first.
    pub starts: Vec<(u16, u64)>,
}

/// What fed one SID register over the run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SinkReport {
    /// Register offset from `$D400`.
    pub reg: u8,
    /// Origin table bases, most-written first.
    pub sources: Vec<(u16, SinkStat)>,
    /// Writes with no trackable origin.
    pub const_writes: u64,
    /// The stream pointer whose bytes most often indexed this register's table
    /// — the per-voice stream→note link (A2). `None` for registers whose value
    /// did not flow from a stream-indexed table.
    pub index_via: Option<u16>,
    /// Distinct table-row indices that fed this register, most-used first —
    /// the instrument ids (ad/sr/pw/ctrl) or note indices (freq) the tune
    /// uses (B5).
    pub index_vals: Vec<(u8, u64)>,
    /// Memory operands arithmetically combined into the written value, most-
    /// frequent first (B6) — the vibrato/porta delta cells, transpose cells
    /// and modulation tables the effects engine works from. Empty for a
    /// register fed by plain table reads (pure transforms like the octave
    /// fold never set a combine source).
    pub combine_srcs: Vec<(u16, u64)>,
}

impl TaintReport {
    /// The most-written origin base feeding SID register offset `reg`
    /// (`0` = `$D400`), if any tracked write reached it.
    #[must_use]
    pub fn top_source(&self, reg: u8) -> Option<u16> {
        self.sinks
            .iter()
            .find(|s| s.reg == reg)?
            .sources
            .first()
            .map(|(base, _)| *base)
    }
}

#[cfg(all(test, feature = "asset-tests"))]
mod tests {
    use super::*;
    use crate::emu::Emulator;

    fn run_taint_on(asset: &str, frames: u32) -> TaintReport {
        let bytes = std::fs::read(format!("../../assets/music/{asset}")).unwrap();
        let header = crate::header::parse(&bytes).unwrap();
        let mut emu = Emulator::new();
        emu.load(&header, &bytes).unwrap();
        emu.call_init(header.init_address, header.start_song, header.songs)
            .unwrap();
        emu.run_taint(header.play_address, frames)
    }

    /// Ground-truth check: the freq registers must trace back to the exact
    /// table the hand-built `CrowtherLayout` reports — `$A5C2` (hi) / `$A5C3`
    /// (lo) for the Chain generation (Ark_Pandora), recovered with no driver
    /// knowledge through the staging cell and the octave-fold `LSR`.
    #[test]
    fn ark_pandora_freq_table_matches_ground_truth() {
        let r = run_taint_on("Ark_Pandora.sid", 400);
        // reg 0x01/0x08/0x0F are v1/v2/v3 freq_hi — all share the one table.
        assert_eq!(r.top_source(0x01), Some(0xA5C2), "v1 freq_hi");
        assert_eq!(r.top_source(0x08), Some(0xA5C2), "v2 freq_hi");
        assert_eq!(r.top_source(0x0F), Some(0xA5C2), "v3 freq_hi");
        assert_eq!(r.top_source(0x00), Some(0xA5C3), "v1 freq_lo");
        // The dominant stream pointer is found.
        assert_eq!(r.stream_ptrs.first().map(|(zp, _)| *zp), Some(0x00AA));
    }

    /// Same, for the Table generation (Cobra): `$FF75` freq table and the
    /// `$60/$61` stream pointer the RE notes record.
    #[test]
    fn cobra_freq_table_and_stream_ptr_match_ground_truth() {
        let r = run_taint_on("Cobra.sid", 400);
        assert_eq!(r.top_source(0x01), Some(0xFF75), "v1 freq_hi");
        assert_eq!(r.top_source(0x0F), Some(0xFF75), "v3 freq_hi");
        assert_eq!(r.stream_ptrs.first().map(|(zp, _)| *zp), Some(0x0060));
    }

    /// Grammar recovery (A1): the immediates the player tests its sequence
    /// bytes against are its own format constants. These are the exact values
    /// the Crowther driver RE extracted by hand, here read off the stream-byte
    /// comparison histogram with no driver knowledge.
    #[test]
    fn grammar_constants_recovered_from_stream_tests() {
        let has = |r: &TaintReport, op: &str, imm: u8| {
            r.cmp_tests.iter().any(|((o, i), _)| *o == op && *i == imm)
        };
        // Crowther Chain (Ark_Pandora): octave fold $8C, note boundary $7F,
        // Chain porta prefix $63.
        let ark = run_taint_on("Ark_Pandora.sid", 600);
        assert!(has(&ark, "CMP", 0x8C), "octave fold $8C");
        assert!(has(&ark, "CMP", 0x7F), "tie / note boundary $7F");
        assert!(has(&ark, "CMP", 0x63), "Chain porta prefix $63");
        // Crowther Table (Cobra): command-range bound $21, Table porta $FF.
        let cobra = run_taint_on("Cobra.sid", 600);
        assert!(has(&cobra, "CMP", 0x21), "cmd_max $21");
        assert!(has(&cobra, "CMP", 0xFF), "Table porta prefix $FF");
    }

    /// Structure (B4): a looping tune revisits pattern starts, so the busiest
    /// stream pointer records more segment visits than distinct starts —
    /// pattern reuse recovered from the read trace alone.
    #[test]
    fn structure_detects_pattern_reuse() {
        let r = run_taint_on("Cobra.sid", 1500);
        let top = r
            .structure
            .first()
            .expect("a stream pointer with structure");
        assert!(top.patterns >= 2, "multiple pattern starts");
        assert!(
            top.segments > top.patterns as u64,
            "patterns are revisited (segments {} > starts {})",
            top.segments,
            top.patterns
        );
        // At least one start is genuinely reused (visited more than once).
        assert!(
            top.starts.iter().any(|(_, n)| *n >= 2),
            "a reused pattern start exists"
        );
    }

    /// Self-modifying code (A3): the live-patched immediates surface as
    /// executed bytes that are also stored to.
    #[test]
    fn self_mod_finds_patched_immediates() {
        // Ark_Pandora's tempo command self-modifies the divider `CMP` immediate
        // at $A07F — exactly CrowtherLayout.divider_imm, with no driver
        // knowledge.
        let ark = run_taint_on("Ark_Pandora.sid", 1500);
        assert!(
            ark.self_mod.iter().any(|(a, _)| *a == 0xA07F),
            "divider immediate $A07F flagged as self-modified"
        );
        // Cobra's jump-table dispatch rewrites its own JSR operand every
        // command — a heavily patched code cell.
        let cobra = run_taint_on("Cobra.sid", 1500);
        assert!(
            cobra.self_mod.iter().any(|(_, n)| *n > 50),
            "a dispatch operand patched many times"
        );
    }

    /// Index-register provenance (A2): the note that selects a voice's freq
    /// table entry is read through the sequence stream pointer, so the freq
    /// sink links back to it — the stream→note connection.
    #[test]
    fn index_via_links_freq_to_stream_pointer() {
        let via = |r: &TaintReport, reg: u8| {
            r.sinks
                .iter()
                .find(|s| s.reg == reg)
                .and_then(|s| s.index_via)
        };
        // Ark_Pandora: freq indexed by a note read through $AA (M1's pointer).
        let ark = run_taint_on("Ark_Pandora.sid", 600);
        assert_eq!(via(&ark, 0x01), Some(0x00AA), "v1 freq indexed via $AA");
        // Cobra: via $60.
        let cobra = run_taint_on("Cobra.sid", 600);
        assert_eq!(via(&cobra, 0x01), Some(0x0060), "v1 freq indexed via $60");
    }

    /// Table-row index recovery (B5): the index value feeding each register is
    /// the note index (freq) or instrument id (ad/sr/ctrl) the tune uses.
    #[test]
    fn index_values_recover_instrument_ids() {
        let ark = run_taint_on("Ark_Pandora.sid", 1500);
        let sink = |reg: u8| ark.sinks.iter().find(|s| s.reg == reg);
        // Crowther's freq table is 12 hi-first semitone pairs, so the
        // top-octave note index stays below 24.
        let freq = sink(0x01).expect("freq_hi sink");
        assert!(!freq.index_vals.is_empty(), "freq index values captured");
        assert!(
            freq.index_vals.iter().all(|(v, _)| *v < 24),
            "freq indices within the 12-semitone top octave: {:?}",
            freq.index_vals
        );
        // ad and sr are read from per-instrument tables by the same index, so
        // they recover the identical instrument-id set.
        let ids = |reg: u8| {
            let mut v: Vec<u8> = sink(reg)
                .expect("sink")
                .index_vals
                .iter()
                .map(|(i, _)| *i)
                .collect();
            v.sort_unstable();
            v
        };
        assert_eq!(
            ids(0x05),
            ids(0x06),
            "ad and sr share the instrument-id set"
        );
    }

    /// Combine sources (B6): memory operands folded into a SID value are the
    /// effect engine's delta cells — and pure transforms (the octave fold's
    /// `LSR` + `SBC #imm`) never produce one, so vibrato/porta separates from
    /// the fold that confounded the single `transformed` bit.
    #[test]
    fn combine_sources_find_effect_cells_not_the_fold() {
        let combines = |r: &TaintReport, reg: u8| -> Vec<u16> {
            r.sinks
                .iter()
                .find(|s| s.reg == reg)
                .map(|s| s.combine_srcs.iter().map(|(a, _)| *a).collect())
                .unwrap_or_default()
        };
        // Hubbard (Commando): vibrato re-reads the freq table ($5428/9) two
        // bytes ahead — the adjacent-semitone entry the delta is scaled from.
        let commando = run_taint_on("Commando.sid", 1500);
        assert!(
            combines(&commando, 0x00).contains(&0x542A),
            "v1 freq_lo vibrato add from the freq table"
        );
        assert!(
            combines(&commando, 0x01).contains(&0x542B),
            "v1 freq_hi vibrato add from the freq table"
        );
        // Crowther (Ark_Pandora): freq_hi passes only the octave fold —
        // immediate arithmetic and shifts — so it must report NO combine
        // source even though the value is transformed.
        let ark = run_taint_on("Ark_Pandora.sid", 1500);
        assert!(
            combines(&ark, 0x01).is_empty(),
            "the octave fold is not a combine: {:?}",
            combines(&ark, 0x01)
        );
        // Its freq_lo does carry per-voice slide/vibrato adds.
        assert!(
            !combines(&ark, 0x00).is_empty(),
            "v1 freq_lo has an effect delta cell"
        );
    }

    /// Per-note instrument binding (B5 → export): the timeline reports the
    /// instrument in effect for a voice at a frame, so a note binds to its
    /// authored instrument.
    #[test]
    fn instrument_timeline_binds_notes() {
        let ark = run_taint_on("Ark_Pandora.sid", 1500);
        // Voice 1 plays instrument 1 throughout (see the test above); the
        // timeline reports it at a mid-song frame.
        assert!(!ark.instruments[0].is_empty(), "v1 timeline non-empty");
        assert_eq!(
            ark.instrument_at(0, 1000),
            Some(1),
            "v1 instrument at f1000"
        );
    }
}
