pub mod bus;
pub mod capture;
pub mod dis;
pub mod probe;
pub mod runner;
pub mod sid;
pub mod taint;

use crate::analysis::SystemClock;
use crate::emu::capture::{
    CAPTURE_SCHEMA_VERSION, CaptureDiagnostic, CaptureDiagnosticKind, CapturedCallSpan,
    CapturedObservation, CapturedSidExecution, CapturedSourceIdentity, CheckpointId,
    CheckpointPolicy, CheckpointRef, SidBusEvent, SidBusEventId, SidCallId, SourceDigest,
};
use crate::header::{
    self, Header, InitAddress, LoadAddress, PlayAddress, SubtuneCount, SubtuneIndex,
};
use crate::trace::{ChipCycle, CpuCycles, FrameIndex, FrameTrace, RegisterWrite, Trace};
use runner::{Cpu, RunError};
use serde::{Deserialize, Serialize};

/// `$02A6` is the C64 KERNAL's PAL/NTSC indicator (`TVSFLG`): `1` = PAL,
/// `0` = NTSC. Some SID tunes read this byte to pick a frequency table; we
/// seed it from the header's clock so the read returns the model the tune
/// expects (see [`Emulator::load`]).
const PAL_NTSC_FLAG_ADDR: u16 = 0x02A6;
const KERNAL_IRQ_VECTOR: u16 = 0x0314;
const KERNAL_NMI_VECTOR: u16 = 0x0318;
const HARDWARE_NMI_VECTOR: u16 = 0xFFFA;
const HARDWARE_IRQ_VECTOR: u16 = 0xFFFE;
const DEFAULT_KERNAL_IRQ_HANDLER: PlayAddress = PlayAddress(0xEA31);
const PAL: u8 = 1;
const NTSC: u8 = 0;

#[derive(Debug, thiserror::Error)]
pub enum EmuError {
    #[error("subtune {subtune} is out of range (songs = {songs})")]
    SubtuneOutOfRange {
        subtune: SubtuneIndex,
        songs: SubtuneCount,
    },
    #[error("data section ({data_len} bytes) plus load address {load} would overflow 64 KiB")]
    DataOverflowsRam { load: LoadAddress, data_len: usize },
    #[error("wall-clock deadline exceeded after {frames_completed} of {frames_requested} frames")]
    WallTimeout {
        frames_completed: u32,
        frames_requested: u32,
    },
    #[error("play address is 0, but init installed no IRQ handler")]
    InterruptHandlerNotInstalled,
    #[error("play address is 0 and init installed only an NMI handler at {handler}")]
    NmiInterruptHandlerUnsupported { handler: PlayAddress },
    #[error(transparent)]
    Header(#[from] header::Error),
    #[error(transparent)]
    Run(#[from] RunError),
    #[error(transparent)]
    Capture(#[from] capture::CaptureError),
}

/// Scheduling inputs shared by emulation and trace analysis.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct PlaybackTiming {
    pub clock: SystemClock,
    pub call_rate: CallRate,
    pub cia_timed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cia_period: Option<CiaTimerPeriod>,
}

impl PlaybackTiming {
    pub fn vblank(clock: SystemClock) -> Self {
        Self {
            clock,
            call_rate: CallRate::vblank(clock),
            cia_timed: false,
            cia_period: None,
        }
    }

    pub fn for_subtune(header: &Header, subtune: SubtuneIndex) -> Self {
        Self::for_subtune_with_clock(header, subtune, SystemClock::from(header.flags.clock))
    }

    pub fn for_subtune_with_clock(
        header: &Header,
        subtune: SubtuneIndex,
        clock: SystemClock,
    ) -> Self {
        Self {
            cia_timed: header.is_cia_timed(subtune),
            ..Self::vblank(clock)
        }
    }

    #[must_use = "the returned timing contains the recovered CIA period"]
    pub fn with_cia_period(self, period: CiaTimerPeriod) -> Self {
        Self {
            call_rate: CallRate::new(u64::from(self.clock.phi2_hz()), period.cycles()),
            cia_period: Some(period),
            ..self
        }
    }

    #[must_use = "the returned timing reflects the trace's resolved schedule"]
    pub fn resolved_from_trace(self, trace: &Trace) -> Self {
        if self.cia_timed && trace.timing_exact {
            return self.with_cia_period(CiaTimerPeriod::new(trace.call_rate.denominator()));
        }
        Self {
            call_rate: trace.call_rate,
            cia_period: None,
            ..self
        }
    }

    #[must_use]
    pub fn exact(self) -> bool {
        !self.cia_timed || self.cia_period.is_some()
    }

    #[must_use]
    pub fn inexact_reason(self) -> Option<TimingInexactReason> {
        (!self.exact()).then_some(TimingInexactReason::CiaPeriodUnknown)
    }

    #[must_use]
    pub fn calls_per_second(self) -> f64 {
        self.call_rate.calls_per_second()
    }

    #[must_use]
    pub fn seconds_per_call(self) -> f64 {
        self.call_rate.seconds_per_call()
    }

    #[must_use]
    pub fn calls_for_duration(self, duration: std::time::Duration) -> u32 {
        (duration.as_secs_f64() * self.calls_per_second())
            .ceil()
            .max(1.0) as u32
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, thiserror::Error)]
#[serde(rename_all = "snake_case")]
#[must_use]
pub enum TimingInexactReason {
    #[error("the player did not program a CIA timer period during init")]
    CiaPeriodUnknown,
}

/// CIA timer A underflow period in Φ2 cycles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
#[must_use]
pub struct CiaTimerPeriod(u64);

impl CiaTimerPeriod {
    pub fn new(cycles: u64) -> Self {
        assert!(cycles > 0);
        Self(cycles)
    }

    #[must_use]
    pub fn cycles(self) -> u64 {
        self.0
    }
}

/// Rational scheduled play-call rate in calls per second.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct CallRate {
    numerator: u64,
    denominator: u64,
}

impl CallRate {
    pub fn new(numerator: u64, denominator: u64) -> Self {
        assert!(numerator > 0 && denominator > 0);
        Self {
            numerator,
            denominator,
        }
    }

