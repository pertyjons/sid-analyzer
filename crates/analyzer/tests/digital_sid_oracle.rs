mod common;

use common::oracle::{
    OracleDisposition, OracleDivergenceDefinition, OracleError, OracleFieldPath,
    OracleObservationId, OracleOperation, audit_document, load_document, load_manifest,
    verify_document, verify_documents,
};
use serde_json::json;
use sid_analyzer::emu::capture::SidBusAccess;
use sid_analyzer::header::{self, SubtuneIndex};
use sid_analyzer::trace::SidRegister;
use std::path::{Path, PathBuf};

fn fixtures() -> PathBuf {
    std::env::var_os("SID_ORACLE_FIXTURE_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/digital_sid_oracle/v1")
        })
}

const GENERATED_FIXTURES: [&str; 7] = [
    "captured_reads.oracle.json",
    "envelope.oracle.json",
    "oscillator.oracle.json",
    "seed.oracle.json",
    "sync.oracle.json",
    "test_noise_6581.oracle.json",
    "test_noise_8580.oracle.json",
];

fn live_read_psid() -> Vec<u8> {
    const HEADER_LEN: usize = 0x7C;
    let mut bytes = vec![0_u8; HEADER_LEN];
    bytes[0..4].copy_from_slice(b"PSID");
    bytes[4..6].copy_from_slice(&2_u16.to_be_bytes());
    bytes[6..8].copy_from_slice(&(HEADER_LEN as u16).to_be_bytes());
    bytes[8..10].copy_from_slice(&0x1000_u16.to_be_bytes());
    bytes[10..12].copy_from_slice(&0x1000_u16.to_be_bytes());
    bytes[12..14].copy_from_slice(&0x101A_u16.to_be_bytes());
    bytes[14..16].copy_from_slice(&1_u16.to_be_bytes());
    bytes[16..18].copy_from_slice(&1_u16.to_be_bytes());
    let flags = (0b01_u16 << 2) | (0b10_u16 << 4);
    bytes[0x76..0x78].copy_from_slice(&flags.to_be_bytes());
    bytes.extend_from_slice(&[
        0xA9, 0xFF, 0x8D, 0x0E, 0xD4, // V3 frequency low
        0xA9, 0x20, 0x8D, 0x0F, 0xD4, // V3 frequency high
        0xA9, 0x00, 0x8D, 0x13, 0xD4, // V3 attack/decay
        0xA9, 0xF0, 0x8D, 0x14, 0xD4, // V3 sustain/release
        0xA9, 0x21, 0x8D, 0x12, 0xD4, // V3 saw + gate
        0x60, // RTS
        0xAD, 0x1B, 0xD4, // LDA OSC3
        0xAD, 0x1C, 0xD4, // LDA ENV3
        0x60, // RTS
    ]);
    bytes
}

#[test]
fn clock_then_write_smoke_vector_matches() {
    let root = fixtures();
    let document = load_document(&root.join("smoke.oracle.json")).unwrap();
    let manifest = load_manifest(&root.join("manifest.json")).unwrap();
    let report = verify_document(&document, &manifest).unwrap();
    assert_eq!(report.fields_checked, 15);
    assert!(report.known_divergences.is_empty());
}

#[test]
fn generated_oracle_vectors_match_the_reviewed_policy() {
    let root = fixtures();
    let manifest = load_manifest(&root.join("manifest.json")).unwrap();
    let mut documents = vec![load_document(&root.join("smoke.oracle.json")).unwrap()];
    for name in GENERATED_FIXTURES {
        documents.push(load_document(&root.join(name)).unwrap());
    }
    let report = verify_documents(&documents, &manifest).unwrap();
    assert!(report.fields_checked > 0);
    assert!(report.known_divergences.is_empty());
}

#[test]
fn test_fill_vectors_straddle_the_reported_countdown_boundary() {
    for name in ["test_noise_6581.oracle.json", "test_noise_8580.oracle.json"] {
        let document = load_document(&fixtures().join(name)).unwrap();
        let case = document
            .cases
            .iter()
            .find(|candidate| candidate.id.0.starts_with("test_noise_"))
            .unwrap();
        let rewritten = case
            .observations
            .iter()
            .find(|observation| observation.id.0 == "test_rewritten")
            .unwrap();
        let remaining = rewritten.values[&OracleFieldPath("voices.3.test_fill_at".to_owned())]
            .as_u64()
            .unwrap();
        let boundary = rewritten.cycle.0 + remaining;
        let before = case
            .observations
            .iter()
            .find(|observation| observation.id.0 == "before_fill")
            .unwrap();
        let at = case
            .observations
            .iter()
            .find(|observation| observation.id.0 == "fill_boundary")
            .unwrap();
        let after = case
            .observations
            .iter()
            .find(|observation| observation.id.0 == "after_fill")
            .unwrap();

        assert_eq!(before.cycle.0, boundary - 1);
        assert_eq!(at.cycle.0, boundary);
        assert_eq!(after.cycle.0, boundary + 1);
        assert_ne!(
            before.values[&OracleFieldPath("voices.3.shift_register".to_owned())],
            at.values[&OracleFieldPath("voices.3.shift_register".to_owned())]
        );
    }
}

