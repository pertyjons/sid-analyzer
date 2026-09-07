//! Minimal NMOS 6502 disassembler over a RAM image, for driver
//! reverse-engineering (`sid-re dis`). Documented opcodes only — an
//! unrecognized byte decodes as a one-byte `???` so table data interleaved
//! with code never derails the walk.

use std::fmt;

/// 6502 addressing mode, named as in the usual assembler notation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Implied,
    Accumulator,
    Immediate,
    ZeroPage,
    ZeroPageX,
    ZeroPageY,
    Relative,
    Absolute,
    AbsoluteX,
    AbsoluteY,
    Indirect,
    IndexedIndirect,
    IndirectIndexed,
    /// Not a real mode: the byte is no documented opcode.
    Unknown,
}

impl Mode {
    #[must_use]
    pub fn size(self) -> u16 {
        match self {
            Self::Implied | Self::Accumulator | Self::Unknown => 1,
            Self::Immediate
            | Self::ZeroPage
            | Self::ZeroPageX
            | Self::ZeroPageY
            | Self::Relative
            | Self::IndexedIndirect
            | Self::IndirectIndexed => 2,
            Self::Absolute | Self::AbsoluteX | Self::AbsoluteY | Self::Indirect => 3,
        }
    }
}

/// One decoded instruction (or one undecodable byte).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Insn {
    pub addr: u16,
    pub mnemonic: &'static str,
    pub mode: Mode,
    /// The raw bytes; only the first `mode.size()` entries are meaningful.
    pub bytes: [u8; 3],
    /// The memory address this instruction touches, when one is statically
    /// known: zero-page / absolute operands (indexed or not), indirect
    /// pointers, and branch destinations. `None` for implied, accumulator,
    /// and immediate modes. This is what annotation hooks key on.
    pub target: Option<u16>,
}

impl Insn {
    #[must_use]
    pub fn size(&self) -> u16 {
        self.mode.size()
    }
}

impl fmt::Display for Insn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let raw: Vec<String> = self.bytes[..self.size() as usize]
            .iter()
            .map(|b| format!("{b:02X}"))
            .collect();
        write!(
            f,
            "${:04X}  {:<9} {}",
            self.addr,
            raw.join(" "),
            self.mnemonic
        )?;
        let lo = self.bytes[1];
        let word = u16::from_le_bytes([self.bytes[1], self.bytes[2]]);
        match self.mode {
            Mode::Implied | Mode::Unknown => Ok(()),
            Mode::Accumulator => write!(f, " A"),
            Mode::Immediate => write!(f, " #${lo:02X}"),
            Mode::ZeroPage => write!(f, " ${lo:02X}"),
            Mode::ZeroPageX => write!(f, " ${lo:02X},X"),
            Mode::ZeroPageY => write!(f, " ${lo:02X},Y"),
            Mode::Relative => match self.target {
                Some(dst) => write!(f, " ${dst:04X}"),
                None => Ok(()),
            },
            Mode::Absolute => write!(f, " ${word:04X}"),
            Mode::AbsoluteX => write!(f, " ${word:04X},X"),
            Mode::AbsoluteY => write!(f, " ${word:04X},Y"),
            Mode::Indirect => write!(f, " (${word:04X})"),
            Mode::IndexedIndirect => write!(f, " (${lo:02X},X)"),
            Mode::IndirectIndexed => write!(f, " (${lo:02X}),Y"),
        }
    }
}

