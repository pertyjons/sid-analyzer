use crate::header::SidModel;
use crate::trace::{Adsr, ChipCycle, CpuCycles, SID_REGISTER_LAST, SidRegister, SubFrameOffset};
use serde::{Deserialize, Serialize};

const SID_REGISTERS: usize = SID_REGISTER_LAST.0 as usize + 1;
const LAST_WRITABLE_REGISTER: u8 = 0x18;
const VOICE_STRIDE: u8 = 7;
const CONTROL_OFFSET: u8 = 4;
const ATTACK_DECAY_OFFSET: u8 = 5;
const SUSTAIN_RELEASE_OFFSET: u8 = 6;
const RATE_COUNTER_MASK: u16 = 0x7FFF;
const ACCUMULATOR_MASK: u32 = 0x00FF_FFFF;
const NOISE_MASK: u32 = 0x007F_FFFF;
const OSCILLATOR_POWER_ON: u32 = 0x0055_5555;
const NOISE_SEED: u32 = 0x003F_FFFF;
const TEST_HOLD_FILL_CYCLES_6581: u32 = 50_000;
const TEST_HOLD_FILL_CYCLES_8580: u32 = 986_000;
const NOISE_ALL_ONES: u32 = NOISE_MASK;
const WAVEFORM_MASK: u16 = 0x0FFF;
const NOISE_WRITEBACK_MASK: u32 =
    !((1 << 2) | (1 << 4) | (1 << 8) | (1 << 11) | (1 << 13) | (1 << 17) | (1 << 20) | (1 << 22));

