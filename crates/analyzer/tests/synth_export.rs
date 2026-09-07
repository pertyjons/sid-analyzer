use serde_json::Value;
use sid_analyzer::analysis::SystemClock;
use sid_analyzer::analysis::sid_program::AnalyzedSidProgram;
use sid_analyzer::emu::PlaybackTiming;
use sid_analyzer::export::synth::{
    EnhancementAmount, SynthOptions, SynthStyle, write_synth, write_synth_with_options,
};
use sid_analyzer::header::SubtuneIndex;
use std::io::Write;
use std::process::Command;

mod common;
use common::{SidPipeline, synthetic_filtered_saw_sid};

const SAMPLE: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../assets/music/Nemesis_the_Warlock.sid"
);

const SCHEMA: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../docs/pertylizer/project.schema.json"
);

/// A Rob Hubbard tune whose lead uses genuine onset portamento (≤ an octave) — so
/// the trace export emits clean glide notes, unlike Nemesis whose only "slides"
/// are far-jump artifacts the exporter now suppresses.
const MONTY: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../assets/music/Auf_Wiedersehen_Monty.sid"
);

const WARHAWK: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../assets/music/Warhawk.sid"
);

const SHAPE_MUSIC_2: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../assets/music/Shape_Music_2.sid"
);

/// Build the full enriched synth export for Nemesis subtune 1. 3000 frames
/// are needed for the patch clusterer to produce ≥ 2-member patches.
fn synth_bytes(frames: u32) -> Vec<u8> {
    synth_bytes_for(1, frames)
}

/// Build the enriched synth export for a chosen Nemesis subtune (1-based).
fn synth_bytes_for(subtune: u16, frames: u32) -> Vec<u8> {
    synth_bytes_path(SAMPLE, subtune, frames)
}

/// Build the enriched synth export for a chosen tune file + subtune (1-based).
fn synth_bytes_path(path: &str, subtune: u16, frames: u32) -> Vec<u8> {
    let f = SidPipeline::run(path, SubtuneIndex(subtune), frames);
    synth_bytes_for_pipeline(&f)
}

fn synth_bytes_for_pipeline(fixture: &SidPipeline) -> Vec<u8> {
    let program = fixture.analyzed_program();
    let mut buf = Vec::new();
    write_synth(&program, &mut buf).expect("write_synth");
    buf
}

fn synth_bytes_path_with_clock(
    path: &str,
    subtune: u16,
    frames: u32,
    clock: SystemClock,
) -> Vec<u8> {
    let bytes = std::fs::read(path).expect("sample SID file present");
    let header = sid_analyzer::header::parse(&bytes).expect("parse SID header");
    let timing = PlaybackTiming::vblank(clock);
    let trace =
        sid_analyzer::emu::run_with_timing(&header, &bytes, SubtuneIndex(subtune), frames, timing)
            .expect("run SID with selected clock");
    let program = AnalyzedSidProgram::from_trace(&header, SubtuneIndex(subtune), timing, &trace);
    let mut output = Vec::new();
    write_synth(&program, &mut output).expect("write_synth");
    output
}

fn modern_synth_bytes(frames: u32) -> Vec<u8> {
    let fixture = SidPipeline::run(SAMPLE, SubtuneIndex(1), frames);
    let program = fixture.analyzed_program();
    let mut output = Vec::new();
    write_synth_with_options(
        &program,
        &mut output,
        SynthOptions {
            style: SynthStyle::ModernAnalog,
            ..SynthOptions::default()
        },
    )
    .expect("write modern synth");
    output
}

fn enhanced_synth_bytes(frames: u32, amount: u8) -> Vec<u8> {
    let fixture = SidPipeline::run(SAMPLE, SubtuneIndex(1), frames);
    let program = fixture.analyzed_program();
    let mut output = Vec::new();
    write_synth_with_options(
        &program,
        &mut output,
        SynthOptions {
            enhancement: Some(EnhancementAmount::new(amount).expect("valid enhancement amount")),
            ..SynthOptions::default()
        },
    )
    .expect("write enhanced synth");
    output
}