#[test]
fn sync_noise_order_has_a_comparable_lfsr_clock_assertion() {
    let root = fixtures();
    let document = load_document(&root.join("sync.oracle.json")).unwrap();
    let manifest = load_manifest(&root.join("manifest.json")).unwrap();
    let case = document
        .cases
        .iter()
        .find(|candidate| candidate.id.0 == "sync_noise_same_cycle")
        .unwrap();
    let field = OracleFieldPath("voices.2.noise_clock_count".to_owned());
    let counts: Vec<_> = case
        .observations
        .iter()
        .map(|observation| observation.values[&field].as_u64().unwrap())
        .collect();
    let policy = manifest
        .rules
        .iter()
        .find(|rule| rule.case == case.id && rule.observation.is_none() && rule.field == field)
        .unwrap();

    assert_eq!(counts, [1, 2, 2]);
    assert!(matches!(policy.disposition, OracleDisposition::MustMatch));
    let _ = verify_document(&document, &manifest).unwrap();
}

#[test]
fn attack_to_release_case_interrupts_attack_before_decay() {
    let root = fixtures();
    let document = load_document(&root.join("envelope.oracle.json")).unwrap();
    let manifest = load_manifest(&root.join("manifest.json")).unwrap();
    let case = document
        .cases
        .iter()
        .find(|candidate| candidate.id.0 == "envelope_attack_to_release")
        .unwrap();
    let phase = OracleFieldPath("voices.3.envelope.phase".to_owned());
    let gate = OracleFieldPath("voices.3.envelope.gate".to_owned());
    let level = OracleFieldPath("voices.3.envelope.level".to_owned());
    let observation = |id: &str| {
        case.observations
            .iter()
            .find(|candidate| candidate.id.0 == id)
            .unwrap()
    };

    assert_eq!(
        observation("attack_before_gate_off").values[&phase],
        "attack"
    );
    assert_eq!(observation("attack_before_gate_off").values[&gate], true);
    assert_eq!(
        observation("gate_off_during_attack").values[&phase],
        "attack"
    );
    assert_eq!(observation("gate_off_during_attack").values[&gate], false);
    assert_eq!(observation("release_started").values[&phase], "release");
    assert_eq!(observation("release_zero").values[&level], 0);
    let _ = verify_document(&document, &manifest).unwrap();
}

#[test]
fn captured_read_fixture_matches_live_read_psid_capture() {
    let bytes = live_read_psid();
    let header = header::parse(&bytes).unwrap();
    let trace = sid_analyzer::emu::run(&header, &bytes, SubtuneIndex(1), 2).unwrap();
    let document = load_document(&fixtures().join("captured_reads.oracle.json")).unwrap();
    let case = document.cases.first().unwrap();
    let mismatches = audit_document(&document).unwrap();

    assert_eq!(case.operations.len(), trace.capture.events.len());
    for (operation, event) in case.operations.iter().zip(&trace.capture.events) {
        match operation {
            OracleOperation::Write {
                cycle,
                register,
                value,
                ..
            } => {
                assert_eq!(event.access, SidBusAccess::Write);
                assert_eq!(cycle, &event.cycle);
                assert_eq!(Some(*register), event.register);
                assert_eq!(value.0, event.value);
            }
            OracleOperation::Observe {
                cycle, observation, ..
            } => {
                assert_eq!(event.access, SidBusAccess::Read);
                assert_eq!(cycle, &event.cycle);
                let (suffix, field) = match event.register {
                    Some(SidRegister(0x1B)) => {
                        ("osc3_read", OracleFieldPath("public.osc3".to_owned()))
                    }
                    Some(SidRegister(0x1C)) => {
                        ("env3_read", OracleFieldPath("public.env3".to_owned()))
                    }
                    register => panic!("unexpected captured read register {register:?}"),
                };
                assert!(observation.0.ends_with(suffix));
                let external = case
                    .observations
                    .iter()
                    .find(|candidate| candidate.id == *observation)
                    .unwrap();
                let replayed = mismatches
                    .iter()
                    .find(|mismatch| {
                        mismatch.observation == *observation && mismatch.field == field
                    })
                    .map_or_else(
                        || external.values.get(&field).unwrap(),
                        |mismatch| &mismatch.actual,
                    );
                assert_eq!(replayed, &json!(event.value));
            }
        }
    }
}

#[test]
#[ignore = "prints raw oracle differences for manifest maintenance"]
fn print_raw_oracle_differences() {
    let root = fixtures();
    for name in std::iter::once("smoke.oracle.json").chain(GENERATED_FIXTURES) {
        let document = load_document(&root.join(name)).unwrap();
        for mismatch in audit_document(&document).unwrap() {
            println!(
                "{}|{}|{}|{}|{}|{}",
                mismatch.case.0,
                mismatch.observation.0,
                mismatch.cycle.0,
                mismatch.field.0,
                mismatch.expected,
                mismatch.actual
            );
        }
    }
}

