use crate::analysis::SystemClock;
use crate::emu::CiaTimerPeriod;
use crate::emu::capture::{
    CapturedSidChip, CheckpointId, CheckpointRef, EventTimestampQuality, ExternalInputModelStatus,
    MirrorModelStatus, SidAddress, SidAddressClass, SidBusAccess, SidBusCheckpoint, SidBusEvent,
    SidBusEventId, SidCallId, SidChipId, SidDataLatch, resolved_register,
};
use crate::emu::sid::DigitalSid;
use crate::header::{Header, SidModel};
use crate::trace::{ChipCycle, SID_REGISTER_LAST, SidRegister, SubFrameOffset};

const RAM_SIZE: usize = 0x1_0000;
const SID_BASE: u16 = 0xD400;
const SID_LAST: u16 = SID_BASE + SID_REGISTER_LAST.0 as u16;
const VIC_RASTER_LINE: u16 = 0xD012;
#[cfg(test)]
const V3_READ_FIRST: u16 = 0xD41B;
#[cfg(test)]
const V3_READ_LAST: u16 = 0xD41C;

/// CIA timer A latch registers. CIA-timed players program their call rate
/// here during `init`; the scheduler adopts the latched period. Base
/// addresses only — the 16-byte CIA mirrors are not snooped.
const CIA1_TIMER_A_LO: u16 = 0xDC04;
const CIA1_TIMER_A_HI: u16 = 0xDC05;
const CIA2_TIMER_A_LO: u16 = 0xDD04;
const CIA2_TIMER_A_HI: u16 = 0xDD05;

/// Incompletely decoded SID mirrors: `$D420-$D7FF` all address the chip on a
/// real C64. Offsets `$1D-$1F` do not resolve to modeled SID registers.
const SID_MIRROR_FIRST: u16 = 0xD420;
const SID_MIRROR_LAST: u16 = 0xD7FF;

struct AdditionalSid {
    descriptor: CapturedSidChip,
    digital_sid: DigitalSid,
    last_write: SidDataLatch,
}

#[derive(Clone, Copy)]
struct SidTarget {
    chip: SidChipId,
    class: SidAddressClass,
    register: Option<SidRegister>,
}

/// 64 KiB RAM with traps for every SID register window declared by the header.
///
/// Read policy for each SID window: oscillator/envelope reads come from its digital
/// core; the write-only registers `$D400-$D418` read back the shared
/// data-bus latch (the last value written to any SID register — hardware
/// decays it to zero within milliseconds, which is not modeled), so RMW
/// digis (`inc $d418`) behave as on hardware; POT lines read `$FF`.
pub struct Bus {
    pub ram: Box<[u8; RAM_SIZE]>,
    pub(crate) frame_events: Vec<SidBusEvent>,
    /// Stamped onto each captured write so writes from the same instruction
    /// share a step value and writes from later instructions sort higher.
    pub offset: SubFrameOffset,
    /// Host CPU cycle count at the current instruction, mirrored from
    /// `cpu.cycles` by the runner before each `single_step`. Raw `u64`
    /// because it shadows the mos6502 cycle counter (an FFI mirror).
    pub(crate) total_cycles: u64,
    pub(crate) chip_cycle: ChipCycle,
    pub(crate) digital_sid: DigitalSid,
    additional_sids: Vec<AdditionalSid>,
    system_clock: SystemClock,
    /// Last bytes written to the CIA 1/2 timer A latches (lo, hi).
    pub(crate) cia1_timer_a: [Option<u8>; 2],
    pub(crate) cia2_timer_a: [Option<u8>; 2],
    monitored_cia_period: Option<CiaTimerPeriod>,
    cia_period_changed: bool,
    /// Primary SID data-bus latch. Each additional SID retains an independent latch.
    pub(crate) last_sid_write: u8,
    /// Writes to unresolved SID mirror offsets. A nonzero count is surfaced
    /// as a diagnostic.
    pub(crate) sid_unmodeled_mirror_writes: u64,
    sid_has_unmodeled_mirror_access: bool,
    next_event_id: SidBusEventId,
    current_call: SidCallId,
}