fn enhancement_distortion(project: &Value) -> &Value {
    project["instruments"][0]["patch"]["modules"]
        .as_array()
        .expect("modules")
        .iter()
        .find(|module| module["id"] == "dst-1")
        .expect("enhancement distortion")
}

#[test]
#[cfg_attr(
    not(feature = "asset-tests"),
    ignore = "requires the optional assets/music corpus"
)]
fn enhanced_synth_preserves_song_and_sid_voice_graphs() {
    let faithful: Value =
        serde_json::from_slice(&synth_bytes(300)).expect("faithful export parses as JSON");
    let enhanced: Value = serde_json::from_slice(&enhanced_synth_bytes(300, 5))
        .expect("enhanced export parses as JSON");

    assert_eq!(faithful["song"], enhanced["song"]);
    let faithful_instruments = faithful["instruments"].as_array().expect("instruments");
    let enhanced_instruments = enhanced["instruments"].as_array().expect("instruments");
    assert_eq!(faithful_instruments.len(), enhanced_instruments.len());

    for (faithful_instrument, enhanced_instrument) in
        faithful_instruments.iter().zip(enhanced_instruments)
    {
        assert_eq!(
            faithful_instrument["patch"]["connections"],
            enhanced_instrument["patch"]["connections"]
        );
        let faithful_modules = faithful_instrument["patch"]["modules"]
            .as_array()
            .expect("faithful modules");
        let enhanced_modules = enhanced_instrument["patch"]["modules"]
            .as_array()
            .expect("enhanced modules");
        for faithful_module in faithful_modules {
            let preserved = enhanced_modules.iter().find(|enhanced_module| {
                enhanced_module["id"] == faithful_module["id"]
                    && enhanced_module["type"] == faithful_module["type"]
            });
            assert_eq!(preserved, Some(faithful_module));
        }
        assert!(
            enhanced_modules
                .iter()
                .any(|module| module["type"] == "sid_oscillator")
        );
        assert!(
            !enhanced_instrument["patch"]["settings"]["effect_chain_order"]
                .as_array()
                .expect("effect chain")
                .is_empty()
        );
    }
}

#[test]
#[cfg_attr(
    not(feature = "asset-tests"),
    ignore = "requires the optional assets/music corpus"
)]
fn enhancement_amount_monotonically_scales_wet_processing() {
    let low: Value = serde_json::from_slice(&enhanced_synth_bytes(300, 1)).expect("level 1 export");
    let high: Value =
        serde_json::from_slice(&enhanced_synth_bytes(300, 10)).expect("level 10 export");
    assert_eq!(low["song"], high["song"]);

    let low_distortion = enhancement_distortion(&low);
    let high_distortion = enhancement_distortion(&high);
    assert!(
        low_distortion["parameters"]["drive"].as_f64()
            < high_distortion["parameters"]["drive"].as_f64()
    );
    assert!(
        low_distortion["parameters"]["mix"].as_f64()
            < high_distortion["parameters"]["mix"].as_f64()
    );
}

#[test]
#[cfg_attr(
    not(feature = "asset-tests"),
    ignore = "requires the optional assets/music corpus"
)]
fn enhancement_rejects_the_modern_resynthesis_style() {
    let fixture = SidPipeline::run(SAMPLE, SubtuneIndex(1), 30);
    let program = fixture.analyzed_program();
    let mut output = Vec::new();
    let result = write_synth_with_options(
        &program,
        &mut output,
        SynthOptions {
            style: SynthStyle::ModernAnalog,
            enhancement: Some(EnhancementAmount::new(5).expect("enhancement amount")),
            ..SynthOptions::default()
        },
    );
    let error = match result {
        Ok(_) => panic!("modern enhancement must be rejected"),
        Err(error) => error,
    };
    assert!(
        error
            .to_string()
            .contains("cannot be combined with the modern analog style")
    );
    assert!(output.is_empty());
}

