use sid_analyzer::header::{
    self, Clock, Error, Flags, Format, Header, InitAddress, LoadAddress, PlayAddress,
    SidBaseAddress, SidModel, SpeedBitmask, SubtuneCount, SubtuneIndex,
};

const V2_LEN: usize = 0x7C;

/// Build a minimal valid PSID v2 header. Caller may patch bytes.
fn v2_template() -> Vec<u8> {
    let mut b = vec![0u8; V2_LEN];
    b[0..4].copy_from_slice(b"PSID");
    b[4..6].copy_from_slice(&2u16.to_be_bytes());
    b[6..8].copy_from_slice(&0x7C_u16.to_be_bytes());
    b[8..10].copy_from_slice(&0x1000_u16.to_be_bytes());
    b[10..12].copy_from_slice(&0x2000_u16.to_be_bytes());
    b[12..14].copy_from_slice(&0x3000_u16.to_be_bytes());
    b[14..16].copy_from_slice(&1u16.to_be_bytes());
    b[16..18].copy_from_slice(&1u16.to_be_bytes());
    b[18..22].copy_from_slice(&0u32.to_be_bytes());
    write_str(&mut b, 0x16, "Title");
    write_str(&mut b, 0x36, "Author");
    write_str(&mut b, 0x56, "1985");
    b[0x76..0x78].copy_from_slice(&0u16.to_be_bytes());
    b
}

fn write_str(b: &mut [u8], off: usize, s: &str) {
    let bytes = s.as_bytes();
    let n = bytes.len().min(31);
    b[off..off + n].copy_from_slice(&bytes[..n]);
    b[off + n] = 0;
}

fn parse(b: &[u8]) -> Header {
    header::parse(b).expect("parse succeeded")
}

#[test]
fn parses_psid_v2_minimal() {
    let b = v2_template();
    let h = parse(&b);
    assert_eq!(h.format, Format::Psid);
    assert_eq!(h.version, 2);
    assert_eq!(h.data_offset, 0x7C);
    assert_eq!(h.load_address, LoadAddress(0x1000));
    assert_eq!(h.init_address, InitAddress(0x2000));
    assert_eq!(h.play_address, PlayAddress(0x3000));
    assert_eq!(h.songs, SubtuneCount(1));
    assert_eq!(h.start_song, SubtuneIndex(1));
    assert_eq!(h.speed, SpeedBitmask(0));
    assert_eq!(h.name, "Title");
    assert_eq!(h.author, "Author");
    assert_eq!(h.released, "1985");
    assert_eq!(h.flags, Flags::default());
    assert_eq!(h.second_sid_address, None);
    assert_eq!(h.third_sid_address, None);
}

#[test]
fn parses_psid_v3_second_sid() {
    let mut b = v2_template();
    b[4..6].copy_from_slice(&3u16.to_be_bytes());
    b[0x7A] = 0x42;
    let h = parse(&b);
    assert_eq!(h.version, 3);
    assert_eq!(h.second_sid_address, Some(SidBaseAddress(0x42)));
    assert_eq!(h.third_sid_address, None);
}

#[test]
fn parses_psid_v4_third_sid() {
    let mut b = v2_template();
    b[4..6].copy_from_slice(&4u16.to_be_bytes());
    b[0x7A] = 0x42;
    b[0x7B] = 0x80;
    let h = parse(&b);
    assert_eq!(h.version, 4);
    assert_eq!(h.second_sid_address, Some(SidBaseAddress(0x42)));
    assert_eq!(h.third_sid_address, Some(SidBaseAddress(0x80)));
}

#[test]
fn parses_rsid_v2() {
    let b = rsid_template();
    let h = parse(&b);
    assert_eq!(h.format, Format::Rsid);
}

#[test]
fn decodes_strings_cp1252() {
    let mut b = v2_template();
    b[0x16..0x36].fill(0);
    b[0x16] = b'C';
    b[0x17] = b'a';
    b[0x18] = b'f';
    b[0x19] = 0xE9;
    let h = parse(&b);
    assert_eq!(h.name, "Café");
}

