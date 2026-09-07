use crate::header::{Format, Header};
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UnsupportedInputKind {
    RsidSystemEnvironment,
    MusStrPayload,
}

impl UnsupportedInputKind {
    #[must_use]
    pub fn classify(header: &Header) -> Option<Self> {
        if header.flags.mus_data {
            Some(Self::MusStrPayload)
        } else if header.format == Format::Rsid {
            Some(Self::RsidSystemEnvironment)
        } else {
            None
        }
    }

    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            Self::RsidSystemEnvironment => "rsid_system_environment",
            Self::MusStrPayload => "mus_str_payload",
        }
    }

    #[must_use]
    pub fn detail(self) -> &'static str {
        match self {
            Self::RsidSystemEnvironment => {
                "emulation requires a Kernal/BASIC/IRQ system environment"
            }
            Self::MusStrPayload => "MUS/STR payload emulation is not implemented",
        }
    }
}

impl std::fmt::Display for UnsupportedInputKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.code())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::header;

    #[test]
    fn mus_classification_takes_precedence_over_container_format() {
        let mut bytes = vec![0_u8; 0x7c];
        bytes[0..4].copy_from_slice(b"PSID");
        bytes[4..6].copy_from_slice(&2_u16.to_be_bytes());
        bytes[6..8].copy_from_slice(&0x7c_u16.to_be_bytes());
        bytes[8..10].copy_from_slice(&0x1000_u16.to_be_bytes());
        bytes[10..12].copy_from_slice(&0x1000_u16.to_be_bytes());
        bytes[12..14].copy_from_slice(&0x1003_u16.to_be_bytes());
        bytes[14..16].copy_from_slice(&1_u16.to_be_bytes());
        bytes[16..18].copy_from_slice(&1_u16.to_be_bytes());
        bytes[0x76..0x78].copy_from_slice(&1_u16.to_be_bytes());
        let parsed = header::parse(&bytes).unwrap();
        assert_eq!(
            UnsupportedInputKind::classify(&parsed),
            Some(UnsupportedInputKind::MusStrPayload)
        );
    }
}
