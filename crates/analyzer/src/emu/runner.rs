use crate::emu::bus::Bus;
use crate::emu::capture::SidBusEvent;
use crate::trace::{ChipCycle, CpuCycles, RegisterRead, RegisterWrite, SubFrameOffset};
use mos6502::cpu::CPU;
use mos6502::instruction::Nmos6502;
use mos6502::registers::Status;

/// JSR-emulation pushes `SENTINEL - 1` onto the stack before jumping, so the
/// matching RTS lands here and the loop terminates.
const SENTINEL: u16 = 0xFFFF;

/// Catches runaway code. A PAL frame is ~19,656 cycles; this allows ~50
/// frames of CPU time per `call` before giving up.
const CYCLE_GUARD: u64 = 1_000_000;

/// How often to check the optional wall-clock deadline inside the step loop.
/// At ~50M emulated cycles/sec, 1024 instructions is ~20 µs of wall time,
/// so deadline-overrun is bounded tightly without adding measurable
/// per-step overhead.
const WALL_CHECK_INTERVAL: u32 = 1024;
const KERNAL_IRQ_CONTINUE: u16 = 0xEA31;
const KERNAL_IRQ_RETURN_WITH_ACK: u16 = 0xEA7E;
const KERNAL_IRQ_RETURN: u16 = 0xEA81;
const KERNAL_NMI_RETURN: u16 = 0xFEBC;

pub type Cpu = CPU<Bus, Nmos6502>;