#[test]
#[cfg_attr(
    not(feature = "asset-tests"),
    ignore = "requires the optional assets/music corpus"
)]
fn modern_synth_export_restyles_music_without_sid_modules() {
    let bytes = modern_synth_bytes(3000);
    let project: Value = serde_json::from_slice(&bytes).expect("parses as JSON");
    let instruments = project["instruments"].as_array().expect("instruments");

    assert!(!instruments.is_empty());
    for instrument in instruments {
        let modules = instrument["patch"]["modules"].as_array().expect("modules");
        assert!(
            modules
                .iter()
                .all(|module| module["type"] != "sid_oscillator")
        );
        assert!(modules.iter().any(|module| module["type"] == "oscillator"));
        assert!(modules.iter().any(|module| module["type"] == "filter"));
        assert!(
            !instrument["patch"]["settings"]["effect_chain_order"]
                .as_array()
                .expect("effect chain")
                .is_empty()
        );
    }

    let module_targets = project["song"]["patterns"]
        .as_array()
        .expect("patterns")
        .iter()
        .flat_map(|pattern| pattern["automation"].as_array().into_iter().flatten())
        .filter_map(|lane| lane["target"].get("Module"));
    for target in module_targets {
        assert_ne!(target["module_type"], "sid_oscillator");
        if target["param_id"] == "pulse_width" {
            assert_eq!(target["module_type"], "oscillator");
        }
    }

    let master = project["global"]["master_effects"]
        .as_array()
        .expect("master effects");
    let types: Vec<&str> = master
        .iter()
        .map(|module| module["type"].as_str().expect("module type"))
        .collect();
    assert_eq!(types, ["compressor", "eq", "limiter"]);
}

#[test]
#[cfg_attr(
    not(feature = "asset-tests"),
    ignore = "requires the optional assets/music corpus"
)]
fn moving_ring_source_is_connected_and_automated_end_to_end() {
    let project: Value =
        serde_json::from_slice(&synth_bytes_path(WARHAWK, 1, 1_500)).expect("Warhawk project JSON");

    let ring_instruments: std::collections::BTreeSet<_> = project["instruments"]
        .as_array()
        .expect("instruments")
        .iter()
        .filter(|instrument| {
            instrument["patch"]["connections"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|connection| {
                    connection["from"] == serde_json::json!(["sid-2", "msb"])
                        && connection["to"] == serde_json::json!(["sid-1", "ring"])
                })
        })
        .filter_map(|instrument| instrument["id"].as_u64())
        .collect();
    assert!(
        !ring_instruments.is_empty(),
        "physical neighbour MSB must drive the ring input"
    );

    let moving_source_instruments: std::collections::BTreeSet<_> = project["song"]["patterns"]
        .as_array()
        .expect("patterns")
        .iter()
        .flat_map(|pattern| pattern["automation"].as_array().into_iter().flatten())
        .filter_map(|lane| {
            let target = &lane["target"]["Module"];
            (target["module_type"] == "sid_oscillator"
                && target["instance"] == 2
                && target["param_id"] == "freq_reg"
                && lane["points"]
                    .as_array()
                    .is_some_and(|points| points.len() >= 2))
            .then(|| target["instrument"].as_u64())
            .flatten()
        })
        .collect();
    assert!(
        ring_instruments
            .intersection(&moving_source_instruments)
            .next()
            .is_some(),
        "the connected ring source must be the oscillator with frequency automation"
    );
}

