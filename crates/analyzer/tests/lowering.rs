use sid_analyzer::analysis::sid_program::AnalyzedSidProgram;
use sid_analyzer::audio::abtest::AudioBudget;
use sid_analyzer::audio::{AlignmentSamples, AudioComparison, Decibels, FrequencyHz};
use sid_analyzer::emu::PlaybackTiming;
use sid_analyzer::export::synth::lowering::{
    BehaviorCapability, CandidateRecipe, CandidateRenderEvaluation, FallbackReason, KnownLoss,
    NoiseStartPolicy, RenderGateRejection, RenderRequirement, RepresentationClass, Requirement,
    StateBudget, StateDomain, TargetCapabilities, apply_render_gate, compile, select_automatic,
};
use sid_analyzer::header::{self, SubtuneIndex};

fn program() -> AnalyzedSidProgram {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../assets/music/Nemesis_the_Warlock.sid");
    let bytes = std::fs::read(path).unwrap();
    let header = header::parse(&bytes).unwrap();
    let subtune = SubtuneIndex(1);
    let timing = PlaybackTiming::for_subtune(&header, subtune);
    let trace = sid_analyzer::emu::run_with_timing(&header, &bytes, subtune, 40, timing).unwrap();
    AnalyzedSidProgram::from_trace(&header, subtune, timing.resolved_from_trace(&trace), &trace)
}

fn exact_audio() -> AudioComparison {
    AudioComparison {
        alignment: AlignmentSamples(0),
        rms_error: 0.0,
        peak_error: 0.0,
        level_error_db: Some(Decibels(0.0)),
        pitch_error_hz: Some(FrequencyHz(0.0)),
        log_spectral_distance: Decibels(0.0),
        gain_normalized_log_spectral_distance: Some(Decibels(0.0)),
        centroid_error_hz: FrequencyHz(0.0),
        rolloff_error_hz: FrequencyHz(0.0),
        flatness_error: 0.0,
        zero_crossing_error: 0.0,
    }
}

#[test]
fn pinned_capabilities_parse_and_unverified_behavior_rejects_candidate() {
    let program = program();
    let capabilities = TargetCapabilities::from_pinned_mirrors().unwrap();
    assert!(capabilities.supports_module("sid_oscillator"));
    let plan = compile(&program, &capabilities, StateBudget::default());
    let sid = plan
        .candidates
        .iter()
        .find(|candidate| candidate.class == RepresentationClass::SidSequence)
        .unwrap();
    let decision = plan
        .decisions
        .iter()
        .find(|decision| decision.candidate == sid.id)
        .unwrap();
    assert!(!decision.accepted);
}

#[test]
fn verified_semantics_enable_exact_domain_candidates_deterministically() {
    let program = program();
    let capabilities = TargetCapabilities::from_pinned_mirrors()
        .unwrap()
        .with_verified_behavior(BehaviorCapability::SequenceContinuation)
        .with_verified_behavior(BehaviorCapability::PhaseReset)
        .with_verified_behavior(BehaviorCapability::CycleStampedControl)
        .with_verified_behavior(BehaviorCapability::OscillatorStateInitialization)
        .with_verified_behavior(BehaviorCapability::NoiseStateInitialization)
        .with_verified_behavior(BehaviorCapability::LiveCrossVoice)
        .with_verified_behavior(BehaviorCapability::DynamicRouting)
        .with_verified_behavior(BehaviorCapability::ChipScopedFilter)
        .with_verified_behavior(BehaviorCapability::GateRetrigger);
    let first = compile(&program, &capabilities, StateBudget::default());
    let second = compile(&program, &capabilities, StateBudget::default());
    assert_eq!(
        serde_json::to_vec(&first).unwrap(),
        serde_json::to_vec(&second).unwrap()
    );
    let waveform = first.selected[&StateDomain::Waveform];
    assert_eq!(
        first
            .candidates
            .iter()
            .find(|candidate| candidate.id == waveform)
            .unwrap()
            .class,
        RepresentationClass::SidSequence
    );
    let filter = first.selected[&StateDomain::Filter];
    assert_eq!(
        first
            .candidates
            .iter()
            .find(|candidate| candidate.id == filter)
            .unwrap()
            .class,
        RepresentationClass::SharedFilter
    );
}

