//! Integration coverage for the SID-native arpeggiator processor. A clean arp
//! plan exports one held base note per arp event plus a pooled Note Graph holding
//! an `Arpeggiator` processor (mode `Custom`) **by default**; the explicit
//! `--unstable-no-arp-processor` diagnostic option opts back into the per-frame
//! bake.

use serde_json::Value;
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_sid-analyzer");

/// Commando has several clean arp plans (census: ~25 converted blocks), so it is
/// the canonical fixture for the processor path.
const COMMANDO: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../assets/music/Commando.sid"
);

/// Run a `synth-native` export of `COMMANDO` to a temp file and return the
/// parsed project JSON. `bake` selects the explicit diagnostic opt-out; the
/// default (false) uses the native processor.
fn export(bake: bool, tag: &str) -> Value {
    let out = std::env::temp_dir().join(format!("sid_arp_test_{tag}.ptz"));
    let mut cmd = Command::new(BIN);
    cmd.args(["--format", "synth-native", "--frames", "6000", "--output"])
        .arg(&out)
        .arg(COMMANDO);
    if bake {
        cmd.arg("--unstable-no-arp-processor");
    }
    let status = cmd.status().expect("run sid-analyzer");
    assert!(status.success(), "export failed");
    let bytes = std::fs::read(&out).expect("read export");
    let _ = std::fs::remove_file(&out);
    serde_json::from_slice(&bytes).expect("parse project JSON")
}

fn patterns(project: &Value) -> &Vec<Value> {
    project["song"]["patterns"]
        .as_array()
        .or_else(|| project["patterns"].as_array())
        .expect("patterns array")
}

/// Every emitted arp processor must be the exact SID-native recipe.
fn assert_recipe(arp: &Value) {
    assert_eq!(arp["mode"], "Custom", "mode Custom");
    assert_eq!(arp["rate"]["MilliHz"], 50_125, "PAL raster-call rate");
    assert_eq!(arp["octaves"], 1);
    assert_eq!(arp["legato"], true);
    assert_eq!(arp["gate"], 1.0);
    assert_eq!(arp["velocity"], "AsPlayed");
    assert!(
        arp["custom"].as_array().is_some_and(|c| !c.is_empty()),
        "non-empty offset table"
    );
}

#[test]
fn default_emits_pooled_arpeggiator_graphs() {
    let project = export(false, "default");
    let mut found = 0;
    for graph in project["song"]["note_graphs"]
        .as_array()
        .expect("note graph pool")
    {
        let arp = &graph["nodes"]["0"]["Processor"]["Arpeggiator"];
        assert!(!arp.is_null(), "only Arpeggiator graphs are emitted");
        assert_recipe(arp);
        found += 1;
    }
    assert!(
        found > 0,
        "Commando exports at least one converted arp plan by default"
    );
    assert!(
        patterns(&project)
            .iter()
            .any(|pattern| pattern.get("note_graph").is_some()),
        "at least one pattern binds a pooled arp graph"
    );
}

#[test]
fn opt_out_emits_no_processors() {
    let project = export(true, "optout");
    assert_eq!(
        project["song"]["note_graphs"].as_array().map(Vec::len),
        Some(0)
    );
    for pat in patterns(&project) {
        // `processors` is skipped when empty, so the field must be absent.
        assert!(
            pat.get("processors").is_none(),
            "SID_NO_ARP_PROCESSOR export carries no note processors (pattern {})",
            pat["name"]
        );
    }
}