#[test]
#[cfg_attr(
    not(feature = "asset-tests"),
    ignore = "requires the optional assets/music corpus"
)]
fn three_class_waveform_sequence_survives_the_full_export() {
    let project: Value = serde_json::from_slice(&synth_bytes_path(SHAPE_MUSIC_2, 1, 1_500))
        .expect("Shape Music 2 project JSON");
    let has_three_class_sequence = project["instruments"]
        .as_array()
        .expect("instruments")
        .iter()
        .flat_map(|instrument| {
            instrument["patch"]["modules"]
                .as_array()
                .into_iter()
                .flatten()
        })
        .filter(|module| {
            module["type"] == "sid_oscillator" && module["parameters"]["seq_len"] == 3.0
        })
        .any(|module| {
            let steps = [
                module["parameters"]["seq_step_0"].as_f64().unwrap_or(0.0) as u8,
                module["parameters"]["seq_step_1"].as_f64().unwrap_or(0.0) as u8,
                module["parameters"]["seq_step_2"].as_f64().unwrap_or(0.0) as u8,
            ];
            steps
                .into_iter()
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                == 3
        });
    assert!(
        has_three_class_sequence,
        "expected three audible waveform classes"
    );
}

#[test]
#[cfg_attr(
    not(feature = "asset-tests"),
    ignore = "requires the optional assets/music corpus"
)]
fn forced_ntsc_clock_reaches_every_sid_module_and_the_project_timebase() {
    let ntsc: Value = serde_json::from_slice(&synth_bytes_path_with_clock(
        SAMPLE,
        1,
        400,
        SystemClock::Ntsc,
    ))
    .expect("NTSC project JSON");
    let pal: Value = serde_json::from_slice(&synth_bytes_path_with_clock(
        SAMPLE,
        1,
        400,
        SystemClock::Pal,
    ))
    .expect("PAL project JSON");

    let clocks: std::collections::BTreeSet<_> = ntsc["instruments"]
        .as_array()
        .expect("instruments")
        .iter()
        .flat_map(|instrument| {
            instrument["patch"]["modules"]
                .as_array()
                .into_iter()
                .flatten()
        })
        .filter(|module| module["type"] == "sid_oscillator")
        .filter_map(|module| module["parameters"]["clock"].as_str())
        .collect();
    assert_eq!(clocks, std::collections::BTreeSet::from(["ntsc"]));
    let pal_clocks: std::collections::BTreeSet<_> = pal["instruments"]
        .as_array()
        .expect("instruments")
        .iter()
        .flat_map(|instrument| {
            instrument["patch"]["modules"]
                .as_array()
                .into_iter()
                .flatten()
        })
        .filter(|module| module["type"] == "sid_oscillator")
        .filter_map(|module| module["parameters"]["clock"].as_str())
        .collect();
    assert_eq!(pal_clocks, std::collections::BTreeSet::from(["pal"]));
    assert_ne!(
        ntsc["song"]["default_tempo"], pal["song"]["default_tempo"],
        "the call-rate-derived project timebase must follow the selected clock"
    );
}

