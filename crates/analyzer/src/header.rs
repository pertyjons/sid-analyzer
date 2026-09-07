use crate::hex_newtype;
use encoding_rs::WINDOWS_1252;
use serde::{Deserialize, Serialize};
use std::fmt;

const HEADER_V1_SIZE: u16 = 0x76;
const HEADER_V2_SIZE: u16 = 0x7C;

const STRING_FIELD_LEN: usize = 32;

pub const MAX_SUBTUNES: SubtuneCount = SubtuneCount(256);

const OFF_MAGIC: usize = 0x00;
const OFF_VERSION: usize = 0x04;
const OFF_DATA_OFFSET: usize = 0x06;
const OFF_LOAD_ADDRESS: usize = 0x08;
const OFF_INIT_ADDRESS: usize = 0x0A;
const OFF_PLAY_ADDRESS: usize = 0x0C;
const OFF_SONGS: usize = 0x0E;
const OFF_START_SONG: usize = 0x10;
const OFF_SPEED: usize = 0x12;
const OFF_NAME: usize = 0x16;
const OFF_AUTHOR: usize = 0x36;
const OFF_RELEASED: usize = 0x56;
const OFF_FLAGS: usize = 0x76;
const OFF_START_PAGE: usize = 0x78;
const OFF_PAGE_LENGTH: usize = 0x79;
const OFF_SECOND_SID: usize = 0x7A;
const OFF_THIRD_SID: usize = 0x7B;

const FLAG_BIT_MUS_DATA: u16 = 1 << 0;
const FLAG_BIT_PSID_SPECIFIC: u16 = 1 << 1;
const FLAG_2BIT_MASK: u16 = 0b11;
const FLAG_SHIFT_CLOCK: u16 = 2;
const FLAG_SHIFT_SID_MODEL: u16 = 4;
const FLAG_SHIFT_SID_MODEL_2: u16 = 6;
const FLAG_SHIFT_SID_MODEL_3: u16 = 8;

hex_newtype!(LoadAddress, u16, "${:04X}");
hex_newtype!(InitAddress, u16, "${:04X}");
hex_newtype!(PlayAddress, u16, "${:04X}");

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
#[must_use]
pub struct SubtuneIndex(pub u16);

impl fmt::Display for SubtuneIndex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(transparent)]
#[must_use]
pub struct SubtuneCount(pub u16);

impl fmt::Display for SubtuneCount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// A SID chip base-address byte from the PSID v3/v4 header. The byte encodes
/// bits 4-11 of the actual `$D000`-page address — i.e. `0x42` means `$D420`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(transparent)]
#[must_use]
pub struct SidBaseAddress(pub u8);

impl fmt::Display for SidBaseAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "$D{:02X}0", self.0)
    }
}

impl SidBaseAddress {
    #[must_use]
    pub fn address(self) -> u16 {
        0xd000 | (u16::from(self.0) << 4)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(transparent)]
#[must_use]
pub struct SpeedBitmask(pub u32);

impl Header {
    #[must_use]
    pub fn is_cia_timed(&self, subtune: SubtuneIndex) -> bool {
        let one_based = subtune.0;
        if one_based == 0 || one_based > self.songs.0.min(MAX_SUBTUNES.0) {
            return false;
        }
        let bit = if self.version == 1 || self.flags.psid_specific {
            (one_based - 1) % 32
        } else {
            (one_based - 1).min(31)
        };
        (self.speed.0 >> bit) & 1 == 1
    }

