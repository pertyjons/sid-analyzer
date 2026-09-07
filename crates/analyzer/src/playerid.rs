//! C64 playroutine identification by code signature.
//!
//! # Attribution
//!
//! This module is a faithful Rust port of **SIDId V1.09**, the HVSC
//! playroutine identity scanner written by **Cadaver** (Lasse Öörni,
//! `loorni@gmail.com`) of Covert Bitops, Copyright © 2006–2012, released under
//! a 3-clause BSD licence. Both the matching algorithm ([`signature_matches`],
//! a port of SIDId's `identifybytes`) and the bundled signature database
//! (`assets/sidid.cfg`) originate there.
//!
//! The **signatures themselves** were contributed by the C64 scene — credited
//! in the SIDId sources as **Ian Coog, Ice00, Ninja, Yodelking, Wilfred/HVSC
//! and Prof. Chaos** — and are maintained as part of the High Voltage SID
//! Collection (HVSC). Upstream: <https://github.com/cadaver/sidid> (and the
//! modern reimplementation <https://github.com/WilfredC64/player-id>).
//!
//! The full upstream copyright and BSD licence text is reproduced verbatim in
//! `assets/sidid.cfg.NOTICE`, which travels with the vendored database. This
//! module re-implements the algorithm in Rust; it is not a copy of the C
//! source. See [`PlayerDb`] for the data and [`signature_matches`] for the
//! matcher.
//!
//! # What `sidid.cfg` is
//!
//! `sidid.cfg` is SIDId's signature database: a flat text file pairing each
//! **playroutine name** (a C64 music driver / editor, e.g. `Rob_Hubbard`,
//! `GoatTracker_V2.x`) with one or more **byte-pattern signatures** that
//! fingerprint that driver's 6502 code. Identifying a SID file means scanning
//! its raw bytes for any known signature — independent of the header's
//! (unreliable) author metadata. The grammar [`PlayerDb::parse`] accepts:
//!
//! - a **literal byte** — two hex digits (`A9`), must match exactly;
//! - `??` — a wildcard matching any single byte;
//! - `AND` — skip forward to the next literal match (a variable-size gap);
//! - `END` — close the current signature;
//! - anything else — a **player name** that begins a new entry, with its
//!   following signature lines as alternatives (a player matches if *any* of
//!   them is found anywhere in the file).

/// One token in a signature pattern.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Token {
    /// A concrete byte that must match.
    Byte(u8),
    /// `??` — matches any single byte.
    Any,
    /// `AND` — skip forward to the next occurrence of the following literal.
    And,
}

/// A single alternative pattern for a player.
#[derive(Debug, Clone)]
pub struct Signature {
    pub(crate) tokens: Vec<Token>,
}

/// A named playroutine with one or more alternative signatures.
#[derive(Debug, Clone)]
pub struct Player {
    pub name: String,
    pub(crate) signatures: Vec<Signature>,
}

/// The full signature database parsed from a `sidid.cfg`.
#[derive(Debug, Clone, Default)]
pub struct PlayerDb {
    pub players: Vec<Player>,
}

/// The vendored `assets/sidid.cfg`, embedded at compile time so callers (e.g.
/// the `synth-native` exporter) can identify drivers with no runtime file
/// dependency. A re-sync of the asset triggers a rebuild. See
/// `assets/sidid.cfg.NOTICE` for upstream copyright and licence.
const EMBEDDED_CFG: &str = include_str!("../../../assets/sidid.cfg");

/// Errors from loading a signature database.
#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("reading config {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("config has no signatures")]
    Empty,
}

