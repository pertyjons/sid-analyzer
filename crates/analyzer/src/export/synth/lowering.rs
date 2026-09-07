use crate::analysis::VoiceId;
use crate::analysis::sid_program::AnalyzedSidProgram;
use crate::analysis::sid_program::ids::{
    ContinuousOccurrenceId, ProgramDefinitionId, ProgramOccurrenceId, ProgramReuseGroupId,
    SignalInterpretationId, SoundRegionId,
};
use crate::analysis::sid_program::interpretation::{
    InterpretedSignalSource, SignalInterpretationKind,
};
use crate::analysis::sid_program::semantic::CausalSupport;
use crate::audio::AudioComparison;
use crate::audio::abtest::AudioBudget;
use crate::emu::capture::{
    EventTimestampQuality, SidBusAccess, SidBusEventId, SidCallId, SidChipId,
};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
#[must_use]
pub struct TargetRevision(pub String);

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
#[must_use]
pub struct ModuleType(pub String);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
#[must_use]
pub enum BehaviorCapability {
    GateRetrigger,
    PhaseReset,
    CycleStampedControl,
    EnvelopeStateInitialization,
    OscillatorStateInitialization,
    NoiseStateInitialization,
    LiveCrossVoice,
    ChipScopedFilter,
    AutomationApplication,
    SequenceContinuation,
    DynamicRouting,
    BundleSampleLoading,
    HeadlessRendering,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct TargetCapabilities {
    pub revision: TargetRevision,
    pub modules: BTreeSet<ModuleType>,
    pub verified_behaviors: BTreeSet<BehaviorCapability>,
}

impl TargetCapabilities {
    pub fn from_pinned_mirrors() -> Result<Self, CapabilityError> {
        let descriptors: serde_json::Value = serde_json::from_str(include_str!(
            "../../../../../docs/pertylizer/descriptors.json"
        ))?;
        let modules = descriptors
            .get("modules")
            .and_then(serde_json::Value::as_object)
            .ok_or(CapabilityError::MissingModules)?
            .keys()
            .cloned()
            .map(ModuleType)
            .collect();
        Ok(Self {
            revision: TargetRevision("3e25e679fd6dd962c44d1b4bd1571b1288e096fc".to_owned()),
            modules,
            verified_behaviors: BTreeSet::new(),
        })
    }

    #[must_use = "the returned capabilities contain the added verified behavior"]
    pub fn with_verified_behavior(mut self, behavior: BehaviorCapability) -> Self {
        self.verified_behaviors.insert(behavior);
        self
    }

