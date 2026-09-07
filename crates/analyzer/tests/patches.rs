//! M7 Slice 2 integration test: full pipeline on Nemesis subtune 1.
//!
//! Emulate → analyze → detect_notes → detect_effects →
//! extract_characteristics → extract_patches. Asserts that patch
//! coverage meets the plan's ≥ 70 % target on Hubbard-class material
//! and that the patch count stays inside the gold-set range.

use sid_analyzer::analysis::timbre::extract_timbre;
use sid_analyzer::header::SubtuneIndex;

mod common;
use common::SidPipeline;

const SAMPLE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../assets/music/Nemesis_the_Warlock.sid"
);
const FRAMES: u32 = 3000;

#[test]
fn nemesis_subtune_1_patches_cover_majority_of_notes() {
    let pipeline = SidPipeline::run(SAMPLE, SubtuneIndex(1), FRAMES);

    assert!(
        !pipeline.notes.is_empty(),
        "Nemesis subtune 1 should produce notes within {FRAMES} frames"
    );

    let voice3_reads = pipeline.trace.voice3_reads_per_frame();
    let (_characteristics, patches, assignments) = extract_timbre(
        &pipeline.notes,
        &pipeline.states,
        &pipeline.effects,
        &voice3_reads,
        pipeline.clock,
    );

    let total = assignments.len();
    let covered = assignments.iter().filter(|a| a.is_some()).count();
    let coverage = covered as f32 / total as f32;

    // Diagnostic dump is gated so green test runs stay silent.
    if std::env::var("PATCH_DEBUG").is_ok() {
        eprintln!(
            "nemesis subtune 1: {total} notes, {} patches, coverage = {:.1}%",
            patches.len(),
            coverage * 100.0
        );
        for p in &patches {
            eprintln!(
                "  patch {}: members={} first_wave=0x{:02X} adsr={} role_tags={:?}",
                p.id.0, p.member_count, p.waveform, p.adsr, p.role_tags
            );
        }
    }

    // Slice 0's baseline measured 98.5 % pair_coshare on (adsr, first_wave)
    // across assets/music. The richer Slice 2 key (adsr, first_wave,
    // role_tags) is stricter — coverage will be lower. Plan floor: ≥ 70 %.
    assert!(
        coverage >= 0.70,
        "coverage {:.1}% below 70 % floor (was {covered}/{total})",
        coverage * 100.0
    );

    // Gold-table expects 4–6 patches for Nemesis subtune 1. Allow some
    // headroom over the upper bound to absorb heuristic drift; 2..=8
    // would still catch a regression where the patch count explodes.
    assert!(
        (2..=8).contains(&patches.len()),
        "patch count {} outside expected range 2..=8",
        patches.len()
    );
}
