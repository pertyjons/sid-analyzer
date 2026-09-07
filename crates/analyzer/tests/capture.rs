use sid_analyzer::emu::capture::{
    CapturedSidExecution, CheckpointCadence, CheckpointPolicy, CheckpointRef, SidBusAccess,
    SidCallId,
};
use sid_analyzer::header::{self, SubtuneIndex};
use sid_analyzer::trace::{RegisterRead, RegisterWrite};

fn psid(init: &[u8], play: &[u8]) -> Vec<u8> {
    const HEADER_LEN: usize = 0x7c;
    const LOAD: u16 = 0x1000;
    let play_address = LOAD + init.len() as u16;
    let mut bytes = vec![0; HEADER_LEN];
    bytes[0..4].copy_from_slice(b"PSID");
    bytes[4..6].copy_from_slice(&2_u16.to_be_bytes());
    bytes[6..8].copy_from_slice(&(HEADER_LEN as u16).to_be_bytes());
    bytes[8..10].copy_from_slice(&LOAD.to_be_bytes());
    bytes[10..12].copy_from_slice(&LOAD.to_be_bytes());
    bytes[12..14].copy_from_slice(&play_address.to_be_bytes());
    bytes[14..16].copy_from_slice(&1_u16.to_be_bytes());
    bytes[16..18].copy_from_slice(&1_u16.to_be_bytes());
    bytes.extend_from_slice(init);
    bytes.extend_from_slice(play);
    bytes
}

#[test]
fn rmw_read_and_write_keep_one_ordered_event_stream() {
    let bytes = psid(
        &[0xa9, 0x0f, 0x8d, 0x18, 0xd4, 0xee, 0x18, 0xd4, 0x60],
        &[0x60],
    );
    let header = header::parse(&bytes).unwrap();
    let trace = sid_analyzer::emu::run(&header, &bytes, SubtuneIndex(1), 1).unwrap();
    let events: Vec<_> = trace
        .capture
        .events
        .iter()
        .filter(|event| event.address.0 == 0xd418)
        .collect();

    assert_eq!(events.len(), 3);
    assert_eq!(events[0].access, SidBusAccess::Write);
    assert_eq!(events[0].value, 0x0f);
    assert_eq!(events[1].access, SidBusAccess::Read);
    assert_eq!(events[1].value, 0x0f);
    assert_eq!(events[2].access, SidBusAccess::Write);
    assert_eq!(events[2].value, 0x10);
    assert_eq!(events[1].offset, events[2].offset);
    assert!(events.windows(2).all(|pair| pair[0].id < pair[1].id));
}

#[test]
fn init_and_play_share_ids_and_legacy_vectors_are_projections() {
    let bytes = psid(
        &[0xa9, 0x11, 0x8d, 0x00, 0xd4, 0x60],
        &[0xee, 0x18, 0xd4, 0x60],
    );
    let header = header::parse(&bytes).unwrap();
    let trace = sid_analyzer::emu::run(&header, &bytes, SubtuneIndex(1), 2).unwrap();

    assert!(
        trace
            .capture
            .events
            .windows(2)
            .all(|pair| pair[0].id < pair[1].id)
    );
    assert!(
        trace
            .capture
            .events
            .iter()
            .any(|event| event.call == SidCallId::Init)
    );
    assert!(
        trace
            .capture
            .events
            .iter()
            .any(|event| event.call == SidCallId::Play(sid_analyzer::trace::FrameIndex(1)))
    );

    let init_writes: Vec<RegisterWrite> = trace
        .capture
        .events
        .iter()
        .filter(|event| event.call == SidCallId::Init)
        .filter_map(|event| event.as_write())
        .collect();
    let init_reads: Vec<RegisterRead> = trace
        .capture
        .events
        .iter()
        .filter(|event| event.call == SidCallId::Init)
        .filter_map(|event| event.as_read())
        .collect();
    assert_eq!(trace.init_writes, init_writes);
    assert_eq!(trace.init_reads, init_reads);
    for frame in &trace.frames {
        let call = SidCallId::Play(frame.frame);
        let writes: Vec<_> = trace
            .capture
            .events
            .iter()
            .filter(|event| event.call == call)
            .filter_map(|event| event.as_write())
            .collect();
        let reads: Vec<_> = trace
            .capture
            .events
            .iter()
            .filter(|event| event.call == call)
            .filter_map(|event| event.as_read())
            .collect();
        assert_eq!(frame.writes, writes);
        assert_eq!(frame.reads, reads);
    }
}

#[test]
fn capture_json_is_deterministic_strict_and_replayable() {
    let bytes = psid(
        &[0xa9, 0x11, 0x8d, 0x00, 0xd4, 0x60],
        &[0xee, 0x18, 0xd4, 0x60],
    );
    let header = header::parse(&bytes).unwrap();
    let trace = sid_analyzer::emu::run(&header, &bytes, SubtuneIndex(1), 8).unwrap();

    let mut first = Vec::new();
    trace.capture.to_json(&mut first, true).unwrap();
    let decoded = CapturedSidExecution::from_json(first.as_slice()).unwrap();
    let mut second = Vec::new();
    decoded.to_json(&mut second, true).unwrap();
    assert_eq!(first, second);
    assert_eq!(decoded, trace.capture);

    for report in decoded.replay_all_checkpoints().unwrap() {
        assert!(report.exact(), "{report:?}");
    }
    assert!(
        decoded
            .checkpoints
            .iter()
            .any(|checkpoint| checkpoint.reference == CheckpointRef::InitReturn)
    );
    assert!(
        decoded
            .checkpoints
            .iter()
            .any(|checkpoint| checkpoint.reference == CheckpointRef::FirstPlayBoundary)
    );

    let mut value: serde_json::Value = serde_json::from_slice(&first).unwrap();
    value
        .as_object_mut()
        .unwrap()
        .insert("future_field".to_owned(), serde_json::Value::Bool(true));
    let invalid = serde_json::to_vec(&value).unwrap();
    assert!(CapturedSidExecution::from_json(invalid.as_slice()).is_err());
}