    pub fn vblank(clock: SystemClock) -> Self {
        match clock {
            SystemClock::Pal => Self::new(985_248, 19_656),
            SystemClock::Ntsc => Self::new(1_022_727, 17_095),
        }
    }

    #[must_use]
    pub fn numerator(self) -> u64 {
        self.numerator
    }

    #[must_use]
    pub fn denominator(self) -> u64 {
        self.denominator
    }

    #[must_use]
    pub fn calls_per_second(self) -> f64 {
        self.numerator as f64 / self.denominator as f64
    }

    #[must_use]
    pub fn seconds_per_call(self) -> f64 {
        self.denominator as f64 / self.numerator as f64
    }
}

impl Default for CallRate {
    fn default() -> Self {
        Self::vblank(SystemClock::Pal)
    }
}

#[derive(Debug)]
struct EmulationClock {
    phi2_hz: u64,
    rate: CallRate,
    boundary_index: u64,
}

impl EmulationClock {
    fn new(timing: PlaybackTiming) -> Self {
        Self {
            phi2_hz: u64::from(timing.clock.phi2_hz()),
            rate: timing.call_rate,
            boundary_index: 0,
        }
    }

    fn boundary(&self, index: u64) -> ChipCycle {
        let numerator =
            u128::from(index) * u128::from(self.phi2_hz) * u128::from(self.rate.denominator);
        ChipCycle((numerator / u128::from(self.rate.numerator)) as u64)
    }

    fn first_boundary_at_or_after(&mut self, cycle: ChipCycle) -> ChipCycle {
        while self.boundary(self.boundary_index) < cycle {
            self.boundary_index += 1;
        }
        self.boundary(self.boundary_index)
    }

    fn next_boundary(&mut self) -> ChipCycle {
        self.boundary_index += 1;
        self.boundary(self.boundary_index)
    }

    fn current_boundary(&self) -> ChipCycle {
        self.boundary(self.boundary_index)
    }
}

pub struct Emulator {
    cpu: Cpu,
    clock: EmulationClock,
    host_clock: SystemClock,
    timing: PlaybackTiming,
    /// CIA timer A period adopted from init's latch writes, in Φ2 cycles.
    /// `Some` only for CIA-timed subtunes whose init programmed a plausible
    /// timer; the scheduler then runs at that rate instead of vblank.
    adopted_cia_period: Option<CiaTimerPeriod>,
    captured_events: Vec<SidBusEvent>,
    captured_calls: Vec<CapturedCallSpan>,
    captured_checkpoints: Vec<capture::SidBusCheckpoint>,
    captured_observations: Vec<CapturedObservation>,
    next_checkpoint_id: CheckpointId,
    checkpoint_policy: CheckpointPolicy,
    oscillator_observation_start: Option<[crate::emu::sid::OscillatorCheckpoint; 3]>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlayCall {
    Subroutine(PlayAddress),
    KernalIrq(PlayAddress),
    HardwareIrq(PlayAddress),
}

impl Default for Emulator {
    fn default() -> Self {
        Self::new()
    }
}

impl Emulator {
    #[must_use]
    pub fn new() -> Self {
        Self::with_timing(PlaybackTiming::vblank(SystemClock::Pal))
    }