impl PlayerDb {
    /// Parse a `sidid.cfg` (the SIDId signature database, see the module docs
    /// for format and attribution) from a string. Mirrors SIDId's `readconfig`:
    /// whitespace-separated tokens; a `NAME` token (anything not `??`/`AND`/
    /// `END`/a 2-hex-digit byte) starts a new player; `END` closes the current
    /// signature; consecutive `END`s or names without bytes are ignored.
    #[must_use]
    pub fn parse(text: &str) -> Self {
        let mut players: Vec<Player> = Vec::new();
        let mut pending: Vec<Token> = Vec::new();

        for tok in text.split_whitespace() {
            if tok.eq_ignore_ascii_case("end") {
                if !pending.is_empty()
                    && let Some(p) = players.last_mut()
                {
                    p.signatures.push(Signature {
                        tokens: std::mem::take(&mut pending),
                    });
                } else {
                    pending.clear();
                }
            } else if tok == "??" {
                pending.push(Token::Any);
            } else if tok.eq_ignore_ascii_case("and") {
                pending.push(Token::And);
            } else if let Some(b) = parse_hex_byte(tok) {
                pending.push(Token::Byte(b));
            } else {
                // NAME token: starts a new player. Any bytes not yet closed by
                // END are dropped, matching the reference parser.
                pending.clear();
                players.push(Player {
                    name: tok.to_string(),
                    signatures: Vec::new(),
                });
            }
        }

        // Drop players that ended up with no signatures (e.g. trailing name).
        players.retain(|p| !p.signatures.is_empty());
        Self { players }
    }

    /// The signature database vendored with the crate (`assets/sidid.cfg`,
    /// embedded via [`EMBEDDED_CFG`]). Never empty in practice.
    #[must_use]
    pub fn embedded() -> Self {
        Self::parse(EMBEDDED_CFG)
    }

    /// Load and parse a `sidid.cfg` from disk.
    pub fn load(path: impl AsRef<std::path::Path>) -> Result<Self, LoadError> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path).map_err(|source| LoadError::Read {
            path: path.display().to_string(),
            source,
        })?;
        let db = Self::parse(&text);
        if db.players.is_empty() {
            return Err(LoadError::Empty);
        }
        Ok(db)
    }

    /// Return the name of the first player whose signature matches `data`,
    /// or `None` if unidentified. Order follows the config file.
    #[must_use]
    pub fn identify<'a>(&'a self, data: &[u8]) -> Option<&'a str> {
        self.players
            .iter()
            .find(|p| player_matches(p, data))
            .map(|p| p.name.as_str())
    }

    /// Return every player whose signature matches `data` (SIDId `-m`).
    #[must_use]
    pub fn identify_all<'a>(&'a self, data: &[u8]) -> Vec<&'a str> {
        self.players
            .iter()
            .filter(|p| player_matches(p, data))
            .map(|p| p.name.as_str())
            .collect()
    }
}

fn parse_hex_byte(tok: &str) -> Option<u8> {
    let bytes = tok.as_bytes();
    if bytes.len() == 2 && bytes[0].is_ascii_hexdigit() && bytes[1].is_ascii_hexdigit() {
        u8::from_str_radix(tok, 16).ok()
    } else {
        None
    }
}

/// True if any of the player's alternative signatures matches.
#[must_use]
pub(crate) fn player_matches(player: &Player, data: &[u8]) -> bool {
    player
        .signatures
        .iter()
        .any(|sig| signature_matches(&sig.tokens, data))
}

/// Faithful port of SIDId's `identifybytes`. Searches `data` for `tokens`,
/// where `Any` matches one byte and `And` skips to the next literal match.
/// The `c/d/rc/rd` cursor logic (including backtracking) mirrors the C source.
#[must_use]
pub(crate) fn signature_matches(tokens: &[Token], data: &[u8]) -> bool {
    let len = data.len();
    let mut c: usize = 0; // cursor into data
    let mut d: usize = 0; // cursor into tokens
    let mut rc: usize = 0; // restart cursor into data
    let mut rd: usize = 0; // restart cursor into tokens

    while c < len {
        if d == rd {
            // Hunting for the anchor. SIDId compares the raw byte here, so the
            // sentinels (`Any`/`And`) never anchor — only a concrete byte does.
            if literal_eq(tokens.get(d), data[c]) {
                rc = c + 1;
                d += 1;
            }
            c += 1;
        } else {
            // The C code checks END here; for us "past the end of tokens"
            // means the whole signature matched.
            if d >= tokens.len() {
                return true;
            }
            if tokens[d] == Token::And {
                d += 1;
                // Skip forward to the next byte equal to the literal at d.
                // (Raw compare, as in SIDId — the token after `And` is a byte.)
                while c < len {
                    if literal_eq(tokens.get(d), data[c]) {
                        rc = c + 1;
                        rd = d;
                        break;
                    }
                    c += 1;
                }
                if c >= len {
                    return false;
                }
            }
            if !token_matches(tokens.get(d), data[c]) {
                // Mismatch: backtrack to the saved restart point.
                c = rc;
                d = rd;
            } else {
                c += 1;
                d += 1;
            }
        }
    }
    // Ran off the end of data: matched iff all tokens were consumed.
    d >= tokens.len()
}