/// Returns `true` if python3 with the `jsonschema` module is available, so
/// the schema-validation test can skip gracefully on CI without python.
fn python_jsonschema_available() -> bool {
    Command::new("python3")
        .args(["-c", "import jsonschema"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

#[test]
#[cfg_attr(
    not(feature = "asset-tests"),
    ignore = "requires the optional assets/music corpus"
)]
fn synth_export_has_expected_top_level_shape() {
    let bytes = synth_bytes(3000);
    let v: Value = serde_json::from_slice(&bytes).expect("parses as JSON");

    assert_eq!(v["file_type"], "project");
    assert_eq!(v["version"], "1.0");
    assert_eq!(v["song"]["name"], "Nemesis the Warlock");
    assert_eq!(v["song"]["author"], "Rob Hubbard");
    // §A6: the tempo is derived from the onsets (a real musical BPM, no longer a
    // hardcoded 125) and the editor grid is a 16th-note grid (240 ticks/row),
    // not one row per SID frame.
    let tempo = v["song"]["default_tempo"].as_f64().expect("tempo");
    assert!(
        (50.0..=300.0).contains(&tempo),
        "derived tempo {tempo} out of musical range"
    );
    assert_eq!(v["song"]["row_resolution"]["ticks_per_row"], 240);
    let rows = v["song"]["row_resolution"]["rows"].as_u64().expect("rows");
    assert!((1..=65535).contains(&rows), "grid rows {rows} out of range");

    // One instrument per output track — never shared, even when two voices
    // cluster into the same patch. Each track gathers one voice's notes that
    // share a graph shape, possibly merged from several patches (Nemesis sub 1 →
    // several tracks).
    let instruments = v["instruments"].as_array().expect("instruments array");
    assert!(!instruments.is_empty(), "expected at least one instrument");

    // Each instrument carries the gold module skeleton (source → amp → out,
    // env → amp.cv = 3 connections) plus one extra in-chain connection per
    // optional module (a `flt-1` for filter routing and/or a `rng-1` for
    // ring-mod). PWM and filter sweeps are now reproduced by per-frame
    // automation lanes (no `lfo-1` module / side wire), so they add no
    // connections.
    let patch = &instruments[0]["patch"];
    let module_types: Vec<&str> = patch["modules"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["type"].as_str().unwrap())
        .collect();
    assert!(module_types.contains(&"envelope"));
    assert!(module_types.contains(&"amplifier"));
    assert!(module_types.contains(&"stereo_output"));
    assert!(
        module_types.contains(&"sid_oscillator"),
        "patch must have a sid_oscillator source"
    );
    // The single-source skeleton (sid-1 → [flt] → amp, env→amp.cv, amp→out =
    // 3 + one per optional insert) holds for ordinary instruments; a ring/sync
    // neighbour source adds one `msb` wire per active input. The drum-drop is
    // a distinct dual-source shape (mixer), and the low-cutoff 6581 leak path
    // also uses a mixer, so check the skeleton invariant on a plain one.
    let single_source = instruments
        .iter()
        .find(|inst| {
            let types: Vec<&str> = inst["patch"]["modules"]
                .as_array()
                .unwrap()
                .iter()
                .map(|m| m["type"].as_str().unwrap())
                .collect();
            !types.contains(&"mixer")
                && types.iter().filter(|t| **t == "sid_oscillator").count() == 1
        })
        .expect("at least one single-source instrument");
    let ss = &single_source["patch"];
    let ss_optional = ss["modules"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|m| m["type"].as_str().unwrap() == "filter")
        .count();
    assert_eq!(ss["connections"].as_array().unwrap().len(), 3 + ss_optional);

    // Connections are 2-string tuples, and there is no pitch/gate wire.
    for c in patch["connections"].as_array().unwrap() {
        assert_eq!(c["from"].as_array().unwrap().len(), 2);
        assert_eq!(c["to"].as_array().unwrap().len(), 2);
    }

    // The source is the native `sid_oscillator` in `fast` quality (measured
    // closer to reSID than the 4× path — PoC matrix). §A9: the coloring lives
    // on the master bus, so no instrument carries a distortion module.
    let mut saw_sid = false;
    for inst in instruments {
        let modules = inst["patch"]["modules"].as_array().unwrap();
        for m in modules {
            if m["type"] == "sid_oscillator" {
                saw_sid = true;
                assert_eq!(
                    m["parameters"]["quality"], "fast",
                    "sid_oscillator must run in fast quality"
                );
            }
        }
        assert!(
            modules.iter().all(|m| m["type"] != "distortion"),
            "coloring is on the master bus, not per instrument"
        );
    }
    assert!(saw_sid, "Nemesis sub 1 should emit sid oscillators");

    // §A9: the master-bus coloring chain is a tube distortion, 3-band eq, and
    // look-ahead limiter, applied to the full mix via global.master_effects.
    let master = v["global"]["master_effects"].as_array().unwrap();
    assert_eq!(master.len(), 3, "tube + eq + limiter on the master bus");
    assert_eq!(master[0]["type"], "distortion");
    assert_eq!(master[0]["parameters"]["type"], "tube");
    assert_eq!(master[1]["type"], "eq");
    assert_eq!(master[2]["type"], "limiter");
    assert_eq!(master[2]["id"], "lmt-1");
    assert_eq!(master[2]["parameters"]["ceiling"], -5.0);

    // One track + pattern + arrangement entry per (voice, shape) group.
    let tracks = v["song"]["tracks"].as_array().unwrap();
    let patterns = v["song"]["patterns"].as_array().unwrap();
    let arrangement = v["song"]["arrangement"].as_array().unwrap();
    assert!(!tracks.is_empty());
    assert_eq!(tracks.len(), patterns.len());
    assert_eq!(tracks.len(), arrangement.len());

    // Instruments are never shared across tracks: one dedicated instrument per
    // track, and every track binds a distinct instrument id. Sharing would let
    // two tracks' automation lanes collide on a single instrument instance.
    assert_eq!(
        instruments.len(),
        tracks.len(),
        "expected one dedicated instrument per track"
    );
    let mut bound: Vec<u64> = tracks
        .iter()
        .map(|t| t["instrument"].as_u64().expect("instrument id"))
        .collect();
    bound.sort_unstable();
    bound.dedup();
    assert_eq!(
        bound.len(),
        tracks.len(),
        "every track must bind a distinct instrument (no reuse)"
    );
}

#[test]
#[cfg_attr(
    not(feature = "asset-tests"),
    ignore = "requires the optional assets/music corpus"
)]
fn synth_export_emits_module_automation_lanes() {
    let bytes = synth_bytes(3000);
    let v: Value = serde_json::from_slice(&bytes).expect("parses as JSON");

    let patterns = v["song"]["patterns"].as_array().expect("patterns array");
    let mut lane_count = 0usize;
    let mut params: Vec<&str> = Vec::new();
    let mut saw_interpolating_curve = false;
    for pat in patterns {
        for lane in pat["automation"].as_array().into_iter().flatten() {
            // §A5: the master-volume contour rides a Global target, not a
            // module one — validate it separately and skip the module checks.
            if let Some(global) = lane["target"].get("Global") {
                assert_eq!(global, "MasterVolume");
                let points = lane["points"].as_array().unwrap();
                assert!(points.len() >= 2);
                for p in points {
                    let val = p["value"].as_f64().unwrap();
                    assert!((0.0..=1.0).contains(&val), "volume {val} out of [0,1]");
                }
                continue;
            }
            if let Some(track) = lane["target"].get("Track") {
                assert_eq!(track["param"], "Pitch");
                let points = lane["points"].as_array().unwrap();
                assert!(points.len() >= 2);
                for point in points {
                    let value = point["value"].as_f64().unwrap();
                    assert!((0.0..=1.0).contains(&value));
                }
                params.push("track_pitch");
                continue;
            }
            lane_count += 1;
            let module = &lane["target"]["Module"];
            assert!(matches!(module["instance"].as_u64(), Some(1 | 2)));
            let param = module["param_id"].as_str().unwrap();
            let module_type = module["module_type"].as_str().unwrap();
            // Each supported param rides its own module type; verify the
            // pairing so a swapped ModuleTarget would fail here (not only in
            // the python-gated schema test).
            let expected_module = match param {
                "cutoff" => "filter",
                "pw_reg" | "freq_reg" => "sid_oscillator",
                "attack" | "decay" | "sustain" | "release" => "envelope",
                "level" => "amplifier",
                other => panic!("unexpected param_id {other}"),
            };
            assert_eq!(
                module_type, expected_module,
                "{param} lane should target {expected_module}, got {module_type}"
            );
            params.push(param);

            // A lane is only emitted when the value moves → at least 2 points,
            // each normalized into [0, 1]. After decimation the curve is a valid
            // CurveType: the bare strings "Linear"/"Step"/"SCurve" or the
            // tagged object { "Exponential": <i8> }.
            let points = lane["points"].as_array().unwrap();
            assert!(points.len() >= 2, "lane should have ≥ 2 points");
            for p in points {
                let val = p["value"].as_f64().unwrap();
                assert!((0.0..=1.0).contains(&val), "value {val} out of [0,1]");
                let curve = &p["curve"];
                let valid = matches!(curve.as_str(), Some("Linear" | "Step" | "SCurve"))
                    || curve.get("Exponential").and_then(Value::as_i64).is_some();
                assert!(valid, "unexpected curve {curve}");
                if curve.as_str() != Some("Step") {
                    saw_interpolating_curve = true;
                }
            }
        }
    }

    // Decimation must engage: at least one non-Step (interpolating) point
    // somewhere, proving ramps collapsed to interpolation rather than a
    // per-frame Step staircase.
    assert!(
        saw_interpolating_curve,
        "expected at least one interpolating (non-Step) automation point"
    );

    assert!(
        lane_count > 0,
        "Nemesis sub 1 should produce at least one automation lane"
    );
    // Nemesis sub 1 has both PWM and filter-swept patches.
    assert!(params.contains(&"cutoff"), "expected a cutoff lane");
    assert!(params.contains(&"pw_reg"), "expected a pw_reg lane");
    // Clean repeated ADSR now stays on the static envelope recipe. ADSR lanes
    // are therefore optional and only appear for a merged plan whose authored
    // register values genuinely change; anomalous SID contours use amplifier
    // automation instead.
}

