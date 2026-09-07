use sid_analyzer::header::Clock;
use sid_analyzer::songlengths::compute_sid_md5;
use std::process::Command;

mod common;

#[test]
fn local_duration_database_is_optional_explicit_and_overrides_environment() {
    let directory = std::env::temp_dir().join(format!(
        "sid-cli-metadata-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir(&directory).unwrap();
    let source = directory.join("synthetic.sid");
    let bytes = common::synthetic_filtered_saw_sid(Clock::Pal);
    std::fs::write(&source, &bytes).unwrap();
    let hash: String = compute_sid_md5(&bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let short = directory.join("short.md5");
    let long = directory.join("long.md5");
    std::fs::write(&short, format!("{hash}=0:01\n")).unwrap();
    std::fs::write(&long, format!("{hash}=0:02\n")).unwrap();
    let analyzer = || {
        let mut command = Command::new(env!("CARGO_BIN_EXE_sid-analyzer"));
        command
            .current_dir(&directory)
            .env_remove("HVSC_SONGLENGTHS")
            .env_remove("HVSC_STIL")
            .arg(&source);
        command
    };

    assert!(analyzer().output().unwrap().status.success());
    let no_duration = analyzer().args(["--format", "json"]).output().unwrap();
    assert!(!no_duration.status.success());
    assert!(no_duration.stdout.is_empty());
    assert!(String::from_utf8_lossy(&no_duration.stderr).contains("no duration available"));

    let fixed = analyzer()
        .args(["--frames", "2", "--format", "json"])
        .output()
        .unwrap();
    assert!(fixed.status.success());
    let fixed: serde_json::Value = serde_json::from_slice(&fixed.stdout).unwrap();
    assert_eq!(fixed["frame_count"], 2);
    assert!(fixed.get("subtune_lengths_secs").is_none());

    for (explicit, expected_frames) in [(false, 51), (true, 101)] {
        let mut command = analyzer();
        command
            .env("HVSC_SONGLENGTHS", &short)
            .args(["--format", "json"]);
        if explicit {
            command.arg("--songlengths").arg(&long);
        }
        let output = command.output().unwrap();
        assert!(output.status.success(), "{:?}", output.stderr);
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["frame_count"], expected_frames);
    }

    let census = Command::new(env!("CARGO_BIN_EXE_sid-composer-census"))
        .env_remove("HVSC_SONGLENGTHS")
        .arg("--corpus")
        .arg(&directory)
        .args(["--subject", "Synthetic"])
        .arg("--full-length")
        .output()
        .unwrap();
    assert!(!census.status.success());
    assert!(
        String::from_utf8_lossy(&census.stderr).contains("HVSC_SONGLENGTHS"),
        "{}",
        String::from_utf8_lossy(&census.stderr)
    );

    let baseline = Command::new(env!("CARGO_BIN_EXE_sid-corpus-baseline"))
        .env_remove("HVSC_SONGLENGTHS")
        .arg("--corpus")
        .arg(&directory)
        .arg("--out-dir")
        .arg(directory.join("baseline"))
        .args(["--fallback-secs", "1", "--no-schema"])
        .output()
        .unwrap();
    assert!(baseline.status.success(), "{:?}", baseline.stderr);
    assert!(!String::from_utf8_lossy(&baseline.stderr).contains("songlengths "));
    std::fs::remove_dir_all(directory).unwrap();
}
