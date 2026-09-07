use sid_analyzer::analysis::SystemClock;
use sid_analyzer::analysis::sid_program::AnalyzedSidProgram;
use sid_analyzer::analysis::sid_program::observable::{
    DigiTimingValidity, reconstruct_d418_streams,
};
use sid_analyzer::audio::SampleRate;
use sid_analyzer::emu::PlaybackTiming;
use sid_analyzer::export::synth::write_synth_quiet;
use sid_analyzer::header::{self, SubtuneIndex};

fn psid_with_init(init: &[u8], play: &[u8], speed: u32) -> Vec<u8> {
    const HEADER_LEN: usize = 0x7c;
    const LOAD: u16 = 0x1000;
    let mut bytes = vec![0; HEADER_LEN];
    bytes[0..4].copy_from_slice(b"PSID");
    bytes[4..6].copy_from_slice(&2_u16.to_be_bytes());
    bytes[6..8].copy_from_slice(&(HEADER_LEN as u16).to_be_bytes());
    bytes[8..10].copy_from_slice(&LOAD.to_be_bytes());
    bytes[10..12].copy_from_slice(&LOAD.to_be_bytes());
    bytes[12..14].copy_from_slice(&(LOAD + init.len() as u16).to_be_bytes());
    bytes[14..16].copy_from_slice(&1_u16.to_be_bytes());
    bytes[16..18].copy_from_slice(&1_u16.to_be_bytes());
    bytes[18..22].copy_from_slice(&speed.to_be_bytes());
    bytes.extend_from_slice(init);
    bytes.extend_from_slice(play);
    bytes
}

fn psid(play: &[u8]) -> Vec<u8> {
    psid_with_init(&[0x60], play, 0)
}

fn analyzed(play: &[u8], frames: u32) -> AnalyzedSidProgram {
    let bytes = psid(play);
    let header = header::parse(&bytes).unwrap();
    let trace = sid_analyzer::emu::run(&header, &bytes, SubtuneIndex(1), frames).unwrap();
    AnalyzedSidProgram::from_trace(
        &header,
        SubtuneIndex(1),
        PlaybackTiming::vblank(SystemClock::Pal),
        &trace,
    )
}

#[test]
fn dense_cycle_stamped_d418_writes_reconstruct_one_bounded_stream() {
    let program = analyzed(
        &[
            0xa9, 0x00, 0x8d, 0x18, 0xd4, 0xa9, 0x0f, 0x8d, 0x18, 0xd4, 0xa9, 0x04, 0x8d, 0x18,
            0xd4, 0xa9, 0x0c, 0x8d, 0x18, 0xd4, 0x60,
        ],
        2,
    );

    let streams = reconstruct_d418_streams(&program, SampleRate(44_100)).unwrap();
    assert_eq!(streams.len(), 1);
    let stream = &streams[0];
    assert_eq!(stream.source_events.len(), 8);
    assert!(stream.source_rate.0 > 1_000);
    assert!(!stream.samples.is_empty());
    assert_eq!(stream.timing, DigiTimingValidity::InstructionStartBounded);
    assert!(stream.source_span.end.0 < program.capture.checkpoints.last().unwrap().cycle.0);
    assert!(stream.samples.iter().any(|sample| sample.0 < 0));
    assert!(stream.samples.iter().any(|sample| sample.0 > 0));
}

#[test]
fn long_detected_digi_reaches_the_census_and_trips_the_master_lane_cap() {
    let program = analyzed(
        &[
            0xee, 0x00, 0x11, 0xa9, 0x00, 0x8d, 0x18, 0xd4, 0xa9, 0x0f, 0x8d, 0x18, 0xd4, 0xa9,
            0x04, 0x8d, 0x18, 0xd4, 0xad, 0x00, 0x11, 0x29, 0x01, 0xf0, 0x02, 0xa9, 0x0f, 0x8d,
            0x18, 0xd4, 0x60,
        ],
        600,
    );
    let mut project = Vec::new();
    let census = write_synth_quiet(&program, &mut project).expect("synth export");
    let census_value = serde_json::to_value(&census).expect("census JSON");
    assert!(census_value["d418_volume_changes"].as_u64().unwrap_or(0) > 512);
    assert_eq!(census_value["d418_digi_dropped"], true);
    let summary = census.summary();
    assert_eq!(summary.d418_pcm_streams, 1);
    assert!(summary.d418_pcm_events > 512);

    let project: serde_json::Value = serde_json::from_slice(&project).expect("project JSON");
    let has_master_volume_lane = project["song"]["patterns"]
        .as_array()
        .expect("patterns")
        .iter()
        .flat_map(|pattern| pattern["automation"].as_array().into_iter().flatten())
        .any(|lane| lane["target"].get("Global").is_some());
    assert!(
        !has_master_volume_lane,
        "digi samples must not modulate the entire master mix"
    );
}

#[test]
fn ordinary_master_volume_updates_do_not_reconstruct_pcm() {
    let program = analyzed(&[0xa9, 0x0f, 0x8d, 0x18, 0xd4, 0x60], 4);
    assert!(
        reconstruct_d418_streams(&program, SampleRate(44_100))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn one_sample_per_fast_cia_call_reconstructs_across_call_boundaries() {
    let init = [
        0xa9, 0xff, 0x8d, 0x04, 0xdc, 0xa9, 0x01, 0x8d, 0x05, 0xdc, 0x60,
    ];
    let bytes = psid_with_init(&init, &[0xa9, 0x08, 0x8d, 0x18, 0xd4, 0x60], 1);
    let header = header::parse(&bytes).unwrap();
    let trace = sid_analyzer::emu::run(&header, &bytes, SubtuneIndex(1), 6).unwrap();
    let timing = PlaybackTiming::for_subtune(&header, SubtuneIndex(1)).resolved_from_trace(&trace);
    let program = AnalyzedSidProgram::from_trace(&header, SubtuneIndex(1), timing, &trace);

    let streams = reconstruct_d418_streams(&program, SampleRate(44_100)).unwrap();
    assert_eq!(streams.len(), 1);
    assert_eq!(streams[0].source_events.len(), 6);
    assert!(streams[0].source_rate.0 > 1_000);
}