#[test]
fn render_gate_never_resurrects_a_state_rejected_candidate() {
    let program = program();
    let capabilities = TargetCapabilities::from_pinned_mirrors().unwrap();
    let plan = compile(&program, &capabilities, StateBudget::default());
    let fallback = plan
        .candidates
        .iter()
        .find(|candidate| candidate.class == RepresentationClass::DenseAutomation)
        .unwrap();
    let sequence = plan
        .candidates
        .iter()
        .find(|candidate| candidate.class == RepresentationClass::SidSequence)
        .unwrap();
    let selection = apply_render_gate(
        &plan,
        &[
            CandidateRenderEvaluation {
                candidate: fallback.id,
                comparison: exact_audio(),
            },
            CandidateRenderEvaluation {
                candidate: sequence.id,
                comparison: exact_audio(),
            },
        ],
        AudioBudget::default(),
    );
    let sequence_decision = selection
        .decisions
        .iter()
        .find(|decision| decision.candidate == sequence.id)
        .unwrap();
    assert_eq!(
        sequence_decision.rejection,
        Some(RenderGateRejection::StateGateRejected)
    );
    assert_eq!(selection.selected[&StateDomain::Waveform], fallback.id);
}

#[test]
fn compilers_emit_concrete_recipes_and_only_pinned_fallback_skips_rendering() {
    let program = program();
    let capabilities = TargetCapabilities::from_pinned_mirrors().unwrap();
    let plan = compile(&program, &capabilities, StateBudget::default());
    assert!(
        plan.candidates
            .iter()
            .any(|candidate| { matches!(candidate.recipe, CandidateRecipe::NativeAdsr { .. }) })
    );
    assert!(
        plan.candidates
            .iter()
            .any(|candidate| { matches!(candidate.recipe, CandidateRecipe::SidSequence { .. }) })
    );
    let fallback = plan
        .candidates
        .iter()
        .find(|candidate| candidate.class == RepresentationClass::DenseAutomation)
        .unwrap();
    assert_eq!(
        fallback.render_requirement,
        RenderRequirement::PinnedBaseline
    );
    assert!(
        plan.candidates
            .iter()
            .filter(|candidate| {
                candidate.render_requirement == RenderRequirement::PinnedBaseline
            })
            .count()
            == 1
    );
}

#[test]
fn automatic_selection_is_deterministic_and_reports_render_uncovered_domains() {
    let program = program();
    let capabilities = TargetCapabilities::from_pinned_mirrors().unwrap();
    let first = select_automatic(
        &program,
        &capabilities,
        StateBudget::default(),
        &[],
        AudioBudget::default(),
    );
    let second = select_automatic(
        &program,
        &capabilities,
        StateBudget::default(),
        &[],
        AudioBudget::default(),
    );
    assert_eq!(
        serde_json::to_vec(&first).unwrap(),
        serde_json::to_vec(&second).unwrap()
    );
    assert!(first.render.uncovered.is_empty());
}

#[test]
fn continuous_timeline_requires_and_capability_gates_sequence_continuation() {
    let mut program = program();
    assert!(
        program
            .semantic
            .continuous
            .occurrences
            .iter()
            .any(|occurrence| occurrence.continuation.is_some())
    );
    for group in &mut program.semantic.continuous.reuse_groups {
        group.policy = sid_analyzer::analysis::sid_program::semantic::ReusePolicy::Unique;
    }
    let plan = compile(
        &program,
        &TargetCapabilities::from_pinned_mirrors().unwrap(),
        StateBudget::default(),
    );
    assert!(plan.required.contains(StateDomain::OscillatorContinuation));
    let sequence = plan
        .candidates
        .iter()
        .find(|candidate| candidate.class == RepresentationClass::SidSequence)
        .unwrap();
    assert!(sequence.requirements.contains(&Requirement::Behavior(
        BehaviorCapability::SequenceContinuation
    )));
    assert!(
        sequence
            .requirements
            .contains(&Requirement::Behavior(BehaviorCapability::PhaseReset))
    );
}

#[test]
fn ordinary_master_volume_writes_do_not_create_a_digi_requirement() {
    let program = program();
    assert!(!program.chip.digi.0.is_empty());
    assert!(!program.semantic.regions.iter().any(|region| {
        region.kind == sid_analyzer::analysis::sid_program::region::SoundRegionKind::DigiStream
    }));
    let plan = compile(
        &program,
        &TargetCapabilities::from_pinned_mirrors().unwrap(),
        StateBudget::default(),
    );
    assert!(!plan.required.contains(StateDomain::Digi));
    assert!(
        plan.candidates
            .iter()
            .all(|candidate| candidate.class != RepresentationClass::Sampler)
    );
}