/// Count (vibrato, glide, legato) notes in a synth export, validating the
/// schema-required field shapes of every vibrato/glide encountered.
fn count_pitch_effects(bytes: &[u8]) -> (usize, usize, usize) {
    let v: Value = serde_json::from_slice(bytes).expect("parses as JSON");
    let (mut vibrato, mut glide, mut legato) = (0usize, 0usize, 0usize);
    for pat in v["song"]["patterns"].as_array().expect("patterns") {
        for note in pat["notes"].as_array().expect("notes") {
            if note
                .get("expression")
                .and_then(|e| e.get("vibrato"))
                .is_some()
            {
                vibrato += 1;
                // Vibrato carries the four required schema fields with sane values.
                let vib = &note["expression"]["vibrato"];
                assert!(vib["depth"].as_f64().unwrap() > 0.0);
                assert!(vib["rate"].as_f64().unwrap() > 0.0);
                assert!(vib["delay"].as_f64().unwrap() >= 0.0);
                assert!(matches!(
                    vib["shape"].as_str().unwrap(),
                    "Sine" | "Triangle" | "Square" | "Saw"
                ));
            }
            if let Some(g) = note.get("glide") {
                glide += 1;
                assert!(g["from"]["Semitones"].as_f64().is_some());
                assert!(g["time"].as_f64().unwrap() > 0.0);
                assert_eq!(g["interp"].as_str().unwrap(), "Continuous");
            }
            if note.get("legato").and_then(Value::as_bool) == Some(true) {
                legato += 1;
            }
        }
    }
    (vibrato, glide, legato)
}