#[test]
fn unknown_json_fields_are_rejected() {
    let root = fixtures();
    let mut value: serde_json::Value =
        serde_json::from_slice(&std::fs::read(root.join("smoke.oracle.json")).unwrap()).unwrap();
    value
        .as_object_mut()
        .unwrap()
        .insert("future_field".to_owned(), json!(true));
    assert!(serde_json::from_value::<common::oracle::SidOracleDocument>(value).is_err());
}

#[test]
fn missing_policy_is_rejected() {
    let root = fixtures();
    let document = load_document(&root.join("smoke.oracle.json")).unwrap();
    let mut manifest = load_manifest(&root.join("manifest.json")).unwrap();
    manifest
        .rules
        .retain(|rule| rule.field != OracleFieldPath("voices.3.envelope.rate_counter".to_owned()));
    assert!(matches!(
        verify_document(&document, &manifest),
        Err(OracleError::MissingPolicy { .. })
    ));
}

#[test]
fn stale_policy_selector_is_rejected_across_the_fixture_set() {
    let root = fixtures();
    let mut documents = vec![load_document(&root.join("smoke.oracle.json")).unwrap()];
    for name in GENERATED_FIXTURES {
        documents.push(load_document(&root.join(name)).unwrap());
    }
    let mut manifest = load_manifest(&root.join("manifest.json")).unwrap();
    let mut stale = manifest
        .rules
        .iter()
        .find(|rule| rule.case.0 == "reset_mos6581" && rule.observation.is_none())
        .unwrap()
        .clone();
    stale.observation = Some(OracleObservationId("removed_observation".to_owned()));
    stale.disposition = OracleDisposition::MustMatch;
    manifest.rules.push(stale);
    assert!(matches!(
        verify_documents(&documents, &manifest),
        Err(OracleError::UnusedPolicy { .. })
    ));
}

#[test]
fn duplicate_sequences_and_backwards_time_are_rejected() {
    let root = fixtures();
    let mut duplicate = load_document(&root.join("smoke.oracle.json")).unwrap();
    duplicate.cases[0].operations[1] = duplicate.cases[0].operations[0].clone();
    assert!(matches!(
        verify_document(
            &duplicate,
            &load_manifest(&root.join("manifest.json")).unwrap()
        ),
        Err(OracleError::DuplicateSequence { .. })
    ));

    let mut backwards = load_document(&root.join("smoke.oracle.json")).unwrap();
    if let common::oracle::OracleOperation::Observe { cycle, .. } =
        &mut backwards.cases[0].operations[4]
    {
        cycle.0 = 1;
    }
    assert!(matches!(
        verify_document(
            &backwards,
            &load_manifest(&root.join("manifest.json")).unwrap()
        ),
        Err(OracleError::CycleMovedBackwards { .. })
    ));
}

#[test]
fn broad_known_divergence_is_rejected() {
    let root = fixtures();
    let document = load_document(&root.join("smoke.oracle.json")).unwrap();
    let mut manifest = load_manifest(&root.join("manifest.json")).unwrap();
    manifest.rules[0].observation = None;
    manifest.rules[0].disposition = OracleDisposition::KnownDivergence {
        issue_id: "oracle-test".to_owned(),
    };
    manifest.divergences.insert(
        "oracle-test".to_owned(),
        OracleDivergenceDefinition {
            reason: "negative test".to_owned(),
            project_policy: "none".to_owned(),
            planned_resolution: "remove the test rule".to_owned(),
        },
    );
    assert!(matches!(
        verify_document(&document, &manifest),
        Err(OracleError::BroadKnownDivergence { .. })
    ));
}

#[test]
fn known_divergence_must_remain_narrow_and_present() {
    let root = fixtures();
    let document = load_document(&root.join("smoke.oracle.json")).unwrap();
    let mut manifest = load_manifest(&root.join("manifest.json")).unwrap();
    let rule = manifest
        .rules
        .iter_mut()
        .find(|rule| rule.field == OracleFieldPath("public.env3".to_owned()))
        .unwrap();
    rule.observation = Some(OracleObservationId("after_gate".to_owned()));
    rule.disposition = OracleDisposition::KnownDivergence {
        issue_id: "oracle-test".to_owned(),
    };
    manifest.divergences.insert(
        "oracle-test".to_owned(),
        OracleDivergenceDefinition {
            reason: "negative test".to_owned(),
            project_policy: "none".to_owned(),
            planned_resolution: "remove the test rule".to_owned(),
        },
    );
    assert!(matches!(
        verify_document(&document, &manifest),
        Err(OracleError::DivergenceDisappeared { .. })
    ));
}
