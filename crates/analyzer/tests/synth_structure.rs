//! The structure-preserving `--format synth-native` layout must be a pure
//! *re-grouping* of the flat one: slicing the per-voice note timeline into the
//! driver's own reused pattern blocks (with placement transpose) and rebuilding
//! the absolute timeline from those placements must reproduce, note for note, the
//! whole-song layout. This guards the slice/re-base/untranspose round-trip — a
//! bug there would silently shift or drop notes.

use serde_json::Value;
use std::collections::BTreeMap;

use sid_analyzer::analysis::SystemClock;
use sid_analyzer::export::native::extract_native;
use sid_analyzer::export::synth::{Census, write_synth};
use sid_analyzer::header::{self, SubtuneIndex};
use sid_analyzer::playerid::PlayerDb;

/// One emitted note, keyed by everything audible: absolute tick, pitch, duration,
/// velocity, legato, and the raw expression/glide JSON. Track id is included so a
/// voice's notes are compared against the same voice's, not merged across voices.
type NoteKey = (u32, String);

/// Flatten a Pertylizer song into a per-track multiset of absolute notes.
/// Placement `start` shifts each note's tick; placement `transpose` shifts its
/// pitch — exactly what the engine does — so a sliced+transposed block flattens
/// back to its absolute position.
fn flatten(project: &Value) -> BTreeMap<NoteKey, usize> {
    let song = &project["song"];
    let patterns: BTreeMap<u64, &Value> = song["patterns"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| (p["id"].as_u64().unwrap(), p))
        .collect();

    let mut bag: BTreeMap<NoteKey, usize> = BTreeMap::new();
    for pl in song["arrangement"].as_array().unwrap() {
        let pat = patterns[&pl["pattern_id"].as_u64().unwrap()];
        let track = pl["track_id"].as_u64().unwrap() as u32;
        let start = pl["start"].as_u64().unwrap() as i64;
        let transpose = pl["transpose"].as_f64().unwrap() as i64;
        let gain = pl["gain"].as_f64().unwrap();
        let length_override = pl["length_override"].as_u64();
        for n in pat["notes"].as_array().unwrap() {
            if length_override.is_some_and(|length| n["start"].as_u64().unwrap() >= length) {
                continue;
            }
            let tick = n["start"].as_u64().unwrap() as i64 + start;
            let pitch = n["pitch"].as_i64().unwrap() + transpose;
            // Position, transpose, and uniform placement gain are lifted out;
            // everything else stays verbatim so expression/glide/legato
            // differences remain visible.
            let mut rest = n.clone();
            rest["start"] = Value::from(0);
            rest["pitch"] = Value::from(pitch);
            rest["id"] = Value::from(0);
            rest["velocity"] =
                Value::from((n["velocity"].as_f64().unwrap() * gain * 1_000_000.0).round() as i64);
            let key = (track, format!("{tick}|{rest}"));
            *bag.entry(key).or_default() += 1;
        }
    }
    bag
}

fn song_json(bytes: &[u8], structured: bool) -> Value {
    // 1500 frames: long enough to loop the orderlist (exercising block reuse and
    // the Monty/Ikari transpose path) yet short enough that every supported
    // variant still clears the decode-agreement gate.
    song_json_frames(bytes, structured, 1500)
}

fn song_json_frames(bytes: &[u8], structured: bool, frames: u32) -> Value {
    let (out, _) = song_export_frames(bytes, structured, frames);
    serde_json::from_slice(&out).unwrap()
}

fn song_export_frames(bytes: &[u8], structured: bool, frames: u32) -> (Vec<u8>, Census) {
    let head = header::parse(bytes).unwrap();
    let db = PlayerDb::embedded();
    let (_driver, _extractor, mut program) = extract_native(
        &db,
        &head,
        bytes,
        SubtuneIndex(1),
        sid_analyzer::emu::PlaybackTiming::vblank(SystemClock::Pal),
        frames,
    )
    .unwrap();

    if !structured {
        program.semantic.structure = None;
    }
    let mut out = Vec::new();
    let census = write_synth(&program, &mut out).unwrap();
    (out, census)
}