#[test]
fn detected_digi_stream_requires_sampler_coverage() {
    use sid_analyzer::analysis::sid_program::ids::SoundRegionId;
    use sid_analyzer::analysis::sid_program::region::{SoundRegion, SoundRegionKind};
    use sid_analyzer::analysis::sid_program::time::SourceSpan;
    use sid_analyzer::trace::ChipCycle;

    let mut program = program();
    let start = program.chip.digi.0.first().unwrap().at;
    let end = ChipCycle(program.chip.digi.0.last().unwrap().at.0.saturating_add(1));
    program.semantic.regions.push(SoundRegion {
        id: SoundRegionId(program.semantic.regions.len() as u64),
        voice: None,
        kind: SoundRegionKind::DigiStream,
        span: SourceSpan { start, end },
        evidence: Vec::new(),
    });
    let plan = compile(
        &program,
        &TargetCapabilities::from_pinned_mirrors().unwrap(),
        StateBudget::default(),
    );
    assert!(plan.required.contains(StateDomain::Digi));
    let sampler = plan
        .candidates
        .iter()
        .find(|candidate| candidate.class == RepresentationClass::Sampler)
        .unwrap();
    assert!(matches!(
        sampler.recipe,
        CandidateRecipe::Sampler {
            sample_points
        } if sample_points.0 > 0
    ));
}

#[test]
fn sid_sequence_retains_ordered_onset_programs_and_explicit_start_policies() {
    let program = program();
    let plan = compile(
        &program,
        &TargetCapabilities::from_pinned_mirrors().unwrap(),
        StateBudget::default(),
    );
    assert!(plan.required.contains(StateDomain::OnsetProgram));
    let sequence = plan
        .candidates
        .iter()
        .find(|candidate| candidate.class == RepresentationClass::SidSequence)
        .unwrap();
    let CandidateRecipe::SidSequence {
        onset_programs,
        oscillator_starts,
        noise_starts,
        ..
    } = &sequence.recipe
    else {
        panic!("SID sequence candidate has the wrong recipe");
    };
    assert!(!onset_programs.is_empty());
    for onset in onset_programs {
        assert_eq!(onset.writes.last(), Some(&onset.trigger));
        assert!(onset.writes.windows(2).all(|ids| ids[0] < ids[1]));
        let mut registers = onset.initial_registers.0;
        assert!(onset.writes.iter().all(|id| {
            let event = program.capture.events[id.0 as usize];
            let valid = event.call == onset.call
                && event.register.is_some_and(|register| {
                    register.0 / 7 == onset.voice.0 - 1 && register.0 < 0x15
                });
            if let Some(register) = event.register {
                registers[usize::from(register.0 % 7)] = event.value;
            }
            valid
        }));
        assert_ne!(registers[4] & 0x01, 0);
    }
    assert!(!oscillator_starts.is_empty());
    assert!(
        noise_starts
            .iter()
            .all(|start| { !matches!(start.policy, NoiseStartPolicy::SequenceRestart) })
    );
    assert!(sequence.requirements.contains(&Requirement::Behavior(
        BehaviorCapability::CycleStampedControl
    )));
    assert!(sequence.requirements.contains(&Requirement::Behavior(
        BehaviorCapability::OscillatorStateInitialization
    )));
}

#[test]
fn pinned_baseline_reports_every_chip_causal_fallback_and_residual() {
    let program = program();
    let plan = compile(
        &program,
        &TargetCapabilities::from_pinned_mirrors().unwrap(),
        StateBudget::default(),
    );
    let fallback = plan
        .candidates
        .iter()
        .find(|candidate| candidate.class == RepresentationClass::DenseAutomation)
        .unwrap();
    assert!(fallback.known_losses.contains(&KnownLoss::TimingBounded));
    assert!(
        fallback
            .known_losses
            .contains(&KnownLoss::StateApproximation)
    );
    assert!(
        fallback
            .known_losses
            .contains(&KnownLoss::TopologyApproximation)
    );
    assert!(fallback.residuals.onset_program.0 > 0);
    assert!(fallback.residuals.oscillator_continuation.0 > 0);
    let CandidateRecipe::Automation { fallbacks, .. } = &fallback.recipe else {
        panic!("dense baseline candidate has the wrong recipe");
    };
    assert!(fallbacks.contains(&FallbackReason::InstructionStartTiming));
    assert!(fallbacks.contains(&FallbackReason::OscillatorContinuation));
    assert!(fallbacks.contains(&FallbackReason::VoiceLocalFilter));
}

#[test]
fn history_sensitive_envelopes_reject_reset_based_recipes() {
    let mut program = program();
    let envelope = &mut program.voices[0].envelope.0;
    assert!(envelope.len() >= 2);
    envelope[0].value.value.level.0 = 7;
    envelope[1].value.value.phase = sid_analyzer::emu::sid::EnvPhase::Attack;
    let plan = compile(
        &program,
        &TargetCapabilities::from_pinned_mirrors().unwrap(),
        StateBudget::default(),
    );
    for class in [
        RepresentationClass::NativeAdsr,
        RepresentationClass::NormalizedAdsr,
        RepresentationClass::Mseg,
    ] {
        let candidate = plan
            .candidates
            .iter()
            .find(|candidate| candidate.class == class)
            .unwrap();
        assert!(candidate.residuals.gate_retrigger.0 > 0);
        assert!(
            !plan
                .decisions
                .iter()
                .find(|decision| decision.candidate == candidate.id)
                .unwrap()
                .accepted
        );
    }
}

