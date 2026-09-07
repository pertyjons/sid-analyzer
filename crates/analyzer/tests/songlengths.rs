use sid_analyzer::songlengths::{SongLengths, compute_sid_md5, format_duration};
use std::io::Write;
use std::time::Duration;

const SAMPLE: &[u8] = b"synthetic SID fixture A";
const SECOND_SAMPLE: &[u8] = b"synthetic SID fixture B";

#[test]
fn md5_matches_plain_file_md5() {
    // HVSC's new format keys on plain MD5 over the entire file.
    let ours = compute_sid_md5(SAMPLE);
    let our_hex: String = ours.iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(our_hex, "17d6e8fd5cca23b862450294da0ce347");
}

#[test]
fn md5_differs_between_distinct_files() {
    assert_ne!(compute_sid_md5(SAMPLE), compute_sid_md5(SECOND_SAMPLE));
}

#[test]
fn parses_songlengths_database_and_looks_up_known_hash() {
    let md5 = compute_sid_md5(SAMPLE);
    let md5_hex: String = md5.iter().map(|b| format!("{b:02x}")).collect();

    // Write a minimal HVSC-style database with one entry that matches our
    // computed MD5, plus some throwaway entries and comments/sections.
    let dir = tempdir();
    let path = dir.join("Songlengths.md5");
    let mut f = std::fs::File::create(&path).unwrap();
    writeln!(f, "[Database]").unwrap();
    writeln!(f, "Version: 7").unwrap();
    writeln!(f).unwrap();
    writeln!(f, ";Hubbard, Rob/Nemesis_the_Warlock").unwrap();
    writeln!(
        f,
        "{md5_hex}=2:38 0:46 1:35 0:54 1:22 1:00 0:48 0:42 0:38 0:30 1:00 1:00 0:36 1:00 1:30"
    )
    .unwrap();
    writeln!(f, "ffffffffffffffffffffffffffffffff=1:00 1:00").unwrap();
    drop(f);

    let db = SongLengths::load(&path).unwrap();
    assert_eq!(db.len(), 2);

    let lens = db.lookup(&md5).expect("Nemesis MD5 should be present");
    assert_eq!(lens.len(), 15, "15 subtunes per the header");
    assert_eq!(lens[0], Duration::from_secs(2 * 60 + 38));
    assert_eq!(lens[14], Duration::from_secs(90));
}

#[test]
fn parses_fractional_seconds_and_strips_flag_suffix() {
    let dir = tempdir();
    let path = dir.join("Songlengths.md5");
    let mut f = std::fs::File::create(&path).unwrap();
    // Real HVSC entries can carry trailing flags like "(L)" for looping
    // tunes; the parser must strip them.
    writeln!(
        f,
        "00112233445566778899aabbccddeeff=0:30.500 1:45(L) 2:00.250(X1)"
    )
    .unwrap();
    drop(f);

    let db = SongLengths::load(&path).unwrap();
    let key = [
        0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee,
        0xff,
    ];
    let lens = db.lookup(&key).unwrap();
    assert_eq!(lens[0], Duration::from_millis(30_500));
    assert_eq!(lens[1], Duration::from_secs(105));
    assert_eq!(lens[2], Duration::from_millis(120_250));
}

#[test]
fn malformed_durations_do_not_panic_or_shift_subtune_indices() {
    let dir = tempdir();
    let path = dir.join("Songlengths.md5");
    let invalid = [
        "18446744073709551615:00",
        "0:1e100",
        "0:NaN",
        "0:inf",
        "0:-1",
        "invalid",
    ];
    for token in invalid {
        std::fs::write(
            &path,
            format!("00000000000000000000000000000000=1:00 {token} 3:00\n"),
        )
        .unwrap();
        let db = SongLengths::load(&path).unwrap();
        assert!(db.lookup(&[0; 16]).is_none(), "accepted {token}");
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn missing_md5_returns_none() {
    let dir = tempdir();
    let path = dir.join("empty.md5");
    std::fs::File::create(&path).unwrap();
    let db = SongLengths::load(&path).unwrap();
    assert!(db.is_empty());
    assert_eq!(db.lookup(&[0; 16]), None);
}

#[test]
fn format_duration_uses_mm_ss() {
    assert_eq!(format_duration(Duration::from_secs(0)), "0:00");
    assert_eq!(format_duration(Duration::from_secs(58)), "0:58");
    assert_eq!(format_duration(Duration::from_secs(60)), "1:00");
    assert_eq!(format_duration(Duration::from_secs(158)), "2:38");
    assert_eq!(format_duration(Duration::from_secs(3725)), "62:05");
}

fn tempdir() -> std::path::PathBuf {
    let base = std::env::temp_dir();
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let dir = base.join(format!("sid-analyzer-test-{pid}-{nanos}"));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}
