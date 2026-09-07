use super::evidence::Evidenced;
use super::ids::SignalPointId;
use super::topology::SidTopology;
use crate::analysis::VoiceId;
use crate::analysis::filter::{Cutoff, Resonance};
use crate::analysis::voice::{Adsr, PulseWidth, SidFreq};
use crate::emu::sid::{EnvelopeCheckpoint, OscillatorCheckpoint};
use crate::header::SidModel;
use crate::trace::ChipCycle;
use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct SignalPoint<T> {
    pub id: SignalPointId,
    pub at: ChipCycle,
    pub value: Evidenced<T>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
#[must_use]
pub struct EventSignal<T>(pub Vec<SignalPoint<T>>);

impl<T> Default for EventSignal<T> {
    fn default() -> Self {
        Self(Vec::new())
    }
}

impl<T> EventSignal<T> {
    pub(crate) fn push(&mut self, point: SignalPoint<T>) {
        self.0.push(point);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
#[must_use]
pub struct StepSignal<T>(pub Vec<SignalPoint<T>>);

impl<T> Default for StepSignal<T> {
    fn default() -> Self {
        Self(Vec::new())
    }
}

impl<T: PartialEq> StepSignal<T> {
    pub(crate) fn push_compact(&mut self, point: SignalPoint<T>) {
        if self
            .0
            .last()
            .is_some_and(|last| last.value.value == point.value.value)
        {
            return;
        }
        self.0.push(point);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(transparent)]
#[must_use]
pub struct ControlRegister(pub u8);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(transparent)]
#[must_use]
pub struct WaveformRegister(pub u8);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(transparent)]
#[must_use]
pub struct FilterRoutingRegister(pub u8);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(transparent)]
#[must_use]
pub struct FilterModeRegister(pub u8);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(transparent)]
#[must_use]
pub struct MasterVolume(pub u8);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(transparent)]
#[must_use]
pub struct SwitchState(pub bool);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(transparent)]
#[must_use]
pub struct SidRegisterFile(pub [u8; 0x1d]);

#[derive(Debug, Clone, Serialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct SidVoiceProgram {
    pub voice: VoiceId,
    pub frequency: EventSignal<SidFreq>,
    pub pulse_width: EventSignal<PulseWidth>,
    pub control: EventSignal<ControlRegister>,
    pub adsr: EventSignal<Adsr>,
    pub waveform: StepSignal<WaveformRegister>,
    pub gate: StepSignal<SwitchState>,
    pub test: StepSignal<SwitchState>,
    pub sync: StepSignal<SwitchState>,
    pub ring: StepSignal<SwitchState>,
    pub envelope: StepSignal<EnvelopeCheckpoint>,
    pub oscillator: StepSignal<OscillatorCheckpoint>,
}

impl SidVoiceProgram {
    pub(crate) fn empty(voice: VoiceId) -> Self {
        Self {
            voice,
            frequency: EventSignal::default(),
            pulse_width: EventSignal::default(),
            control: EventSignal::default(),
            adsr: EventSignal::default(),
            waveform: StepSignal::default(),
            gate: StepSignal::default(),
            test: StepSignal::default(),
            sync: StepSignal::default(),
            ring: StepSignal::default(),
            envelope: StepSignal::default(),
            oscillator: StepSignal::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct SidChipProgram {
    pub sid_model: SidModel,
    pub initial_registers: Evidenced<SidRegisterFile>,
    pub cutoff: EventSignal<Cutoff>,
    pub resonance: EventSignal<Resonance>,
    pub routing: EventSignal<FilterRoutingRegister>,
    pub filter_mode: EventSignal<FilterModeRegister>,
    pub volume: EventSignal<MasterVolume>,
    pub digi: EventSignal<MasterVolume>,
    pub topology: SidTopology,
}