/// Throwaway A/B dumper: write both the flat and the structured native export of
/// one asset to `/tmp` so they can be rendered and compared in Pertylizer. Reads
/// `AB_ASSET` (default Auf_Wiedersehen_Monty.sid) and `AB_FRAMES` (default 18400).
/// Run: `AB_ASSET=Auf_Wiedersehen_Monty.sid cargo test -p sid-analyzer --test
/// synth_structure dump_flat_and_structured -- --ignored --nocapture`.
#[test]
#[ignore]
fn dump_flat_and_structured() {
    let file = std::env::var("AB_ASSET").unwrap_or_else(|_| "Auf_Wiedersehen_Monty.sid".into());
    let frames: u32 = std::env::var("AB_FRAMES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(18400);
    let bytes = std::fs::read(format!("../../assets/music/{file}")).unwrap();
    let stem = file.trim_end_matches(".sid");

    for (structured, tag) in [(false, "flat"), (true, "structured")] {
        let song = song_json_frames(&bytes, structured, frames);
        let path = format!("/tmp/{stem}_{tag}.ptz");
        std::fs::write(&path, serde_json::to_vec_pretty(&song).unwrap()).unwrap();
        println!("wrote {path}");
    }
}

/// For every supported Hubbard asset, the structured export flattens to the exact
/// same per-track note multiset as the flat export — proving the structure layer
/// only regroups, never alters, the music.
#[test]
fn structured_layout_flattens_to_flat() {
    let assets = [
        "Commando.sid",
        "Sigma_Seven.sid",
        "Auf_Wiedersehen_Monty.sid",
        "Knucklebusters.sid",
        "Warhawk.sid",
        "Human_Race.sid",
        "Ikari_Union.sid",
    ];

    let mut tested = 0;
    for file in assets {
        let path = format!("../../assets/music/{file}");
        let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("{path}: {e}"));

        let flat = flatten(&song_json(&bytes, false));
        let structured = flatten(&song_json(&bytes, true));

        if structured != flat {
            let structured_only: Vec<_> = structured
                .iter()
                .filter(|(note, count)| flat.get(note) != Some(count))
                .take(8)
                .collect();
            let flat_only: Vec<_> = flat
                .iter()
                .filter(|(note, count)| structured.get(note) != Some(count))
                .take(8)
                .collect();
            panic!(
                "{file}: structured note timeline diverges from the flat layout; structured-only {structured_only:?}; flat-only {flat_only:?}"
            );
        }
        // Guard against a degenerate pass on an empty song.
        assert!(!flat.is_empty(), "{file}: produced no notes");
        tested += 1;
    }
    assert_eq!(tested, 7, "expected all 7 assets to be exercised");
}

#[test]
fn warhawk_fragmentation_baseline() {
    let bytes = std::fs::read("../../assets/music/Warhawk.sid").expect("Warhawk fixture");
    let (serialized, census) = song_export_frames(&bytes, true, 1500);
    let summary = census.summary();
    let project: Value = serde_json::from_slice(&serialized).expect("project JSON");
    let patterns = project["song"]["patterns"].as_array().expect("patterns");
    let arrangement = project["song"]["arrangement"]
        .as_array()
        .expect("arrangement");

    assert_eq!(summary.patterns, 50, "musical-pattern baseline changed");
    assert_eq!(summary.automation_patterns, 20);
    assert_eq!(summary.serialized_patterns, 70);
    assert_eq!(summary.placements, 87);
    assert_eq!(summary.automation_points, 1501);
    assert_eq!(summary.serialized_size, serialized.len() as u64);
    assert!(
        summary.serialized_size <= 450_000,
        "Warhawk project grew beyond the fragmentation budget"
    );
    assert_eq!(
        patterns
            .iter()
            .filter(|pattern| pattern["notes"]
                .as_array()
                .is_some_and(|notes| !notes.is_empty()))
            .count(),
        summary.patterns,
        "automation-only lane containers must not count as musical patterns"
    );
    assert!(
        patterns.iter().any(|pattern| {
            let pattern_id = pattern["id"].as_u64();
            let tracks: std::collections::BTreeSet<u64> = arrangement
                .iter()
                .filter(|placement| placement["pattern_id"].as_u64() == pattern_id)
                .filter_map(|placement| placement["track_id"].as_u64())
                .collect();
            tracks.len() > 1
        }),
        "fixture must exercise safe cross-track pattern reuse"
    );
    assert!(
        arrangement
            .iter()
            .all(|placement| placement["loop_mode"] == "clip"),
        "extended placements must preserve source silence instead of repeating"
    );
    let census_json = serde_json::to_value(census).expect("census JSON");
    assert_eq!(
        census_json["physical_voices"]["cross_plan_overlap_calls"], 0,
        "canonicalization must not create rendered physical-voice overlap"
    );
}

