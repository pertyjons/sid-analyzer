//! Per-asset note-fidelity budgets (forward-model gate slice 6,
//! `docs/forward-model-gate.md`): the census numbers the export currently
//! achieves on the flagship assets, pinned as ceilings so a future special
//! case (or the deletion of one) cannot silently regress pitch fidelity or
//! coverage. Lowering a budget is progress and should be done deliberately;
//! raising one demands the same justification a failing exit gate would.

mod common;

use common::SidPipeline;
use serde_json::Value;
use sid_analyzer::export::native;
use sid_analyzer::export::synth;
use sid_analyzer::header::{self, SubtuneIndex};
use sid_analyzer::playerid::PlayerDb;

fn asset(name: &str) -> String {
    format!("{}/../../assets/music/{name}", env!("CARGO_MANIFEST_DIR"))
}

/// The `note_fidelity` census block for a heuristic (`--format synth`)
/// export of one asset.
fn heuristic_fidelity(name: &str, frames: u32) -> Value {
    let f = SidPipeline::run(&asset(name), SubtuneIndex(1), frames);
    let program = f.analyzed_program();
    let census = synth::write_synth(&program, &mut std::io::sink()).expect("write");
    serde_json::to_value(&census).expect("census json")["note_fidelity"].clone()
}

/// The `note_fidelity` census block for a native (`--format synth-native`)
/// export of one asset.
fn native_census(name: &str, frames: u32) -> Value {
    let bytes = std::fs::read(asset(name)).expect("asset present");
    let header = header::parse(&bytes).expect("parse header");
    let clock = header.flags.clock.into();
    let db = PlayerDb::embedded();
    let (_driver, _extractor, program) = native::extract_native(
        &db,
        &header,
        &bytes,
        SubtuneIndex(1),
        sid_analyzer::emu::PlaybackTiming::vblank(clock),
        frames,
    )
    .expect("native extraction");
    let census = synth::write_synth(&program, &mut std::io::sink()).expect("write");
    serde_json::to_value(&census).expect("census json")
}

fn get(v: &Value, key: &str) -> u64 {
    v[key]
        .as_u64()
        .unwrap_or_else(|| panic!("census field {key}"))
}

/// Assert one asset's ceilings: melodic fails, degradations, silent drops,
/// and total uncovered gated frames must not exceed the pinned budget.
#[allow(clippy::too_many_arguments)]
fn assert_budget(
    label: &str,
    nf: &Value,
    max_fail: u64,
    max_degraded: u64,
    max_silent: u64,
    max_uncovered: u64,
) {
    let fail = get(nf, "notes_fail");
    let degraded = get(nf, "events_degraded");
    let silent = get(nf, "events_silent");
    let uncovered: u64 = nf["uncovered_gated_frames"]
        .as_array()
        .expect("uncovered array")
        .iter()
        .map(|v| v.as_u64().unwrap_or(0))
        .sum();
    assert!(
        fail <= max_fail,
        "{label}: notes_fail {fail} exceeds budget {max_fail}"
    );
    assert!(
        degraded <= max_degraded,
        "{label}: events_degraded {degraded} exceeds budget {max_degraded}"
    );
    assert!(
        silent <= max_silent,
        "{label}: events_silent {silent} exceeds budget {max_silent}"
    );
    assert!(
        uncovered <= max_uncovered,
        "{label}: uncovered gated frames {uncovered} exceeds budget {max_uncovered}"
    );
}

#[test]
fn note_fidelity_stays_within_asset_budgets() {
    // Native path — the flagship: authored notes must verify with zero
    // melodic fails; degradations/silent drops are the measured status quo
    // (each unit below these ceilings is decode/export progress).
    // Physical-voice ownership raises the degradation count 88 → 95 because
    // seven formerly overlapping release proposals are now checked/choked at
    // the next hardware-voice attack and correctly fall to the pitch bake. In
    // exchange, silent drops fall to zero and uncovered gated calls collapse
    // from the old 1057-call ceiling to 7.
    let monty = native_census("Auf_Wiedersehen_Monty.sid", 3000);
    assert_eq!(
        get(&monty["physical_voices"], "cross_plan_overlap_calls"),
        0,
        "physical SID voice ownership must eliminate rendered cross-plan overlap"
    );
    assert!(
        get(&monty["physical_voices"], "source_release_overlap_calls") > 0,
        "fixture must exercise the release-tail choke"
    );
    assert_budget("Monty native", &monty["note_fidelity"], 0, 95, 0, 7);

    // Commando's degradation ceiling rose 8 → 18 with slice 7: the moving
    // release tails became verifiable, and ten drum events whose zap sweep
    // continues through the release degrade to the bake — which now renders
    // the sweep's real per-frame pitches (percussion bucket stays all-ok).
    let commando = native_census("Commando.sid", 3000);
    assert_budget("Commando native", &commando["note_fidelity"], 0, 18, 0, 95);

    // Heuristic path — the universal fallback. Zero melodic fails here too
    // since slices 3–5 (was 132 at slice 2).
    let nemesis = heuristic_fidelity("Nemesis_the_Warlock.sid", 3000);
    assert_budget("Nemesis heuristic", &nemesis, 0, 35, 0, 0);
}

#[test]
fn auf_wiedersehen_monty_has_no_multiframe_duration_gap_at_full_length() {
    let census = native_census("Auf_Wiedersehen_Monty.sid", 18_446);
    let fidelity = &census["note_fidelity"];
    let worst = fidelity["worst_uncovered_gated_spans"]
        .as_array()
        .expect("uncovered span list");
    let longest = worst
        .iter()
        .map(|span| get(span, "end_frame").saturating_sub(get(span, "start_frame")))
        .max()
        .unwrap_or(0);
    assert!(
        longest <= 1,
        "AWM full length: longest uncovered gated span is {longest} calls"
    );
    assert_budget("AWM full length", fidelity, 0, 1_294, 0, 49);
}

#[test]
fn former_full_length_native_outliers_reject_every_wrong_subnote() {
    // Each window includes a formerly reported one-frame offender. The complete
    // corpus baseline remains the full-length gate; keeping the regression
    // windows bounded avoids adding all 74,585 calls to the normal suite.
    for (name, frames) in [
        ("Sigma_Seven.sid", 100),
        ("Knucklebusters.sid", 5_900),
        ("Nemesis_the_Warlock.sid", 200),
    ] {
        let census = native_census(name, frames);
        assert_eq!(
            get(&census["note_fidelity"], "notes_fail"),
            0,
            "{name}: every emitted melodic subnote must pass the forward gate"
        );
    }
}