#[test]
fn truncates_string_at_null() {
    let mut b = v2_template();
    b[0x16..0x36].fill(0);
    b[0x16] = b'A';
    b[0x17] = b'B';
    b[0x18] = b'C';
    b[0x19] = 0;
    b[0x1A] = b'X';
    b[0x1B] = b'Y';
    b[0x1C] = b'Z';
    let h = parse(&b);
    assert_eq!(h.name, "ABC");
}

#[test]
fn flags_clock_and_sid_model() {
    let cases: [(u16, Clock); 4] = [
        (0b00_00 << 2, Clock::Unknown),
        (0b00_01 << 2, Clock::Pal),
        (0b00_10 << 2, Clock::Ntsc),
        (0b00_11 << 2, Clock::Both),
    ];
    for (raw, expected) in cases {
        let mut b = v2_template();
        b[0x76..0x78].copy_from_slice(&raw.to_be_bytes());
        let h = parse(&b);
        assert_eq!(h.flags.clock, expected, "raw={raw:#06x}");
    }

    let sid_cases: [(u16, SidModel); 4] = [
        (0b00_00 << 4, SidModel::Unknown),
        (0b00_01 << 4, SidModel::Mos6581),
        (0b00_10 << 4, SidModel::Mos8580),
        (0b00_11 << 4, SidModel::Both),
    ];
    for (raw, expected) in sid_cases {
        let mut b = v2_template();
        b[0x76..0x78].copy_from_slice(&raw.to_be_bytes());
        let h = parse(&b);
        assert_eq!(h.flags.sid_model, expected, "raw={raw:#06x}");
    }
}

#[test]
fn flags_secondary_sid_models_gated_by_version() {
    let mut b = v2_template();
    let raw: u16 = (0b01 << 6) | (0b10 << 8);
    b[0x76..0x78].copy_from_slice(&raw.to_be_bytes());
    let h = parse(&b);
    assert_eq!(h.flags.sid_model_2, None);
    assert_eq!(h.flags.sid_model_3, None);

    b[4..6].copy_from_slice(&3u16.to_be_bytes());
    let h = parse(&b);
    assert_eq!(h.flags.sid_model_2, Some(SidModel::Mos6581));
    assert_eq!(h.flags.sid_model_3, None);

    b[4..6].copy_from_slice(&4u16.to_be_bytes());
    let h = parse(&b);
    assert_eq!(h.flags.sid_model_2, Some(SidModel::Mos6581));
    assert_eq!(h.flags.sid_model_3, Some(SidModel::Mos8580));
}

#[test]
fn speed_bitmask_is_cia_timed() {
    let mut s = parse(&v2_template());
    s.songs = SubtuneCount(256);
    s.speed = SpeedBitmask((1 << 0) | (1 << 5) | (1 << 31));
    assert!(s.is_cia_timed(SubtuneIndex(1)));
    assert!(!s.is_cia_timed(SubtuneIndex(2)));
    assert!(s.is_cia_timed(SubtuneIndex(6)));
    assert!(!s.is_cia_timed(SubtuneIndex(7)));
    assert!(s.is_cia_timed(SubtuneIndex(32)));
    assert!(s.is_cia_timed(SubtuneIndex(33)));
    assert!(s.is_cia_timed(SubtuneIndex(256)));
    assert!(!s.is_cia_timed(SubtuneIndex(257)));
    assert!(!s.is_cia_timed(SubtuneIndex(0)));
}

#[test]
fn speed_bitmask_classify_mixed() {
    let mut s = parse(&v2_template());
    s.speed = SpeedBitmask(0b1010);
    s.songs = SubtuneCount(5);
    let summary = s.speed_summary();
    assert_eq!(
        summary.vblank,
        vec![SubtuneIndex(1), SubtuneIndex(3), SubtuneIndex(5)]
    );
    assert_eq!(summary.cia, vec![SubtuneIndex(2), SubtuneIndex(4)]);
}