/// Decode the single instruction at `addr`. `ram` is normally a full 64 KiB
/// image; operand reads wrap at the address-space boundary like the CPU's PC
/// does. Reads past the end of a shorter image yield `0` (BRK) rather than
/// panicking, so a truncated dump degrades to visibly-wrong output.
#[must_use]
pub fn decode(ram: &[u8], addr: u16) -> Insn {
    let byte = |off: u16| {
        ram.get(addr.wrapping_add(off) as usize & 0xFFFF)
            .copied()
            .unwrap_or(0)
    };
    let op = byte(0);
    let (mnemonic, mode) = opcode(op);
    let mut bytes = [op, 0, 0];
    for (i, slot) in bytes
        .iter_mut()
        .enumerate()
        .skip(1)
        .take(mode.size() as usize - 1)
    {
        *slot = byte(i as u16);
    }
    let lo = bytes[1];
    let word = u16::from_le_bytes([bytes[1], bytes[2]]);
    let target = match mode {
        Mode::ZeroPage | Mode::ZeroPageX | Mode::ZeroPageY => Some(u16::from(lo)),
        Mode::IndexedIndirect | Mode::IndirectIndexed => Some(u16::from(lo)),
        Mode::Absolute | Mode::AbsoluteX | Mode::AbsoluteY | Mode::Indirect => Some(word),
        Mode::Relative => Some(
            addr.wrapping_add(2)
                .wrapping_add(i16::from(lo as i8) as u16),
        ),
        Mode::Implied | Mode::Accumulator | Mode::Immediate | Mode::Unknown => None,
    };
    Insn {
        addr,
        mnemonic,
        mode,
        bytes,
        target,
    }
}

/// Decode `[lo, hi)`, one instruction at a time. The walk is linear: it does
/// not follow control flow, so a misaligned start self-corrects within a few
/// bytes (the usual 6502 disassembly property).
#[must_use]
pub fn disassemble(ram: &[u8], lo: u16, hi: u16) -> Vec<Insn> {
    let mut out = Vec::new();
    let mut pc = lo;
    while pc < hi {
        let insn = decode(ram, pc);
        let next = pc.saturating_add(insn.size());
        out.push(insn);
        if next == pc {
            break;
        }
        pc = next;
    }
    out
}

