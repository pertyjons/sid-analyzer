use sid_analyzer::analysis::sid_program::AnalyzedSidProgram;
use sid_analyzer::analysis::sid_program::topology::TopologyEdgeKind;
use sid_analyzer::emu::PlaybackTiming;
use sid_analyzer::header::{self, SubtuneIndex};

fn nemesis(frames: u32) -> AnalyzedSidProgram {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../assets/music/Nemesis_the_Warlock.sid");
    let bytes = std::fs::read(path).unwrap();
    let header = header::parse(&bytes).unwrap();
    let subtune = SubtuneIndex(1);
    let timing = PlaybackTiming::for_subtune(&header, subtune);
    let trace =
        sid_analyzer::emu::run_with_timing(&header, &bytes, subtune, frames, timing).unwrap();
    let timing = timing.resolved_from_trace(&trace);
    AnalyzedSidProgram::from_trace(&header, subtune, timing, &trace)
}

#[test]
fn physical_program_projects_frames_and_filter_exactly() {
    let program = nemesis(30);
    let report = program.validate().unwrap();
    assert_eq!(report.frames, 30);
    for checkpoint in &program.capture.checkpoints {
        assert_eq!(
            program.register_file_at(checkpoint.cycle).0,
            checkpoint.digital_sid.registers
        );
    }

    let projected = program.project_frames();
    assert_eq!(
        program.project_filter_program(),
        sid_analyzer::analysis::programs::build_filter_program(&projected)
    );
}

#[test]
fn lossless_voice_signals_retain_repeated_equal_writes() {
    let program = nemesis(80);
    for voice_index in 0..3 {
        let first_register = voice_index as u8 * 7;
        let captured = program
            .capture
            .events
            .iter()
            .filter(|event| {
                event.access == sid_analyzer::emu::capture::SidBusAccess::Write
                    && event.register == Some(sid_analyzer::trace::SidRegister(first_register + 4))
            })
            .count();
        assert_eq!(program.voices[voice_index].control.0.len(), captured);
    }
    let mut seen = std::collections::BTreeSet::new();
    assert!(program.capture.events.iter().any(|event| {
        event.access == sid_analyzer::emu::capture::SidBusAccess::Write
            && !seen.insert((event.register, event.value))
    }));
}

#[test]
fn topology_keeps_live_cross_voice_edges_and_chip_filter_singleton() {
    let program = nemesis(300);
    assert!(
        program
            .chip
            .topology
            .edges
            .iter()
            .any(|edge| matches!(edge.kind, TopologyEdgeKind::Sync | TopologyEdgeKind::Ring))
    );
    assert_eq!(
        program
            .chip
            .topology
            .nodes
            .iter()
            .filter(|node| {
                **node == sid_analyzer::analysis::sid_program::topology::TopologyNodeId::Filter
            })
            .count(),
        1
    );
}

#[test]
fn debug_json_is_deterministic() {
    let program = nemesis(20);
    let mut first = Vec::new();
    let mut second = Vec::new();
    program.write_debug_json(&mut first, true).unwrap();
    program.write_debug_json(&mut second, true).unwrap();
    assert_eq!(first, second);
    let decoded = AnalyzedSidProgram::read_debug_json(&mut first.as_slice()).unwrap();
    assert_eq!(
        decoded.schema_version,
        sid_analyzer::analysis::sid_program::PROGRAM_DEBUG_SCHEMA_VERSION
    );

    let mut value: serde_json::Value = serde_json::from_slice(&first).unwrap();
    value
        .as_object_mut()
        .unwrap()
        .insert("future_field".to_owned(), serde_json::Value::Bool(true));
    let invalid = serde_json::to_vec(&value).unwrap();
    assert!(AnalyzedSidProgram::read_debug_json(&mut invalid.as_slice()).is_err());
}

#[test]
fn interpretations_and_occurrences_preserve_exact_source_lineage() {
    let program = nemesis(120);
    assert!(!program.semantic.interpretations.is_empty());
    for interpretation in &program.semantic.interpretations {
        assert!(
            interpretation
                .steps
                .windows(2)
                .all(|steps| steps[0].at <= steps[1].at)
        );
        assert_eq!(interpretation.initial_position.0, 0);
    }
    for occurrence in &program.semantic.occurrences {
        if let Some(next) = occurrence.continuation {
            let next = program
                .semantic
                .occurrences
                .iter()
                .find(|candidate| candidate.id == next)
                .unwrap();
            assert_eq!(next.previous, Some(occurrence.id));
            assert_eq!(next.voice, occurrence.voice);
        }
        assert_eq!(occurrence.local_automation.len(), 4);
        assert!(occurrence.initial.oscillator.is_some());
        assert!(occurrence.initial.envelope.is_some());
    }
}