    #[must_use]
    pub fn with_timing(timing: PlaybackTiming) -> Self {
        let mut cpu = runner::make_cpu();
        cpu.memory.set_system_clock(timing.clock);
        cpu.memory.ram[PAL_NTSC_FLAG_ADDR as usize] = PAL;
        let [irq_lo, irq_hi] = DEFAULT_KERNAL_IRQ_HANDLER.0.to_le_bytes();
        cpu.memory.ram[KERNAL_IRQ_VECTOR as usize] = irq_lo;
        cpu.memory.ram[KERNAL_IRQ_VECTOR.wrapping_add(1) as usize] = irq_hi;
        let adopted_cia_period = timing.cia_period;
        Self {
            cpu,
            clock: EmulationClock::new(timing),
            host_clock: timing.clock,
            timing,
            adopted_cia_period,
            captured_events: Vec::new(),
            captured_calls: Vec::new(),
            captured_checkpoints: Vec::new(),
            captured_observations: Vec::new(),
            next_checkpoint_id: CheckpointId(0),
            checkpoint_policy: CheckpointPolicy::DenseCallBoundaries,
            oscillator_observation_start: None,
        }
    }

    #[must_use = "the returned emulator uses the selected checkpoint policy"]
    pub fn with_checkpoint_policy(mut self, policy: CheckpointPolicy) -> Self {
        self.checkpoint_policy = policy;
        self
    }

    /// Copy the music data from the SID file into RAM at the effective load
    /// address. When the header's `load_address` is `0`, the first two
    /// bytes of the data section are the little-endian load address and are
    /// stripped before the rest is loaded.
    pub fn load(&mut self, header: &Header, file_bytes: &[u8]) -> Result<(), EmuError> {
        // Seed TVSFLG ($02A6) from the resolved host clock so tunes that read it
        // select the matching frequency table.
        self.cpu.memory.ram[PAL_NTSC_FLAG_ADDR as usize] = match self.host_clock {
            SystemClock::Ntsc => NTSC,
            SystemClock::Pal => PAL,
        };
        self.cpu.memory.configure_sids(header);

        let load = header.effective_load_address(file_bytes)?;
        let embedded_addr_bytes = if header.load_address.0 == 0 { 2 } else { 0 };
        let data_start = header.data_offset as usize + embedded_addr_bytes;
        let data = &file_bytes[data_start..];

        let end = load.0 as usize + data.len();
        if end > self.cpu.memory.ram.len() {
            return Err(EmuError::DataOverflowsRam {
                load,
                data_len: data.len(),
            });
        }
        self.cpu.memory.load(load.0, data);
        Ok(())
    }

    fn capture_checkpoint(&mut self, reference: CheckpointRef) {
        let retained = match (self.checkpoint_policy, reference) {
            (_, CheckpointRef::InitReturn | CheckpointRef::FirstPlayBoundary) => true,
            (CheckpointPolicy::DenseCallBoundaries, _) => true,
            (
                CheckpointPolicy::SparseEveryCalls { calls },
                CheckpointRef::PlayStart(frame) | CheckpointRef::SamplingBoundary(frame),
            ) => calls.0 != 0 && frame.0 % calls.0 == 0,
            (CheckpointPolicy::SparseEveryCalls { .. }, CheckpointRef::Event(_)) => true,
        };
        if !retained {
            return;
        }
        let id = self.next_checkpoint_id;
        self.next_checkpoint_id.0 += 1;
        self.captured_checkpoints
            .push(self.cpu.memory.sid_checkpoint(id, reference));
    }

    #[allow(clippy::too_many_arguments)]
    fn retain_call(
        &mut self,
        call: SidCallId,
        start_cycle: ChipCycle,
        return_cycle: ChipCycle,
        sampling_boundary: ChipCycle,
        duration: CpuCycles,
        overrun: CpuCycles,
        events: &[SidBusEvent],
    ) {
        let first_event = events
            .first()
            .map_or(self.cpu.memory.next_event_id(), |event| event.id);
        let next_event = events
            .last()
            .map_or(first_event, |event| SidBusEventId(event.id.0 + 1));
        self.captured_calls.push(CapturedCallSpan {
            call,
            start_cycle,
            return_cycle,
            sampling_boundary,
            duration,
            overrun,
            first_event,
            next_event,
        });
        self.captured_events.extend_from_slice(events);
    }