/// Documented-opcode table: mnemonic + addressing mode.
#[must_use]
fn opcode(op: u8) -> (&'static str, Mode) {
    use Mode::{
        Absolute as Abs, AbsoluteX as Abx, AbsoluteY as Aby, Accumulator as Acc, Immediate as Imm,
        Implied as Imp, IndexedIndirect as Izx, Indirect as Ind, IndirectIndexed as Izy,
        Relative as Rel, Unknown, ZeroPage as Zp, ZeroPageX as Zpx, ZeroPageY as Zpy,
    };
    match op {
        0x00 => ("BRK", Imp),
        0x01 => ("ORA", Izx),
        0x05 => ("ORA", Zp),
        0x06 => ("ASL", Zp),
        0x08 => ("PHP", Imp),
        0x09 => ("ORA", Imm),
        0x0A => ("ASL", Acc),
        0x0D => ("ORA", Abs),
        0x0E => ("ASL", Abs),
        0x10 => ("BPL", Rel),
        0x11 => ("ORA", Izy),
        0x15 => ("ORA", Zpx),
        0x16 => ("ASL", Zpx),
        0x18 => ("CLC", Imp),
        0x19 => ("ORA", Aby),
        0x1D => ("ORA", Abx),
        0x1E => ("ASL", Abx),
        0x20 => ("JSR", Abs),
        0x21 => ("AND", Izx),
        0x24 => ("BIT", Zp),
        0x25 => ("AND", Zp),
        0x26 => ("ROL", Zp),
        0x28 => ("PLP", Imp),
        0x29 => ("AND", Imm),
        0x2A => ("ROL", Acc),
        0x2C => ("BIT", Abs),
        0x2D => ("AND", Abs),
        0x2E => ("ROL", Abs),
        0x30 => ("BMI", Rel),
        0x31 => ("AND", Izy),
        0x35 => ("AND", Zpx),
        0x36 => ("ROL", Zpx),
        0x38 => ("SEC", Imp),
        0x39 => ("AND", Aby),
        0x3D => ("AND", Abx),
        0x3E => ("ROL", Abx),
        0x40 => ("RTI", Imp),
        0x41 => ("EOR", Izx),
        0x45 => ("EOR", Zp),
        0x46 => ("LSR", Zp),
        0x48 => ("PHA", Imp),
        0x49 => ("EOR", Imm),
        0x4A => ("LSR", Acc),
        0x4C => ("JMP", Abs),
        0x4D => ("EOR", Abs),
        0x4E => ("LSR", Abs),
        0x50 => ("BVC", Rel),
        0x51 => ("EOR", Izy),
        0x55 => ("EOR", Zpx),
        0x56 => ("LSR", Zpx),
        0x58 => ("CLI", Imp),
        0x59 => ("EOR", Aby),
        0x5D => ("EOR", Abx),
        0x5E => ("LSR", Abx),
        0x60 => ("RTS", Imp),
        0x61 => ("ADC", Izx),
        0x65 => ("ADC", Zp),
        0x66 => ("ROR", Zp),
        0x68 => ("PLA", Imp),
        0x69 => ("ADC", Imm),
        0x6A => ("ROR", Acc),
        0x6C => ("JMP", Ind),
        0x6D => ("ADC", Abs),
        0x6E => ("ROR", Abs),
        0x70 => ("BVS", Rel),
        0x71 => ("ADC", Izy),
        0x75 => ("ADC", Zpx),
        0x76 => ("ROR", Zpx),
        0x78 => ("SEI", Imp),
        0x79 => ("ADC", Aby),
        0x7D => ("ADC", Abx),
        0x7E => ("ROR", Abx),
        0x81 => ("STA", Izx),
        0x84 => ("STY", Zp),
        0x85 => ("STA", Zp),
        0x86 => ("STX", Zp),
        0x88 => ("DEY", Imp),
        0x8A => ("TXA", Imp),
        0x8C => ("STY", Abs),
        0x8D => ("STA", Abs),
        0x8E => ("STX", Abs),
        0x90 => ("BCC", Rel),
        0x91 => ("STA", Izy),
        0x94 => ("STY", Zpx),
        0x95 => ("STA", Zpx),
        0x96 => ("STX", Zpy),
        0x98 => ("TYA", Imp),
        0x99 => ("STA", Aby),
        0x9A => ("TXS", Imp),
        0x9D => ("STA", Abx),
        0xA0 => ("LDY", Imm),
        0xA1 => ("LDA", Izx),
        0xA2 => ("LDX", Imm),
        0xA4 => ("LDY", Zp),
        0xA5 => ("LDA", Zp),
        0xA6 => ("LDX", Zp),
        0xA8 => ("TAY", Imp),
        0xA9 => ("LDA", Imm),
        0xAA => ("TAX", Imp),
        0xAC => ("LDY", Abs),
        0xAD => ("LDA", Abs),
        0xAE => ("LDX", Abs),
        0xB0 => ("BCS", Rel),
        0xB1 => ("LDA", Izy),
        0xB4 => ("LDY", Zpx),
        0xB5 => ("LDA", Zpx),
        0xB6 => ("LDX", Zpy),
        0xB8 => ("CLV", Imp),
        0xB9 => ("LDA", Aby),
        0xBA => ("TSX", Imp),
        0xBC => ("LDY", Abx),
        0xBD => ("LDA", Abx),
        0xBE => ("LDX", Aby),
        0xC0 => ("CPY", Imm),
        0xC1 => ("CMP", Izx),
        0xC4 => ("CPY", Zp),
        0xC5 => ("CMP", Zp),
        0xC6 => ("DEC", Zp),
        0xC8 => ("INY", Imp),
        0xC9 => ("CMP", Imm),
        0xCA => ("DEX", Imp),
        0xCC => ("CPY", Abs),
        0xCD => ("CMP", Abs),
        0xCE => ("DEC", Abs),
        0xD0 => ("BNE", Rel),
        0xD1 => ("CMP", Izy),
        0xD5 => ("CMP", Zpx),
        0xD6 => ("DEC", Zpx),
        0xD8 => ("CLD", Imp),
        0xD9 => ("CMP", Aby),
        0xDD => ("CMP", Abx),
        0xDE => ("DEC", Abx),
        0xE0 => ("CPX", Imm),
        0xE1 => ("SBC", Izx),
        0xE4 => ("CPX", Zp),
        0xE5 => ("SBC", Zp),
        0xE6 => ("INC", Zp),
        0xE8 => ("INX", Imp),
        0xE9 => ("SBC", Imm),
        0xEA => ("NOP", Imp),
        0xEC => ("CPX", Abs),
        0xED => ("SBC", Abs),
        0xEE => ("INC", Abs),
        0xF0 => ("BEQ", Rel),
        0xF1 => ("SBC", Izy),
        0xF5 => ("SBC", Zpx),
        0xF6 => ("INC", Zpx),
        0xF8 => ("SED", Imp),
        0xF9 => ("SBC", Aby),
        0xFD => ("SBC", Abx),
        0xFE => ("INC", Abx),
        _ => ("???", Unknown),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(at: u16, code: &[u8]) -> Vec<u8> {
        let mut ram = vec![0u8; 0x10000];
        ram[at as usize..at as usize + code.len()].copy_from_slice(code);
        ram
    }

    #[test]
    fn decodes_a_basic_sequence() {
        // LDA #$00 / STA $D418 / RTS
        let ram = image(0x1000, &[0xA9, 0x00, 0x8D, 0x18, 0xD4, 0x60]);
        let insns = disassemble(&ram, 0x1000, 0x1006);
        let text: Vec<String> = insns.iter().map(ToString::to_string).collect();
        assert_eq!(
            text,
            vec![
                "$1000  A9 00     LDA #$00",
                "$1002  8D 18 D4  STA $D418",
                "$1005  60        RTS",
            ]
        );
        assert_eq!(insns[1].target, Some(0xD418));
    }

    #[test]
    fn branch_targets_resolve_both_directions() {
        // $2000: BPL +2 → $2004; $2002: BNE -4 → $2000
        let ram = image(0x2000, &[0x10, 0x02, 0xD0, 0xFC]);
        let insns = disassemble(&ram, 0x2000, 0x2004);
        assert_eq!(insns[0].target, Some(0x2004));
        assert_eq!(insns[1].target, Some(0x2000));
    }

    #[test]
    fn unknown_opcode_is_one_byte_and_resyncs() {
        // $80 is undocumented; the next instruction must still decode.
        let ram = image(0x3000, &[0x80, 0xEA]);
        let insns = disassemble(&ram, 0x3000, 0x3002);
        assert_eq!(insns[0].mnemonic, "???");
        assert_eq!(insns[0].size(), 1);
        assert_eq!(insns[1].mnemonic, "NOP");
    }

    #[test]
    fn indexed_and_indirect_modes_format_like_the_python_tool() {
        let ram = image(
            0x4000,
            &[
                0xB9, 0x34, 0x12, // LDA $1234,Y
                0xB1, 0xFC, // LDA ($FC),Y
                0x6C, 0x00, 0x80, // JMP ($8000)
                0xB5, 0x40, // LDA $40,X
            ],
        );
        let insns = disassemble(&ram, 0x4000, 0x400A);
        let text: Vec<String> = insns.iter().map(ToString::to_string).collect();
        assert_eq!(
            text,
            vec![
                "$4000  B9 34 12  LDA $1234,Y",
                "$4003  B1 FC     LDA ($FC),Y",
                "$4005  6C 00 80  JMP ($8000)",
                "$4008  B5 40     LDA $40,X",
            ]
        );
        assert_eq!(insns[1].target, Some(0x00FC));
    }

    #[test]
    fn operand_reads_wrap_at_the_address_space_boundary() {
        let mut ram = vec![0u8; 0x10000];
        ram[0xFFFF] = 0xAD; // LDA abs — operand bytes wrap to $0000/$0001
        ram[0x0000] = 0x34;
        ram[0x0001] = 0x12;
        let insn = decode(&ram, 0xFFFF);
        assert_eq!(insn.target, Some(0x1234));
    }
}