#[test]
fn rejects_short_file() {
    let b = vec![0u8; 100];
    let e = header::parse(&b).unwrap_err();
    assert!(matches!(e, Error::TooShort { got: 100, .. }));
}

#[test]
fn rejects_bad_magic() {
    let mut b = v2_template();
    b[0..4].copy_from_slice(b"XSID");
    let e = header::parse(&b).unwrap_err();
    assert!(matches!(e, Error::BadMagic(_)));
}

#[test]
fn rejects_unsupported_version_zero() {
    let mut b = v2_template();
    b[4..6].copy_from_slice(&0u16.to_be_bytes());
    let e = header::parse(&b).unwrap_err();
    assert!(matches!(e, Error::UnsupportedVersion(0)));
}

#[test]
fn rejects_unsupported_version_five() {
    let mut b = v2_template();
    b[4..6].copy_from_slice(&5u16.to_be_bytes());
    let e = header::parse(&b).unwrap_err();
    assert!(matches!(e, Error::UnsupportedVersion(5)));
}

#[test]
fn rejects_inconsistent_data_offset() {
    let mut b = v2_template();
    b[6..8].copy_from_slice(&0x76_u16.to_be_bytes());
    let e = header::parse(&b).unwrap_err();
    assert!(matches!(e, Error::BadDataOffset(0x76, 2)));
}

#[test]
fn embedded_load_address_decoded() {
    let mut b = v2_template();
    b[8..10].copy_from_slice(&0u16.to_be_bytes());
    b.extend_from_slice(&0x1000_u16.to_le_bytes());
    let h = parse(&b);
    assert_eq!(h.load_address, LoadAddress(0));
    assert_eq!(h.effective_load_address(&b).unwrap(), LoadAddress(0x1000));
}

#[test]
fn embedded_load_address_missing_errors() {
    let mut b = v2_template();
    b[8..10].copy_from_slice(&0u16.to_be_bytes());
    let h = parse(&b);
    let e = h.effective_load_address(&b).unwrap_err();
    assert!(matches!(e, Error::MissingEmbeddedLoadAddress));
}

#[test]
fn sid_base_address_display() {
    assert_eq!(SidBaseAddress(0x42).to_string(), "$D420");
    assert_eq!(SidBaseAddress(0xE0).to_string(), "$DE00");
}

#[test]
fn address_newtypes_display_hex() {
    assert_eq!(LoadAddress(0xF160).to_string(), "$F160");
    assert_eq!(InitAddress(0).to_string(), "$0000");
    assert_eq!(PlayAddress(0x1234).to_string(), "$1234");
}

fn rsid_template() -> Vec<u8> {
    let mut bytes = v2_template();
    bytes[..4].copy_from_slice(b"RSID");
    bytes[8..10].fill(0);
    bytes[10..12].copy_from_slice(&0x1000_u16.to_be_bytes());
    bytes[12..14].fill(0);
    bytes.extend_from_slice(&[0x00, 0x10, 0x60]);
    bytes
}

#[test]
fn speed_policy_covers_all_subtunes_for_each_header_mode() {
    for version in 1..=4 {
        for specific in [false, true] {
            let mut h = parse(&v2_template());
            h.version = version;
            h.flags.psid_specific = specific;
            h.songs = SubtuneCount(256);
            for mask in [1, 2, 0x80000000, 0x80000001] {
                h.speed = SpeedBitmask(mask);
                let summary = h.speed_summary();
                assert_eq!(summary.cia.len() + summary.vblank.len(), 256);
                for subtune in [1, 2, 31, 32, 33, 34, 64, 65, 255, 256] {
                    let bit = if version == 1 || specific {
                        (subtune - 1) % 32
                    } else {
                        (subtune - 1).min(31)
                    };
                    let expected = mask & (1 << bit) != 0;
                    assert_eq!(
                        h.is_cia_timed(SubtuneIndex(subtune)),
                        expected,
                        "version={version}, specific={specific}, mask={mask:x}, subtune={subtune}"
                    );
                    assert_eq!(summary.cia.contains(&SubtuneIndex(subtune)), expected);
                }
            }
            h.songs = SubtuneCount(32);
            assert!(!h.is_cia_timed(SubtuneIndex(33)));
        }
    }
}