#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error("subroutine at ${target:04X} exceeded {CYCLE_GUARD} cycles without returning")]
    CycleGuardTripped { target: u16 },
    #[error("subroutine at ${target:04X} exceeded its wall-clock deadline")]
    WallDeadlineExceeded { target: u16 },
    #[error("subroutine at ${target:04X} jammed the CPU with opcode ${opcode:02X} at ${pc:04X}")]
    CpuJam { target: u16, pc: u16, opcode: u8 },
    #[error(
        "subroutine at ${target:04X} hit unimplemented opcode ${opcode:02X} at ${pc:04X} \
         (mos6502 skips unstable illegal opcodes)"
    )]
    UnimplementedOpcode { target: u16, pc: u16, opcode: u8 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CapturedCall {
    pub events: Vec<SidBusEvent>,
    pub writes: Vec<RegisterWrite>,
    pub reads: Vec<RegisterRead>,
    pub duration: CpuCycles,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum InterruptEntry {
    KernalVector,
    HardwareVector,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CallKind {
    Subroutine,
    Interrupt(InterruptEntry),
}

#[must_use]
pub fn make_cpu() -> Cpu {
    CPU::new(Bus::new(), Nmos6502)
}

/// Invoke a subroutine at `target` with the given register values, running
/// until the matching RTS returns to `SENTINEL` (or the cycle guard trips).
/// Returns the writes captured during the call, each stamped with the CPU
/// cycle count at which the host instruction ran.
pub fn call(
    cpu: &mut Cpu,
    target: u16,
    a: u8,
    x: u8,
    y: u8,
) -> Result<Vec<RegisterWrite>, RunError> {
    call_with_deadline(cpu, target, a, x, y, None)
}

/// Like [`call`] but bails out with `WallDeadlineExceeded` if the optional
/// wall-clock deadline passes. Needed for the corpus scanner: some HVSC
/// files hit step loops where `cpu.cycles` doesn't advance (mos6502
/// edge-case), so the `CYCLE_GUARD` alone can't bound runtime.
pub fn call_with_deadline(
    cpu: &mut Cpu,
    target: u16,
    a: u8,
    x: u8,
    y: u8,
    deadline: Option<std::time::Instant>,
) -> Result<Vec<RegisterWrite>, RunError> {
    let call_origin = cpu.memory.digital_sid.cycle();
    Ok(call_captured_with_deadline(cpu, target, a, x, y, call_origin, deadline)?.writes)
}

pub(crate) fn call_captured_with_deadline(
    cpu: &mut Cpu,
    target: u16,
    a: u8,
    x: u8,
    y: u8,
    call_origin: ChipCycle,
    deadline: Option<std::time::Instant>,
) -> Result<CapturedCall, RunError> {
    call_captured(
        cpu,
        target,
        a,
        x,
        y,
        call_origin,
        deadline,
        CallKind::Subroutine,
        None,
    )
}

pub(crate) fn call_captured_observed<F>(
    cpu: &mut Cpu,
    target: u16,
    call_origin: ChipCycle,
    observer: &mut F,
) -> Result<CapturedCall, RunError>
where
    F: FnMut(&Cpu),
{
    call_captured(
        cpu,
        target,
        0,
        0,
        0,
        call_origin,
        None,
        CallKind::Subroutine,
        Some(observer),
    )
}

pub(crate) fn call_interrupt_captured_with_deadline(
    cpu: &mut Cpu,
    target: u16,
    entry: InterruptEntry,
    call_origin: ChipCycle,
    deadline: Option<std::time::Instant>,
) -> Result<CapturedCall, RunError> {
    call_captured(
        cpu,
        target,
        cpu.registers.accumulator,
        cpu.registers.index_x,
        cpu.registers.index_y,
        call_origin,
        deadline,
        CallKind::Interrupt(entry),
        None,
    )
}

#[allow(clippy::too_many_arguments)]
fn call_captured(
    cpu: &mut Cpu,
    target: u16,
    a: u8,
    x: u8,
    y: u8,
    call_origin: ChipCycle,
    deadline: Option<std::time::Instant>,
    kind: CallKind,
    mut observer: Option<&mut dyn FnMut(&Cpu)>,
) -> Result<CapturedCall, RunError> {
    cpu.memory.frame_events.clear();
    cpu.memory.offset = SubFrameOffset(0);
    cpu.memory.chip_cycle = call_origin;

    cpu.registers.accumulator = a;
    cpu.registers.index_x = x;
    cpu.registers.index_y = y;

    match kind {
        CallKind::Subroutine => push_return_address(cpu, SENTINEL.wrapping_sub(1)),
        CallKind::Interrupt(entry) => push_interrupt_frame(cpu, entry),
    }
    cpu.registers.program_counter = target;

    let start_cycles = cpu.cycles;
    let mut steps_since_check: u32 = 0;
    while cpu.registers.program_counter != SENTINEL {
        if kind == CallKind::Interrupt(InterruptEntry::KernalVector)
            && matches!(
                cpu.registers.program_counter,
                KERNAL_IRQ_CONTINUE
                    | KERNAL_IRQ_RETURN_WITH_ACK
                    | KERNAL_IRQ_RETURN
                    | KERNAL_NMI_RETURN
            )
        {
            finish_kernal_interrupt(cpu);
            break;
        }
        let elapsed = cpu.cycles - start_cycles;
        if elapsed >= CYCLE_GUARD {
            return Err(RunError::CycleGuardTripped { target });
        }
        steps_since_check = steps_since_check.wrapping_add(1);
        if steps_since_check >= WALL_CHECK_INTERVAL {
            steps_since_check = 0;
            if let Some(d) = deadline
                && std::time::Instant::now() >= d
            {
                return Err(RunError::WallDeadlineExceeded { target });
            }
        }
        cpu.memory.offset = SubFrameOffset(elapsed as u32);
        cpu.memory.total_cycles = cpu.cycles;
        cpu.memory.chip_cycle = ChipCycle(call_origin.0 + elapsed);
        if let Some(observer) = observer.as_mut() {
            observer(cpu);
        }
        let pc = cpu.registers.program_counter;
        let opcode = cpu.memory.ram[pc as usize];
        if is_jam_opcode(opcode) {
            return Err(RunError::CpuJam { target, pc, opcode });
        }
        // `single_step` returns false when the opcode failed to decode
        // (mos6502 skips the unstable illegal opcodes) — PC and `cpu.cycles`
        // then never advance, so without this check the loop would spin
        // forever with the cycle guard unable to trip.
        if !cpu.single_step() {
            let pc = cpu.registers.program_counter;
            let opcode = cpu.memory.ram[pc as usize];
            return Err(RunError::UnimplementedOpcode { target, pc, opcode });
        }
    }

    let duration = CpuCycles(cpu.cycles - start_cycles);
    let events = std::mem::take(&mut cpu.memory.frame_events);
    let writes = events.iter().filter_map(|event| event.as_write()).collect();
    let reads = events.iter().filter_map(|event| event.as_read()).collect();
    Ok(CapturedCall {
        events,
        writes,
        reads,
        duration,
    })
}

fn is_jam_opcode(opcode: u8) -> bool {
    matches!(
        opcode,
        0x02 | 0x12 | 0x22 | 0x32 | 0x42 | 0x52 | 0x62 | 0x72 | 0x92 | 0xB2 | 0xD2 | 0xF2
    )
}

/// Like [`call`] but invokes `probe(cpu)` *before* each instruction, giving a
/// driver reverse-engineering micro-trace per-instruction visibility (program
/// counter, registers, RAM) that the batch [`call`] hides. No deadline or
/// write collection — strictly a debug driver; production tracing uses
/// [`call`]. The cycle guard still bounds runaway code.
pub fn call_stepwise<F>(cpu: &mut Cpu, target: u16, a: u8, x: u8, y: u8, mut probe: F)
where
    F: FnMut(&Cpu),
{
    cpu.memory.frame_events.clear();
    cpu.memory.offset = SubFrameOffset(0);

    cpu.registers.accumulator = a;
    cpu.registers.index_x = x;
    cpu.registers.index_y = y;

    push_return_address(cpu, SENTINEL.wrapping_sub(1));
    cpu.registers.program_counter = target;

    let start_cycles = cpu.cycles;
    let call_origin = cpu.memory.digital_sid.cycle();
    while cpu.registers.program_counter != SENTINEL {
        if cpu.cycles - start_cycles >= CYCLE_GUARD {
            return;
        }
        probe(cpu);
        cpu.memory.total_cycles = cpu.cycles;
        cpu.memory.chip_cycle = ChipCycle(call_origin.0 + cpu.cycles - start_cycles);
        // An undecodable opcode never advances PC or cycles; stop like the
        // cycle guard does rather than spinning forever.
        if !cpu.single_step() {
            return;
        }
    }
}

/// High byte first, then low byte — matches the order JSR pushes.
fn push_return_address(cpu: &mut Cpu, addr: u16) {
    let hi = (addr >> 8) as u8;
    let lo = (addr & 0xFF) as u8;
    push(cpu, hi);
    push(cpu, lo);
}

fn push_interrupt_frame(cpu: &mut Cpu, entry: InterruptEntry) {
    push_return_address(cpu, SENTINEL);
    push(cpu, (cpu.registers.status.bits() | 0x20) & !0x10);
    cpu.registers.status.insert(Status::PS_DISABLE_INTERRUPTS);
    if entry == InterruptEntry::KernalVector {
        push(cpu, cpu.registers.accumulator);
        push(cpu, cpu.registers.index_x);
        push(cpu, cpu.registers.index_y);
    }
}

fn finish_kernal_interrupt(cpu: &mut Cpu) {
    cpu.registers.index_y = pull(cpu);
    cpu.registers.index_x = pull(cpu);
    cpu.registers.accumulator = pull(cpu);
    cpu.registers.status = Status::from_bits_truncate(pull(cpu));
    let lo = pull(cpu);
    let hi = pull(cpu);
    cpu.registers.program_counter = u16::from_le_bytes([lo, hi]);
}

fn push(cpu: &mut Cpu, value: u8) {
    let sp_addr = cpu.registers.stack_pointer.to_u16();
    cpu.memory.ram[sp_addr as usize] = value;
    cpu.registers.stack_pointer.decrement();
}

fn pull(cpu: &mut Cpu) -> u8 {
    cpu.registers.stack_pointer.increment();
    let address = cpu.registers.stack_pointer.to_u16();
    cpu.memory.ram[address as usize]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_jam_opcode_has_a_distinct_error() {
        for opcode in [
            0x02, 0x12, 0x22, 0x32, 0x42, 0x52, 0x62, 0x72, 0x92, 0xB2, 0xD2, 0xF2,
        ] {
            let mut cpu = make_cpu();
            cpu.memory.ram[0x1000] = opcode;
            let result = call(&mut cpu, 0x1000, 0, 0, 0);
            assert!(matches!(
                result,
                Err(RunError::CpuJam {
                    target: 0x1000,
                    pc: 0x1000,
                    opcode: actual,
                }) if actual == opcode
            ));
        }
    }

    #[test]
    fn unimplemented_opcode_fails_loudly_instead_of_hanging() {
        let mut cpu = make_cpu();
        // SHA (zp),Y — one of the unstable illegal opcodes mos6502 skips
        // (decode returns None, PC and cycles never advance).
        cpu.memory.ram[0x1000] = 0x93;
        let result = call(&mut cpu, 0x1000, 0, 0, 0);
        assert!(matches!(
            result,
            Err(RunError::UnimplementedOpcode {
                target: 0x1000,
                pc: 0x1000,
                opcode: 0x93,
            })
        ));
    }

    #[test]
    fn kernal_interrupt_vector_recognizes_rom_return_entries() {
        for return_address in [
            KERNAL_IRQ_CONTINUE,
            KERNAL_IRQ_RETURN_WITH_ACK,
            KERNAL_IRQ_RETURN,
            KERNAL_NMI_RETURN,
        ] {
            let mut cpu = make_cpu();
            let [return_lo, return_hi] = return_address.to_le_bytes();
            cpu.memory.ram[0x1000..0x1008]
                .copy_from_slice(&[0xA9, 0xAA, 0x8D, 0x00, 0xD4, 0x4C, return_lo, return_hi]);
            cpu.registers.accumulator = 0x11;
            cpu.registers.index_x = 0x22;
            cpu.registers.index_y = 0x33;
            let stack_pointer = cpu.registers.stack_pointer;
            let captured = call_interrupt_captured_with_deadline(
                &mut cpu,
                0x1000,
                InterruptEntry::KernalVector,
                ChipCycle(0),
                None,
            )
            .unwrap();
            assert_eq!(captured.writes.len(), 1);
            assert_eq!(captured.writes[0].value, 0xAA);
            assert_eq!(cpu.registers.accumulator, 0x11);
            assert_eq!(cpu.registers.index_x, 0x22);
            assert_eq!(cpu.registers.index_y, 0x33);
            assert_eq!(cpu.registers.stack_pointer, stack_pointer);
        }
    }

    #[test]
    fn hardware_interrupt_vector_returns_through_rti() {
        let mut cpu = make_cpu();
        cpu.memory.ram[0x1000..0x1006].copy_from_slice(&[0xA9, 0xAA, 0x8D, 0x00, 0xD4, 0x40]);
        let stack_pointer = cpu.registers.stack_pointer;
        let captured = call_interrupt_captured_with_deadline(
            &mut cpu,
            0x1000,
            InterruptEntry::HardwareVector,
            ChipCycle(0),
            None,
        )
        .unwrap();
        assert_eq!(captured.writes.len(), 1);
        assert_eq!(captured.writes[0].value, 0xAA);
        assert_eq!(cpu.registers.stack_pointer, stack_pointer);
    }
}