#[test]
#[cfg_attr(
    not(feature = "asset-tests"),
    ignore = "requires the optional assets/music corpus"
)]
fn named_vblank_and_cia_fixtures_replay_from_every_checkpoint() {
    for name in ["Auf_Wiedersehen_Monty.sid", "Warhawk.sid", "Human_Race.sid"] {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../assets/music")
            .join(name);
        let bytes = std::fs::read(path).unwrap();
        let header = header::parse(&bytes).unwrap();
        let subtune = header.start_song;
        let timing = sid_analyzer::emu::PlaybackTiming::for_subtune(&header, subtune);
        let trace =
            sid_analyzer::emu::run_with_timing(&header, &bytes, subtune, 4, timing).unwrap();
        for report in trace.capture.replay_all_checkpoints().unwrap() {
            assert!(report.exact(), "{name}: {report:?}");
        }
    }
}

#[test]
fn sparse_checkpoint_policy_preserves_replay() {
    let bytes = psid(
        &[0xa9, 0x11, 0x8d, 0x00, 0xd4, 0x60],
        &[0xee, 0x18, 0xd4, 0x60],
    );
    let header = header::parse(&bytes).unwrap();
    let trace = sid_analyzer::emu::run(&header, &bytes, SubtuneIndex(1), 8).unwrap();
    let mut capture = trace.capture;
    let dense_count = capture.checkpoints.len();
    capture
        .retain_sparse_checkpoints(CheckpointCadence(3))
        .unwrap();
    assert!(capture.checkpoints.len() < dense_count);
    assert_eq!(
        capture.checkpoint_policy,
        CheckpointPolicy::SparseEveryCalls {
            calls: CheckpointCadence(3)
        }
    );
    for report in capture.replay_all_checkpoints().unwrap() {
        assert!(report.exact(), "{report:?}");
    }
}

#[test]
fn live_sparse_policy_and_event_checkpoints_are_replayable() {
    let bytes = psid(
        &[0xa9, 0x11, 0x8d, 0x00, 0xd4, 0x60],
        &[0xee, 0x18, 0xd4, 0x60],
    );
    let header = header::parse(&bytes).unwrap();
    let subtune = SubtuneIndex(1);
    let timing = sid_analyzer::emu::PlaybackTiming::for_subtune(&header, subtune);
    let mut capture = sid_analyzer::emu::run_with_checkpoint_policy(
        &header,
        &bytes,
        subtune,
        8,
        timing,
        CheckpointPolicy::SparseEveryCalls {
            calls: CheckpointCadence(3),
        },
    )
    .unwrap()
    .capture;
    assert_eq!(capture.checkpoints.len(), 8);
    let selected = [capture.events[1].id, capture.events[3].id]
        .into_iter()
        .collect();
    capture.add_event_checkpoints(&selected).unwrap();
    assert_eq!(
        capture
            .checkpoints
            .iter()
            .filter(|checkpoint| matches!(checkpoint.reference, CheckpointRef::Event(_)))
            .count(),
        2
    );
    for report in capture.replay_all_checkpoints().unwrap() {
        assert!(report.exact(), "{report:?}");
    }
    assert!(capture.observations.iter().all(|observation| {
        observation.oscillator_end.sync_resets >= observation.oscillator_start.sync_resets
            && observation.oscillator_end.source_msb_edges
                >= observation.oscillator_start.source_msb_edges
    }));
}

#[test]
fn modeled_mirror_registers_replay_and_unresolved_offsets_stay_diagnostic() {
    let bytes = psid(
        &[
            0xa9, 0x12, 0x8d, 0x20, 0xd4, 0xad, 0x20, 0xd4, 0x8d, 0x3d, 0xd4, 0x60,
        ],
        &[0x60],
    );
    let header = header::parse(&bytes).unwrap();
    let trace = sid_analyzer::emu::run(&header, &bytes, SubtuneIndex(1), 1).unwrap();
    let modeled: Vec<_> = trace
        .capture
        .events
        .iter()
        .filter(|event| event.address.0 == 0xd420)
        .collect();
    assert_eq!(modeled.len(), 2);
    assert!(
        modeled
            .iter()
            .all(|event| event.register == Some(sid_analyzer::trace::SidRegister(0)))
    );
    assert_eq!(modeled[1].value, 0x12);
    assert!(
        trace
            .capture
            .diagnostics
            .iter()
            .all(|diagnostic| diagnostic.event != Some(modeled[0].id))
    );
    assert!(
        trace
            .capture
            .events
            .iter()
            .any(|event| event.address.0 == 0xd43d && event.register.is_none())
    );
}