#[test]
fn continuous_program_covers_non_parked_regions_and_keeps_reuse_voice_local() {
    use sid_analyzer::analysis::sid_program::region::SoundRegionKind;
    use sid_analyzer::analysis::sid_program::semantic::ReusePolicy;

    let program = nemesis(240);
    let continuous = &program.semantic.continuous;
    let expected = program
        .semantic
        .regions
        .iter()
        .filter(|region| region.kind != SoundRegionKind::SilentParked)
        .count();
    assert_eq!(continuous.occurrences.len(), expected);
    assert_eq!(
        continuous.complexity.occurrences.0,
        continuous.occurrences.len() as u64
    );
    assert!(continuous.complexity.serialized_size.0 > 0);
    for group in &continuous.reuse_groups {
        for occurrence in &group.occurrences {
            let occurrence = &continuous.occurrences[occurrence.0 as usize];
            assert_eq!(occurrence.voice, group.voice);
            assert_eq!(occurrence.content_digest, group.content_digest);
        }
        if group.policy != ReusePolicy::Unique {
            assert!(group.occurrences.len() > 1);
        }
    }
    for occurrence in &continuous.occurrences {
        if let Some(next) = occurrence.continuation {
            let next = &continuous.occurrences[next.0 as usize];
            assert_eq!(next.previous, Some(occurrence.id));
            assert_eq!(next.voice, occurrence.voice);
            assert!(occurrence.span.end <= next.span.start);
        }
    }
}

#[test]
fn continuous_initial_state_matches_the_exact_region_start() {
    let program = nemesis(240);
    let occurrence = program
        .semantic
        .continuous
        .occurrences
        .iter()
        .find(|occurrence| {
            occurrence.voice.is_some()
                && program
                    .capture
                    .checkpoints
                    .iter()
                    .rfind(|checkpoint| checkpoint.cycle <= occurrence.span.start)
                    .is_some_and(|checkpoint| checkpoint.cycle < occurrence.span.start)
        })
        .expect("fixture has a region starting inside a call");
    let exact = program.state_at(occurrence.span.start);
    assert_eq!(occurrence.initial.digital_sid, exact.digital_sid);
}

#[test]
fn continuous_lineage_uses_only_the_non_overlapping_physical_layer() {
    use sid_analyzer::analysis::sid_program::region::SoundRegionKind;

    let program = nemesis(240);
    let continuous = &program.semantic.continuous;
    let is_physical = |kind| {
        matches!(
            kind,
            SoundRegionKind::TonalAttack
                | SoundRegionKind::Sustain
                | SoundRegionKind::ReleaseTail
                | SoundRegionKind::NoiseTransient
                | SoundRegionKind::SilentModulator
        )
    };
    for occurrence in &continuous.occurrences {
        if !is_physical(occurrence.kind) {
            assert_eq!(occurrence.previous, None);
            assert_eq!(occurrence.continuation, None);
        }
    }
    for voice in [
        sid_analyzer::analysis::VoiceId::V1,
        sid_analyzer::analysis::VoiceId::V2,
        sid_analyzer::analysis::VoiceId::V3,
    ] {
        let physical: Vec<_> = continuous
            .occurrences
            .iter()
            .filter(|occurrence| occurrence.voice == Some(voice) && is_physical(occurrence.kind))
            .collect();
        for pair in physical.windows(2) {
            assert!(pair[0].span.end <= pair[1].span.start);
            assert_eq!(pair[0].continuation, Some(pair[1].id));
            assert_eq!(pair[1].previous, Some(pair[0].id));
        }
    }
}

#[test]
fn program_validation_rejects_stale_continuous_state_and_overlay_links() {
    use sid_analyzer::analysis::sid_program::ProgramValidationError;
    use sid_analyzer::analysis::sid_program::region::SoundRegionKind;

    let mut stale = nemesis(120);
    stale.semantic.continuous.occurrences[0]
        .initial
        .digital_sid
        .registers[0] ^= 1;
    assert!(matches!(
        stale.validate(),
        Err(ProgramValidationError::ContinuousInitialStateMismatch(_))
    ));

    let mut linked_overlay = nemesis(120);
    let physical = linked_overlay
        .semantic
        .continuous
        .occurrences
        .iter()
        .find(|occurrence| {
            matches!(
                occurrence.kind,
                SoundRegionKind::TonalAttack
                    | SoundRegionKind::Sustain
                    | SoundRegionKind::ReleaseTail
                    | SoundRegionKind::NoiseTransient
                    | SoundRegionKind::SilentModulator
            )
        })
        .unwrap()
        .id;
    let overlay = linked_overlay
        .semantic
        .continuous
        .occurrences
        .iter_mut()
        .find(|occurrence| occurrence.kind == SoundRegionKind::WaveformTransient)
        .unwrap();
    overlay.previous = Some(physical);
    assert!(matches!(
        linked_overlay.validate(),
        Err(ProgramValidationError::InvalidContinuousLineage(_))
    ));
}