const RATE_PERIOD: [u16; 16] = [
    9, 32, 63, 95, 149, 220, 267, 313, 392, 977, 1954, 3126, 3907, 11720, 19532, 31251,
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(transparent)]
#[must_use]
pub struct EnvLevel(pub u8);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(transparent)]
#[must_use]
pub struct EnvRateCounter(pub u16);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(transparent)]
#[must_use]
pub struct EnvRatePeriod(pub u16);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(transparent)]
#[must_use]
pub struct EnvExponentialCounter(pub u8);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(transparent)]
#[must_use]
pub struct EnvExponentialPeriod(pub u8);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(transparent)]
#[must_use]
pub struct PipelineCycles(pub u8);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(transparent)]
#[must_use]
pub struct TestFillCycles(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[must_use]
pub enum EnvPhase {
    Attack,
    DecaySustain,
    #[default]
    Release,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct EnvelopeSnapshot {
    pub level: EnvLevel,
    pub phase: EnvPhase,
    pub rate_counter: EnvRateCounter,
    pub exponential_counter: EnvExponentialCounter,
    pub exponential_period: EnvExponentialPeriod,
    pub gate: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[must_use]
pub enum EnvelopeEventKind {
    EnteredAttack,
    LeftZero,
    ReachedZero,
    EnteredDecaySustain,
    EnteredRelease,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct EnvelopeEvent {
    pub offset: SubFrameOffset,
    pub kind: EnvelopeEventKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct EnvelopeFrameActivity {
    pub start_level: EnvLevel,
    pub end_level: EnvLevel,
    pub peak_level: EnvLevel,
    pub active_cycles: CpuCycles,
    pub first_nonzero: Option<SubFrameOffset>,
    pub reached_zero: Option<SubFrameOffset>,
    pub events: Vec<EnvelopeEvent>,
}

/// Oscillator state at a sampling point.
///
/// From [`DigitalSid::oscillator_snapshots`] the counters (`sync_resets`,
/// `source_msb_edges`) are **cumulative since power-on**; `analyze` rewrites
/// them to **per-frame deltas** before storing the snapshot on `FrameState`.
/// Consumers must know which flavor they hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct OscillatorSnapshot {
    pub accumulator: u32,
    pub noise_shift_register: u32,
    /// Hard-sync resets applied to *this* voice (as a sync destination).
    pub sync_resets: u64,
    /// This voice's *own* accumulator-MSB rising edges — its activity *as*
    /// the sync/ring source of the next voice, hence the name.
    pub source_msb_edges: u64,
    pub combined_waveform_exact: bool,
    /// Noise is currently combined with another waveform, so destructive
    /// shift-register write-back is active.
    pub noise_state_poisoned: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Oscillator {
    accumulator: u32,
    noise_shift_register: u32,
    noise_shift_latch: u32,
    noise_output: u16,
    waveform_output: u16,
    tri_saw_pipeline: u16,
    osc3: u16,
    pulse_output: u16,
    shift_pipeline: PipelineCycles,
    test_or_reset: bool,
    sync_resets: u64,
    source_msb_edges: u64,
    noise_poisoned: bool,
    test_fill: TestFillCycles,
}

impl Oscillator {
    fn new() -> Self {
        Self {
            accumulator: OSCILLATOR_POWER_ON,
            noise_shift_register: NOISE_SEED,
            noise_shift_latch: NOISE_ALL_ONES,
            noise_output: noise_output(NOISE_SEED),
            waveform_output: 0,
            tri_saw_pipeline: 0x0555,
            osc3: 0,
            pulse_output: WAVEFORM_MASK,
            shift_pipeline: PipelineCycles(0),
            test_or_reset: true,
            sync_resets: 0,
            source_msb_edges: 0,
            noise_poisoned: false,
            test_fill: TestFillCycles(0),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct Observation {
    origin: ChipCycle,
    start_level: EnvLevel,
    peak_level: EnvLevel,
    active_cycles: u64,
    first_nonzero: Option<ChipCycle>,
    reached_zero: Option<ChipCycle>,
    events: Vec<(ChipCycle, EnvelopeEventKind)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Envelope {
    cycle: ChipCycle,
    level: EnvLevel,
    env3: EnvLevel,
    phase: EnvPhase,
    next_phase: EnvPhase,
    rate_counter: u16,
    rate_period: u16,
    rate_match_pending: bool,
    exp_counter: u8,
    exp_period: u8,
    next_exp_period: Option<EnvExponentialPeriod>,
    state_pipeline: PipelineCycles,
    envelope_pipeline: PipelineCycles,
    exponential_pipeline: PipelineCycles,
    gate: bool,
    hold_zero: bool,
    adsr: Adsr,
    observation: Option<Observation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct EnvelopeObservationCheckpoint {
    pub origin: ChipCycle,
    pub start_level: EnvLevel,
    pub peak_level: EnvLevel,
    pub active_cycles: CpuCycles,
    pub first_nonzero: Option<ChipCycle>,
    pub reached_zero: Option<ChipCycle>,
    pub events: Vec<(ChipCycle, EnvelopeEventKind)>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct EnvelopeCheckpoint {
    pub cycle: ChipCycle,
    pub level: EnvLevel,
    pub env3: EnvLevel,
    pub phase: EnvPhase,
    pub next_phase: EnvPhase,
    pub rate_counter: EnvRateCounter,
    pub rate_period: EnvRatePeriod,
    pub rate_match_pending: bool,
    pub exponential_counter: EnvExponentialCounter,
    pub exponential_period: EnvExponentialPeriod,
    pub next_exponential_period: Option<EnvExponentialPeriod>,
    pub state_pipeline: PipelineCycles,
    pub envelope_pipeline: PipelineCycles,
    pub exponential_pipeline: PipelineCycles,
    pub gate: bool,
    pub hold_zero: bool,
    pub adsr: Adsr,
    pub observation: Option<EnvelopeObservationCheckpoint>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct OscillatorCheckpoint {
    pub accumulator: u32,
    pub noise_shift_register: u32,
    pub noise_shift_latch: u32,
    pub noise_output: u16,
    pub waveform_output: u16,
    pub tri_saw_pipeline: u16,
    pub osc3: u16,
    pub pulse_output: u16,
    pub shift_pipeline: PipelineCycles,
    pub test_or_reset: bool,
    pub sync_resets: u64,
    pub source_msb_edges: u64,
    pub noise_poisoned: bool,
    pub test_fill: TestFillCycles,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct DigitalSidCheckpoint {
    pub model: SidModel,
    pub registers: [u8; SID_REGISTERS],
    pub envelopes: [EnvelopeCheckpoint; 3],
    pub oscillators: [OscillatorCheckpoint; 3],
    pub cycle: ChipCycle,
}

impl Envelope {
    fn new() -> Self {
        Self {
            cycle: ChipCycle(0),
            level: EnvLevel(0xAA),
            env3: EnvLevel(0),
            phase: EnvPhase::Release,
            next_phase: EnvPhase::Release,
            rate_counter: 0,
            rate_period: RATE_PERIOD[0],
            rate_match_pending: true,
            exp_counter: 0,
            exp_period: 1,
            next_exp_period: None,
            state_pipeline: PipelineCycles(0),
            envelope_pipeline: PipelineCycles(0),
            exponential_pipeline: PipelineCycles(0),
            gate: false,
            hold_zero: false,
            adsr: Adsr::default(),
            observation: None,
        }
    }

    fn clock_to(&mut self, target: ChipCycle) {
        debug_assert!(target >= self.cycle, "envelope clock moved backwards");
        while self.cycle < target {
            let transient = self.next_exp_period.is_some()
                || self.state_pipeline.0 != 0
                || self.envelope_pipeline.0 != 0
                || self.exponential_pipeline.0 != 0
                || self.rate_match_pending;
            if transient {
                self.clock_one();
                continue;
            }

            let threshold = self.rate_period - 1;
            let until_match = if self.rate_counter <= threshold {
                threshold - self.rate_counter + 1
            } else {
                (RATE_COUNTER_MASK - self.rate_counter) + threshold + 1
            };
            let remaining = target.0 - self.cycle.0;
            let advance = remaining.min(u64::from(until_match));
            self.observe_active(advance);
            self.cycle.0 += advance;
            self.env3 = self.level;
            if advance == u64::from(until_match) {
                self.rate_counter = threshold;
                self.rate_match_pending = true;
            } else {
                let advanced = u64::from(self.rate_counter) + advance;
                self.rate_counter = if advanced <= u64::from(RATE_COUNTER_MASK) {
                    advanced as u16
                } else {
                    (advanced - u64::from(RATE_COUNTER_MASK)) as u16
                };
            }
        }
    }

    fn clock_one(&mut self) {
        self.observe_active(1);
        self.cycle.0 += 1;
        self.env3 = self.level;

        if let Some(period) = self.next_exp_period.take() {
            self.exp_period = period.0;
        }

        if self.state_pipeline.0 != 0 {
            self.clock_state_pipeline();
        }

        if self.envelope_pipeline.0 != 0 {
            self.envelope_pipeline.0 -= 1;
            if self.envelope_pipeline.0 == 0 {
                self.step_envelope();
            }
        } else if self.exponential_pipeline.0 != 0 {
            self.exponential_pipeline.0 -= 1;
            if self.exponential_pipeline.0 == 0 {
                self.exp_counter = 0;
                if (self.phase == EnvPhase::DecaySustain
                    && self.level.0 != sustain_level(self.adsr.sustain))
                    || self.phase == EnvPhase::Release
                {
                    self.envelope_pipeline = PipelineCycles(1);
                }
            }
        } else if self.rate_match_pending {
            self.rate_counter = 0;
            self.rate_match_pending = false;
            if self.phase == EnvPhase::Attack {
                self.exp_counter = 0;
                self.envelope_pipeline = PipelineCycles(2);
            } else if !self.hold_zero {
                self.exp_counter = self.exp_counter.wrapping_add(1);
                if self.exp_counter == self.exp_period {
                    self.exponential_pipeline =
                        PipelineCycles(if self.exp_period == 1 { 1 } else { 2 });
                }
            }
        }

        if self.rate_counter == self.rate_period - 1 {
            self.rate_match_pending = true;
        } else {
            self.rate_counter = if self.rate_counter == RATE_COUNTER_MASK {
                1
            } else {
                self.rate_counter + 1
            };
        }
    }

    fn clock_state_pipeline(&mut self) {
        self.state_pipeline.0 -= 1;
        match self.next_phase {
            EnvPhase::Attack if self.state_pipeline.0 == 1 => {
                self.rate_period = RATE_PERIOD[self.adsr.decay as usize];
            }
            EnvPhase::Attack if self.state_pipeline.0 == 0 => {
                self.phase = EnvPhase::Attack;
                self.rate_period = RATE_PERIOD[self.adsr.attack as usize];
                self.hold_zero = false;
                self.observe_event(EnvelopeEventKind::EnteredAttack);
            }
            EnvPhase::DecaySustain if self.state_pipeline.0 == 0 => {
                self.phase = EnvPhase::DecaySustain;
                self.rate_period = RATE_PERIOD[self.adsr.decay as usize];
                self.observe_event(EnvelopeEventKind::EnteredDecaySustain);
            }
            EnvPhase::Release
                if (self.phase == EnvPhase::Attack && self.state_pipeline.0 == 0)
                    || (self.phase == EnvPhase::DecaySustain && self.state_pipeline.0 == 1) =>
            {
                self.phase = EnvPhase::Release;
                self.rate_period = RATE_PERIOD[self.adsr.release as usize];
                self.observe_event(EnvelopeEventKind::EnteredRelease);
            }
            _ => {}
        }
    }

    fn step_envelope(&mut self) {
        if self.hold_zero {
            return;
        }
        let old_level = self.level;
        if self.phase == EnvPhase::Attack {
            self.level.0 = self.level.0.wrapping_add(1);
            if self.level.0 == u8::MAX {
                self.next_phase = EnvPhase::DecaySustain;
                self.state_pipeline = PipelineCycles(3);
            }
        } else {
            self.level.0 = self.level.0.wrapping_sub(1);
            if self.level.0 == 0 {
                self.hold_zero = true;
            }
        }

        if old_level.0 != 0 && self.level.0 == 0 {
            self.observe_event(EnvelopeEventKind::ReachedZero);
            if let Some(observation) = &mut self.observation {
                observation.reached_zero.get_or_insert(self.cycle);
            }
        }
        if old_level.0 == 0 && self.level.0 != 0 {
            self.observe_event(EnvelopeEventKind::LeftZero);
            if let Some(observation) = &mut self.observation {
                observation.first_nonzero.get_or_insert(self.cycle);
            }
        }
        if let Some(observation) = &mut self.observation {
            observation.peak_level.0 = observation.peak_level.0.max(self.level.0);
        }
        if let Some(period) = exp_period(self.level.0) {
            self.next_exp_period = Some(EnvExponentialPeriod(period));
        }
    }

    fn write_control(&mut self, value: u8) {
        let gate = value & 0x01 != 0;
        if gate && !self.gate {
            self.next_phase = EnvPhase::Attack;
            self.state_pipeline = PipelineCycles(2);
            if self.rate_match_pending || self.exponential_pipeline.0 == 2 {
                self.envelope_pipeline = PipelineCycles(
                    if self.exp_period == 1 || self.exponential_pipeline.0 == 2 {
                        2
                    } else {
                        4
                    },
                );
            } else if self.exponential_pipeline.0 == 1 {
                self.state_pipeline = PipelineCycles(3);
            }
        } else if !gate && self.gate {
            self.next_phase = EnvPhase::Release;
            self.state_pipeline = PipelineCycles(if self.envelope_pipeline.0 > 0 { 3 } else { 2 });
        }
        self.gate = gate;
    }

    fn write_attack_decay(&mut self, value: u8) {
        self.adsr.attack = value >> 4;
        self.adsr.decay = value & 0x0F;
        self.rate_period = match self.phase {
            EnvPhase::Attack => RATE_PERIOD[self.adsr.attack as usize],
            EnvPhase::DecaySustain => RATE_PERIOD[self.adsr.decay as usize],
            EnvPhase::Release => self.rate_period,
        };
    }

    fn write_sustain_release(&mut self, value: u8) {
        self.adsr.sustain = value >> 4;
        self.adsr.release = value & 0x0F;
        if self.phase == EnvPhase::Release {
            self.rate_period = RATE_PERIOD[self.adsr.release as usize];
        }
    }

    fn begin_observation(&mut self, origin: ChipCycle) {
        debug_assert_eq!(self.cycle, origin);
        self.observation = Some(Observation {
            origin,
            start_level: self.level,
            peak_level: self.level,
            ..Observation::default()
        });
    }

    fn finish_observation(&mut self) -> EnvelopeFrameActivity {
        let Some(observation) = self.observation.take() else {
            return EnvelopeFrameActivity::default();
        };
        EnvelopeFrameActivity {
            start_level: observation.start_level,
            end_level: self.level,
            peak_level: observation.peak_level,
            active_cycles: CpuCycles(observation.active_cycles),
            first_nonzero: observation
                .first_nonzero
                .map(|cycle| offset_from(observation.origin, cycle)),
            reached_zero: observation
                .reached_zero
                .map(|cycle| offset_from(observation.origin, cycle)),
            events: observation
                .events
                .into_iter()
                .map(|(cycle, kind)| EnvelopeEvent {
                    offset: offset_from(observation.origin, cycle),
                    kind,
                })
                .collect(),
        }
    }

    fn observe_active(&mut self, cycles: u64) {
        if self.level.0 != 0
            && let Some(observation) = &mut self.observation
        {
            observation.active_cycles += cycles;
        }
    }

    fn observe_event(&mut self, kind: EnvelopeEventKind) {
        if let Some(observation) = &mut self.observation {
            observation.events.push((self.cycle, kind));
        }
    }

    fn snapshot(&self) -> EnvelopeSnapshot {
        EnvelopeSnapshot {
            level: self.level,
            phase: self.phase,
            rate_counter: EnvRateCounter(if self.rate_match_pending {
                0
            } else {
                self.rate_counter
            }),
            exponential_counter: EnvExponentialCounter(self.exp_counter),
            exponential_period: EnvExponentialPeriod(self.exp_period),
            gate: self.gate,
        }
    }

    fn checkpoint(&self) -> EnvelopeCheckpoint {
        let Self {
            cycle,
            level,
            env3,
            phase,
            next_phase,
            rate_counter,
            rate_period,
            rate_match_pending,
            exp_counter,
            exp_period,
            next_exp_period,
            state_pipeline,
            envelope_pipeline,
            exponential_pipeline,
            gate,
            hold_zero,
            adsr,
            observation,
        } = self;
        EnvelopeCheckpoint {
            cycle: *cycle,
            level: *level,
            env3: *env3,
            phase: *phase,
            next_phase: *next_phase,
            rate_counter: EnvRateCounter(if *rate_match_pending {
                0
            } else {
                *rate_counter
            }),
            rate_period: EnvRatePeriod(*rate_period),
            rate_match_pending: *rate_match_pending,
            exponential_counter: EnvExponentialCounter(*exp_counter),
            exponential_period: EnvExponentialPeriod(*exp_period),
            next_exponential_period: *next_exp_period,
            state_pipeline: *state_pipeline,
            envelope_pipeline: *envelope_pipeline,
            exponential_pipeline: *exponential_pipeline,
            gate: *gate,
            hold_zero: *hold_zero,
            adsr: *adsr,
            observation: observation.as_ref().map(Observation::checkpoint),
        }
    }

    fn from_checkpoint(checkpoint: EnvelopeCheckpoint) -> Self {
        let EnvelopeCheckpoint {
            cycle,
            level,
            env3,
            phase,
            next_phase,
            rate_counter,
            rate_period,
            rate_match_pending,
            exponential_counter,
            exponential_period,
            next_exponential_period,
            state_pipeline,
            envelope_pipeline,
            exponential_pipeline,
            gate,
            hold_zero,
            adsr,
            observation,
        } = checkpoint;
        Self {
            cycle,
            level,
            env3,
            phase,
            next_phase,
            rate_counter: if rate_match_pending {
                rate_period.0 - 1
            } else {
                rate_counter.0
            },
            rate_period: rate_period.0,
            rate_match_pending,
            exp_counter: exponential_counter.0,
            exp_period: exponential_period.0,
            next_exp_period: next_exponential_period,
            state_pipeline,
            envelope_pipeline,
            exponential_pipeline,
            gate,
            hold_zero,
            adsr,
            observation: observation.map(Observation::from_checkpoint),
        }
    }
}

impl Observation {
    fn checkpoint(&self) -> EnvelopeObservationCheckpoint {
        let Self {
            origin,
            start_level,
            peak_level,
            active_cycles,
            first_nonzero,
            reached_zero,
            events,
        } = self;
        EnvelopeObservationCheckpoint {
            origin: *origin,
            start_level: *start_level,
            peak_level: *peak_level,
            active_cycles: CpuCycles(*active_cycles),
            first_nonzero: *first_nonzero,
            reached_zero: *reached_zero,
            events: events.clone(),
        }
    }

    fn from_checkpoint(checkpoint: EnvelopeObservationCheckpoint) -> Self {
        let EnvelopeObservationCheckpoint {
            origin,
            start_level,
            peak_level,
            active_cycles,
            first_nonzero,
            reached_zero,
            events,
        } = checkpoint;
        Self {
            origin,
            start_level,
            peak_level,
            active_cycles: active_cycles.0,
            first_nonzero,
            reached_zero,
            events,
        }
    }
}

impl Oscillator {
    fn clock(&mut self, control: u8, frequency: u32) -> bool {
        if control & 0x08 != 0 {
            if self.test_fill.0 != 0 {
                self.test_fill.0 -= 1;
                if self.test_fill.0 == 0 {
                    self.noise_shift_register |= self.noise_shift_register >> 1;
                    self.noise_shift_register |= 1 << 22;
                    self.noise_shift_latch = self.noise_shift_register;
                    self.noise_output = noise_output(self.noise_shift_register);
                }
            }
            self.test_or_reset = true;
            self.pulse_output = WAVEFORM_MASK;
            return false;
        }

        let old = self.accumulator;
        self.accumulator = self.accumulator.wrapping_add(frequency) & ACCUMULATOR_MASK;
        let rising = !old & self.accumulator;
        let msb_rising = rising & 0x800000 != 0;
        if msb_rising {
            self.source_msb_edges += 1;
        }

        if rising & 0x080000 != 0 {
            self.shift_pipeline = PipelineCycles(2);
        } else if self.shift_pipeline.0 != 0 {
            self.shift_pipeline.0 -= 1;
            if self.shift_pipeline.0 == 1 {
                self.test_or_reset = false;
                self.noise_shift_latch = self.noise_shift_register;
            } else if self.shift_pipeline.0 == 0 {
                self.complete_noise_shift(control >> 4 > 0x08);
            }
        }
        msb_rising
    }

    fn complete_noise_shift(&mut self, combined_writeback: bool) {
        if combined_writeback {
            self.noise_shift_latch = (self.noise_shift_register & NOISE_WRITEBACK_MASK)
                | noise_writeback(self.waveform_output);
        }
        let input = u32::from(self.test_or_reset) | self.noise_shift_latch;
        let feedback = ((input ^ (self.noise_shift_latch >> 5)) & 1) << 22;
        self.noise_shift_register = (self.noise_shift_latch >> 1) | feedback;
        self.noise_output = noise_output(self.noise_shift_register);
    }

    fn update_output(
        &mut self,
        control: u8,
        pulse_width: u16,
        source_accumulator: u32,
        is_6581: bool,
    ) {
        let waveform = control >> 4;
        if waveform != 0 {
            let ring_mask = if control & 0x04 != 0 && control & 0x20 == 0 {
                0x800000
            } else {
                0
            };
            let index = ((self.accumulator ^ (!source_accumulator & ring_mask)) >> 12) as u16;
            let base_output = match waveform & 0x03 {
                0 => WAVEFORM_MASK,
                1 => triangle_output(index),
                2 => index,
                3 => 0,
                _ => 0,
            };
            let pulse_mask = if waveform & 0x04 != 0 {
                self.pulse_output
            } else {
                WAVEFORM_MASK
            };
            let noise_mask = if waveform & 0x08 != 0 {
                self.noise_output
            } else {
                WAVEFORM_MASK
            };
            self.waveform_output = base_output & pulse_mask & noise_mask;

            if waveform & 0x03 != 0 && !is_6581 {
                self.osc3 = if waveform & 0x03 == 0x03 {
                    0
                } else {
                    self.tri_saw_pipeline & pulse_mask & noise_mask
                };
                self.tri_saw_pipeline = base_output;
            } else {
                self.osc3 = self.waveform_output;
            }

            if waveform > 0x08 {
                if self.shift_pipeline.0 != 1 && control & 0x08 == 0 {
                    self.noise_shift_register &=
                        NOISE_WRITEBACK_MASK | noise_writeback(self.waveform_output);
                    self.noise_output &= self.waveform_output;
                } else {
                    self.noise_output = self.waveform_output;
                }
            }
        }

        self.pulse_output = if self.accumulator >> 12 >= u32::from(pulse_width) {
            WAVEFORM_MASK
        } else {
            0
        };
    }

    fn checkpoint(&self) -> OscillatorCheckpoint {
        let Self {
            accumulator,
            noise_shift_register,
            noise_shift_latch,
            noise_output,
            waveform_output,
            tri_saw_pipeline,
            osc3,
            pulse_output,
            shift_pipeline,
            test_or_reset,
            sync_resets,
            source_msb_edges,
            noise_poisoned,
            test_fill,
        } = self;
        OscillatorCheckpoint {
            accumulator: *accumulator,
            noise_shift_register: *noise_shift_register,
            noise_shift_latch: *noise_shift_latch,
            noise_output: *noise_output,
            waveform_output: *waveform_output,
            tri_saw_pipeline: *tri_saw_pipeline,
            osc3: *osc3,
            pulse_output: *pulse_output,
            shift_pipeline: *shift_pipeline,
            test_or_reset: *test_or_reset,
            sync_resets: *sync_resets,
            source_msb_edges: *source_msb_edges,
            noise_poisoned: *noise_poisoned,
            test_fill: *test_fill,
        }
    }

    fn from_checkpoint(checkpoint: OscillatorCheckpoint) -> Self {
        let OscillatorCheckpoint {
            accumulator,
            noise_shift_register,
            noise_shift_latch,
            noise_output,
            waveform_output,
            tri_saw_pipeline,
            osc3,
            pulse_output,
            shift_pipeline,
            test_or_reset,
            sync_resets,
            source_msb_edges,
            noise_poisoned,
            test_fill,
        } = checkpoint;
        Self {
            accumulator,
            noise_shift_register,
            noise_shift_latch,
            noise_output,
            waveform_output,
            tri_saw_pipeline,
            osc3,
            pulse_output,
            shift_pipeline,
            test_or_reset,
            sync_resets,
            source_msb_edges,
            noise_poisoned,
            test_fill,
        }
    }
}

fn noise_output(register: u32) -> u16 {
    (((register & (1 << 2)) << 9)
        | ((register & (1 << 4)) << 6)
        | ((register & (1 << 8)) << 1)
        | ((register & (1 << 11)) >> 3)
        | ((register & (1 << 13)) >> 6)
        | ((register & (1 << 17)) >> 11)
        | ((register & (1 << 20)) >> 15)
        | ((register & (1 << 22)) >> 18)) as u16
}

fn noise_writeback(output: u16) -> u32 {
    ((u32::from(output) & (1 << 11)) >> 9)
        | ((u32::from(output) & (1 << 10)) >> 6)
        | ((u32::from(output) & (1 << 9)) >> 1)
        | ((u32::from(output) & (1 << 8)) << 3)
        | ((u32::from(output) & (1 << 7)) << 6)
        | ((u32::from(output) & (1 << 6)) << 11)
        | ((u32::from(output) & (1 << 5)) << 15)
        | ((u32::from(output) & (1 << 4)) << 18)
}

fn triangle_output(index: u16) -> u16 {
    let folded = if index & 0x0800 == 0 {
        index
    } else {
        index ^ WAVEFORM_MASK
    };
    (folded << 1) & WAVEFORM_MASK
}

fn rising_bit_edges(start: u64, frequency: u32, cycles: u64, offset: u64, span: u64) -> u64 {
    let end = start + u64::from(frequency).saturating_mul(cycles);
    (end + offset) / span - (start + offset) / span
}

fn sustain_level(nibble: u8) -> u8 {
    (nibble << 4) | nibble
}

fn exp_period(level: u8) -> Option<u8> {
    match level {
        0xFF => Some(1),
        0x5D => Some(2),
        0x36 => Some(4),
        0x1A => Some(8),
        0x0E => Some(16),
        0x06 => Some(30),
        0x00 => Some(1),
        _ => None,
    }
}

fn offset_from(origin: ChipCycle, cycle: ChipCycle) -> SubFrameOffset {
    SubFrameOffset(cycle.0.saturating_sub(origin.0) as u32)
}

/// Digital SID state driven by an absolute cycle/event timeline.
///
/// The state machines are bit-exact for a supplied cycle/event timeline.
/// Whole-tune reconstruction is cycle-accurate only to the fidelity of the
/// captured timeline — bounded by instruction-start write stamping, assumed
/// idle time, unavailable CIA timing, the init→first-play host policy, and the
/// selected NTSC raster model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DigitalSid {
    model: SidModel,
    registers: [u8; SID_REGISTERS],
    envelopes: [Envelope; 3],
    oscillators: [Oscillator; 3],
    cycle: ChipCycle,
}

impl DigitalSid {
    #[must_use]
    pub fn new() -> Self {
        Self::with_model(SidModel::Unknown)
    }

    #[must_use]
    pub fn with_model(model: SidModel) -> Self {
        Self {
            model,
            registers: [0; SID_REGISTERS],
            envelopes: std::array::from_fn(|_| Envelope::new()),
            oscillators: std::array::from_fn(|_| Oscillator::new()),
            cycle: ChipCycle(0),
        }
    }

    #[must_use]
    pub fn model(&self) -> SidModel {
        self.model
    }

    fn test_hold_fill_cycles(&self) -> TestFillCycles {
        match self.model {
            SidModel::Mos8580 => TestFillCycles(TEST_HOLD_FILL_CYCLES_8580),
            SidModel::Unknown | SidModel::Mos6581 | SidModel::Both => {
                TestFillCycles(TEST_HOLD_FILL_CYCLES_6581)
            }
        }
    }

    fn is_6581(&self) -> bool {
        !matches!(self.model, SidModel::Mos8580)
    }

    pub fn clock_to(&mut self, cycle: ChipCycle) {
        debug_assert!(cycle >= self.cycle, "digital SID clock moved backwards");
        for envelope in &mut self.envelopes {
            envelope.clock_to(cycle);
        }
        let elapsed = cycle.0.saturating_sub(self.cycle.0);
        if elapsed > 2 && self.can_jump_oscillators() {
            self.jump_oscillators(elapsed);
        } else {
            for _ in 0..elapsed {
                self.clock_oscillators();
            }
        }
        self.cycle = self.cycle.max(cycle);
    }

    fn can_jump_oscillators(&self) -> bool {
        (0..3).all(|voice| {
            let control =
                self.registers[voice * usize::from(VOICE_STRIDE) + usize::from(CONTROL_OFFSET)];
            let waveform = control >> 4;
            control & 0x0A == 0
                && waveform <= 0x08
                && self.oscillators[voice].shift_pipeline.0 == 0
                && self.oscillators[voice].test_fill.0 == 0
        })
    }

    fn jump_oscillators(&mut self, elapsed: u64) {
        let starts = self
            .oscillators
            .each_ref()
            .map(|oscillator| oscillator.accumulator);
        let mut previous = [0; 3];
        for voice in 0..3 {
            let base = voice * usize::from(VOICE_STRIDE);
            let frequency =
                u32::from(self.registers[base]) | (u32::from(self.registers[base + 1]) << 8);
            let start = u64::from(starts[voice]);
            let delta = u64::from(frequency).saturating_mul(elapsed);
            let before_delta = u64::from(frequency).saturating_mul(elapsed - 1);
            let end = start + delta;
            previous[voice] = ((start + before_delta) as u32) & ACCUMULATOR_MASK;
            self.oscillators[voice].accumulator = (end as u32) & ACCUMULATOR_MASK;

            let msb_edges = rising_bit_edges(start, frequency, elapsed, 0x800000, 0x1000000);
            self.oscillators[voice].source_msb_edges += msb_edges;

            let completed = rising_bit_edges(
                start,
                frequency,
                elapsed.saturating_sub(2),
                0x080000,
                0x100000,
            );
            for _ in 0..completed {
                self.oscillators[voice].noise_shift_latch =
                    self.oscillators[voice].noise_shift_register;
                self.oscillators[voice].test_or_reset = false;
                self.oscillators[voice].complete_noise_shift(false);
            }
            let through_previous =
                rising_bit_edges(start, frequency, elapsed - 1, 0x080000, 0x100000);
            let through_end = rising_bit_edges(start, frequency, elapsed, 0x080000, 0x100000);
            self.oscillators[voice].shift_pipeline = if through_end > through_previous {
                PipelineCycles(2)
            } else if through_previous > completed {
                self.oscillators[voice].noise_shift_latch =
                    self.oscillators[voice].noise_shift_register;
                self.oscillators[voice].test_or_reset = false;
                PipelineCycles(1)
            } else {
                PipelineCycles(0)
            };
        }

        let final_accumulators = self
            .oscillators
            .each_ref()
            .map(|oscillator| oscillator.accumulator);
        let is_6581 = self.is_6581();
        for voice in 0..3 {
            let base = voice * usize::from(VOICE_STRIDE);
            let control = self.registers[base + usize::from(CONTROL_OFFSET)];
            let pulse_width = u16::from(self.registers[base + 2])
                | ((u16::from(self.registers[base + 3]) & 0x0F) << 8);
            let source = (voice + 2) % 3;
            self.oscillators[voice].pulse_output =
                if previous[voice] >> 12 >= u32::from(pulse_width) {
                    WAVEFORM_MASK
                } else {
                    0
                };
            let waveform = control >> 4;
            if waveform & 0x03 != 0 && !is_6581 {
                let ring_mask = if control & 0x04 != 0 && control & 0x20 == 0 {
                    0x800000
                } else {
                    0
                };
                let index = ((previous[voice] ^ (!previous[source] & ring_mask)) >> 12) as u16;
                self.oscillators[voice].tri_saw_pipeline = match waveform & 0x03 {
                    1 => triangle_output(index),
                    2 => index,
                    _ => 0,
                };
            }
            self.oscillators[voice].update_output(
                control,
                pulse_width,
                final_accumulators[source],
                is_6581,
            );
        }
    }

    fn clock_oscillators(&mut self) {
        let mut edges = [false; 3];
        for (voice, edge) in edges.iter_mut().enumerate() {
            let base = voice * usize::from(VOICE_STRIDE);
            let control = self.registers[base + usize::from(CONTROL_OFFSET)];
            let frequency =
                u32::from(self.registers[base]) | (u32::from(self.registers[base + 1]) << 8);
            *edge = self.oscillators[voice].clock(control, frequency);
        }
        let sync: [bool; 3] = std::array::from_fn(|voice| {
            let base = voice * usize::from(VOICE_STRIDE);
            self.registers[base + usize::from(CONTROL_OFFSET)] & 0x02 != 0
        });
        for voice in 0..3 {
            let source = (voice + 2) % 3;
            let source_source = (source + 2) % 3;
            if sync[voice] && edges[source] && !(sync[source] && edges[source_source]) {
                self.oscillators[voice].accumulator = 0;
                self.oscillators[voice].sync_resets += 1;
            }
        }

        let accumulators = self
            .oscillators
            .each_ref()
            .map(|oscillator| oscillator.accumulator);
        let is_6581 = self.is_6581();
        for voice in 0..3 {
            let base = voice * usize::from(VOICE_STRIDE);
            let control = self.registers[base + usize::from(CONTROL_OFFSET)];
            let pulse_width = u16::from(self.registers[base + 2])
                | ((u16::from(self.registers[base + 3]) & 0x0F) << 8);
            let source = (voice + 2) % 3;
            self.oscillators[voice].update_output(
                control,
                pulse_width,
                accumulators[source],
                is_6581,
            );
        }
    }

    pub fn write(&mut self, reg: SidRegister, value: u8, cycle: ChipCycle) {
        self.clock_to(cycle);
        let test_hold_fill_cycles = self.test_hold_fill_cycles();
        let old_value = self.registers.get(reg.0 as usize).copied().unwrap_or(0);
        if reg.0 <= LAST_WRITABLE_REGISTER {
            self.registers[reg.0 as usize] = value;
        }
        if reg.0 < 3 * VOICE_STRIDE {
            let voice = usize::from(reg.0 / VOICE_STRIDE);
            match reg.0 % VOICE_STRIDE {
                CONTROL_OFFSET => {
                    self.envelopes[voice].write_control(value);
                    let waveform = value >> 4;
                    self.oscillators[voice].noise_poisoned =
                        waveform & 0x08 != 0 && waveform & 0x07 != 0;
                    let old_test = old_value & 0x08 != 0;
                    let new_test = value & 0x08 != 0;
                    if new_test && !old_test {
                        self.oscillators[voice].accumulator = 0;
                        self.oscillators[voice].shift_pipeline = PipelineCycles(0);
                        self.oscillators[voice].noise_shift_latch =
                            self.oscillators[voice].noise_shift_register;
                        self.oscillators[voice].test_fill = test_hold_fill_cycles;
                    } else if old_test && !new_test {
                        self.oscillators[voice].complete_noise_shift(false);
                        self.oscillators[voice].test_fill = TestFillCycles(0);
                    }
                }
                ATTACK_DECAY_OFFSET => self.envelopes[voice].write_attack_decay(value),
                SUSTAIN_RELEASE_OFFSET => self.envelopes[voice].write_sustain_release(value),
                _ => {}
            }
        }
    }

    #[must_use]
    pub fn read(&mut self, reg: SidRegister, cycle: ChipCycle) -> u8 {
        self.clock_to(cycle);
        match reg.0 {
            // Open POT lines charge high with no paddles — mirror the Bus.
            0x19 | 0x1A => 0xFF,
            0x1B => (self.oscillators[2].osc3 >> 4) as u8,
            0x1C => self.envelopes[2].env3.0,
            _ => self.registers.get(reg.0 as usize).copied().unwrap_or(0),
        }
    }

    pub fn oscillator_snapshots(&self) -> [OscillatorSnapshot; 3] {
        std::array::from_fn(|voice| {
            let base = voice * usize::from(VOICE_STRIDE);
            OscillatorSnapshot {
                accumulator: self.oscillators[voice].accumulator,
                noise_shift_register: self.oscillators[voice].noise_shift_register,
                sync_resets: self.oscillators[voice].sync_resets,
                source_msb_edges: self.oscillators[voice].source_msb_edges,
                combined_waveform_exact: (self.registers[base + usize::from(CONTROL_OFFSET)] >> 4)
                    .count_ones()
                    <= 1,
                noise_state_poisoned: self.oscillators[voice].noise_poisoned,
            }
        })
    }

    #[cfg(test)]
    fn oscillator_output(&self, voice: usize) -> u8 {
        (self.oscillators[voice].osc3 >> 4) as u8
    }

    #[must_use]
    pub fn register(&self, reg: SidRegister) -> u8 {
        self.registers.get(reg.0 as usize).copied().unwrap_or(0)
    }

    pub fn begin_observation(&mut self, origin: ChipCycle) {
        self.clock_to(origin);
        for envelope in &mut self.envelopes {
            envelope.begin_observation(origin);
        }
    }

    pub fn finish_observation(&mut self) -> [EnvelopeFrameActivity; 3] {
        std::array::from_fn(|voice| self.envelopes[voice].finish_observation())
    }

    pub fn envelope_snapshots(&self) -> [EnvelopeSnapshot; 3] {
        std::array::from_fn(|voice| self.envelopes[voice].snapshot())
    }

    pub fn cycle(&self) -> ChipCycle {
        self.cycle
    }

    pub fn checkpoint(&self) -> DigitalSidCheckpoint {
        let Self {
            model,
            registers,
            envelopes,
            oscillators,
            cycle,
        } = self;
        DigitalSidCheckpoint {
            model: *model,
            registers: *registers,
            envelopes: std::array::from_fn(|voice| envelopes[voice].checkpoint()),
            oscillators: std::array::from_fn(|voice| oscillators[voice].checkpoint()),
            cycle: *cycle,
        }
    }

    pub fn restore(&mut self, checkpoint: DigitalSidCheckpoint) {
        let DigitalSidCheckpoint {
            model,
            registers,
            envelopes,
            oscillators,
            cycle,
        } = checkpoint;
        *self = Self {
            model,
            registers,
            envelopes: envelopes.map(Envelope::from_checkpoint),
            oscillators: oscillators.map(Oscillator::from_checkpoint),
            cycle,
        };
    }

    #[must_use]
    pub fn from_checkpoint(checkpoint: DigitalSidCheckpoint) -> Self {
        let mut sid = Self::new();
        sid.restore(checkpoint);
        sid
    }
}

impl Default for DigitalSid {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const V3_CTRL: SidRegister = SidRegister(0x12);
    const V3_AD: SidRegister = SidRegister(0x13);
    const V3_SR: SidRegister = SidRegister(0x14);
    const ENV3: SidRegister = SidRegister(0x1C);

    /// Per-cycle reSID reference: `if (++rate_counter & 0x8000)
    /// rate_counter = ++rate_counter & 0x7fff;` — the wrap skips 0.
    fn clock_reference(envelope: &mut Envelope, cycles: u64) {
        for _ in 0..cycles {
            envelope.clock_one();
        }
    }

    fn settle_envelope(sid: &mut DigitalSid) -> ChipCycle {
        while !sid.envelopes[2].hold_zero {
            sid.clock_to(ChipCycle(sid.cycle.0 + 1));
        }
        sid.cycle
    }

    fn clock_until_level(sid: &mut DigitalSid, level: EnvLevel) -> ChipCycle {
        while sid.envelopes[2].level != level {
            sid.clock_to(ChipCycle(sid.cycle.0 + 1));
        }
        sid.cycle
    }

    #[test]
    fn checkpoint_round_trip_preserves_every_hidden_state_field() {
        let mut sid = DigitalSid::with_model(SidModel::Mos8580);
        sid.write(SidRegister(0x00), 0x34, ChipCycle(0));
        sid.write(SidRegister(0x01), 0x12, ChipCycle(0));
        sid.write(V3_AD, 0xa4, ChipCycle(3));
        sid.write(V3_CTRL, 0x89, ChipCycle(7));
        sid.begin_observation(ChipCycle(11));
        sid.clock_to(ChipCycle(1234));
        sid.write(V3_CTRL, 0x80, ChipCycle(1234));
        let checkpoint = sid.checkpoint();

        let mut restored = DigitalSid::new();
        restored.restore(checkpoint.clone());
        assert_eq!(restored.checkpoint(), checkpoint);
        assert_eq!(restored, sid);

        sid.clock_to(ChipCycle(4321));
        restored.clock_to(ChipCycle(4321));
        assert_eq!(restored.checkpoint(), sid.checkpoint());
        assert_eq!(restored.finish_observation(), sid.finish_observation());
    }

    #[test]
    fn power_on_state_and_read_only_write_behavior() {
        let mut sid = DigitalSid::new();
        assert_eq!(sid.read(ENV3, ChipCycle(0)), 0);
        assert_eq!(sid.envelopes[2].level, EnvLevel(0xAA));
        assert_eq!(sid.envelopes[2].phase, EnvPhase::Release);
        assert!(!sid.envelopes[2].hold_zero);
        assert_eq!(sid.oscillators[2].accumulator, OSCILLATOR_POWER_ON);
        assert_eq!(sid.oscillators[2].noise_shift_register, NOISE_SEED);
        sid.write(ENV3, 0xAA, ChipCycle(7));
        assert_eq!(sid.read(ENV3, ChipCycle(7)), sid.envelopes[2].env3.0);
    }

    #[test]
    fn attack_steps_after_the_selected_rate_period() {
        let mut sid = DigitalSid::new();
        let gate = settle_envelope(&mut sid);
        sid.write(V3_AD, 0x00, gate);
        sid.write(V3_CTRL, 0x01, gate);
        let first = clock_until_level(&mut sid, EnvLevel(1));
        assert_eq!(sid.read(ENV3, first), 0);
        let second = clock_until_level(&mut sid, EnvLevel(2));
        assert_eq!(second.0 - first.0, 9);
        assert_eq!(sid.read(ENV3, ChipCycle(second.0 + 1)), 2);
    }

    #[test]
    fn all_rate_periods_fire_on_the_documented_cycle() {
        for (rate, period) in RATE_PERIOD.iter().copied().enumerate() {
            let mut sid = DigitalSid::new();
            let gate = settle_envelope(&mut sid);
            sid.write(V3_AD, (rate as u8) << 4, gate);
            sid.write(V3_CTRL, 1, gate);
            let first = clock_until_level(&mut sid, EnvLevel(1));
            let second = clock_until_level(&mut sid, EnvLevel(2));
            assert_eq!(second.0 - first.0, u64::from(period), "rate {rate}");
        }
    }

    #[test]
    fn every_exponential_threshold_selects_its_period() {
        for (level, period) in [
            (0xFF, 1),
            (0x5D, 2),
            (0x36, 4),
            (0x1A, 8),
            (0x0E, 16),
            (0x06, 30),
            (0x00, 1),
        ] {
            assert_eq!(exp_period(level), Some(period));
        }
    }

    #[test]
    fn attack_reaches_ff_then_enters_decay() {
        let mut sid = DigitalSid::new();
        let gate = settle_envelope(&mut sid);
        sid.write(V3_AD, 0x00, gate);
        sid.write(V3_CTRL, 0x01, gate);
        let _ = clock_until_level(&mut sid, EnvLevel(0xFF));
        sid.clock_to(ChipCycle(sid.cycle.0 + 3));
        assert_eq!(sid.envelopes[2].level, EnvLevel(0xFF));
        assert_eq!(sid.envelopes[2].phase, EnvPhase::DecaySustain);
    }

    #[test]
    fn gate_off_during_attack_releases_to_zero() {
        let mut sid = DigitalSid::new();
        let gate = settle_envelope(&mut sid);
        sid.write(V3_CTRL, 0x01, gate);
        let off = clock_until_level(&mut sid, EnvLevel(10));
        sid.write(V3_CTRL, 0x00, off);
        let _ = clock_until_level(&mut sid, EnvLevel(0));
        assert!(sid.envelopes[2].hold_zero);
    }

    #[test]
    fn gate_on_during_release_restarts_attack_from_current_level() {
        let mut sid = DigitalSid::new();
        let gate = settle_envelope(&mut sid);
        sid.write(V3_CTRL, 0x01, gate);
        let off = clock_until_level(&mut sid, EnvLevel(10));
        sid.write(V3_CTRL, 0x00, off);
        let retrigger = clock_until_level(&mut sid, EnvLevel(9));
        sid.write(V3_CTRL, 0x01, retrigger);
        let _ = clock_until_level(&mut sid, EnvLevel(10));
        assert_eq!(sid.envelopes[2].level, EnvLevel(10));
    }

    #[test]
    fn attack_rate_match_resets_a_partial_exponential_count() {
        let mut envelope = Envelope::new();
        envelope.hold_zero = false;
        envelope.phase = EnvPhase::Attack;
        envelope.level = EnvLevel(0x5D);
        envelope.exp_period = 2;
        envelope.exp_counter = 1;
        envelope.rate_match_pending = true;
        envelope.clock_one();
        assert_eq!(envelope.exp_counter, 0);
        assert_eq!(envelope.envelope_pipeline, PipelineCycles(2));
    }

    #[test]
    fn rate_counter_wrap_reproduces_delay_bug() {
        let mut envelope = Envelope::new();
        envelope.hold_zero = false;
        envelope.phase = EnvPhase::Attack;
        envelope.level = EnvLevel(0);
        envelope.env3 = EnvLevel(0);
        envelope.rate_match_pending = false;
        envelope.rate_counter = 0x7FFF;
        envelope.rate_period = 9;
        envelope.clock_to(ChipCycle(1));
        assert_eq!(envelope.rate_counter, 1);
        assert_eq!(envelope.level, EnvLevel(0));
        envelope.clock_to(ChipCycle(8));
        assert_eq!(envelope.level, EnvLevel(0));
        envelope.clock_to(ChipCycle(9));
        assert!(envelope.rate_match_pending);
        envelope.clock_to(ChipCycle(11));
        assert_eq!(envelope.level, EnvLevel(0));
        envelope.clock_to(ChipCycle(12));
        assert_eq!(envelope.level, EnvLevel(1));
    }

    #[test]
    fn release_uses_exponential_period_thresholds() {
        let mut envelope = Envelope::new();
        envelope.hold_zero = false;
        envelope.phase = EnvPhase::Release;
        envelope.level = EnvLevel(0x5E);
        envelope.exp_period = 1;
        envelope.clock_to(ChipCycle(9));
        assert_eq!(envelope.level, EnvLevel(0x5D));
        assert_eq!(envelope.exp_period, 2);
        envelope.clock_to(ChipCycle(18));
        assert_eq!(envelope.level, EnvLevel(0x5D));
        envelope.clock_to(ChipCycle(27));
        assert_eq!(envelope.level, EnvLevel(0x5C));
    }

    #[test]
    fn sustain_change_never_makes_decay_seek_upward() {
        let mut envelope = Envelope::new();
        envelope.hold_zero = false;
        envelope.phase = EnvPhase::DecaySustain;
        envelope.level = EnvLevel(0x22);
        envelope.adsr.sustain = 1;
        envelope.exp_period = 1;
        envelope.write_sustain_release(0x40);
        envelope.clock_to(ChipCycle(9));
        assert_eq!(envelope.level, EnvLevel(0x21));
    }

    #[test]
    fn sustain_raise_at_zero_in_decay_sustain_stays_frozen() {
        let mut sid = DigitalSid::new();
        sid.write(V3_AD, 0x00, ChipCycle(0));
        sid.write(V3_SR, 0x00, ChipCycle(0));
        sid.write(V3_CTRL, 0x01, ChipCycle(0));
        assert_eq!(sid.read(ENV3, ChipCycle(100_000)), 0);
        assert_eq!(sid.envelopes[2].phase, EnvPhase::DecaySustain);
        assert!(sid.envelopes[2].hold_zero);
        // Driver pre-sets the next note's sustain while the old gate is on.
        sid.write(V3_SR, 0xF0, ChipCycle(100_000));
        assert_eq!(sid.read(ENV3, ChipCycle(100_100)), 0);
        assert_eq!(sid.read(ENV3, ChipCycle(150_000)), 0);
    }

    #[test]
    fn release_after_sustain_raise_at_zero_starts_from_zero() {
        let mut sid = DigitalSid::new();
        sid.write(V3_AD, 0x00, ChipCycle(0));
        sid.write(V3_SR, 0x00, ChipCycle(0));
        sid.write(V3_CTRL, 0x01, ChipCycle(0));
        assert_eq!(sid.read(ENV3, ChipCycle(100_000)), 0);
        sid.write(V3_SR, 0xFF, ChipCycle(100_000));
        sid.write(V3_CTRL, 0x00, ChipCycle(100_050));
        assert_eq!(sid.read(ENV3, ChipCycle(100_060)), 0);
        assert_eq!(sid.read(ENV3, ChipCycle(120_000)), 0);
    }

    #[test]
    fn decay_to_zero_emits_reached_zero_and_holds() {
        let mut sid = DigitalSid::new();
        sid.write(V3_AD, 0x00, ChipCycle(0));
        sid.write(V3_SR, 0x00, ChipCycle(0));
        sid.begin_observation(ChipCycle(0));
        sid.write(V3_CTRL, 0x01, ChipCycle(0));
        sid.clock_to(ChipCycle(100_000));
        let activity = sid.finish_observation();
        assert!(
            activity[2]
                .events
                .iter()
                .any(|event| event.kind == EnvelopeEventKind::ReachedZero),
            "DecaySustain decay to zero must emit ReachedZero"
        );
        assert!(activity[2].reached_zero.is_some());
        assert!(sid.envelopes[2].hold_zero);
    }

    #[test]
    fn same_cycle_gate_writes_do_not_emit_pipeline_events_early() {
        let mut sid = DigitalSid::new();
        sid.begin_observation(ChipCycle(0));
        sid.write(V3_CTRL, 0x01, ChipCycle(0));
        sid.write(V3_CTRL, 0x00, ChipCycle(0));
        let activity = sid.finish_observation();
        assert!(activity[2].events.is_empty());
        assert!(!sid.envelopes[2].gate);
    }

    #[test]
    fn observation_is_interval_aware_when_release_reaches_zero() {
        let mut sid = DigitalSid::new();
        let gate = settle_envelope(&mut sid);
        sid.write(V3_CTRL, 1, gate);
        let off = clock_until_level(&mut sid, EnvLevel(10));
        sid.begin_observation(off);
        sid.write(V3_CTRL, 0, off);
        let zero = clock_until_level(&mut sid, EnvLevel(0));
        let activity = sid.finish_observation();
        assert_eq!(activity[2].start_level, EnvLevel(10));
        assert_eq!(activity[2].end_level, EnvLevel(0));
        assert_eq!(activity[2].peak_level, EnvLevel(10));
        assert_eq!(activity[2].reached_zero, Some(offset_from(off, zero)));
        assert!(activity[2].active_cycles.0 <= zero.0 - off.0);
    }

    #[test]
    fn jump_clock_matches_slow_reference_for_random_event_streams() {
        let mut seed = 0x5EED_CAFE_u64;
        let mut fast = Envelope::new();
        let mut slow = fast.clone();
        for _ in 0..2_000 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let delta = (seed >> 32) % 50_000;
            let target = ChipCycle(fast.cycle.0 + delta);
            fast.clock_to(target);
            clock_reference(&mut slow, delta);
            assert_eq!(fast, slow);

            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let value = (seed >> 40) as u8;
            match seed & 3 {
                0 => {
                    fast.write_control(value);
                    slow.write_control(value);
                }
                1 => {
                    fast.write_attack_decay(value);
                    slow.write_attack_decay(value);
                }
                _ => {
                    fast.write_sustain_release(value);
                    slow.write_sustain_release(value);
                }
            }
            assert_eq!(fast, slow);
        }
    }

    #[test]
    fn oscillator_jump_matches_per_cycle_clock_for_random_register_streams() {
        for model in [SidModel::Mos6581, SidModel::Mos8580] {
            let mut seed = 0x51D0_5C11_u64;
            let mut jumped = DigitalSid::with_model(model);
            let mut stepped = jumped.clone();
            for _ in 0..500 {
                for voice in 0..3 {
                    let base = voice * usize::from(VOICE_STRIDE);
                    seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                    let frequency = (seed >> 32) as u16;
                    jumped.registers[base] = frequency as u8;
                    jumped.registers[base + 1] = (frequency >> 8) as u8;
                    stepped.registers[base] = jumped.registers[base];
                    stepped.registers[base + 1] = jumped.registers[base + 1];

                    seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                    let pulse_width = ((seed >> 40) as u16) & 0x0FFF;
                    jumped.registers[base + 2] = pulse_width as u8;
                    jumped.registers[base + 3] = (pulse_width >> 8) as u8;
                    stepped.registers[base + 2] = jumped.registers[base + 2];
                    stepped.registers[base + 3] = jumped.registers[base + 3];

                    seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                    let waveform = ((seed >> 32) % 9) as u8;
                    let ring = ((seed >> 60) as u8 & 1) << 2;
                    jumped.registers[base + usize::from(CONTROL_OFFSET)] = (waveform << 4) | ring;
                    stepped.registers[base + usize::from(CONTROL_OFFSET)] =
                        jumped.registers[base + usize::from(CONTROL_OFFSET)];
                }

                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                let delta = (seed >> 32) % 10_000 + 3;
                let target = ChipCycle(jumped.cycle.0 + delta);
                jumped.clock_to(target);
                for envelope in &mut stepped.envelopes {
                    envelope.clock_to(target);
                }
                for _ in 0..delta {
                    stepped.clock_oscillators();
                }
                stepped.cycle = target;
                assert_eq!(jumped.oscillators, stepped.oscillators);
            }
        }
    }

    #[test]
    #[should_panic(expected = "digital SID clock moved backwards")]
    fn moving_clock_backwards_violates_the_timeline_contract() {
        let mut sid = DigitalSid::new();
        sid.clock_to(ChipCycle(12));
        sid.clock_to(ChipCycle(5));
    }

    #[test]
    fn sr_write_reloads_release_rate_in_place() {
        let mut sid = DigitalSid::new();
        sid.write(V3_CTRL, 0x01, ChipCycle(0));
        sid.clock_to(ChipCycle(90));
        sid.write(V3_CTRL, 0, ChipCycle(90));
        sid.write(V3_SR, 0x0F, ChipCycle(90));
        sid.clock_to(ChipCycle(93));
        assert_eq!(sid.envelopes[2].rate_period, RATE_PERIOD[15]);
    }

    #[test]
    fn oscillator_outputs_simple_waveforms_and_test_resets_phase() {
        let mut sid = DigitalSid::with_model(SidModel::Mos8580);
        sid.write(SidRegister(0x0f), 0x10, ChipCycle(0));
        sid.write(V3_CTRL, 0x28, ChipCycle(0));
        assert_eq!(sid.oscillators[2].accumulator, 0);
        sid.write(V3_CTRL, 0x20, ChipCycle(1));
        assert_eq!(sid.read(SidRegister(0x1b), ChipCycle(2)), 0);
        assert_eq!(sid.read(SidRegister(0x1b), ChipCycle(18)), 0x01);
    }

    #[test]
    fn sync_resets_each_destination_on_its_source_msb_edge() {
        for destination in 0..3 {
            let source = (destination + 2) % 3;
            let mut sid = DigitalSid::new();
            let dest_base = (destination as u8) * VOICE_STRIDE;
            let source_base = (source as u8) * VOICE_STRIDE;
            sid.write(SidRegister(dest_base), 1, ChipCycle(0));
            sid.write(SidRegister(dest_base + CONTROL_OFFSET), 0x0A, ChipCycle(0));
            sid.write(SidRegister(source_base), 0xff, ChipCycle(0));
            sid.write(SidRegister(source_base + 1), 0xff, ChipCycle(0));
            sid.write(
                SidRegister(source_base + CONTROL_OFFSET),
                0x08,
                ChipCycle(0),
            );
            sid.write(SidRegister(dest_base + CONTROL_OFFSET), 0x02, ChipCycle(1));
            sid.write(
                SidRegister(source_base + CONTROL_OFFSET),
                0x20,
                ChipCycle(1),
            );
            sid.clock_to(ChipCycle(130));
            assert_eq!(sid.oscillators[destination].accumulator, 0);
            assert_eq!(sid.oscillators[destination].sync_resets, 1);
        }
    }

    #[test]
    fn sync_suppresses_a_reset_from_a_source_reset_on_the_same_cycle() {
        let mut sid = DigitalSid::new();
        for voice in 0..3 {
            let base = (voice as u8) * VOICE_STRIDE;
            sid.write(SidRegister(base), 0xff, ChipCycle(0));
            sid.write(SidRegister(base + 1), 0xff, ChipCycle(0));
            sid.write(SidRegister(base + CONTROL_OFFSET), 0x22, ChipCycle(0));
        }
        sid.clock_to(ChipCycle(129));
        assert_eq!(sid.oscillators[0].sync_resets, 0);
        assert_eq!(sid.oscillators[1].sync_resets, 0);
        assert_eq!(sid.oscillators[2].sync_resets, 0);
    }

    #[test]
    fn sync_reset_does_not_swallow_a_coincident_noise_clock() {
        let mut sid = DigitalSid::new();
        // Voice 1 (dest, synced by voice 3): natural increment crosses bit 19
        // on the same cycle as the source's MSB rise.
        sid.write(SidRegister(0x00), 1, ChipCycle(0));
        sid.write(SidRegister(0x04), 0x82, ChipCycle(0));
        sid.write(SidRegister(0x0E), 1, ChipCycle(0));
        sid.write(SidRegister(0x12), 0x20, ChipCycle(0));
        sid.oscillators[0].accumulator = 0x07FFFF;
        sid.oscillators[2].accumulator = 0x7FFFFF;
        sid.clock_to(ChipCycle(1));
        assert_eq!(sid.oscillators[0].accumulator, 0, "sync reset applied");
        assert_eq!(sid.oscillators[0].sync_resets, 1);
        assert_eq!(sid.oscillators[0].shift_pipeline, PipelineCycles(2));
        sid.clock_to(ChipCycle(3));
        assert_eq!(
            sid.oscillators[0].noise_shift_register, 0x1F_FFFF,
            "the coincident bit-19 rise must still clock the LFSR"
        );
    }

    #[test]
    fn ring_modulation_uses_source_msb() {
        let mut sid = DigitalSid::with_model(SidModel::Mos6581);
        sid.write(SidRegister(0x00), 0, ChipCycle(0));
        sid.write(SidRegister(0x01), 0x40, ChipCycle(0));
        sid.write(SidRegister(0x04), 0x14, ChipCycle(0));
        sid.oscillators[0].accumulator = 0x400000;
        sid.oscillators[2].accumulator = 0x800000;
        sid.oscillators[0].update_output(0x14, 0, 0x800000, true);
        assert_eq!(sid.oscillator_output(0), 0x80);
        sid.oscillators[2].accumulator = 0;
        sid.oscillators[0].update_output(0x14, 0, 0, true);
        assert_eq!(sid.oscillator_output(0), 0x7f);
    }

    #[test]
    fn noise_uses_lfsr_and_combined_waveforms_remain_explicitly_bounded() {
        let mut sid = DigitalSid::new();
        assert_eq!(sid.oscillators[2].noise_shift_register, NOISE_SEED);
        sid.write(SidRegister(0x0f), 0x10, ChipCycle(0));
        sid.write(V3_CTRL, 0x80, ChipCycle(0));
        let initial = sid.read(SidRegister(0x1b), ChipCycle(0));
        let advanced = sid.read(SidRegister(0x1b), ChipCycle(1024));
        assert_ne!(initial, advanced);
        sid.write(V3_CTRL, 0x30, ChipCycle(1024));
        assert_eq!(sid.read(SidRegister(0x1b), ChipCycle(1025)), 0);
        assert!(!sid.oscillator_snapshots()[2].combined_waveform_exact);
    }

    #[test]
    fn combined_noise_waveform_poisons_the_lfsr_state() {
        let mut sid = DigitalSid::new();
        sid.write(SidRegister(0x0f), 0x10, ChipCycle(0));
        sid.write(V3_CTRL, 0x80, ChipCycle(0));
        sid.clock_to(ChipCycle(256));
        assert!(!sid.oscillator_snapshots()[2].noise_state_poisoned);
        // Noise + pulse clocked for real cycles: destructive write-back on
        // hardware, so the modeled LFSR state is no longer trustworthy.
        sid.write(V3_CTRL, 0xC0, ChipCycle(256));
        assert!(sid.oscillator_snapshots()[2].noise_state_poisoned);
        sid.clock_to(ChipCycle(512));
        assert!(sid.oscillator_snapshots()[2].noise_state_poisoned);
        sid.write(V3_CTRL, 0x80, ChipCycle(512));
        sid.clock_to(ChipCycle(768));
        assert!(!sid.oscillator_snapshots()[2].noise_state_poisoned);
    }

    #[test]
    fn test_bit_resets_phase_and_clocks_noise_on_release() {
        let mut sid = DigitalSid::new();
        sid.oscillators[2].accumulator = 0x654321;
        sid.write(V3_CTRL, 0x88, ChipCycle(0));
        assert_eq!(sid.oscillators[2].accumulator, 0);
        assert_eq!(sid.oscillators[2].noise_shift_register, NOISE_SEED);
        sid.write(V3_CTRL, 0x80, ChipCycle(10));
        assert_eq!(sid.oscillators[2].noise_shift_register, 0x1f_ffff);
    }

    #[test]
    fn test_hold_fills_shift_register_with_ones() {
        let mut sid = DigitalSid::new();
        sid.write(V3_CTRL, 0x88, ChipCycle(0));
        sid.clock_to(ChipCycle(u64::from(TEST_HOLD_FILL_CYCLES_6581 - 1)));
        assert_eq!(sid.oscillators[2].noise_shift_register, NOISE_SEED);
        sid.clock_to(ChipCycle(u64::from(TEST_HOLD_FILL_CYCLES_6581)));
        assert_eq!(
            sid.oscillators[2].noise_shift_register, 0x7F_FFFF,
            "held TEST reaches the model-specific SRAM fill boundary"
        );
        sid.write(
            V3_CTRL,
            0x80,
            ChipCycle(u64::from(TEST_HOLD_FILL_CYCLES_6581)),
        );
        assert_eq!(sid.oscillators[2].noise_shift_register, NOISE_SEED);
    }

    #[test]
    fn test_hold_fill_time_depends_on_sid_model() {
        let mut sid_6581 = DigitalSid::with_model(SidModel::Mos6581);
        let mut sid_8580 = DigitalSid::with_model(SidModel::Mos8580);
        sid_6581.write(V3_CTRL, 0x88, ChipCycle(0));
        sid_8580.write(V3_CTRL, 0x88, ChipCycle(0));

        sid_6581.clock_to(ChipCycle(u64::from(TEST_HOLD_FILL_CYCLES_6581)));
        sid_8580.clock_to(ChipCycle(u64::from(TEST_HOLD_FILL_CYCLES_6581)));
        assert_eq!(sid_6581.oscillators[2].noise_shift_register, NOISE_ALL_ONES);
        assert_eq!(sid_8580.oscillators[2].noise_shift_register, NOISE_SEED);

        sid_8580.clock_to(ChipCycle(u64::from(TEST_HOLD_FILL_CYCLES_8580)));
        assert_eq!(sid_8580.oscillators[2].noise_shift_register, NOISE_ALL_ONES);
    }

    #[test]
    fn control_rewrites_while_test_held_preserve_the_fill_countdown() {
        let mut sid = DigitalSid::new();
        sid.write(V3_CTRL, 0x88, ChipCycle(0));
        sid.write(V3_CTRL, 0x88, ChipCycle(20_000));
        sid.clock_to(ChipCycle(40_000));
        assert_eq!(sid.oscillators[2].noise_shift_register, NOISE_SEED);
        sid.clock_to(ChipCycle(u64::from(TEST_HOLD_FILL_CYCLES_6581)));
        assert_eq!(sid.oscillators[2].noise_shift_register, 0x7F_FFFF);
    }

    #[test]
    fn test_release_shift_feedback_is_forced_by_test() {
        let mut sid = DigitalSid::new();
        sid.write(V3_CTRL, 0x88, ChipCycle(0));
        sid.oscillators[2].noise_shift_register = 0x00_0001;
        sid.oscillators[2].noise_shift_latch = 0x00_0001;
        sid.write(V3_CTRL, 0x80, ChipCycle(10));
        assert_eq!(sid.oscillators[2].noise_shift_register, 0x40_0000);
    }
}