    #[must_use]
    pub fn supports_module(&self, module: &str) -> bool {
        self.modules.contains(&ModuleType(module.to_owned()))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CapabilityError {
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error("Pertylizer descriptor mirror has no modules object")]
    MissingModules,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize)]
#[serde(transparent)]
#[must_use]
pub struct CandidateId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
#[must_use]
pub enum RepresentationClass {
    NativeAdsr,
    NormalizedAdsr,
    Mseg,
    Arpeggiator,
    Glide,
    Lfo,
    KineticModulator,
    DirectCv,
    ModMatrix,
    Script,
    SidOscillator,
    SidSequence,
    SharedFilter,
    SparseAutomation,
    DenseAutomation,
    Sampler,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[must_use]
pub enum RenderRequirement {
    Required,
    PinnedBaseline,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
#[must_use]
pub struct RecipePointCount(pub u32);

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct CandidateScope {
    pub voices: BTreeSet<VoiceId>,
    pub regions: Vec<SoundRegionId>,
    pub definitions: Vec<ProgramDefinitionId>,
    pub occurrences: Vec<ProgramOccurrenceId>,
    pub continuous_occurrences: Vec<ContinuousOccurrenceId>,
    pub reuse_groups: Vec<ProgramReuseGroupId>,
    pub interpretations: Vec<SignalInterpretationId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "compiler", rename_all = "snake_case")]
#[must_use]
pub enum CandidateRecipe {
    NativeAdsr {
        envelope_points: RecipePointCount,
        history_sensitive_entries: StateMismatchCount,
    },
    NormalizedAdsr {
        envelope_points: RecipePointCount,
        history_sensitive_entries: StateMismatchCount,
    },
    Mseg {
        envelope_points: RecipePointCount,
        reset_from_current_entries: StateMismatchCount,
    },
    GateDrivenEnvelope {
        envelope_points: RecipePointCount,
    },
    Arpeggiator {
        interpretations: Vec<SignalInterpretationId>,
    },
    Glide {
        interpretations: Vec<SignalInterpretationId>,
    },
    Lfo {
        interpretations: Vec<SignalInterpretationId>,
    },
    KineticModulator {
        interpretations: Vec<SignalInterpretationId>,
    },
    DirectCv {
        dependency_count: RecipePointCount,
    },
    ModMatrix {
        dependency_count: RecipePointCount,
    },
    Script {
        interpretations: Vec<SignalInterpretationId>,
    },
    SidOscillator {
        waveform_points: RecipePointCount,
    },
    SidSequence {
        definitions: Vec<ProgramDefinitionId>,
        occurrences: Vec<ContinuousOccurrenceId>,
        reuse_groups: Vec<ProgramReuseGroupId>,
        onset_programs: Vec<OnsetProgramRecipe>,
        oscillator_starts: Vec<OscillatorStartRecipe>,
        noise_starts: Vec<NoiseStartRecipe>,
    },
    SharedFilter {
        control_points: RecipePointCount,
        topology_edges: RecipePointCount,
    },
    Automation {
        dense: bool,
        control_points: RecipePointCount,
        fallbacks: Vec<FallbackReason>,
    },
    Sampler {
        sample_points: RecipePointCount,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct OnsetProgramRecipe {
    pub voice: VoiceId,
    pub call: SidCallId,
    pub trigger: SidBusEventId,
    pub initial_registers: SidVoiceRegisterState,
    pub writes: Vec<SidBusEventId>,
    pub timestamp_quality: EventTimestampQuality,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(transparent)]
#[must_use]
pub struct SidVoiceRegisterState(pub [u8; 7]);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[must_use]
pub enum OscillatorStartPolicy {
    ExplicitState,
    FreeRunning,
    LegatoContinued,
    SyncReset,
    TestReset,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct OscillatorStartRecipe {
    pub occurrence: ContinuousOccurrenceId,
    pub voice: VoiceId,
    pub policy: OscillatorStartPolicy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[must_use]
pub enum NoiseStartPolicy {
    FreeRunning,
    Seeded,
    TestReset,
    LegatoContinued,
    SequenceRestart,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct NoiseStartRecipe {
    pub occurrence: ContinuousOccurrenceId,
    pub voice: VoiceId,
    pub policy: NoiseStartPolicy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
#[must_use]
pub enum FallbackReason {
    InstructionStartTiming,
    EnvelopeHistory,
    OscillatorContinuation,
    NoiseContinuation,
    VoiceLocalFilter,
    CrossVoiceTopology,
    AmbiguousCausalModulation,
    RandomModulation,
    DigiUnsupported,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
#[must_use]
pub enum StateDomain {
    OnsetProgram,
    Pitch,
    Amplitude,
    GateRetrigger,
    Waveform,
    PulseWidth,
    OscillatorContinuation,
    NoiseContinuation,
    Filter,
    Routing,
    CrossVoice,
    Digi,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
#[serde(transparent)]
#[must_use]
pub struct Coverage(pub BTreeSet<StateDomain>);

impl Coverage {
    #[must_use]
    pub fn contains(&self, domain: StateDomain) -> bool {
        self.0.contains(&domain)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
#[must_use]
pub enum Requirement {
    Module(ModuleType),
    Behavior(BehaviorCapability),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
#[must_use]
pub enum KnownLoss {
    Exact,
    TimingBounded,
    StateApproximation,
    TopologyApproximation,
    RenderedFallback,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
#[must_use]
pub enum EditabilityClass {
    Native,
    Structured,
    Scripted,
    SparseAutomation,
    DenseAutomation,
    Rendered,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct EditCost {
    pub known_loss: KnownLoss,
    pub editability: EditabilityClass,
    pub graph_complexity: GraphComplexity,
    pub automation_density: AutomationDensity,
    pub serialized_size: SerializedSize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
#[must_use]
pub struct GraphComplexity(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
#[must_use]
pub struct AutomationDensity(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
#[must_use]
pub struct SerializedSize(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(transparent)]
#[must_use]
pub struct CentsResidual(pub f32);

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(transparent)]
#[must_use]
pub struct LevelResidual(pub f32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(transparent)]
#[must_use]
pub struct StateMismatchCount(pub u64);

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct StateResiduals {
    pub onset_program: StateMismatchCount,
    pub maximum_pitch: CentsResidual,
    pub maximum_level: LevelResidual,
    pub gate_retrigger: StateMismatchCount,
    pub waveform: StateMismatchCount,
    pub pulse_width: StateMismatchCount,
    pub oscillator_continuation: StateMismatchCount,
    pub noise_continuation: StateMismatchCount,
    pub filter: StateMismatchCount,
    pub routing: StateMismatchCount,
    pub cross_voice: StateMismatchCount,
    pub digi: StateMismatchCount,
}

impl StateResiduals {
    fn exact() -> Self {
        Self {
            onset_program: StateMismatchCount(0),
            maximum_pitch: CentsResidual(0.0),
            maximum_level: LevelResidual(0.0),
            gate_retrigger: StateMismatchCount(0),
            waveform: StateMismatchCount(0),
            pulse_width: StateMismatchCount(0),
            oscillator_continuation: StateMismatchCount(0),
            noise_continuation: StateMismatchCount(0),
            filter: StateMismatchCount(0),
            routing: StateMismatchCount(0),
            cross_voice: StateMismatchCount(0),
            digi: StateMismatchCount(0),
        }
    }

    fn within(&self, budget: StateBudget) -> bool {
        self.onset_program.0 <= budget.onset_program.0
            && self.maximum_pitch.0 <= budget.maximum_pitch.0
            && self.maximum_level.0 <= budget.maximum_level.0
            && self.gate_retrigger.0 <= budget.gate_retrigger.0
            && self.waveform.0 <= budget.waveform.0
            && self.pulse_width.0 <= budget.pulse_width.0
            && self.oscillator_continuation.0 <= budget.oscillator_continuation.0
            && self.noise_continuation.0 <= budget.noise_continuation.0
            && self.filter.0 <= budget.filter.0
            && self.routing.0 <= budget.routing.0
            && self.cross_voice.0 <= budget.cross_voice.0
            && self.digi.0 <= budget.digi.0
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct RepresentationCandidate {
    pub id: CandidateId,
    pub class: RepresentationClass,
    pub coverage: Coverage,
    pub requirements: Vec<Requirement>,
    pub known_losses: Vec<KnownLoss>,
    pub cost: EditCost,
    pub residuals: StateResiduals,
    pub render_requirement: RenderRequirement,
    pub scope: CandidateScope,
    pub recipe: CandidateRecipe,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct StateBudget {
    pub onset_program: StateMismatchCount,
    pub maximum_pitch: CentsResidual,
    pub maximum_level: LevelResidual,
    pub gate_retrigger: StateMismatchCount,
    pub waveform: StateMismatchCount,
    pub pulse_width: StateMismatchCount,
    pub oscillator_continuation: StateMismatchCount,
    pub noise_continuation: StateMismatchCount,
    pub filter: StateMismatchCount,
    pub routing: StateMismatchCount,
    pub cross_voice: StateMismatchCount,
    pub digi: StateMismatchCount,
}

impl Default for StateBudget {
    fn default() -> Self {
        Self {
            onset_program: StateMismatchCount(0),
            maximum_pitch: CentsResidual(50.0),
            maximum_level: LevelResidual(0.02),
            gate_retrigger: StateMismatchCount(0),
            waveform: StateMismatchCount(0),
            pulse_width: StateMismatchCount(0),
            oscillator_continuation: StateMismatchCount(0),
            noise_continuation: StateMismatchCount(0),
            filter: StateMismatchCount(0),
            routing: StateMismatchCount(0),
            cross_voice: StateMismatchCount(0),
            digi: StateMismatchCount(0),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "reason", content = "detail", rename_all = "snake_case")]
#[must_use]
pub enum RejectionReason {
    MissingRequirement(Requirement),
    StateBudgetExceeded,
}

#[derive(Debug, Clone, Serialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct CandidateDecision {
    pub candidate: CandidateId,
    pub accepted: bool,
    pub rejected: Vec<RejectionReason>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct PertylizerLoweringPlan {
    pub target: TargetRevision,
    pub required: Coverage,
    pub candidates: Vec<RepresentationCandidate>,
    pub decisions: Vec<CandidateDecision>,
    pub selected: BTreeMap<StateDomain, CandidateId>,
    pub uncovered: BTreeSet<StateDomain>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[must_use]
pub enum RenderGateRejection {
    StateGateRejected,
    NotRendered,
    AudioBudgetExceeded,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct CandidateRenderEvaluation {
    pub candidate: CandidateId,
    pub comparison: AudioComparison,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct RenderGateDecision {
    pub candidate: CandidateId,
    pub accepted: bool,
    pub rejection: Option<RenderGateRejection>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct RenderGatedSelection {
    pub decisions: Vec<RenderGateDecision>,
    pub selected: BTreeMap<StateDomain, CandidateId>,
    pub uncovered: BTreeSet<StateDomain>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(deny_unknown_fields)]
#[must_use]
pub struct AutomaticSelection {
    pub state: PertylizerLoweringPlan,
    pub render: RenderGatedSelection,
}

struct ChipCausalAnalysis {
    onset_programs: Vec<OnsetProgramRecipe>,
    oscillator_starts: Vec<OscillatorStartRecipe>,
    noise_starts: Vec<NoiseStartRecipe>,
    history_sensitive_retriggers: u64,
    continuation_edges: u64,
    cross_voice_edges: u64,
}

impl ChipCausalAnalysis {
    fn build(program: &AnalyzedSidProgram) -> Self {
        Self {
            onset_programs: onset_programs(program),
            oscillator_starts: oscillator_start_recipes(program),
            noise_starts: noise_start_recipes(program),
            history_sensitive_retriggers: history_sensitive_retriggers(program),
            continuation_edges: continuation_edge_count(program),
            cross_voice_edges: cross_voice_edge_count(program),
        }
    }
}

pub fn select_automatic(
    program: &AnalyzedSidProgram,
    capabilities: &TargetCapabilities,
    state_budget: StateBudget,
    evaluations: &[CandidateRenderEvaluation],
    audio_budget: AudioBudget,
) -> AutomaticSelection {
    let state = compile(program, capabilities, state_budget);
    let render = apply_render_gate(&state, evaluations, audio_budget);
    AutomaticSelection { state, render }
}

pub fn compile(
    program: &AnalyzedSidProgram,
    capabilities: &TargetCapabilities,
    budget: StateBudget,
) -> PertylizerLoweringPlan {
    let mut candidates = CandidateBuilder::default();
    let scope = candidate_scope(program);
    let causal = ChipCausalAnalysis::build(program);
    let required = required_domains(program, &causal);
    let history_resets = causal.history_sensitive_retriggers;
    let sid_sequence_loss =
        if causal.onset_programs.iter().any(|program| {
            program.timestamp_quality == EventTimestampQuality::InstructionStartBounded
        }) {
            KnownLoss::TimingBounded
        } else {
            KnownLoss::Exact
        };
    candidates.push(
        RepresentationClass::NativeAdsr,
        coverage([StateDomain::Amplitude, StateDomain::GateRetrigger]),
        vec![module("envelope")],
        KnownLoss::Exact,
        EditabilityClass::Native,
        GraphComplexity(1),
        AutomationDensity(0),
        StateResiduals {
            gate_retrigger: StateMismatchCount(history_resets),
            ..StateResiduals::exact()
        },
        RenderRequirement::Required,
        scope.clone(),
        CandidateRecipe::NativeAdsr {
            envelope_points: envelope_points(program),
            history_sensitive_entries: StateMismatchCount(history_resets),
        },
    );
    candidates.push(
        RepresentationClass::NormalizedAdsr,
        coverage([StateDomain::Amplitude, StateDomain::GateRetrigger]),
        vec![module("envelope")],
        KnownLoss::Exact,
        EditabilityClass::Native,
        GraphComplexity(1),
        AutomationDensity(0),
        StateResiduals {
            gate_retrigger: StateMismatchCount(history_resets),
            ..StateResiduals::exact()
        },
        RenderRequirement::Required,
        scope.clone(),
        CandidateRecipe::NormalizedAdsr {
            envelope_points: envelope_points(program),
            history_sensitive_entries: StateMismatchCount(history_resets),
        },
    );
    candidates.push(
        RepresentationClass::Mseg,
        coverage([StateDomain::Amplitude, StateDomain::GateRetrigger]),
        vec![module("mseg")],
        KnownLoss::Exact,
        EditabilityClass::Structured,
        GraphComplexity(1),
        AutomationDensity(0),
        StateResiduals {
            gate_retrigger: StateMismatchCount(history_resets),
            ..StateResiduals::exact()
        },
        RenderRequirement::Required,
        scope.clone(),
        CandidateRecipe::Mseg {
            envelope_points: envelope_points(program),
            reset_from_current_entries: StateMismatchCount(history_resets),
        },
    );
    candidates.push(
        RepresentationClass::Script,
        coverage([StateDomain::Amplitude, StateDomain::GateRetrigger]),
        vec![
            module("script"),
            module("mod_matrix"),
            Requirement::Behavior(BehaviorCapability::GateRetrigger),
            Requirement::Behavior(BehaviorCapability::EnvelopeStateInitialization),
        ],
        KnownLoss::Exact,
        EditabilityClass::Scripted,
        GraphComplexity(2),
        AutomationDensity(0),
        StateResiduals::exact(),
        RenderRequirement::Required,
        scope.clone(),
        CandidateRecipe::GateDrivenEnvelope {
            envelope_points: envelope_points(program),
        },
    );
    let arpeggios = interpretation_ids(program, |source, kind| {
        matches!(source, InterpretedSignalSource::VoiceFrequency { .. })
            && kind == SignalInterpretationKind::Periodic
    });
    if !arpeggios.is_empty() {
        candidates.push(
            RepresentationClass::Arpeggiator,
            coverage([StateDomain::Pitch]),
            Vec::new(),
            KnownLoss::Exact,
            EditabilityClass::Native,
            GraphComplexity(1),
            AutomationDensity(0),
            StateResiduals::exact(),
            RenderRequirement::Required,
            scope.clone(),
            CandidateRecipe::Arpeggiator {
                interpretations: arpeggios,
            },
        );
    }
    let glides = interpretation_ids(program, |source, kind| {
        matches!(source, InterpretedSignalSource::VoiceFrequency { .. })
            && kind == SignalInterpretationKind::Ramp
    });
    if !glides.is_empty() {
        candidates.push(
            RepresentationClass::Glide,
            coverage([StateDomain::Pitch]),
            Vec::new(),
            KnownLoss::Exact,
            EditabilityClass::Native,
            GraphComplexity(1),
            AutomationDensity(0),
            StateResiduals::exact(),
            RenderRequirement::Required,
            scope.clone(),
            CandidateRecipe::Glide {
                interpretations: glides,
            },
        );
    }
    let periodic = interpretation_ids(program, |_, kind| {
        kind == SignalInterpretationKind::Periodic
    });
    if !periodic.is_empty() {
        candidates.push(
            RepresentationClass::Lfo,
            interpretation_coverage(program, &periodic),
            vec![
                module("lfo"),
                Requirement::Behavior(BehaviorCapability::GateRetrigger),
            ],
            KnownLoss::Exact,
            EditabilityClass::Native,
            GraphComplexity(1),
            AutomationDensity(0),
            StateResiduals::exact(),
            RenderRequirement::Required,
            scope.clone(),
            CandidateRecipe::Lfo {
                interpretations: periodic,
            },
        );
    }
    let ramps = interpretation_ids(program, |_, kind| kind == SignalInterpretationKind::Ramp);
    if !ramps.is_empty() {
        candidates.push(
            RepresentationClass::KineticModulator,
            interpretation_coverage(program, &ramps),
            vec![module("kinetic_modulator")],
            KnownLoss::Exact,
            EditabilityClass::Structured,
            GraphComplexity(1),
            AutomationDensity(0),
            StateResiduals::exact(),
            RenderRequirement::Required,
            scope.clone(),
            CandidateRecipe::KineticModulator {
                interpretations: ramps,
            },
        );
    }
    let dependency_count = eligible_causal_dependencies(program).count() as u32;
    if dependency_count > 0 {
        let dependency_coverage = causal_coverage(program);
        candidates.push(
            RepresentationClass::DirectCv,
            dependency_coverage.clone(),
            Vec::new(),
            KnownLoss::Exact,
            EditabilityClass::Structured,
            GraphComplexity(1),
            AutomationDensity(0),
            StateResiduals::exact(),
            RenderRequirement::Required,
            scope.clone(),
            CandidateRecipe::DirectCv {
                dependency_count: RecipePointCount(dependency_count),
            },
        );
        candidates.push(
            RepresentationClass::ModMatrix,
            dependency_coverage,
            vec![module("mod_matrix")],
            KnownLoss::Exact,
            EditabilityClass::Structured,
            GraphComplexity(dependency_count.max(1)),
            AutomationDensity(0),
            StateResiduals::exact(),
            RenderRequirement::Required,
            scope.clone(),
            CandidateRecipe::ModMatrix {
                dependency_count: RecipePointCount(dependency_count),
            },
        );
    }
    let scripts = interpretation_ids(program, |_, kind| {
        matches!(
            kind,
            SignalInterpretationKind::Table | SignalInterpretationKind::BoundedScript
        )
    });
    if !scripts.is_empty() {
        candidates.push(
            RepresentationClass::Script,
            interpretation_coverage(program, &scripts),
            vec![module("script")],
            KnownLoss::Exact,
            EditabilityClass::Scripted,
            GraphComplexity(2),
            AutomationDensity(0),
            StateResiduals::exact(),
            RenderRequirement::Required,
            scope.clone(),
            CandidateRecipe::Script {
                interpretations: scripts,
            },
        );
    }
    let waveform_changes = waveform_changes(program);
    candidates.push(
        RepresentationClass::SidOscillator,
        coverage([StateDomain::Waveform, StateDomain::PulseWidth]),
        vec![module("sid_oscillator")],
        KnownLoss::StateApproximation,
        EditabilityClass::Native,
        GraphComplexity(1),
        AutomationDensity(0),
        StateResiduals {
            waveform: StateMismatchCount(waveform_changes),
            ..StateResiduals::exact()
        },
        RenderRequirement::Required,
        scope.clone(),
        CandidateRecipe::SidOscillator {
            waveform_points: waveform_points(program),
        },
    );
    candidates.push(
        RepresentationClass::SidSequence,
        coverage([
            StateDomain::OnsetProgram,
            StateDomain::GateRetrigger,
            StateDomain::Waveform,
            StateDomain::PulseWidth,
            StateDomain::OscillatorContinuation,
            StateDomain::NoiseContinuation,
            StateDomain::CrossVoice,
        ]),
        sid_sequence_requirements(&causal),
        sid_sequence_loss,
        EditabilityClass::Native,
        GraphComplexity(3),
        AutomationDensity(0),
        StateResiduals::exact(),
        RenderRequirement::Required,
        scope.clone(),
        CandidateRecipe::SidSequence {
            definitions: scope.definitions.clone(),
            occurrences: scope.continuous_occurrences.clone(),
            reuse_groups: scope.reuse_groups.clone(),
            onset_programs: causal.onset_programs.clone(),
            oscillator_starts: causal.oscillator_starts.clone(),
            noise_starts: causal.noise_starts.clone(),
        },
    );
    candidates.push(
        RepresentationClass::SharedFilter,
        coverage([StateDomain::Filter, StateDomain::Routing]),
        vec![
            module("filter"),
            Requirement::Behavior(BehaviorCapability::DynamicRouting),
            Requirement::Behavior(BehaviorCapability::ChipScopedFilter),
        ],
        KnownLoss::Exact,
        EditabilityClass::Native,
        GraphComplexity(1),
        AutomationDensity(program.chip.cutoff.0.len() as u32),
        StateResiduals::exact(),
        RenderRequirement::Required,
        scope.clone(),
        CandidateRecipe::SharedFilter {
            control_points: filter_points(program),
            topology_edges: RecipePointCount(program.chip.topology.edges.len() as u32),
        },
    );
    if has_digi_stream(program) {
        candidates.push(
            RepresentationClass::Sampler,
            coverage([StateDomain::Digi]),
            vec![
                module("sampler"),
                Requirement::Behavior(BehaviorCapability::BundleSampleLoading),
            ],
            KnownLoss::RenderedFallback,
            EditabilityClass::Rendered,
            GraphComplexity(1),
            AutomationDensity(0),
            StateResiduals::exact(),
            RenderRequirement::Required,
            scope.clone(),
            CandidateRecipe::Sampler {
                sample_points: detected_digi_points(program),
            },
        );
    }
    let control_points = program
        .capture
        .events
        .iter()
        .filter(|event| event.chip == SidChipId::PRIMARY)
        .count() as u32;
    candidates.push(
        RepresentationClass::SparseAutomation,
        required.clone(),
        Vec::new(),
        KnownLoss::TimingBounded,
        EditabilityClass::SparseAutomation,
        GraphComplexity(3),
        AutomationDensity(program.semantic.interpretations.len() as u32),
        sparse_residuals(program),
        RenderRequirement::Required,
        scope.clone(),
        CandidateRecipe::Automation {
            dense: false,
            control_points: RecipePointCount(program.semantic.interpretations.len() as u32),
            fallbacks: fallback_reasons(program, &causal),
        },
    );
    candidates.push_with_losses(
        RepresentationClass::DenseAutomation,
        required.clone(),
        Vec::new(),
        vec![
            KnownLoss::TimingBounded,
            KnownLoss::StateApproximation,
            KnownLoss::TopologyApproximation,
        ],
        EditabilityClass::DenseAutomation,
        GraphComplexity(6),
        AutomationDensity(control_points),
        fallback_residuals(program, &causal),
        RenderRequirement::PinnedBaseline,
        scope,
        CandidateRecipe::Automation {
            dense: true,
            control_points: RecipePointCount(control_points),
            fallbacks: fallback_reasons(program, &causal),
        },
    );

    let candidates = candidates.items;
    let decisions: Vec<_> = candidates
        .iter()
        .map(|candidate| decide(candidate, capabilities, budget))
        .collect();
    let mut selected = BTreeMap::new();
    for domain in &required.0 {
        let best = candidates
            .iter()
            .zip(&decisions)
            .filter(|(candidate, decision)| {
                decision.accepted && candidate.coverage.contains(*domain)
            })
            .min_by_key(|(candidate, _)| (candidate.cost, candidate.id))
            .map(|(candidate, _)| candidate.id);
        if let Some(best) = best {
            selected.insert(*domain, best);
        }
    }
    let uncovered = required
        .0
        .iter()
        .filter(|domain| !selected.contains_key(domain))
        .copied()
        .collect();
    PertylizerLoweringPlan {
        target: capabilities.revision.clone(),
        required,
        candidates,
        decisions,
        selected,
        uncovered,
    }
}

pub fn apply_render_gate(
    plan: &PertylizerLoweringPlan,
    evaluations: &[CandidateRenderEvaluation],
    budget: AudioBudget,
) -> RenderGatedSelection {
    let state_decisions: BTreeMap<_, _> = plan
        .decisions
        .iter()
        .map(|decision| (decision.candidate, decision.accepted))
        .collect();
    let render_results: BTreeMap<_, _> = evaluations
        .iter()
        .map(|evaluation| (evaluation.candidate, &evaluation.comparison))
        .collect();
    let decisions: Vec<_> = plan
        .candidates
        .iter()
        .map(|candidate| {
            let rejection = if !state_decisions.get(&candidate.id).copied().unwrap_or(false) {
                Some(RenderGateRejection::StateGateRejected)
            } else {
                match (
                    candidate.render_requirement,
                    render_results.get(&candidate.id),
                ) {
                    (RenderRequirement::Required, None) => Some(RenderGateRejection::NotRendered),
                    (_, Some(comparison)) => (!budget.accepts(comparison))
                        .then_some(RenderGateRejection::AudioBudgetExceeded),
                    (RenderRequirement::PinnedBaseline, None) => None,
                }
            };
            RenderGateDecision {
                candidate: candidate.id,
                accepted: rejection.is_none(),
                rejection,
            }
        })
        .collect();
    let accepted: BTreeSet<_> = decisions
        .iter()
        .filter(|decision| decision.accepted)
        .map(|decision| decision.candidate)
        .collect();
    let mut selected = BTreeMap::new();
    for domain in &plan.required.0 {
        if let Some(candidate) = plan
            .candidates
            .iter()
            .filter(|candidate| {
                accepted.contains(&candidate.id) && candidate.coverage.contains(*domain)
            })
            .min_by_key(|candidate| (candidate.cost, candidate.id))
        {
            selected.insert(*domain, candidate.id);
        }
    }
    RenderGatedSelection {
        decisions,
        uncovered: plan
            .required
            .0
            .iter()
            .filter(|domain| !selected.contains_key(domain))
            .copied()
            .collect(),
        selected,
    }
}

fn decide(
    candidate: &RepresentationCandidate,
    capabilities: &TargetCapabilities,
    budget: StateBudget,
) -> CandidateDecision {
    let mut rejected = Vec::new();
    for requirement in &candidate.requirements {
        let supported = match requirement {
            Requirement::Module(module) => capabilities.modules.contains(module),
            Requirement::Behavior(behavior) => capabilities.verified_behaviors.contains(behavior),
        };
        if !supported {
            rejected.push(RejectionReason::MissingRequirement(requirement.clone()));
        }
    }
    if candidate.render_requirement != RenderRequirement::PinnedBaseline
        && !candidate.residuals.within(budget)
    {
        rejected.push(RejectionReason::StateBudgetExceeded);
    }
    CandidateDecision {
        candidate: candidate.id,
        accepted: rejected.is_empty(),
        rejected,
    }
}

#[derive(Default)]
struct CandidateBuilder {
    items: Vec<RepresentationCandidate>,
}

impl CandidateBuilder {
    #[allow(clippy::too_many_arguments)]
    fn push(
        &mut self,
        class: RepresentationClass,
        coverage: Coverage,
        requirements: Vec<Requirement>,
        known_loss: KnownLoss,
        editability: EditabilityClass,
        graph_complexity: GraphComplexity,
        automation_density: AutomationDensity,
        residuals: StateResiduals,
        render_requirement: RenderRequirement,
        scope: CandidateScope,
        recipe: CandidateRecipe,
    ) {
        self.push_with_losses(
            class,
            coverage,
            requirements,
            (known_loss != KnownLoss::Exact)
                .then_some(known_loss)
                .into_iter()
                .collect(),
            editability,
            graph_complexity,
            automation_density,
            residuals,
            render_requirement,
            scope,
            recipe,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn push_with_losses(
        &mut self,
        class: RepresentationClass,
        coverage: Coverage,
        requirements: Vec<Requirement>,
        known_losses: Vec<KnownLoss>,
        editability: EditabilityClass,
        graph_complexity: GraphComplexity,
        automation_density: AutomationDensity,
        residuals: StateResiduals,
        render_requirement: RenderRequirement,
        scope: CandidateScope,
        recipe: CandidateRecipe,
    ) {
        let id = CandidateId(self.items.len() as u64);
        let serialized_size = serde_json::to_vec(&recipe).map_or(SerializedSize(0), |encoded| {
            SerializedSize(encoded.len() as u64)
        });
        let known_loss = known_losses
            .iter()
            .copied()
            .max()
            .unwrap_or(KnownLoss::Exact);
        self.items.push(RepresentationCandidate {
            id,
            class,
            coverage,
            requirements,
            known_losses,
            cost: EditCost {
                known_loss,
                editability,
                graph_complexity,
                automation_density,
                serialized_size,
            },
            residuals,
            render_requirement,
            scope,
            recipe,
        });
    }
}

fn module(name: &str) -> Requirement {
    Requirement::Module(ModuleType(name.to_owned()))
}

fn coverage(domains: impl IntoIterator<Item = StateDomain>) -> Coverage {
    Coverage(domains.into_iter().collect())
}

fn required_domains(program: &AnalyzedSidProgram, causal: &ChipCausalAnalysis) -> Coverage {
    let mut domains = BTreeSet::new();
    if !program.frames().is_empty() {
        domains.extend([
            StateDomain::Pitch,
            StateDomain::Amplitude,
            StateDomain::GateRetrigger,
            StateDomain::Waveform,
            StateDomain::PulseWidth,
        ]);
    }
    if !causal.onset_programs.is_empty() {
        domains.insert(StateDomain::OnsetProgram);
    }
    if causal.continuation_edges > 0 {
        domains.insert(StateDomain::OscillatorContinuation);
    }
    if program.voices.iter().any(|voice| {
        voice
            .waveform
            .0
            .iter()
            .any(|point| point.value.value.0 & 0x80 != 0)
    }) {
        domains.insert(StateDomain::NoiseContinuation);
    }
    if !program.chip.cutoff.0.is_empty()
        || !program.chip.resonance.0.is_empty()
        || !program.chip.filter_mode.0.is_empty()
    {
        domains.insert(StateDomain::Filter);
    }
    if !program.chip.routing.0.is_empty() {
        domains.insert(StateDomain::Routing);
    }
    if program.chip.topology.edges.iter().any(|edge| {
        matches!(
            edge.kind,
            crate::analysis::sid_program::topology::TopologyEdgeKind::Sync
                | crate::analysis::sid_program::topology::TopologyEdgeKind::Ring
        )
    }) {
        domains.insert(StateDomain::CrossVoice);
    }
    if has_digi_stream(program) {
        domains.insert(StateDomain::Digi);
    }
    Coverage(domains)
}

fn candidate_scope(program: &AnalyzedSidProgram) -> CandidateScope {
    CandidateScope {
        voices: program.voices.iter().map(|voice| voice.voice).collect(),
        regions: program
            .semantic
            .regions
            .iter()
            .map(|region| region.id)
            .collect(),
        definitions: program
            .semantic
            .program_definitions
            .iter()
            .map(|definition| definition.id)
            .collect(),
        occurrences: program
            .semantic
            .occurrences
            .iter()
            .map(|occurrence| occurrence.id)
            .collect(),
        continuous_occurrences: program
            .semantic
            .continuous
            .occurrences
            .iter()
            .map(|occurrence| occurrence.id)
            .collect(),
        reuse_groups: program
            .semantic
            .continuous
            .reuse_groups
            .iter()
            .map(|group| group.id)
            .collect(),
        interpretations: program
            .semantic
            .interpretations
            .iter()
            .map(|interpretation| interpretation.id)
            .collect(),
    }
}

fn interpretation_ids(
    program: &AnalyzedSidProgram,
    include: impl Fn(InterpretedSignalSource, SignalInterpretationKind) -> bool,
) -> Vec<SignalInterpretationId> {
    program
        .semantic
        .interpretations
        .iter()
        .filter(|interpretation| include(interpretation.source, interpretation.kind))
        .map(|interpretation| interpretation.id)
        .collect()
}

fn interpretation_coverage(
    program: &AnalyzedSidProgram,
    ids: &[SignalInterpretationId],
) -> Coverage {
    let wanted: BTreeSet<_> = ids.iter().copied().collect();
    let mut domains = BTreeSet::new();
    for interpretation in &program.semantic.interpretations {
        if !wanted.contains(&interpretation.id) {
            continue;
        }
        domains.insert(match interpretation.source {
            InterpretedSignalSource::VoiceFrequency { .. } => StateDomain::Pitch,
            InterpretedSignalSource::VoicePulseWidth { .. } => StateDomain::PulseWidth,
            InterpretedSignalSource::ChipCutoff => StateDomain::Filter,
            InterpretedSignalSource::MasterVolume
            | InterpretedSignalSource::EnvelopeLevel { .. } => StateDomain::Amplitude,
        });
    }
    Coverage(domains)
}

fn causal_coverage(program: &AnalyzedSidProgram) -> Coverage {
    let mut domains = BTreeSet::new();
    for dependency in eligible_causal_dependencies(program) {
        for consumer in &dependency.consumers {
            let register = consumer.register.0;
            if register < 0x15 {
                match register % 7 {
                    0 | 1 => {
                        domains.insert(StateDomain::Pitch);
                    }
                    2 | 3 => {
                        domains.insert(StateDomain::PulseWidth);
                    }
                    4 => {
                        domains.extend([StateDomain::Waveform, StateDomain::CrossVoice]);
                    }
                    _ => {
                        domains.insert(StateDomain::Amplitude);
                    }
                }
            } else {
                match register {
                    0x15 | 0x16 => {
                        domains.insert(StateDomain::Filter);
                    }
                    0x17 => {
                        domains.extend([StateDomain::Filter, StateDomain::Routing]);
                    }
                    0x18 => {
                        domains.extend([
                            StateDomain::Amplitude,
                            StateDomain::Filter,
                            StateDomain::Digi,
                        ]);
                    }
                    _ => {}
                }
            }
        }
    }
    Coverage(domains)
}

fn eligible_causal_dependencies(
    program: &AnalyzedSidProgram,
) -> impl Iterator<Item = &crate::analysis::sid_program::semantic::CausalDependency> {
    program
        .semantic
        .causal_dependencies
        .iter()
        .filter(|dependency| eligible_causal_dependency(dependency))
}

fn eligible_causal_dependency(
    dependency: &crate::analysis::sid_program::semantic::CausalDependency,
) -> bool {
    dependency.support == CausalSupport::ProducerTransformConsumer
        && dependency.consumers.len() == 1
        && !matches!(
            dependency.source,
            crate::analysis::sid_program::semantic::CausalSource::Random(_)
        )
        && dependency.evidence.iter().any(|evidence| {
            evidence.confidence.0 >= 750
                && matches!(
                    evidence.validity,
                    crate::analysis::sid_program::evidence::EvidenceValidity::Valid
                        | crate::analysis::sid_program::evidence::EvidenceValidity::TimingBounded
                )
        })
}

fn envelope_points(program: &AnalyzedSidProgram) -> RecipePointCount {
    RecipePointCount(
        program
            .voices
            .iter()
            .map(|voice| voice.envelope.0.len() as u32)
            .sum(),
    )
}

fn waveform_points(program: &AnalyzedSidProgram) -> RecipePointCount {
    RecipePointCount(
        program
            .voices
            .iter()
            .map(|voice| voice.waveform.0.len() as u32)
            .sum(),
    )
}

fn waveform_changes(program: &AnalyzedSidProgram) -> u64 {
    program
        .voices
        .iter()
        .map(|voice| voice.waveform.0.len().saturating_sub(1) as u64)
        .sum()
}

fn filter_points(program: &AnalyzedSidProgram) -> RecipePointCount {
    RecipePointCount(
        (program.chip.cutoff.0.len()
            + program.chip.resonance.0.len()
            + program.chip.routing.0.len()
            + program.chip.filter_mode.0.len()) as u32,
    )
}

fn sparse_residuals(program: &AnalyzedSidProgram) -> StateResiduals {
    let mut residuals = StateResiduals::exact();
    for interpretation in &program.semantic.interpretations {
        if !matches!(
            interpretation.kind,
            SignalInterpretationKind::Table | SignalInterpretationKind::BoundedScript
        ) {
            continue;
        }
        let points = interpretation.steps.len() as u64;
        match interpretation.source {
            InterpretedSignalSource::VoiceFrequency { .. } => {
                residuals.maximum_pitch = CentsResidual(100.0);
            }
            InterpretedSignalSource::VoicePulseWidth { .. } => {
                residuals.pulse_width.0 += points;
            }
            InterpretedSignalSource::ChipCutoff => {
                residuals.filter.0 += points;
            }
            InterpretedSignalSource::MasterVolume
            | InterpretedSignalSource::EnvelopeLevel { .. } => {
                residuals.maximum_level = LevelResidual(0.1);
            }
        }
    }
    residuals.waveform = StateMismatchCount(waveform_changes(program));
    residuals.oscillator_continuation = StateMismatchCount(continuation_edge_count(program));
    residuals.noise_continuation = StateMismatchCount(
        program
            .voices
            .iter()
            .flat_map(|voice| &voice.waveform.0)
            .filter(|point| point.value.value.0 & 0x80 != 0)
            .count() as u64,
    );
    residuals.cross_voice = StateMismatchCount(
        program
            .chip
            .topology
            .edges
            .iter()
            .filter(|edge| {
                matches!(
                    edge.kind,
                    crate::analysis::sid_program::topology::TopologyEdgeKind::Sync
                        | crate::analysis::sid_program::topology::TopologyEdgeKind::Ring
                )
            })
            .count() as u64,
    );
    residuals.digi = StateMismatchCount(u64::from(detected_digi_points(program).0));
    residuals
}

fn onset_programs(program: &AnalyzedSidProgram) -> Vec<OnsetProgramRecipe> {
    let mut gate = [false; 3];
    let mut call = [None; 3];
    let mut registers = [[0_u8; 7]; 3];
    let mut pending: [Vec<SidBusEventId>; 3] = std::array::from_fn(|_| Vec::new());
    let mut pending_initial = [SidVoiceRegisterState([0; 7]); 3];
    let mut onsets = Vec::new();
    for event in &program.capture.events {
        if event.chip != SidChipId::PRIMARY || event.access != SidBusAccess::Write {
            continue;
        }
        let Some(register) = event.register else {
            continue;
        };
        if register.0 >= 0x15 {
            continue;
        }
        let voice_index = usize::from(register.0 / 7);
        if call[voice_index] != Some(event.call) {
            call[voice_index] = Some(event.call);
            pending[voice_index].clear();
        }
        let register_offset = usize::from(register.0 % 7);
        if pending[voice_index].is_empty() {
            pending_initial[voice_index] = SidVoiceRegisterState(registers[voice_index]);
        }
        pending[voice_index].push(event.id);
        if register_offset != 4 {
            registers[voice_index][register_offset] = event.value;
            continue;
        }
        let next_gate = event.value & 0x01 != 0;
        if !gate[voice_index] && next_gate {
            let timestamp_quality = if pending[voice_index]
                .iter()
                .filter_map(|id| program.capture.events.get(id.0 as usize))
                .any(|write| {
                    write.timestamp_quality == EventTimestampQuality::InstructionStartBounded
                }) {
                EventTimestampQuality::InstructionStartBounded
            } else {
                EventTimestampQuality::Exact
            };
            onsets.push(OnsetProgramRecipe {
                voice: VoiceId::from_index(voice_index),
                call: event.call,
                trigger: event.id,
                initial_registers: pending_initial[voice_index],
                writes: pending[voice_index].clone(),
                timestamp_quality,
            });
            pending[voice_index].clear();
        } else if gate[voice_index] && !next_gate {
            pending[voice_index].clear();
            pending_initial[voice_index] = SidVoiceRegisterState(registers[voice_index]);
            pending[voice_index].push(event.id);
        }
        gate[voice_index] = next_gate;
        registers[voice_index][register_offset] = event.value;
    }
    onsets
}

fn oscillator_start_recipes(program: &AnalyzedSidProgram) -> Vec<OscillatorStartRecipe> {
    program
        .semantic
        .continuous
        .occurrences
        .iter()
        .filter_map(|occurrence| {
            let voice = occurrence.voice?;
            let oscillator = &occurrence.initial.digital_sid.oscillators[voice.to_index()];
            let policy = if oscillator.noise_poisoned {
                OscillatorStartPolicy::Unknown
            } else if occurrence.previous.is_some() {
                OscillatorStartPolicy::LegatoContinued
            } else if oscillator.test_or_reset {
                OscillatorStartPolicy::TestReset
            } else if oscillator.accumulator == 0 && oscillator.sync_resets > 0 {
                OscillatorStartPolicy::SyncReset
            } else if occurrence.span.start.0 > 0 {
                OscillatorStartPolicy::FreeRunning
            } else {
                OscillatorStartPolicy::ExplicitState
            };
            Some(OscillatorStartRecipe {
                occurrence: occurrence.id,
                voice,
                policy,
            })
        })
        .collect()
}

fn noise_start_recipes(program: &AnalyzedSidProgram) -> Vec<NoiseStartRecipe> {
    const NOISE_SEED: u32 = 0x003F_FFFF;
    program
        .semantic
        .continuous
        .occurrences
        .iter()
        .filter_map(|occurrence| {
            let voice = occurrence.voice?;
            let checkpoint = &occurrence.initial.digital_sid;
            let control = checkpoint.registers[voice.to_index() * 7 + 4];
            if control & 0x80 == 0 {
                return None;
            }
            let oscillator = &checkpoint.oscillators[voice.to_index()];
            let policy = if oscillator.noise_poisoned {
                NoiseStartPolicy::Unknown
            } else if occurrence.previous.is_some() {
                NoiseStartPolicy::LegatoContinued
            } else if oscillator.test_or_reset || control & 0x08 != 0 {
                NoiseStartPolicy::TestReset
            } else if oscillator.noise_shift_register == NOISE_SEED {
                NoiseStartPolicy::Seeded
            } else {
                NoiseStartPolicy::FreeRunning
            };
            Some(NoiseStartRecipe {
                occurrence: occurrence.id,
                voice,
                policy,
            })
        })
        .collect()
}

fn fallback_reasons(
    program: &AnalyzedSidProgram,
    causal: &ChipCausalAnalysis,
) -> Vec<FallbackReason> {
    let mut reasons = BTreeSet::new();
    if causal
        .onset_programs
        .iter()
        .any(|onset| onset.timestamp_quality == EventTimestampQuality::InstructionStartBounded)
    {
        reasons.insert(FallbackReason::InstructionStartTiming);
    }
    if causal.history_sensitive_retriggers > 0 {
        reasons.insert(FallbackReason::EnvelopeHistory);
    }
    if causal.continuation_edges > 0 {
        reasons.insert(FallbackReason::OscillatorContinuation);
    }
    if !causal.noise_starts.is_empty() {
        reasons.insert(FallbackReason::NoiseContinuation);
    }
    if !program.chip.cutoff.0.is_empty() || !program.chip.routing.0.is_empty() {
        reasons.insert(FallbackReason::VoiceLocalFilter);
    }
    if causal.cross_voice_edges > 0 {
        reasons.insert(FallbackReason::CrossVoiceTopology);
    }
    if program
        .semantic
        .causal_dependencies
        .iter()
        .any(|dependency| !eligible_causal_dependency(dependency))
    {
        reasons.insert(FallbackReason::AmbiguousCausalModulation);
    }
    if program
        .semantic
        .causal_dependencies
        .iter()
        .any(|dependency| {
            matches!(
                dependency.source,
                crate::analysis::sid_program::semantic::CausalSource::Random(_)
            )
        })
    {
        reasons.insert(FallbackReason::RandomModulation);
    }
    if has_digi_stream(program) {
        reasons.insert(FallbackReason::DigiUnsupported);
    }
    reasons.into_iter().collect()
}

fn fallback_residuals(program: &AnalyzedSidProgram, causal: &ChipCausalAnalysis) -> StateResiduals {
    StateResiduals {
        onset_program: StateMismatchCount(causal.onset_programs.len() as u64),
        gate_retrigger: StateMismatchCount(causal.history_sensitive_retriggers),
        waveform: StateMismatchCount(waveform_changes(program)),
        oscillator_continuation: StateMismatchCount(causal.continuation_edges),
        noise_continuation: StateMismatchCount(causal.noise_starts.len() as u64),
        filter: StateMismatchCount(u64::from(filter_points(program).0)),
        routing: StateMismatchCount(program.chip.routing.0.len() as u64),
        cross_voice: StateMismatchCount(causal.cross_voice_edges),
        digi: StateMismatchCount(u64::from(detected_digi_points(program).0)),
        ..StateResiduals::exact()
    }
}

fn cross_voice_edge_count(program: &AnalyzedSidProgram) -> u64 {
    program
        .chip
        .topology
        .edges
        .iter()
        .filter(|edge| {
            matches!(
                edge.kind,
                crate::analysis::sid_program::topology::TopologyEdgeKind::Sync
                    | crate::analysis::sid_program::topology::TopologyEdgeKind::Ring
            )
        })
        .count() as u64
}

fn sid_sequence_requirements(causal: &ChipCausalAnalysis) -> Vec<Requirement> {
    let mut requirements = vec![module("sid_oscillator")];
    if !causal.onset_programs.is_empty() {
        requirements.push(Requirement::Behavior(
            BehaviorCapability::CycleStampedControl,
        ));
    }
    if causal.continuation_edges > 0 {
        requirements.push(Requirement::Behavior(
            BehaviorCapability::SequenceContinuation,
        ));
    }
    if !causal.oscillator_starts.is_empty() {
        requirements.push(Requirement::Behavior(BehaviorCapability::PhaseReset));
        requirements.push(Requirement::Behavior(
            BehaviorCapability::OscillatorStateInitialization,
        ));
    }
    if !causal.noise_starts.is_empty() {
        requirements.push(Requirement::Behavior(
            BehaviorCapability::NoiseStateInitialization,
        ));
    }
    if causal.cross_voice_edges > 0 {
        requirements.push(Requirement::Behavior(BehaviorCapability::LiveCrossVoice));
    }
    requirements
}

fn continuation_edge_count(program: &AnalyzedSidProgram) -> u64 {
    program
        .semantic
        .continuous
        .occurrences
        .iter()
        .filter(|occurrence| occurrence.continuation.is_some())
        .count() as u64
}

fn has_digi_stream(program: &AnalyzedSidProgram) -> bool {
    program.semantic.regions.iter().any(|region| {
        region.kind == crate::analysis::sid_program::region::SoundRegionKind::DigiStream
    })
}

fn detected_digi_points(program: &AnalyzedSidProgram) -> RecipePointCount {
    let count = program
        .chip
        .digi
        .0
        .iter()
        .filter(|point| {
            program.semantic.regions.iter().any(|region| {
                region.kind == crate::analysis::sid_program::region::SoundRegionKind::DigiStream
                    && region.span.start <= point.at
                    && point.at < region.span.end
            })
        })
        .count();
    RecipePointCount(count as u32)
}

fn history_sensitive_retriggers(program: &AnalyzedSidProgram) -> u64 {
    program
        .voices
        .iter()
        .map(|voice| {
            voice
                .envelope
                .0
                .windows(2)
                .filter(|pair| {
                    pair[0].value.value.level.0 > 0
                        && pair[1].value.value.phase == crate::emu::sid::EnvPhase::Attack
                })
                .count() as u64
        })
        .sum()
}