#[test]
fn ambiguous_or_multi_consumer_dependencies_do_not_become_direct_modulation() {
    use sid_analyzer::analysis::sid_program::ids::CausalDependencyId;
    use sid_analyzer::analysis::sid_program::semantic::{
        CausalConsumer, CausalDependency, CausalSource, CausalSupport, CausalTransform,
    };
    use sid_analyzer::trace::SidRegister;

    let mut program = program();
    let producer = program.capture.events[0].id;
    program.semantic.causal_dependencies = vec![CausalDependency {
        id: CausalDependencyId(0),
        source: CausalSource::Oscillator3Read,
        producer,
        producer_value: 0,
        consumers: vec![
            CausalConsumer {
                event: producer,
                register: SidRegister(0),
                transform: CausalTransform::Identity,
            },
            CausalConsumer {
                event: producer,
                register: SidRegister(2),
                transform: CausalTransform::Identity,
            },
        ],
        support: CausalSupport::OrderedSameCallHypothesis,
        evidence: Vec::new(),
    }];
    let plan = compile(
        &program,
        &TargetCapabilities::from_pinned_mirrors().unwrap(),
        StateBudget::default(),
    );
    assert!(plan.candidates.iter().all(|candidate| !matches!(
        candidate.class,
        RepresentationClass::DirectCv | RepresentationClass::ModMatrix
    )));
    let fallback = plan
        .candidates
        .iter()
        .find(|candidate| candidate.class == RepresentationClass::DenseAutomation)
        .unwrap();
    assert!(matches!(
        &fallback.recipe,
        CandidateRecipe::Automation { fallbacks, .. }
            if fallbacks.contains(&FallbackReason::AmbiguousCausalModulation)
    ));
}

#[test]
fn verified_table_flow_lowers_but_random_flow_remains_measured() {
    use sid_analyzer::analysis::sid_program::evidence::{
        ConfidencePermille, Evidence, EvidenceValidity, Provenance, SourceReference,
    };
    use sid_analyzer::analysis::sid_program::ids::CausalDependencyId;
    use sid_analyzer::analysis::sid_program::semantic::{
        CausalConsumer, CausalDependency, CausalSource, CausalSupport, CausalTransform, RamAddress,
        RandomSource,
    };
    use sid_analyzer::trace::SidRegister;

    let mut program = program();
    let producer = program.capture.events[0].id;
    let evidence = vec![Evidence {
        provenance: Provenance::TraceMeasured,
        confidence: ConfidencePermille(900),
        source: SourceReference::Event(producer),
        validity: EvidenceValidity::Valid,
    }];
    let consumer = CausalConsumer {
        event: producer,
        register: SidRegister(0),
        transform: CausalTransform::Ordered6502Path,
    };
    program.semantic.causal_dependencies = vec![
        CausalDependency {
            id: CausalDependencyId(0),
            source: CausalSource::TableLookup(RamAddress(0x4000)),
            producer,
            producer_value: 0,
            consumers: vec![consumer],
            support: CausalSupport::ProducerTransformConsumer,
            evidence: evidence.clone(),
        },
        CausalDependency {
            id: CausalDependencyId(1),
            source: CausalSource::Random(RandomSource::DriverCell(RamAddress(0x0040))),
            producer,
            producer_value: 0,
            consumers: vec![consumer],
            support: CausalSupport::ProducerTransformConsumer,
            evidence,
        },
    ];
    let plan = compile(
        &program,
        &TargetCapabilities::from_pinned_mirrors().unwrap(),
        StateBudget::default(),
    );
    let direct = plan
        .candidates
        .iter()
        .find(|candidate| candidate.class == RepresentationClass::DirectCv)
        .unwrap();
    assert!(matches!(
        direct.recipe,
        CandidateRecipe::DirectCv {
            dependency_count
        } if dependency_count.0 == 1
    ));
    let fallback = plan
        .candidates
        .iter()
        .find(|candidate| candidate.class == RepresentationClass::DenseAutomation)
        .unwrap();
    assert!(matches!(
        &fallback.recipe,
        CandidateRecipe::Automation { fallbacks, .. }
            if fallbacks.contains(&FallbackReason::RandomModulation)
    ));
}
