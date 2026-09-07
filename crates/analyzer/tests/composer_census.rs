use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;

fn corpus() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/music")
}

#[test]
fn census_reports_named_traits_and_native_validation_deterministically() {
    let output = std::env::temp_dir().join(format!(
        "sid-analyzer-composer-census-{}-first.json",
        std::process::id()
    ));
    let repeated = std::env::temp_dir().join(format!(
        "sid-analyzer-composer-census-{}-second.json",
        std::process::id()
    ));
    for path in [&output, &repeated] {
        let status = Command::new(env!("CARGO_BIN_EXE_sid-composer-census"))
            .args([
                "--corpus",
                corpus().to_str().unwrap(),
                "--subject",
                "Rob Hubbard",
                "--driver-filter",
                "Rob_Hubbard",
                "--frames",
                "400",
                "--workers",
                "1",
                "--limit",
                "1",
                "--output",
                path.to_str().unwrap(),
            ])
            .status()
            .unwrap();
        assert!(status.success());
    }

    let bytes = std::fs::read(&output).unwrap();
    assert_eq!(bytes, std::fs::read(&repeated).unwrap());
    let report: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(report["schema_version"], 3);
    assert_eq!(report["subject"], "Rob Hubbard");
    assert_eq!(report["driver_filter"], "Rob_Hubbard");
    assert_eq!(report["summary"]["selected_files"], 1);
    assert_eq!(report["summary"]["analyzed_subtunes"], 1);
    assert_eq!(report["summary"]["analysis_windows"]["fixed_calls"], 1);
    assert_eq!(report["summary"]["inexact_timing_subtunes"], 0);
    assert_eq!(report["summary"]["native"]["accepted_subtunes"], 1);
    assert_eq!(report["results"][0]["path"], "Auf_Wiedersehen_Monty.sid");
    assert_eq!(
        report["results"][0]["analysis_window"]["timing_exact"],
        true
    );
    assert!(
        report["summary"]["traits"]["effects"]["pwm"]["frames"]
            .as_u64()
            .unwrap()
            > 0
    );

    std::fs::remove_file(output).unwrap();
    std::fs::remove_file(repeated).unwrap();
}