/// Whether token `t` accepts byte `b`. `Any` always accepts; `And` never
/// matches as a literal (it is handled before this is called); a missing token
/// (past the end) corresponds to the C `END` sentinel and never matches a byte.
#[inline]
fn token_matches(t: Option<&Token>, b: u8) -> bool {
    match t {
        Some(Token::Byte(x)) => *x == b,
        Some(Token::Any) => true,
        Some(Token::And) | None => false,
    }
}

/// Raw byte equality, mirroring SIDId's `buffer[c] == bytes[d]`: only a
/// concrete byte token matches. Used for anchoring and `And` skip-forward,
/// where the sentinels must never compare equal to a real byte.
#[inline]
fn literal_eq(t: Option<&Token>, b: u8) -> bool {
    matches!(t, Some(Token::Byte(x)) if *x == b)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db_from(s: &str) -> PlayerDb {
        PlayerDb::parse(s)
    }

    #[test]
    fn parses_simple_signature() {
        let db = db_from("Test_Player\nA9 1F 8D 18 D4 END\n");
        assert_eq!(db.players.len(), 1);
        assert_eq!(db.players[0].name, "Test_Player");
        assert_eq!(db.players[0].signatures.len(), 1);
        assert_eq!(db.players[0].signatures[0].tokens.len(), 5);
    }

    #[test]
    fn matches_literal_run_anywhere() {
        let db = db_from("P\nAA BB CC END\n");
        let data = [0x00, 0x11, 0xAA, 0xBB, 0xCC, 0x99];
        assert_eq!(db.identify(&data), Some("P"));
        let nope = [0xAA, 0xBB, 0x00];
        assert_eq!(db.identify(&nope), None);
    }

    #[test]
    fn wildcard_matches_any_byte() {
        let db = db_from("P\nAA ?? CC END\n");
        assert!(signature_matches(
            &db.players[0].signatures[0].tokens,
            &[0xAA, 0x42, 0xCC]
        ));
        assert!(signature_matches(
            &db.players[0].signatures[0].tokens,
            &[0xAA, 0xFF, 0xCC]
        ));
        assert!(!signature_matches(
            &db.players[0].signatures[0].tokens,
            &[0xAA, 0x42, 0xCD]
        ));
    }

    #[test]
    fn and_skips_to_next_literal() {
        let db = db_from("P\nAA AND CC END\n");
        let toks = &db.players[0].signatures[0].tokens;
        // AA, then somewhere later a CC.
        assert!(signature_matches(toks, &[0xAA, 0x01, 0x02, 0xCC]));
        assert!(signature_matches(toks, &[0xAA, 0xCC]));
        // No CC after AA → no match.
        assert!(!signature_matches(toks, &[0xAA, 0x01, 0x02]));
    }

    #[test]
    fn alternatives_match_any() {
        let db = db_from("P\nAA AA END\nBB BB END\n");
        assert_eq!(db.players[0].signatures.len(), 2);
        assert_eq!(db.identify(&[0x00, 0xBB, 0xBB]), Some("P"));
    }

    #[test]
    fn backtracking_finds_later_start() {
        // A false start (AA AB) followed by a true match (AA AC).
        let db = db_from("P\nAA AC END\n");
        let toks = &db.players[0].signatures[0].tokens;
        assert!(signature_matches(toks, &[0xAA, 0xAB, 0xAA, 0xAC]));
    }

    #[test]
    fn identify_all_returns_multiple() {
        let db = db_from("P1\nAA END\nP2\nBB END\n");
        let mut hits = db.identify_all(&[0xAA, 0xBB]);
        hits.sort_unstable();
        assert_eq!(hits, vec!["P1", "P2"]);
    }
}