/// The oscillator waveform of the instrument backing a named track, if any.
/// Walks track → instrument → patch.modules to read the source oscillator.
/// The module `type`s of the instrument bound to the first track whose name
/// contains `track_name_contains`.
fn track_module_types(project: &Value, track_name_contains: &str) -> Option<Vec<String>> {
    let song = &project["song"];
    let track = song["tracks"].as_array()?.iter().find(|t| {
        t["name"]
            .as_str()
            .unwrap_or("")
            .contains(track_name_contains)
    })?;
    let inst_id = track["instrument"].as_u64()?;
    let inst = project["instruments"]
        .as_array()?
        .iter()
        .find(|i| i["id"].as_u64() == Some(inst_id))?;
    Some(
        inst["patch"]["modules"]
            .as_array()?
            .iter()
            .filter_map(|m| m["type"].as_str().map(str::to_owned))
            .collect(),
    )
}

/// The dominant waveform label of the first `sid_oscillator` (`sid-1`) on the
/// named track — derived from the module's waveform bits, pulse > saw > tri >
/// noise (the same priority the exporter names instruments by).
fn track_osc_waveform(project: &Value, track_name_contains: &str) -> Option<String> {
    let song = &project["song"];
    let track = song["tracks"].as_array()?.iter().find(|t| {
        t["name"]
            .as_str()
            .unwrap_or("")
            .contains(track_name_contains)
    })?;
    let inst_id = track["instrument"].as_u64()?;
    let inst = project["instruments"]
        .as_array()?
        .iter()
        .find(|i| i["id"].as_u64() == Some(inst_id))?;
    for m in inst["patch"]["modules"].as_array()? {
        if m["type"] == "sid_oscillator" && m["id"] == "sid-1" {
            let p = &m["parameters"];
            let bit = |name: &str| p[name].as_f64().unwrap_or(0.0) > 0.5;
            let label = if bit("pulse") {
                "pulse"
            } else if bit("sawtooth") {
                "sawtooth"
            } else if bit("triangle") {
                "triangle"
            } else if bit("noise") {
                "noise"
            } else {
                return None;
            };
            return Some(label.to_owned());
        }
    }
    None
}

/// Auf Wiedersehen Monty's drum-drops (a gate then a far downward sweep to a held
/// body) split onto a dedicated percussion track sounding the body's true pulse
/// waveform, not the triangle V2 Lead patch clustering bound them to. Commando's
/// authored drum/zap instruments (the `+7` bit0 drum-drop flag was RE'd from its
/// own driver) route the same way — the exclusive `end_frame` fix let their body
/// plateau reach `MIN_LEGATO_FRAMES`, so the split now fires for them too. Sigma
/// Seven has no such notes, so it must gain no drum track — guarding both the
/// routing and that drum-drop-free tunes stay untouched.
#[test]
fn drum_drops_split_onto_a_pulse_percussion_track() {
    // 3000 frames: Monty's first drum-drop section lands ~frame 1937, past the
    // 1500-frame default used by the round-trip test.
    let monty =
        std::fs::read("../../assets/music/Auf_Wiedersehen_Monty.sid").expect("Monty fixture");
    let song = song_json_frames(&monty, true, 3000);
    assert_eq!(
        track_osc_waveform(&song, "drum (drop)").as_deref(),
        Some("pulse"),
        "Monty must emit a pulse-bodied drum-drop track"
    );
    // The drum body carries a noise-click attack: a two-source graph (pulse
    // sid + noise sid) summed in a mixer, not a bare pulse oscillator.
    let mods = track_module_types(&song, "drum (drop)").expect("drum-drop instrument");
    assert!(
        mods.iter().filter(|m| *m == "sid_oscillator").count() == 2
            && mods.contains(&"mixer".to_string()),
        "drum-drop must mix a noise-click attack under the pulse body, got {mods:?}"
    );

    let commando = std::fs::read("../../assets/music/Commando.sid").expect("Commando fixture");
    let song = song_json_frames(&commando, true, 3000);
    assert_eq!(
        track_osc_waveform(&song, "drum (drop)").as_deref(),
        Some("pulse"),
        "Commando's authored zap drums must route to a drum-drop track"
    );

    let sigma = std::fs::read("../../assets/music/Sigma_Seven.sid").expect("Sigma fixture");
    let song = song_json_frames(&sigma, true, 3000);
    assert!(
        track_osc_waveform(&song, "drum (drop)").is_none(),
        "Sigma Seven has no drum-drops, so it must gain no drum track"
    );
}