    fn captured_execution(
        &self,
        source: CapturedSourceIdentity,
        sid_model: crate::header::SidModel,
        timing_exact: bool,
    ) -> CapturedSidExecution {
        let mut diagnostics = Vec::new();
        for event in &self.captured_events {
            if event.address_class == capture::SidAddressClass::Mirror && event.register.is_none() {
                diagnostics.push(CaptureDiagnostic {
                    kind: CaptureDiagnosticKind::MirrorAccessUnsupported,
                    event: Some(event.id),
                    detail: format!("unsupported SID mirror access at ${:04X}", event.address.0),
                });
            }
        }
        if !timing_exact {
            diagnostics.push(CaptureDiagnostic {
                kind: CaptureDiagnosticKind::TimingInexact,
                event: None,
                detail: "scheduled call timing is not eligible for exact replay".to_owned(),
            });
        }
        if self.cpu.memory.captured_sid_chips().len() > 1 {
            diagnostics.push(CaptureDiagnostic {
                kind: CaptureDiagnosticKind::AdditionalSidCheckpointUnsupported,
                event: None,
                detail: "secondary SID bus events are captured and analyzed, but digital-state checkpoints and replay remain primary-chip only".to_owned(),
            });
        }
        CapturedSidExecution {
            schema_version: CAPTURE_SCHEMA_VERSION,
            source,
            sid_model,
            system_clock: self.host_clock,
            call_rate: self.timing.call_rate,
            timing_exact,
            checkpoint_policy: self.checkpoint_policy,
            chips: self.cpu.memory.captured_sid_chips(),
            events: self.captured_events.clone(),
            calls: self.captured_calls.clone(),
            checkpoints: self.captured_checkpoints.clone(),
            observations: self.captured_observations.clone(),
            diagnostics,
        }
    }

    /// Run `init(subtune)` once. `subtune` is 1-based as in the header; the
    /// accumulator is set to `subtune - 1` per the PSID spec.
    pub fn call_init(
        &mut self,
        init: InitAddress,
        subtune: SubtuneIndex,
        songs: SubtuneCount,
    ) -> Result<Vec<RegisterWrite>, EmuError> {
        self.call_init_with_deadline(init, subtune, songs, None)
    }

    pub fn call_init_with_deadline(
        &mut self,
        init: InitAddress,
        subtune: SubtuneIndex,
        songs: SubtuneCount,
        deadline: Option<std::time::Instant>,
    ) -> Result<Vec<RegisterWrite>, EmuError> {
        Ok(self
            .call_init_captured_with_deadline(init, subtune, songs, deadline)?
            .writes)
    }

    fn call_init_captured_with_deadline(
        &mut self,
        init: InitAddress,
        subtune: SubtuneIndex,
        songs: SubtuneCount,
        deadline: Option<std::time::Instant>,
    ) -> Result<runner::CapturedCall, EmuError> {
        if subtune.0 == 0 || subtune.0 > songs.0 {
            return Err(EmuError::SubtuneOutOfRange { subtune, songs });
        }
        let song_zero_based = (subtune.0 - 1) as u8;
        self.cpu.memory.begin_call(SidCallId::Init);
        let captured = runner::call_captured_with_deadline(
            &mut self.cpu,
            init.0,
            song_zero_based,
            0,
            0,
            ChipCycle(0),
            deadline,
        )?;
        // A CIA-timed subtune that programmed its timer during init gets its
        // real call rate; the vblank schedule remains the loud fallback.
        if self.timing.cia_timed
            && let Some(period) = self.cpu.memory.captured_cia_period()
        {
            let timing = self.timing.with_cia_period(period);
            self.clock = EmulationClock::new(timing);
            self.timing = timing;
            self.adopted_cia_period = Some(period);
            self.cpu.memory.monitor_cia_period_changes(period);
        }
        let init_end = ChipCycle(captured.duration.0);
        self.cpu.memory.clock_all_sids_to(init_end);
        self.capture_checkpoint(CheckpointRef::InitReturn);
        let first_nominal_boundary = self.clock.boundary(1);
        let first_play = self.clock.first_boundary_at_or_after(init_end);
        self.cpu.memory.clock_all_sids_to(first_play);
        self.capture_checkpoint(CheckpointRef::FirstPlayBoundary);
        self.retain_call(
            SidCallId::Init,
            ChipCycle(0),
            init_end,
            first_play,
            captured.duration,
            CpuCycles(init_end.0.saturating_sub(first_nominal_boundary.0)),
            &captured.events,
        );
        Ok(captured)
    }

    /// First scheduled call boundary (one call period) on the adopted
    /// schedule — CIA-derived when captured, vblank otherwise.
    fn first_scheduled_boundary(&self) -> ChipCycle {
        self.clock.boundary(1)
    }

    /// Effective play-call timing after init has had an opportunity to
    /// program a CIA timer.
    pub fn playback_timing(&self) -> PlaybackTiming {
        self.timing
    }

    /// Read a raw RAM byte (no read traps). Useful after [`Self::call_init`]
    /// to inspect the post-`init` memory image — e.g. a driver-native
    /// extractor scanning the player's code and data tables.
    #[must_use]
    pub fn read_ram(&self, addr: u16) -> u8 {
        self.cpu.memory.ram[addr as usize]
    }