impl Bus {
    #[must_use]
    pub fn new() -> Self {
        Self {
            ram: Box::new([0; RAM_SIZE]),
            frame_events: Vec::new(),
            offset: SubFrameOffset(0),
            total_cycles: 0,
            chip_cycle: ChipCycle(0),
            digital_sid: DigitalSid::new(),
            additional_sids: Vec::new(),
            system_clock: SystemClock::Pal,
            cia1_timer_a: [None; 2],
            cia2_timer_a: [None; 2],
            monitored_cia_period: None,
            cia_period_changed: false,
            last_sid_write: 0,
            sid_unmodeled_mirror_writes: 0,
            sid_has_unmodeled_mirror_access: false,
            next_event_id: SidBusEventId(0),
            current_call: SidCallId::Init,
        }
    }

    pub fn load(&mut self, addr: u16, data: &[u8]) {
        let start = addr as usize;
        let end = start + data.len();
        assert!(end <= RAM_SIZE, "data load overruns RAM");
        self.ram[start..end].copy_from_slice(data);
    }

    pub(crate) fn configure_sids(&mut self, header: &Header) {
        self.digital_sid = DigitalSid::with_model(header.flags.sid_model);
        self.additional_sids.clear();
        if let Some(address) = header.second_sid_address {
            let model = header.flags.sid_model_2.unwrap_or(SidModel::Unknown);
            self.additional_sids.push(AdditionalSid {
                descriptor: CapturedSidChip::additional(SidChipId(1), address, model),
                digital_sid: DigitalSid::with_model(model),
                last_write: SidDataLatch(0),
            });
        }
        if let Some(address) = header.third_sid_address {
            let model = header.flags.sid_model_3.unwrap_or(SidModel::Unknown);
            self.additional_sids.push(AdditionalSid {
                descriptor: CapturedSidChip::additional(SidChipId(2), address, model),
                digital_sid: DigitalSid::with_model(model),
                last_write: SidDataLatch(0),
            });
        }
    }

    #[must_use]
    pub(crate) fn captured_sid_chips(&self) -> Vec<CapturedSidChip> {
        let mut chips = Vec::with_capacity(1 + self.additional_sids.len());
        chips.push(CapturedSidChip::primary(self.digital_sid.model()));
        chips.extend(self.additional_sids.iter().map(|sid| sid.descriptor));
        chips
    }

    pub(crate) fn clock_all_sids_to(&mut self, cycle: ChipCycle) {
        self.digital_sid.clock_to(cycle);
        for sid in &mut self.additional_sids {
            sid.digital_sid.clock_to(cycle);
        }
    }

    pub(crate) fn set_system_clock(&mut self, clock: SystemClock) {
        self.system_clock = clock;
    }

    fn raster_line(&self) -> u8 {
        let (cycles_per_line, lines_per_frame) = match self.system_clock {
            SystemClock::Pal => (63, 312),
            SystemClock::Ntsc => (65, 263),
        };
        ((self.chip_cycle.0 / cycles_per_line) % lines_per_frame) as u8
    }

    /// Scheduled call period implied by the last complete CIA timer A latch
    /// (CIA 1 preferred), in Φ2 cycles. The 6526 underflows every
    /// `latch + 1` cycles in continuous mode. Degenerate latches outside
    /// 512..=65536 cycles (~1.9 kHz..~15 Hz PAL) are rejected as
    /// not-a-call-rate.
    pub(crate) fn captured_cia_period(&self) -> Option<CiaTimerPeriod> {
        let latch = |bytes: [Option<u8>; 2]| -> Option<u64> {
            Some(u64::from(bytes[0]?) | (u64::from(bytes[1]?) << 8))
        };
        let period = latch(self.cia1_timer_a).or_else(|| latch(self.cia2_timer_a))? + 1;
        (512..=65_536)
            .contains(&period)
            .then(|| CiaTimerPeriod::new(period))
    }

    pub(crate) fn monitor_cia_period_changes(&mut self, adopted: CiaTimerPeriod) {
        self.monitored_cia_period = Some(adopted);
        self.cia_period_changed = false;
    }

    #[must_use]
    pub(crate) fn cia_period_changed(&self) -> bool {
        self.cia_period_changed
    }