    #[must_use]
    pub fn speed_summary(&self) -> SpeedSummary {
        let listed = self.songs.0.min(MAX_SUBTUNES.0);
        let mut vblank = Vec::new();
        let mut cia = Vec::new();
        for s in 1..=listed {
            let idx = SubtuneIndex(s);
            if self.is_cia_timed(idx) {
                cia.push(idx);
            } else {
                vblank.push(idx);
            }
        }
        SpeedSummary { vblank, cia }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpeedSummary {
    pub vblank: Vec<SubtuneIndex>,
    pub cia: Vec<SubtuneIndex>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Format {
    Psid,
    Rsid,
}

impl fmt::Display for Format {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Psid => "PSID",
            Self::Rsid => "RSID",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub enum Clock {
    #[default]
    Unknown,
    Pal,
    Ntsc,
    Both,
}

impl Clock {
    fn from_bits(bits: u16) -> Self {
        match bits & FLAG_2BIT_MASK {
            0 => Self::Unknown,
            1 => Self::Pal,
            2 => Self::Ntsc,
            _ => Self::Both,
        }
    }
}

impl fmt::Display for Clock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Unknown => "unknown",
            Self::Pal => "PAL",
            Self::Ntsc => "NTSC",
            Self::Both => "PAL+NTSC",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum SidModel {
    #[default]
    Unknown,
    Mos6581,
    Mos8580,
    Both,
}

impl SidModel {
    fn from_bits(bits: u16) -> Self {
        match bits & FLAG_2BIT_MASK {
            0 => Self::Unknown,
            1 => Self::Mos6581,
            2 => Self::Mos8580,
            _ => Self::Both,
        }
    }
}

impl fmt::Display for SidModel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Unknown => "unknown",
            Self::Mos6581 => "MOS6581",
            Self::Mos8580 => "MOS8580",
            Self::Both => "MOS6581+MOS8580",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
pub struct Flags {
    pub mus_data: bool,
    pub psid_specific: bool,
    pub clock: Clock,
    pub sid_model: SidModel,
    pub sid_model_2: Option<SidModel>,
    pub sid_model_3: Option<SidModel>,
}

impl Flags {
    fn from_raw(raw: u16, version: u16) -> Self {
        Self {
            mus_data: raw & FLAG_BIT_MUS_DATA != 0,
            psid_specific: raw & FLAG_BIT_PSID_SPECIFIC != 0,
            clock: Clock::from_bits(raw >> FLAG_SHIFT_CLOCK),
            sid_model: SidModel::from_bits(raw >> FLAG_SHIFT_SID_MODEL),
            sid_model_2: (version >= 3).then(|| SidModel::from_bits(raw >> FLAG_SHIFT_SID_MODEL_2)),
            sid_model_3: (version >= 4).then(|| SidModel::from_bits(raw >> FLAG_SHIFT_SID_MODEL_3)),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Header {
    pub format: Format,
    pub version: u16,
    pub data_offset: u16,
    pub load_address: LoadAddress,
    pub init_address: InitAddress,
    pub play_address: PlayAddress,
    pub songs: SubtuneCount,
    pub start_song: SubtuneIndex,
    pub speed: SpeedBitmask,
    pub name: String,
    pub author: String,
    pub released: String,
    pub flags: Flags,
    pub start_page: u8,
    pub page_length: u8,
    pub second_sid_address: Option<SidBaseAddress>,
    pub third_sid_address: Option<SidBaseAddress>,
}

impl Header {
    pub fn effective_load_address(&self, bytes: &[u8]) -> Result<LoadAddress, Error> {
        if self.load_address.0 != 0 {
            return Ok(self.load_address);
        }
        let off = self.data_offset as usize;
        let need = off + 2;
        if bytes.len() < need {
            return Err(Error::MissingEmbeddedLoadAddress);
        }
        let lo = bytes[off];
        let hi = bytes[off + 1];
        Ok(LoadAddress(u16::from_le_bytes([lo, hi])))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("file too short: {got} bytes, need at least {need}")]
    TooShort { got: usize, need: usize },
    #[error("bad magic: {0:?} (expected PSID or RSID)")]
    BadMagic([u8; 4]),
    #[error("unsupported version: {0}")]
    UnsupportedVersion(u16),
    #[error("data offset {0:#x} inconsistent with version {1}")]
    BadDataOffset(u16, u16),
    #[error("load address is 0 but data section has no embedded LE address")]
    MissingEmbeddedLoadAddress,
    #[error("song count {0} is outside 1..=256")]
    InvalidSongCount(SubtuneCount),
    #[error("start song {start} is outside 1..={songs}")]
    InvalidStartSong {
        start: SubtuneIndex,
        songs: SubtuneCount,
    },
    #[error("RSID header load address must be zero, got {0}")]
    RsidLoadAddress(LoadAddress),
    #[error("RSID play address must be zero, got {0}")]
    RsidPlayAddress(PlayAddress),
    #[error("RSID speed must be zero, got {0:?}")]
    RsidSpeed(SpeedBitmask),
    #[error("RSID effective load address {0} is below $07E8")]
    RsidEffectiveLoadAddress(LoadAddress),
    #[error(
        "invalid RSID init address {0} (BASIC requires zero; machine code requires $07E8-$9FFF or $C000-$CFFF)"
    )]
    RsidInitAddress(InitAddress),
}

pub fn parse(bytes: &[u8]) -> Result<Header, Error> {
    if bytes.len() < HEADER_V1_SIZE as usize {
        return Err(Error::TooShort {
            got: bytes.len(),
            need: HEADER_V1_SIZE as usize,
        });
    }

    let magic = [
        bytes[OFF_MAGIC],
        bytes[OFF_MAGIC + 1],
        bytes[OFF_MAGIC + 2],
        bytes[OFF_MAGIC + 3],
    ];
    let format = match &magic {
        b"PSID" => Format::Psid,
        b"RSID" => Format::Rsid,
        _ => return Err(Error::BadMagic(magic)),
    };

    let version = read_u16_be(bytes, OFF_VERSION);
    if !(1..=4).contains(&version) || (format == Format::Rsid && version == 1) {
        return Err(Error::UnsupportedVersion(version));
    }

    let data_offset = read_u16_be(bytes, OFF_DATA_OFFSET);
    let expected_offset = if version == 1 {
        HEADER_V1_SIZE
    } else {
        HEADER_V2_SIZE
    };
    if data_offset != expected_offset {
        return Err(Error::BadDataOffset(data_offset, version));
    }

    let need = data_offset as usize;
    if bytes.len() < need {
        return Err(Error::TooShort {
            got: bytes.len(),
            need,
        });
    }

    let load_address = LoadAddress(read_u16_be(bytes, OFF_LOAD_ADDRESS));
    let init_address = InitAddress(read_u16_be(bytes, OFF_INIT_ADDRESS));
    let play_address = PlayAddress(read_u16_be(bytes, OFF_PLAY_ADDRESS));
    let songs = SubtuneCount(read_u16_be(bytes, OFF_SONGS));
    let start_song = SubtuneIndex(read_u16_be(bytes, OFF_START_SONG));
    let speed = SpeedBitmask(read_u32_be(bytes, OFF_SPEED));
    if !(1..=MAX_SUBTUNES.0).contains(&songs.0) {
        return Err(Error::InvalidSongCount(songs));
    }
    if !(1..=songs.0).contains(&start_song.0) {
        return Err(Error::InvalidStartSong {
            start: start_song,
            songs,
        });
    }
    if format == Format::Rsid {
        if load_address.0 != 0 {
            return Err(Error::RsidLoadAddress(load_address));
        }
        if play_address.0 != 0 {
            return Err(Error::RsidPlayAddress(play_address));
        }
        if speed.0 != 0 {
            return Err(Error::RsidSpeed(speed));
        }
    }

    let name = read_string(bytes, OFF_NAME);
    let author = read_string(bytes, OFF_AUTHOR);
    let released = read_string(bytes, OFF_RELEASED);

    let mut flags = Flags::default();
    let mut start_page = 0;
    let mut page_length = 0;
    let mut second_sid_address = None;
    let mut third_sid_address = None;
    if version >= 2 {
        flags = Flags::from_raw(read_u16_be(bytes, OFF_FLAGS), version);
        start_page = bytes[OFF_START_PAGE];
        page_length = bytes[OFF_PAGE_LENGTH];
        if version >= 3 {
            second_sid_address = Some(bytes[OFF_SECOND_SID])
                .filter(|&b| b != 0)
                .map(SidBaseAddress);
        }
        if version >= 4 {
            third_sid_address = Some(bytes[OFF_THIRD_SID])
                .filter(|&b| b != 0)
                .map(SidBaseAddress);
        }
    }

    let header = Header {
        format,
        version,
        data_offset,
        load_address,
        init_address,
        play_address,
        songs,
        start_song,
        speed,
        name,
        author,
        released,
        flags,
        start_page,
        page_length,
        second_sid_address,
        third_sid_address,
    };
    if format == Format::Rsid {
        let load = header.effective_load_address(bytes)?;
        if load.0 < 0x07e8 {
            return Err(Error::RsidEffectiveLoadAddress(load));
        }
        if flags.psid_specific {
            if init_address.0 != 0 {
                return Err(Error::RsidInitAddress(init_address));
            }
        } else {
            let init = if init_address.0 == 0 {
                InitAddress(load.0)
            } else {
                init_address
            };
            if !matches!(init.0, 0x07e8..=0x9fff | 0xc000..=0xcfff) {
                return Err(Error::RsidInitAddress(init));
            }
        }
    }
    Ok(header)
}

fn read_u16_be(bytes: &[u8], off: usize) -> u16 {
    u16::from_be_bytes([bytes[off], bytes[off + 1]])
}

fn read_u32_be(bytes: &[u8], off: usize) -> u32 {
    u32::from_be_bytes([bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3]])
}

fn read_string(bytes: &[u8], off: usize) -> String {
    let raw = &bytes[off..off + STRING_FIELD_LEN];
    let end = raw.iter().position(|&b| b == 0).unwrap_or(STRING_FIELD_LEN);
    let (decoded, _, _) = WINDOWS_1252.decode(&raw[..end]);
    decoded.into_owned()
}
