//! Regression tests for the corpus-baseline harness (PLAN §1).
//!
//! Two properties matter and neither is visible from the binary's exit code:
//! the census summary must not conflate native decoding with native structure
//! recovery, and an unattended run must account for every input — including the
//! ones it cannot export — rather than silently emitting fewer rows.

use serde_json::Value;
use sid_analyzer::export::synth::{NativeCapability, write_synth_quiet};
use sid_analyzer::header::SubtuneIndex;
use std::path::{Path, PathBuf};
use std::process::Command;

mod common;
use common::SidPipeline;

const CORPUS: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../assets/music");

const MONTY: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../assets/music/Auf_Wiedersehen_Monty.sid"
);

const LAZY_JONES: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../assets/music/GoatTracker_V1_Lazy_Jones.sid"
);

/// A trace export never claims a native capability: it did not read the
/// driver's tables, so both `decoded` and `structured` would be false claims.
#[test]
fn trace_export_reports_no_native_capability() {
    let f = SidPipeline::run(MONTY, SubtuneIndex(1), 600);
    let program = f.analyzed_program();
    let mut buf = Vec::new();
    let census = write_synth_quiet(&program, &mut buf).expect("write_synth_quiet");
    let summary = census.summary();

    assert_eq!(summary.native, NativeCapability::None);
    assert!(summary.native_driver.is_none());
    assert!(summary.native_extractor.is_none());
    assert!(summary.notes_total > 0, "the export still carries notes");
}

fn baseline_binary() -> PathBuf {
    // The integration test binary lives in target/<profile>/deps.
    let mut path = std::env::current_exe().expect("test binary path");
    path.pop();
    if path.ends_with("deps") {
        path.pop();
    }
    path.join("sid-corpus-baseline")
}

/// Every input gets a row, including the ones the harness cannot export, and
/// native rejections carry a reason instead of silently becoming trace output.
#[test]
fn baseline_accounts_for_every_input() {
    let binary = baseline_binary();
    if !binary.is_file() {
        eprintln!("skipping: {} not built", binary.display());
        return;
    }
    let corpus = Path::new(CORPUS);
    let temp_dir = std::env::temp_dir();
    let out_dir = temp_dir.join(format!("sid-analyzer-baseline-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&out_dir);
    std::fs::create_dir_all(&out_dir).expect("create baseline test directory");
    let songlengths = out_dir.join("empty-Songlengths.md5");
    std::fs::write(&songlengths, []).expect("write empty Songlengths fixture");
    let report = out_dir.join("baseline.json");

    let status = Command::new(&binary)
        .args(["--corpus", CORPUS])
        .arg("--songlengths")
        .arg(&songlengths)
        .arg("--out-dir")
        .arg(&out_dir)
        .args(["--diagnostic-frames", "200", "--fallback-secs", "4"])
        // Schema validation needs python3 + jsonschema; the harness records the
        // skip reason itself, but this test is about accounting, not schemas.
        .arg("--no-schema")
        .current_dir(&temp_dir)
        .status()
        .expect("run sid-corpus-baseline");
    assert!(status.success(), "baseline exited with {status}");

    let text = std::fs::read_to_string(&report).expect("report written");
    let v: Value = serde_json::from_str(&text).expect("report parses");
    let rows = v["rows"].as_array().expect("rows array");

    let sids = std::fs::read_dir(corpus)
        .expect("corpus readable")
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().is_some_and(|x| x == "sid"))
        .count();
    assert_eq!(rows.len(), sids, "one row per .sid file");

    // Rows are sorted by name, so two runs compare cleanly.
    let names: Vec<&str> = rows.iter().filter_map(|r| r["name"].as_str()).collect();
    let mut sorted = names.clone();
    sorted.sort_unstable();
    assert_eq!(names, sorted, "rows are name-sorted");

    let mut rejections = 0;
    for row in rows {
        if row.get("skipped").is_some() {
            // A skipped input must say why, and must not carry export results.
            assert!(row["skipped"]["kind"].as_str().is_some());
            assert!(
                row["skipped"]["detail"]
                    .as_str()
                    .is_some_and(|detail| !detail.is_empty())
            );
            assert!(row.get("trace").is_none());
            assert!(row.get("native").is_none());
            continue;
        }
        for mode in ["trace", "native"] {
            let outcome = &row[mode];
            if outcome["outcome"] == "exported" {
                assert_eq!(
                    outcome["bytes"], outcome["summary"]["serialized_size"],
                    "{mode} report size must come from the reproducible census"
                );
            }
        }
        let native = &row["native"];
        match native["outcome"].as_str() {
            Some("exported") => {
                let cap = native["summary"]["native"].as_str().expect("capability");
                assert!(
                    cap == "decoded" || cap == "structured",
                    "a native export reports a native capability, got {cap}"
                );
            }
            Some("rejected") => {
                rejections += 1;
                let reason = native["reason"].as_str().unwrap_or_default();
                assert!(!reason.is_empty(), "a rejection carries a typed reason");
            }
            other => panic!("unexpected native outcome {other:?}"),
        }
    }
    assert!(
        rejections > 0,
        "the asset corpus still contains unsupported variants; if this fires, \
         the fixture set changed and the baseline should be re-pinned"
    );

    let _ = std::fs::remove_dir_all(&out_dir);
}

#[test]
fn explicit_songlengths_path_is_working_directory_independent() {
    let binary = baseline_binary();
    if !binary.is_file() {
        eprintln!("skipping: {} not built", binary.display());
        return;
    }

    let root = std::env::temp_dir().join(format!(
        "sid-analyzer-songlengths-test-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    let corpus = root.join("corpus");
    let out_dir = root.join("out");
    std::fs::create_dir_all(&corpus).expect("create Songlengths test corpus");
    std::fs::copy(LAZY_JONES, corpus.join("GoatTracker_V1_Lazy_Jones.sid"))
        .expect("copy short Songlengths fixture");
    let bytes = std::fs::read(LAZY_JONES).unwrap();
    let digest = sid_analyzer::songlengths::compute_sid_md5(&bytes);
    let hash: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    let database = root.join("Songlengths.md5");
    std::fs::write(&database, format!("{hash}=0:01\n")).unwrap();

    let output = Command::new(&binary)
        .arg("--corpus")
        .arg(&corpus)
        .arg("--out-dir")
        .arg(&out_dir)
        .arg("--no-schema")
        .arg("--songlengths")
        .arg(&database)
        .current_dir(std::env::temp_dir())
        .output()
        .expect("run sid-corpus-baseline outside the repository");
    assert!(
        output.status.success(),
        "baseline exited with {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );

    let report =
        std::fs::read_to_string(out_dir.join("baseline.json")).expect("read baseline report");
    let value: Value = serde_json::from_str(&report).expect("parse baseline report");
    let rows = value["rows"].as_array().expect("rows array");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["window"], "full_length");

    let _ = std::fs::remove_dir_all(&root);
}