    fn read_ram_word(&self, address: u16) -> PlayAddress {
        PlayAddress(u16::from_le_bytes([
            self.read_ram(address),
            self.read_ram(address.wrapping_add(1)),
        ]))
    }

    fn resolve_play_call(&self, play: PlayAddress) -> Result<PlayCall, EmuError> {
        if play.0 != 0 {
            return Ok(PlayCall::Subroutine(play));
        }
        let kernal_irq = self.read_ram_word(KERNAL_IRQ_VECTOR);
        if kernal_irq.0 != 0 && kernal_irq != DEFAULT_KERNAL_IRQ_HANDLER {
            return Ok(PlayCall::KernalIrq(kernal_irq));
        }
        let hardware_irq = self.read_ram_word(HARDWARE_IRQ_VECTOR);
        if hardware_irq.0 != 0 {
            return Ok(PlayCall::HardwareIrq(hardware_irq));
        }
        let nmi = [
            self.read_ram_word(KERNAL_NMI_VECTOR),
            self.read_ram_word(HARDWARE_NMI_VECTOR),
        ]
        .into_iter()
        .find(|handler| handler.0 != 0);
        if let Some(handler) = nmi {
            return Err(EmuError::NmiInterruptHandlerUnsupported { handler });
        }
        Err(EmuError::InterruptHandlerNotInstalled)
    }

    /// Overwrite a raw RAM byte (no write traps) — the [`probe`] mutates one
    /// stream byte in the post-`init` image before replaying.
    pub fn poke(&mut self, addr: u16, value: u8) {
        self.cpu.memory.ram[addr as usize] = value;
    }

    /// Snapshot the full 64 KiB RAM image (no read traps) — the post-`init`
    /// image the native locators and `sid-re` work from.
    #[must_use]
    pub fn ram_image(&self) -> Vec<u8> {
        self.cpu.memory.ram.to_vec()
    }

    pub fn run_play_frame(
        &mut self,
        play: PlayAddress,
        frame: FrameIndex,
    ) -> Result<FrameTrace, EmuError> {
        self.run_play_frame_with_deadline(play, frame, None)
    }

    pub(crate) fn run_play_frame_observed<F>(
        &mut self,
        play: PlayAddress,
        frame: FrameIndex,
        mut observer: F,
    ) -> Result<FrameTrace, EmuError>
    where
        F: FnMut(&runner::Cpu),
    {
        let start_cycle = self.clock.current_boundary();
        self.capture_checkpoint(CheckpointRef::PlayStart(frame));
        self.cpu.memory.begin_call(SidCallId::Play(frame));
        self.cpu.memory.digital_sid.begin_observation(start_cycle);
        self.oscillator_observation_start =
            Some(self.cpu.memory.digital_sid.checkpoint().oscillators);
        let captured =
            runner::call_captured_observed(&mut self.cpu, play.0, start_cycle, &mut observer)?;
        Ok(self.finish_play_frame(frame, start_cycle, captured))
    }

    /// Single-step one `play` call, invoking `probe(cpu)` before each
    /// instruction. For driver reverse-engineering micro-traces only — it
    /// exposes per-instruction CPU state (program counter, registers, RAM via
    /// `cpu.memory.ram`) that the batch [`Self::run_play_frame`] hides. No
    /// trace is collected; production tracing uses [`Self::run_play_frame`].
    ///
    /// The timeline is **idle-compressed**: each call starts where the
    /// previous one's last write ended, with no advancement to scheduler
    /// boundaries — OSC3/ENV3 values a driver reads here differ from a
    /// production trace. Differential probing stays self-consistent (both
    /// runs are compressed identically).
    pub fn run_play_frame_stepwise<F>(&mut self, play: PlayAddress, probe: F)
    where
        F: FnMut(&runner::Cpu),
    {
        runner::call_stepwise(&mut self.cpu, play.0, 0, 0, 0, probe);
    }

    /// Run `frames` consecutive `play` calls with the dynamic taint tracker
    /// attached, returning its discovery report. See [`taint`] — this is the
    /// engine behind `sid-re taint`. The timeline is idle-compressed (see
    /// [`Self::run_play_frame_stepwise`]); do not compare absolute OSC3/ENV3
    /// values against a production trace.
    #[must_use]
    pub fn run_taint(&mut self, play: PlayAddress, frames: u32) -> taint::TaintReport {
        let mut tracker = taint::Taint::new();
        for frame in 0..frames {
            tracker.enter_frame(frame);
            runner::call_stepwise(&mut self.cpu, play.0, 0, 0, 0, |cpu| tracker.step(cpu));
        }
        tracker.report()
    }