#[test]
#[cfg_attr(
    not(feature = "asset-tests"),
    ignore = "requires the optional assets/music corpus"
)]
fn synth_export_emits_per_note_pitch_effects() {
    // Each effect is exercised end-to-end by a tune that actually carries it.
    // Nemesis sub 1's portamento is all mid-note pitch oscillation (no onset slide
    // settling on a target), so it yields delayed vibrato as a track-pitch lane
    // but — correctly — no glide and no strict legato. Its only "slides" are
    // far-jump artifacts the exporter now
    // suppresses (a glide may not relocate a note more than an octave from its own
    // gated pitch — that is a percussive drop, not a melodic slide), so the glide
    // case uses Monty, whose stabs carry genuine falls. Those live past frame
    // 3000 — the earlier V2 stabs that used to satisfy this assertion were
    // mis-decomposed held notes (B3+glide over a held F#5, ~475 ct wrong) that
    // the forward gate now correctly degrades to the chip's pitches; the
    // surviving glides are the ones that verify against the trace. Counts are
    // lower bounds so threshold tweaks don't make this brittle.
    let sub1 = synth_bytes_for(1, 3000);
    let (vibrato, _, _) = count_pitch_effects(&sub1);
    let project: Value = serde_json::from_slice(&sub1).expect("project JSON");
    let pitch_lanes = project["song"]["patterns"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|pattern| pattern["automation"].as_array().into_iter().flatten())
        .filter(|lane| lane["target"]["Track"]["param"] == "Pitch")
        .count();
    assert!(
        vibrato > 0 || pitch_lanes > 0,
        "sub 1 should emit structured or track-automated vibrato"
    );

    let (_, glide, _) = count_pitch_effects(&synth_bytes_path(MONTY, 1, 6000));
    assert!(glide > 0, "Monty should emit trace-verified glide notes");

    let (_, _, legato) = count_pitch_effects(&synth_bytes_for(13, 3000));
    assert!(legato > 0, "sub 13 should emit legato-tied notes");
}