    fn observe_cia_period(&mut self) {
        if let (Some(adopted), Some(current)) =
            (self.monitored_cia_period, self.captured_cia_period())
            && current != adopted
        {
            self.cia_period_changed = true;
        }
    }

    pub(crate) fn begin_call(&mut self, call: SidCallId) {
        self.frame_events.clear();
        self.current_call = call;
    }

    pub(crate) fn next_event_id(&self) -> SidBusEventId {
        self.next_event_id
    }

    pub(crate) fn sid_checkpoint(
        &self,
        id: CheckpointId,
        reference: CheckpointRef,
    ) -> SidBusCheckpoint {
        SidBusCheckpoint {
            id,
            reference,
            cycle: self.digital_sid.cycle(),
            next_event: self.next_event_id,
            digital_sid: self.digital_sid.checkpoint(),
            data_latch: SidDataLatch(self.last_sid_write),
            mirror_model: if self.sid_has_unmodeled_mirror_access {
                MirrorModelStatus::UnsupportedRetained
            } else {
                MirrorModelStatus::Modeled
            },
            external_input_model: ExternalInputModelStatus::Disconnected,
        }
    }

    pub fn restore_sid_checkpoint(&mut self, checkpoint: SidBusCheckpoint) {
        let SidBusCheckpoint {
            id: _,
            reference: _,
            cycle,
            next_event,
            digital_sid,
            data_latch,
            mirror_model,
            external_input_model: _,
        } = checkpoint;
        self.digital_sid.restore(digital_sid);
        self.chip_cycle = cycle;
        self.next_event_id = next_event;
        self.last_sid_write = data_latch.0;
        self.sid_has_unmodeled_mirror_access =
            mirror_model == MirrorModelStatus::UnsupportedRetained;
        self.frame_events.clear();
    }

    fn record_sid_event(
        &mut self,
        address: u16,
        target: SidTarget,
        value: u8,
        access: SidBusAccess,
    ) {
        let id = self.next_event_id;
        self.next_event_id.0 += 1;
        self.frame_events.push(SidBusEvent {
            id,
            chip: target.chip,
            call: self.current_call,
            address: SidAddress(address),
            address_class: target.class,
            register: target.register,
            value,
            access,
            cycle: self.chip_cycle,
            offset: self.offset,
            timestamp_quality: EventTimestampQuality::InstructionStartBounded,
        });
    }

    fn sid_target(&self, address: u16) -> Option<SidTarget> {
        if (SID_BASE..=SID_LAST).contains(&address) {
            return Some(SidTarget {
                chip: SidChipId::PRIMARY,
                class: SidAddressClass::Base,
                register: Some(SidRegister((address - SID_BASE) as u8)),
            });
        }
        for sid in &self.additional_sids {
            let base = sid.descriptor.base_address.0;
            if (base..=base + 0x1f).contains(&address) {
                let offset = (address - base) as u8;
                return Some(SidTarget {
                    chip: sid.descriptor.id,
                    class: SidAddressClass::Base,
                    register: (offset <= SID_REGISTER_LAST.0).then_some(SidRegister(offset)),
                });
            }
        }
        if (SID_MIRROR_FIRST..=SID_MIRROR_LAST).contains(&address) {
            let class = SidAddressClass::Mirror;
            return Some(SidTarget {
                chip: SidChipId::PRIMARY,
                class,
                register: resolved_register(SidAddress(address), class),
            });
        }
        None
    }

    fn read_primary_sid_register(&mut self, register: SidRegister) -> u8 {
        match register.0 {
            0x00..=0x18 => {
                self.digital_sid.clock_to(self.chip_cycle);
                self.last_sid_write
            }
            0x19 | 0x1a => {
                self.digital_sid.clock_to(self.chip_cycle);
                0xff
            }
            _ => self.digital_sid.read(register, self.chip_cycle),
        }
    }