    pub fn run_play_frame_with_deadline(
        &mut self,
        play: PlayAddress,
        frame: FrameIndex,
        deadline: Option<std::time::Instant>,
    ) -> Result<FrameTrace, EmuError> {
        let play = self.resolve_play_call(play)?;
        self.run_play_call_frame_with_deadline(play, frame, deadline)
    }

    fn run_play_call_frame_with_deadline(
        &mut self,
        play: PlayCall,
        frame: FrameIndex,
        deadline: Option<std::time::Instant>,
    ) -> Result<FrameTrace, EmuError> {
        let start_cycle = self.clock.current_boundary();
        self.capture_checkpoint(CheckpointRef::PlayStart(frame));
        self.cpu.memory.begin_call(SidCallId::Play(frame));
        self.cpu.memory.digital_sid.begin_observation(start_cycle);
        self.oscillator_observation_start =
            Some(self.cpu.memory.digital_sid.checkpoint().oscillators);
        let captured = match play {
            PlayCall::Subroutine(address) => runner::call_captured_with_deadline(
                &mut self.cpu,
                address.0,
                0,
                0,
                0,
                start_cycle,
                deadline,
            )?,
            PlayCall::KernalIrq(address) => runner::call_interrupt_captured_with_deadline(
                &mut self.cpu,
                address.0,
                runner::InterruptEntry::KernalVector,
                start_cycle,
                deadline,
            )?,
            PlayCall::HardwareIrq(address) => runner::call_interrupt_captured_with_deadline(
                &mut self.cpu,
                address.0,
                runner::InterruptEntry::HardwareVector,
                start_cycle,
                deadline,
            )?,
        };
        Ok(self.finish_play_frame(frame, start_cycle, captured))
    }

