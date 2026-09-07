use serde_json::Value;
use sid_analyzer::export::synth::{
    EnhancementAmount, SynthOptions, SynthStyle, write_synth_with_options,
};
use sid_analyzer::header::{Clock, SubtuneIndex};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output, Stdio};

mod common;

fn validate(project: &Value) -> Output {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let python = std::env::var_os("SID_SCHEMA_PYTHON").unwrap_or_else(|| "python3".into());
    let mut child = Command::new(python)
        .arg(root.join("tools/schema-validation/validate.py"))
        .arg(root.join("docs/pertylizer/project.schema.json"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("start Python schema validator; see CONTRIBUTING.md for setup");
    let bytes = serde_json::to_vec(project).unwrap();
    let written = child.stdin.take().unwrap().write_all(&bytes);
    let output = child.wait_with_output().unwrap();
    assert!(
        written.is_ok(),
        "validator input failed: {written:?}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

#[test]
fn synthetic_exports_conform_to_the_full_project_schema() {
    for clock in [Clock::Pal, Clock::Ntsc] {
        let bytes = common::synthetic_filtered_saw_sid(clock);
        let fixture = common::SidPipeline::from_bytes(&bytes, SubtuneIndex(1), 120);
        let program = fixture.analyzed_program();
        let variants = [
            SynthOptions::default(),
            SynthOptions {
                style: SynthStyle::ModernAnalog,
                ..SynthOptions::default()
            },
            SynthOptions {
                enhancement: Some(EnhancementAmount::new(1).unwrap()),
                ..SynthOptions::default()
            },
            SynthOptions {
                enhancement: Some(EnhancementAmount::new(5).unwrap()),
                ..SynthOptions::default()
            },
            SynthOptions {
                enhancement: Some(EnhancementAmount::new(10).unwrap()),
                ..SynthOptions::default()
            },
        ];
        for options in variants {
            let mut bytes = Vec::new();
            write_synth_with_options(&program, &mut bytes, options).unwrap();
            let project: Value = serde_json::from_slice(&bytes).unwrap();
            assert!(!project["instruments"].as_array().unwrap().is_empty());
            let output = validate(&project);
            assert!(
                output.status.success(),
                "{clock:?}, {options:?}:\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
}

#[test]
fn schema_validator_rejects_invalid_project_structure() {
    let bytes = common::synthetic_filtered_saw_sid(Clock::Pal);
    let fixture = common::SidPipeline::from_bytes(&bytes, SubtuneIndex(1), 120);
    let mut bytes = Vec::new();
    write_synth_with_options(
        &fixture.analyzed_program(),
        &mut bytes,
        SynthOptions::default(),
    )
    .unwrap();
    let mut project: Value = serde_json::from_slice(&bytes).unwrap();
    project["instruments"] = Value::String("invalid instruments".into());
    let output = validate(&project);
    assert_eq!(output.status.code(), Some(1));
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(
        error.contains("instruments") && error.contains("is not of type"),
        "{error}"
    );
}