#[test]
fn validates_song_count_and_default_subtune() {
    for songs in [0u16, 257, u16::MAX] {
        let mut bytes = v2_template();
        bytes[14..16].copy_from_slice(&songs.to_be_bytes());
        assert!(matches!(
            header::parse(&bytes),
            Err(Error::InvalidSongCount(_))
        ));
    }
    for start in [0u16, 2, u16::MAX] {
        let mut bytes = v2_template();
        bytes[16..18].copy_from_slice(&start.to_be_bytes());
        assert!(matches!(
            header::parse(&bytes),
            Err(Error::InvalidStartSong { .. })
        ));
    }
    let mut bytes = v2_template();
    bytes[14..16].copy_from_slice(&256u16.to_be_bytes());
    bytes[16..18].copy_from_slice(&256u16.to_be_bytes());
    assert_eq!(parse(&bytes).start_song, SubtuneIndex(256));
}

#[test]
fn rejects_rsid_reserved_fields_and_version_one() {
    for version in 2u16..=4 {
        let mut bytes = rsid_template();
        bytes[4..6].copy_from_slice(&version.to_be_bytes());
        assert_eq!(parse(&bytes).version, version);
        for offset in [9, 13, 21] {
            let mut invalid = bytes.clone();
            invalid[offset] = 1;
            let error = header::parse(&invalid).unwrap_err();
            assert!(matches!(
                (offset, error),
                (9, Error::RsidLoadAddress(_))
                    | (13, Error::RsidPlayAddress(_))
                    | (21, Error::RsidSpeed(_))
            ));
        }
    }
    let mut bytes = rsid_template();
    bytes[4..6].copy_from_slice(&1u16.to_be_bytes());
    bytes[6..8].copy_from_slice(&0x76u16.to_be_bytes());
    assert!(matches!(
        header::parse(&bytes),
        Err(Error::UnsupportedVersion(1))
    ));
}

#[test]
fn validates_rsid_load_and_basic_init_constraints() {
    let mut bytes = rsid_template();
    bytes.truncate(V2_LEN + 1);
    assert!(matches!(
        header::parse(&bytes),
        Err(Error::MissingEmbeddedLoadAddress)
    ));
    for load in [0u16, 0x07e7, 0x07e8, 0x1000, 0xffff] {
        let mut bytes = rsid_template();
        bytes[V2_LEN..V2_LEN + 2].copy_from_slice(&load.to_le_bytes());
        assert_eq!(header::parse(&bytes).is_ok(), load >= 0x07e8);
    }
    for init in [
        0u16, 0x07e7, 0x07e8, 0x9fff, 0xa000, 0xbfff, 0xc000, 0xcfff, 0xd000, 0xffff,
    ] {
        let mut bytes = rsid_template();
        bytes[10..12].copy_from_slice(&init.to_be_bytes());
        assert_eq!(
            header::parse(&bytes).is_ok(),
            matches!(init, 0 | 0x07e8..=0x9fff | 0xc000..=0xcfff)
        );
        bytes[0x77] = 2;
        assert_eq!(header::parse(&bytes).is_ok(), init == 0);
    }
    let mut bytes = rsid_template();
    bytes[10..12].fill(0);
    bytes[V2_LEN..V2_LEN + 2].copy_from_slice(&0xa000u16.to_le_bytes());
    assert!(matches!(
        header::parse(&bytes),
        Err(Error::RsidInitAddress(_))
    ));
}