    fn finish_play_frame(
        &mut self,
        frame: FrameIndex,
        start_cycle: ChipCycle,
        captured: runner::CapturedCall,
    ) -> FrameTrace {
        let call_end = ChipCycle(start_cycle.0 + captured.duration.0);
        let mut next_call = self.clock.next_boundary();
        let overrun = CpuCycles(call_end.0.saturating_sub(next_call.0));
        if next_call < call_end {
            next_call = self.clock.first_boundary_at_or_after(call_end);
        }
        self.cpu.memory.clock_all_sids_to(next_call);
        let activity = self.cpu.memory.digital_sid.finish_observation();
        let oscillator_end = self.cpu.memory.digital_sid.checkpoint().oscillators;
        let oscillator_start = self
            .oscillator_observation_start
            .take()
            .unwrap_or_else(|| oscillator_end.clone());
        for (voice, envelope) in activity.into_iter().enumerate() {
            self.captured_observations.push(CapturedObservation {
                call: SidCallId::Play(frame),
                voice: crate::analysis::VoiceId::from_index(voice),
                start_cycle,
                end_cycle: next_call,
                envelope,
                sync_resets: capture::OscillatorEventCount(
                    oscillator_end[voice]
                        .sync_resets
                        .saturating_sub(oscillator_start[voice].sync_resets),
                ),
                source_msb_edges: capture::OscillatorEventCount(
                    oscillator_end[voice]
                        .source_msb_edges
                        .saturating_sub(oscillator_start[voice].source_msb_edges),
                ),
                oscillator_start: oscillator_start[voice].clone(),
                oscillator_end: oscillator_end[voice].clone(),
            });
        }
        self.retain_call(
            SidCallId::Play(frame),
            start_cycle,
            call_end,
            next_call,
            captured.duration,
            overrun,
            &captured.events,
        );
        self.capture_checkpoint(CheckpointRef::SamplingBoundary(frame));
        FrameTrace {
            frame,
            start_cycle,
            duration: captured.duration,
            overrun,
            end_cycle: next_call,
            writes: captured.writes,
            reads: captured.reads,
        }
    }
}

/// Top-level convenience: parse-loaded `header`, run `init` for `subtune`,
/// then `frames` consecutive `play` calls, returning the accumulated trace.
///
/// Each frame is exactly one scheduled `play` call, not necessarily one raster
/// frame. CIA-timer-driven subtunes adopt a period programmed during init;
/// otherwise they retain a loud, inexact vblank fallback schedule.
pub fn run(
    header: &Header,
    bytes: &[u8],
    subtune: SubtuneIndex,
    frames: u32,
) -> Result<Trace, EmuError> {
    run_inner(
        header,
        bytes,
        subtune,
        frames,
        PlaybackTiming::for_subtune(header, subtune),
        None,
        CheckpointPolicy::DenseCallBoundaries,
    )
}

/// Load a tune and run only its init routine to recover the effective play
/// schedule. CIA-timed tunes can program their timer period during init, so
/// header flags alone are insufficient to determine their call rate.
pub fn resolve_playback_timing(
    header: &Header,
    bytes: &[u8],
    subtune: SubtuneIndex,
    timing: PlaybackTiming,
) -> Result<PlaybackTiming, EmuError> {
    let mut emulator = Emulator::with_timing(timing);
    emulator.load(header, bytes)?;
    emulator.call_init(header.init_address, subtune, header.songs)?;
    Ok(emulator.playback_timing())
}

pub fn run_with_timing(
    header: &Header,
    bytes: &[u8],
    subtune: SubtuneIndex,
    frames: u32,
    timing: PlaybackTiming,
) -> Result<Trace, EmuError> {
    run_inner(
        header,
        bytes,
        subtune,
        frames,
        timing,
        None,
        CheckpointPolicy::DenseCallBoundaries,
    )
}

pub fn run_with_checkpoint_policy(
    header: &Header,
    bytes: &[u8],
    subtune: SubtuneIndex,
    frames: u32,
    timing: PlaybackTiming,
    checkpoint_policy: CheckpointPolicy,
) -> Result<Trace, EmuError> {
    if matches!(
        checkpoint_policy,
        CheckpointPolicy::SparseEveryCalls { calls } if calls.0 == 0
    ) {
        return Err(capture::CaptureError::ZeroCheckpointCadence.into());
    }
    run_inner(
        header,
        bytes,
        subtune,
        frames,
        timing,
        None,
        checkpoint_policy,
    )
}

/// Like [`run`] but bails out between frames if `deadline` has passed.
/// Returns `EmuError::WallTimeout` on bail. The per-subroutine
/// `CYCLE_GUARD` still applies inside each frame — `deadline` is a
/// coarser wall-clock budget for files whose `play` legitimately
/// runs millions of cycles per frame.
pub fn run_with_deadline(
    header: &Header,
    bytes: &[u8],
    subtune: SubtuneIndex,
    frames: u32,
    deadline: std::time::Instant,
) -> Result<Trace, EmuError> {
    run_inner(
        header,
        bytes,
        subtune,
        frames,
        PlaybackTiming::for_subtune(header, subtune),
        Some(deadline),
        CheckpointPolicy::DenseCallBoundaries,
    )
}

fn run_inner(
    header: &Header,
    bytes: &[u8],
    subtune: SubtuneIndex,
    frames: u32,
    timing: PlaybackTiming,
    deadline: Option<std::time::Instant>,
    checkpoint_policy: CheckpointPolicy,
) -> Result<Trace, EmuError> {
    let mut emu = Emulator::with_timing(timing).with_checkpoint_policy(checkpoint_policy);
    emu.load(header, bytes)?;

    let init = match emu.call_init_captured_with_deadline(
        header.init_address,
        subtune,
        header.songs,
        deadline,
    ) {
        Ok(call) => call,
        Err(EmuError::Run(RunError::WallDeadlineExceeded { .. })) => {
            return Err(EmuError::WallTimeout {
                frames_completed: 0,
                frames_requested: frames,
            });
        }
        Err(e) => return Err(e),
    };
    emu.resolve_play_call(header.play_address)?;

    if header.play_address.0 == 0 {
        eprintln!(
            "warning: play address zero uses the IRQ handler installed by init; \
             interrupt-source and raster timing are not modeled, so the trace is marked inexact"
        );
    }
    if timing.cia_timed && emu.adopted_cia_period.is_none() {
        eprintln!(
            "warning: subtune {subtune} is CIA-timed but init programmed no CIA timer; \
             vblank fallback timing is inexact and excluded from ground-truth gates"
        );
    }

    let first_nominal_boundary = emu.first_scheduled_boundary();
    let init_overrun = CpuCycles(init.duration.0.saturating_sub(first_nominal_boundary.0));
    let mut trace = Trace {
        capture: CapturedSidExecution::default(),
        sid_model: header.flags.sid_model,
        init_writes: init.writes,
        init_reads: init.reads,
        init_duration: init.duration,
        init_overrun,
        timing_exact: emu.timing.exact() && header.play_address.0 != 0,
        call_rate: emu.timing.call_rate,
        frames: Vec::with_capacity(frames as usize),
    };
    for n in 0..frames {
        if let Some(d) = deadline
            && std::time::Instant::now() >= d
        {
            return Err(EmuError::WallTimeout {
                frames_completed: n,
                frames_requested: frames,
            });
        }
        let play = emu.resolve_play_call(header.play_address)?;
        match emu.run_play_call_frame_with_deadline(play, FrameIndex(n), deadline) {
            Ok(frame) => trace.frames.push(frame),
            Err(EmuError::Run(RunError::WallDeadlineExceeded { .. })) => {
                return Err(EmuError::WallTimeout {
                    frames_completed: n,
                    frames_requested: frames,
                });
            }
            Err(e) => return Err(e),
        }
    }
    if emu.cpu.memory.sid_unmodeled_mirror_writes > 0 {
        eprintln!(
            "warning: {} write(s) to unresolved SID mirror offsets were not applied \
             to the SID model",
            emu.cpu.memory.sid_unmodeled_mirror_writes
        );
    }
    // A player that reprograms the timer during play (tempo change) has left
    // the captured schedule behind — demote the trace rather than present a
    // stale rate as ground truth.
    if emu.adopted_cia_period.is_some() && emu.cpu.memory.cia_period_changed() {
        eprintln!(
            "warning: subtune {subtune} reprogrammed its CIA timer during play; \
             the captured schedule is stale — timing marked inexact"
        );
        trace.timing_exact = false;
    }
    trace.capture = emu.captured_execution(
        CapturedSourceIdentity {
            digest: SourceDigest(crate::songlengths::compute_sid_md5(bytes)),
            subtune,
        },
        header.flags.sid_model,
        trace.timing_exact,
    );
    trace.capture.validate()?;
    Ok(trace.capture.project_trace())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rational_scheduler_carries_fraction_without_long_run_drift() {
        let timing = PlaybackTiming {
            clock: SystemClock::Pal,
            call_rate: CallRate::new(3, 1),
            cia_timed: false,
            cia_period: None,
        };
        let scheduler = EmulationClock::new(timing);
        let calls = 100_000_u64;
        let actual = scheduler.boundary(calls).0;
        let scaled = u128::from(calls) * u128::from(timing.clock.phi2_hz());
        let expected_floor = (scaled / 3) as u64;
        assert_eq!(actual, expected_floor);
        assert!(
            scaled % 3 < 3,
            "scheduling error must remain below one cycle"
        );
    }

    #[test]
    fn pal_vblank_boundary_is_the_selected_host_model() {
        let scheduler = EmulationClock::new(PlaybackTiming::vblank(SystemClock::Pal));
        assert_eq!(scheduler.boundary(1), ChipCycle(19_656));
    }

    #[test]
    fn vblank_timing_exposes_rational_call_duration() {
        let pal = PlaybackTiming::vblank(SystemClock::Pal);
        let ntsc = PlaybackTiming::vblank(SystemClock::Ntsc);
        assert_eq!(pal.call_rate.numerator(), 985_248);
        assert_eq!(pal.call_rate.denominator(), 19_656);
        assert!((pal.calls_per_second() - 50.124_542).abs() < 0.000_001);
        assert!((ntsc.calls_per_second() - 59.826_089_5).abs() < 0.000_001);
        assert!((pal.calls_per_second() * pal.seconds_per_call() - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn host_clock_seeds_tvsflg_consistently() {
        let mut bytes = vec![0_u8; 0x7C];
        bytes[0..4].copy_from_slice(b"PSID");
        bytes[4..6].copy_from_slice(&2_u16.to_be_bytes());
        bytes[6..8].copy_from_slice(&0x7C_u16.to_be_bytes());
        bytes[8..14].copy_from_slice(&[0x10, 0x00, 0x10, 0x00, 0x10, 0x00]);
        bytes[14..18].copy_from_slice(&[0x00, 0x01, 0x00, 0x01]);
        bytes.push(0x60);
        let header = crate::header::parse(&bytes).unwrap();

        let mut pal = Emulator::with_timing(PlaybackTiming::vblank(SystemClock::Pal));
        pal.load(&header, &bytes).unwrap();
        let mut ntsc = Emulator::with_timing(PlaybackTiming::vblank(SystemClock::Ntsc));
        ntsc.load(&header, &bytes).unwrap();
        assert_eq!(pal.read_ram(PAL_NTSC_FLAG_ADDR), PAL);
        assert_eq!(ntsc.read_ram(PAL_NTSC_FLAG_ADDR), NTSC);
    }

    #[test]
    fn frame_runner_resolves_zero_play_address_through_irq_vector() {
        let mut emulator = Emulator::new();
        let [handler_lo, handler_hi] = 0x1000_u16.to_le_bytes();
        emulator.poke(HARDWARE_IRQ_VECTOR, handler_lo);
        emulator.poke(HARDWARE_IRQ_VECTOR.wrapping_add(1), handler_hi);
        for (offset, byte) in [0xA9, 0xAA, 0x8D, 0x00, 0xD4, 0x40].into_iter().enumerate() {
            emulator.poke(0x1000 + u16::try_from(offset).unwrap_or_default(), byte);
        }
        let frame = emulator
            .run_play_frame(PlayAddress(0), FrameIndex(0))
            .unwrap();
        assert_eq!(frame.writes.len(), 1);
        assert_eq!(frame.writes[0].value, 0xAA);
    }
}
