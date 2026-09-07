use sid_analyzer::audio::abtest::{
    CandidateExpectation, RenderFixtureProvenance, run_fixture_matrix,
};
use sid_analyzer::header::{self, Clock, SidModel, SubtuneIndex};
use std::path::{Path, PathBuf};

mod common;
use common::synthetic_filtered_saw_sid;

fn manifest() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/render_reference/v1/manifest.json")
}

#[test]
fn offline_render_matrix_is_deterministic_and_enforces_expected_candidates() {
    let first = run_fixture_matrix(&manifest(), None).unwrap();
    let second = run_fixture_matrix(&manifest(), None).unwrap();
    assert_eq!(
        serde_json::to_vec(&first).unwrap(),
        serde_json::to_vec(&second).unwrap()
    );
    assert!(first.expectations_met);
    assert_eq!(first.fixtures.len(), 8);
    for fixture in &first.fixtures {
        assert!(!fixture.candidates.is_empty());
        assert!(
            fixture
                .candidates
                .iter()
                .all(|candidate| candidate.expectation_met)
        );
    }
    let candidates = first
        .fixtures
        .iter()
        .flat_map(|fixture| &fixture.candidates);
    assert!(candidates.clone().any(|candidate| {
        candidate.accepted && candidate.expected == CandidateExpectation::Accept
    }));
    assert!(candidates.clone().any(|candidate| {
        !candidate.accepted && candidate.expected == CandidateExpectation::Reject
    }));
    assert!(first.fixtures.iter().all(|fixture| matches!(
        fixture.provenance,
        RenderFixtureProvenance::GeneratedProfile { .. }
    )));

    for fixture in first
        .fixtures
        .iter()
        .filter(|fixture| fixture.name.0.starts_with("c61_"))
    {
        let RenderFixtureProvenance::GeneratedProfile { recipe } = &fixture.provenance else {
            panic!("c61 fixtures must carry synthetic provenance");
        };
        assert!(recipe.contains("synthetic 6581 pulse+saw"));
    }

    let triangle_saw = first
        .fixtures
        .iter()
        .filter(|fixture| fixture.name.0.starts_with("c31_"));
    assert_eq!(triangle_saw.clone().count(), 3);
    for fixture in triangle_saw {
        let calibration_target = fixture
            .candidates
            .iter()
            .find(|candidate| candidate.name.0 == "calibration_target")
            .unwrap();
        assert!(calibration_target.accepted);

        let pulse_width_controls: Vec<_> = fixture
            .candidates
            .iter()
            .filter(|candidate| candidate.name.0.starts_with("pw"))
            .collect();
        assert_eq!(pulse_width_controls.len(), 6);
        assert!(pulse_width_controls.iter().all(|candidate| {
            !candidate.accepted && candidate.expected == CandidateExpectation::Reject
        }));
        let candidate_digests: std::collections::BTreeSet<_> = pulse_width_controls
            .iter()
            .map(|candidate| candidate.candidate_digest)
            .collect();
        assert_eq!(candidate_digests.len(), 1);

        let comparison = &pulse_width_controls[0].comparison;
        assert!(comparison.level_error_db.unwrap().0 > 19.0);
        assert!(comparison.gain_normalized_log_spectral_distance.unwrap().0 > 3.0);
    }
}

#[test]
fn synthetic_ntsc_fixture_is_vblank_and_executes_deterministically() {
    let bytes = synthetic_filtered_saw_sid(Clock::Ntsc);
    let parsed = header::parse(&bytes).unwrap();
    let trace = sid_analyzer::emu::run(&parsed, &bytes, SubtuneIndex(1), 2).unwrap();

    assert_eq!(parsed.flags.clock, Clock::Ntsc);
    assert_eq!(parsed.flags.sid_model, SidModel::Mos6581);
    assert!(!parsed.is_cia_timed(SubtuneIndex(1)));
    assert_eq!(trace.init_writes.len(), 11);
    assert_eq!(trace.frames.len(), 2);
}