#[test]
fn raw_filtered_voice_keeps_filter_and_low_cutoff_leak_path() {
    let sid = synthetic_filtered_saw_sid(sid_analyzer::header::Clock::Pal);
    let fixture = SidPipeline::from_bytes(&sid, SubtuneIndex(1), 250);
    let bytes = synth_bytes_for_pipeline(&fixture);
    let project: Value = serde_json::from_slice(&bytes).expect("parses as JSON");
    let modules = project["instruments"][0]["patch"]["modules"]
        .as_array()
        .expect("modules");
    assert!(modules.iter().all(|module| module["type"] != "filter"));
    let return_effects = project["global"]["return_bus_effects"]
        .as_array()
        .expect("return bus effects");
    let filter = return_effects[0]["effects"]
        .as_array()
        .expect("shared filter effects")
        .iter()
        .find(|module| module["type"] == "filter")
        .expect("shared SID filter");
    assert_eq!(filter["parameters"]["cutoff"], 420.0);
    assert_eq!(
        project["song"]["return_busses"]
            .as_array()
            .expect("return busses")
            .len(),
        2
    );
    assert_eq!(
        project["song"]["tracks"][0]["sends"]
            .as_array()
            .expect("track sends")
            .len(),
        2
    );
}

#[test]
#[cfg_attr(
    not(feature = "asset-tests"),
    ignore = "requires the optional assets/music corpus"
)]
fn synth_export_validates_against_pertylizer_schema() {
    validate_schema(&synth_bytes(3000), "faithful");
}

#[test]
#[cfg_attr(
    not(feature = "asset-tests"),
    ignore = "requires the optional assets/music corpus"
)]
fn modern_synth_export_validates_against_pertylizer_schema() {
    validate_schema(&modern_synth_bytes(3000), "modern-analog");
}

#[test]
#[cfg_attr(
    not(feature = "asset-tests"),
    ignore = "requires the optional assets/music corpus"
)]
fn enhanced_synth_export_validates_against_pertylizer_schema() {
    validate_schema(&enhanced_synth_bytes(300, 5), "enhanced-balanced");
    validate_schema(&enhanced_synth_bytes(300, 10), "enhanced-heavy");
}

fn validate_schema(bytes: &[u8], variant: &str) {
    if !python_jsonschema_available() {
        eprintln!("skipping schema validation: python3 + jsonschema unavailable");
        return;
    }

    let mut tmp = std::env::temp_dir();
    tmp.push(format!("sid-analyzer-synth-export-{variant}-test.ptz"));
    {
        let mut file = std::fs::File::create(&tmp).expect("create temp file");
        file.write_all(bytes).expect("write temp file");
    }

    let script = format!(
        "import json, jsonschema, sys; \
         schema = json.load(open({schema:?})); \
         data = json.load(open({data:?})); \
         errs = list(jsonschema.Draft202012Validator(schema).iter_errors(data)); \
         print(len(errs)); \
         [print(list(e.absolute_path), e.message) for e in errs[:10]]; \
         sys.exit(1 if errs else 0)",
        schema = SCHEMA,
        data = tmp.to_string_lossy(),
    );
    let output = Command::new("python3")
        .args(["-c", &script])
        .output()
        .expect("run python3 validator");

    let _ = std::fs::remove_file(&tmp);

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "schema validation failed:\n{stdout}"
    );
}