    fn read_sid_target(&mut self, target: SidTarget, address: u16) -> u8 {
        let Some(register) = target.register else {
            self.sid_has_unmodeled_mirror_access = true;
            return self.ram[address as usize];
        };
        if target.chip == SidChipId::PRIMARY {
            return self.read_primary_sid_register(register);
        }
        let Some(sid) = self
            .additional_sids
            .iter_mut()
            .find(|sid| sid.descriptor.id == target.chip)
        else {
            return self.ram[address as usize];
        };
        match register.0 {
            0x00..=0x18 => {
                sid.digital_sid.clock_to(self.chip_cycle);
                sid.last_write.0
            }
            0x19 | 0x1a => {
                sid.digital_sid.clock_to(self.chip_cycle);
                0xff
            }
            _ => sid.digital_sid.read(register, self.chip_cycle),
        }
    }

    fn write_sid_target(&mut self, target: SidTarget, value: u8) {
        let Some(register) = target.register else {
            self.sid_unmodeled_mirror_writes += 1;
            self.sid_has_unmodeled_mirror_access = true;
            return;
        };
        if target.chip == SidChipId::PRIMARY {
            self.digital_sid.write(register, value, self.chip_cycle);
            self.last_sid_write = value;
            return;
        }
        if let Some(sid) = self
            .additional_sids
            .iter_mut()
            .find(|sid| sid.descriptor.id == target.chip)
        {
            sid.digital_sid.write(register, value, self.chip_cycle);
            sid.last_write = SidDataLatch(value);
        }
    }
}

impl Default for Bus {
    fn default() -> Self {
        Self::new()
    }
}

impl mos6502::memory::Bus for Bus {
    fn get_byte(&mut self, address: u16) -> u8 {
        if address == VIC_RASTER_LINE {
            return self.raster_line();
        }
        if let Some(target) = self.sid_target(address) {
            let value = self.read_sid_target(target, address);
            self.record_sid_event(address, target, value, SidBusAccess::Read);
            return value;
        }
        self.ram[address as usize]
    }

