use serde_json::Value;
use sid_analyzer::analysis::timbre::extract_timbre;
use sid_analyzer::export::json::{EnrichedNote, Export, write_json};
use sid_analyzer::header::SubtuneIndex;

mod common;
use common::SidPipeline;

const SAMPLE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../assets/music/Nemesis_the_Warlock.sid"
);

fn fixture(frames: u32) -> SidPipeline {
    SidPipeline::run(SAMPLE, SubtuneIndex(1), frames)
}

/// Bare export with no timbre extraction — patch_id and characteristics
/// fields stay absent in the JSON.
fn export_bare(f: &SidPipeline) -> Export<'_> {
    Export {
        header: &f.header,
        subtune: f.subtune,
        timing: sid_analyzer::emu::PlaybackTiming::vblank(f.clock),
        frame_count: f.frame_count(),
        subtune_lengths_secs: None,
        stil: None,
        patches: None,
        notes: EnrichedNote::enrich(&f.notes, None, None),
        effects: &f.effects,
        voice_relations: &[],
        digi_streams: Vec::new(),
        additional_sid_chips: Vec::new(),
        structure: None,
        native: None,
    }
}

#[test]
fn json_export_round_trips_through_serde() {
    let f = fixture(20);
    let mut buf = Vec::new();
    write_json(&export_bare(&f), &mut buf, false).expect("write_json");

    let v: Value = serde_json::from_slice(&buf).expect("parses as JSON");
    assert_eq!(v["subtune"], 1);
    assert_eq!(v["frame_count"], 20);
    assert_eq!(v["timing"]["clock"], "PAL");
    assert_eq!(v["header"]["name"], "Nemesis the Warlock");
    assert_eq!(v["header"]["author"], "Rob Hubbard");
    assert_eq!(v["header"]["songs"], 15);
    // `LoadAddress` serializes transparently as its inner u16.
    assert_eq!(v["header"]["init_address"], 0xF160);
    assert_eq!(v["header"]["play_address"], 0xF190);
}

#[test]
fn json_export_emits_effect_variants_as_pascal_strings() {
    let f = fixture(50);
    let mut buf = Vec::new();
    write_json(&export_bare(&f), &mut buf, false).expect("write_json");

    let v: Value = serde_json::from_slice(&buf).expect("parses");
    let effect_kinds: Vec<&str> = v["effects"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["effect"].as_str().unwrap())
        .collect();
    let known = [
        "HardSync",
        "RingMod",
        "Sample",
        "PWM",
        "FilterSweep",
        "Portamento",
        "Vibrato",
        "Arpeggio",
    ];
    for k in &effect_kinds {
        assert!(known.contains(k), "unknown effect kind in JSON: {k:?}");
    }
}

#[test]
fn pretty_printing_uses_indentation() {
    let f = fixture(5);
    let mut compact = Vec::new();
    write_json(&export_bare(&f), &mut compact, false).unwrap();
    let mut pretty = Vec::new();
    write_json(&export_bare(&f), &mut pretty, true).unwrap();

    assert!(pretty.len() > compact.len(), "pretty should be larger");
    assert!(pretty.contains(&b'\n'), "pretty should contain newlines");
    assert!(!compact.contains(&b'\n'), "compact should be single-line");
}

#[test]
fn timbre_fields_absent_when_extraction_not_run() {
    let f = fixture(20);
    let mut buf = Vec::new();
    write_json(&export_bare(&f), &mut buf, false).unwrap();
    let v: Value = serde_json::from_slice(&buf).unwrap();

    // patches[] missing entirely when None.
    assert!(
        v.get("patches").is_none(),
        "patches should not appear when None"
    );

    // notes[].patch_id and notes[].characteristics omitted per-note.
    if let Some(first) = v["notes"].as_array().and_then(|a| a.first()) {
        assert!(
            first.get("patch_id").is_none(),
            "patch_id should be omitted when None"
        );
        assert!(
            first.get("characteristics").is_none(),
            "characteristics should be omitted when None"
        );
    }
}

#[test]
fn enriched_export_includes_patches_and_per_note_characteristics() {
    // 3000 frames are needed for Nemesis subtune 1 to produce
    // ≥ 2-member patch clusters; lower counts give an empty patches[]
    // and the !patches_json.is_empty() check below would fail.
    let f = fixture(3000);
    let voice3_reads = f.trace.voice3_reads_per_frame();
    let (characteristics, patches, assignments) =
        extract_timbre(&f.notes, &f.states, &f.effects, &voice3_reads, f.clock);
    let enriched = EnrichedNote::enrich(&f.notes, Some(&assignments), Some(&characteristics));
    let export = Export {
        header: &f.header,
        subtune: f.subtune,
        timing: sid_analyzer::emu::PlaybackTiming::vblank(f.clock),
        frame_count: f.frame_count(),
        subtune_lengths_secs: None,
        stil: None,
        patches: Some(&patches),
        notes: enriched,
        effects: &f.effects,
        voice_relations: &[],
        digi_streams: Vec::new(),
        additional_sid_chips: Vec::new(),
        structure: None,
        native: None,
    };
    let mut buf = Vec::new();
    write_json(&export, &mut buf, false).unwrap();
    let v: Value = serde_json::from_slice(&buf).unwrap();

    // patches[] is present and non-empty (Nemesis sub 1 → ~6 patches).
    let patches_json = v["patches"].as_array().expect("patches array present");
    assert!(
        !patches_json.is_empty(),
        "Nemesis subtune 1 should yield patches"
    );

    // Each patch has the expected (key-shared) top-level keys; the voice-
    // specific timbre now lives under `voices[]`.
    let first_patch = &patches_json[0];
    for key in [
        "id",
        "adsr",
        "waveform",
        "role_tags",
        "member_count",
        "voices",
    ] {
        assert!(
            first_patch.get(key).is_some(),
            "patch should expose `{key}` field"
        );
    }

    // Each per-voice profile exposes the voice-specific timbre keys.
    let first_profile = &first_patch["voices"]
        .as_array()
        .expect("patch.voices array")[0];
    for key in [
        "voice",
        "pw_envelope",
        "filter_routed",
        "filter_contour",
        "filter_mode",
        "filter_resonance",
        "hardware_tricks",
        "waveform_loop",
        "arpeggio_loop",
    ] {
        assert!(
            first_profile.get(key).is_some(),
            "patch voice profile should expose `{key}` field"
        );
    }

    // Inline note fields: patch_id, characteristics (incl. role_tags + envelope).
    let assigned = v["notes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n.get("patch_id").is_some())
        .expect("at least one note should carry patch_id");
    assert!(assigned.get("midi").is_some(), "flattened NoteEvent midi");
    let c = assigned
        .get("characteristics")
        .expect("characteristics inline");
    assert!(c.get("attack").is_some(), "characteristics.attack");
    assert!(c.get("role_tags").is_some(), "characteristics.role_tags");
    assert!(
        c.get("pw_envelope").is_some(),
        "characteristics.pw_envelope"
    );
}