    fn set_byte(&mut self, address: u16, value: u8) {
        let cia_timer_write = match address {
            CIA1_TIMER_A_LO => {
                self.cia1_timer_a[0] = Some(value);
                true
            }
            CIA1_TIMER_A_HI => {
                self.cia1_timer_a[1] = Some(value);
                true
            }
            CIA2_TIMER_A_LO => {
                self.cia2_timer_a[0] = Some(value);
                true
            }
            CIA2_TIMER_A_HI => {
                self.cia2_timer_a[1] = Some(value);
                true
            }
            _ => false,
        };
        if cia_timer_write {
            self.observe_cia_period();
        }
        if let Some(target) = self.sid_target(address) {
            self.write_sid_target(target, value);
            self.record_sid_event(address, target, value, SidBusAccess::Write);
        }
        self.ram[address as usize] = value;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mos6502::memory::Bus as _;

    #[test]
    fn all_base_window_reads_are_captured() {
        let mut bus = Bus::new();
        let _ = bus.get_byte(0xD41A);
        let _ = bus.get_byte(0xD41D);
        let _ = bus.get_byte(0x1000);
        assert_eq!(bus.frame_events.len(), 1);
    }

    #[test]
    fn reading_v3_osc_register_increments_counter() {
        let mut bus = Bus::new();
        let _ = bus.get_byte(V3_READ_FIRST);
        assert_eq!(bus.frame_events.len(), 1);
        let _ = bus.get_byte(V3_READ_FIRST);
        let _ = bus.get_byte(V3_READ_LAST);
        assert_eq!(bus.frame_events.len(), 3);
    }

    #[test]
    fn osc3_sawtooth_advances_with_cycles() {
        let mut bus = Bus::new();
        bus.set_byte(0xD40E, 0x00); // freq lo
        bus.set_byte(0xD40F, 0x10); // freq hi → freq = 0x1000
        bus.set_byte(0xD412, 0x20); // sawtooth

        bus.chip_cycle = ChipCycle(256);
        let a = bus.get_byte(V3_READ_FIRST);
        bus.chip_cycle = ChipCycle(512);
        let b = bus.get_byte(V3_READ_FIRST);

        // A free-running sawtooth read at two cycle counts must move and be
        // non-zero — not the old flat `0`.
        assert_ne!(a, b, "OSC3 should advance between reads");
        assert!(a != 0 || b != 0, "OSC3 should not be stuck at zero");
    }

    #[test]
    fn osc3_test_bit_holds_zero() {
        let mut bus = Bus::new();
        bus.set_byte(0xD40E, 0x00);
        bus.set_byte(0xD40F, 0x10);
        bus.set_byte(0xD412, 0x20 | 0x08); // sawtooth + test bit
        bus.chip_cycle = ChipCycle(4096);
        assert_eq!(bus.get_byte(V3_READ_FIRST), 0);
    }

    #[test]
    fn raster_line_advances_with_chip_cycles_for_each_clock() {
        let mut bus = Bus::new();
        bus.chip_cycle = ChipCycle(63 * 140);
        assert_eq!(bus.get_byte(VIC_RASTER_LINE), 140);

        bus.set_system_clock(SystemClock::Ntsc);
        bus.chip_cycle = ChipCycle(65 * 180);
        assert_eq!(bus.get_byte(VIC_RASTER_LINE), 180);
    }

    #[test]
    fn write_only_registers_read_the_shared_bus_latch() {
        let mut bus = Bus::new();
        bus.set_byte(0xD400, 0x55);
        assert_eq!(bus.get_byte(0xD418), 0x55);
        bus.set_byte(0xD40B, 0x81);
        assert_eq!(bus.get_byte(0xD400), 0x81);
        assert_eq!(
            bus.frame_events
                .iter()
                .filter(|event| event.access == SidBusAccess::Read)
                .count(),
            2
        );
    }

    #[test]
    fn pot_lines_read_high_and_unresolved_mirror_writes_are_counted() {
        let mut bus = Bus::new();
        assert_eq!(bus.get_byte(0xD419), 0xFF);
        assert_eq!(bus.get_byte(0xD41A), 0xFF);
        assert_eq!(bus.sid_unmodeled_mirror_writes, 0);
        bus.set_byte(0xD41D, 0xCC); // unused SID register, not a mirror
        assert_eq!(bus.sid_unmodeled_mirror_writes, 0);
        bus.set_byte(0xD420, 0x12);
        bus.set_byte(0xD500, 0x34);
        bus.set_byte(0xD43D, 0x56);
        assert_eq!(bus.sid_unmodeled_mirror_writes, 1);
        assert_eq!(bus.get_byte(0xD420), 0x34);
        assert_eq!(
            bus.frame_events
                .iter()
                .filter(|event| event.address_class == crate::emu::capture::SidAddressClass::Mirror)
                .count(),
            4
        );
    }

    #[test]
    fn cia_timer_latch_capture_and_period_bounds() {
        let mut bus = Bus::new();
        assert_eq!(bus.captured_cia_period(), None);
        bus.set_byte(0xDC04, 0x25);
        assert_eq!(bus.captured_cia_period(), None, "partial latch is unusable");
        bus.set_byte(0xDC05, 0x40);
        assert_eq!(bus.captured_cia_period(), Some(CiaTimerPeriod::new(0x4026)));
        bus.set_byte(0xDC04, 0x01);
        bus.set_byte(0xDC05, 0x00);
        assert_eq!(
            bus.captured_cia_period(),
            None,
            "degenerate latch is not a call rate"
        );
    }

    #[test]
    fn cia_period_monitor_remembers_a_temporary_change() {
        let mut bus = Bus::new();
        bus.set_byte(CIA1_TIMER_A_LO, 0x25);
        bus.set_byte(CIA1_TIMER_A_HI, 0x40);
        bus.monitor_cia_period_changes(CiaTimerPeriod::new(0x4026));

        bus.set_byte(CIA1_TIMER_A_LO, 0x35);
        bus.set_byte(CIA1_TIMER_A_LO, 0x25);
        assert_eq!(bus.captured_cia_period(), Some(CiaTimerPeriod::new(0x4026)));
        assert!(bus.cia_period_changed());
    }

    #[test]
    fn same_offset_writes_keep_capture_order() {
        let mut bus = Bus::new();
        bus.offset = SubFrameOffset(9);
        bus.set_byte(SID_BASE, 0x11);
        bus.set_byte(SID_BASE, 0x22);
        assert_eq!(bus.frame_events[0].value, 0x11);
        assert_eq!(bus.frame_events[1].value, 0x22);
        assert_eq!(bus.frame_events[0].offset, bus.frame_events[1].offset);
    }
}
