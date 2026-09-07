# Repository agent instructions

These repository-wide instructions apply to any automated coding agent. The
generic `AGENTS.md` entry point resolves to this file so tools with either
convention receive the same guidance.

## Project

`sid-analyzer` is a Rust project (edition 2024) that analyzes SID files — the
Commodore 64 music/sound format produced by the MOS 6581/8580 SID chip. It
parses headers, executes PSID players, captures cycle-ordered SID bus activity,
derives musical and digital chip state, identifies effects and timbres, and
exports analysis, MIDI, reconstructed digi streams, and editable Pertylizer
projects. See `plans/PLAN.md` for the current roadmap and design references.

Licensed local SID files can be placed under the ignored `assets/music/`
directory. Tests that depend on this optional corpus use the `asset-tests`
feature and are excluded from the default public test gate.

An optional local `Songlengths.md5` can be kept at the ignored
`assets/Songlengths.md5` path. Supply it with `--songlengths` or
`HVSC_SONGLENGTHS`; no database is distributed or loaded implicitly. The
canonical "new format" uses plain MD5 of the full SID file. The pre-#71
"old format" uses a different hash algorithm and is **not** supported.

## Language

All code, comments, CLI strings, documentation, and commit messages in **English**. Conversations with the user may be in Swedish; that does not change the artifact language.

## Project Phase

Active development — **no backward compatibility required**. Break APIs and on-disk/JSON output formats freely until the first tagged release.

## Commands

- Build: `cargo build --workspace`
- Build the optional SQLite corpus scanner: `cargo build --workspace --features corpus-scan`
- Run analyzer CLI: `cargo run -p sid-analyzer --bin sid-analyzer -- <args>`
- Test: `cargo test --workspace` (single test: `cargo test -p <crate> <name>`)
- Optional local-corpus tests: `cargo test --workspace --features asset-tests`
- Lint: `cargo clippy --workspace --all-targets`
- Format check: `cargo fmt --check` (apply formatting with `cargo fmt`)
- RE toolkit: `cargo run -p sid-analyzer --bin sid-re -- <command> …`
  — post-init disassembly, RAM dumps, cell watches, signature scans, clustering,
  IRQ audits, taint tracking, differential probes, and WAV band profiles. Use
  this toolkit for driver reverse engineering instead of throwaway helpers.
- Structural Rust search: `ast-grep --lang rust --pattern '<pattern>' <paths>`;
  use `rg` for plain-text and path searches.

### Before committing

Run the complete gate from the repository root before committing:

```bash
cargo fmt --check
cargo build --workspace
cargo clippy --workspace --all-targets
cargo test --workspace
```

All four must pass with **zero warnings or errors** before a commit lands. Keep
unrelated user changes out of the commit and report any that remain locally.

## Architecture

This is a single-crate Cargo workspace. See `plans/PLAN.md` for current
priorities and its reference map for reusable utilities and implementation
records.

### `analyzer` — SID analysis + CLI

| Module | Purpose |
|---|---|
| `header` | PSID v1-v4 and RSID v2-v4 header parsing and metadata |
| `emu::{bus, runner, sid, capture}` | 6502 host, 64 KiB RAM, SID bus traps, digital SID state, and replayable capture |
| `trace` | Per-call register reads/writes with cycle-level ordering and timing diagnostics |
| `songlengths`, `stil`, `playerid` | HVSC durations, optional STIL metadata, and SIDId player signatures |
| `analysis::{voice, note, filter, effects}` | Physical voice/filter state, notes, effects, and cross-voice relations |
| `analysis::{timbre, programs, sid_program}` | Timbre characterization, reusable patches, and target-neutral causal program IR |
| `export::{text, json, midi}` | Tracker text, analysis JSON, and expressive format-1 MIDI |
| `export::{forward, native, synth}` | Fidelity gate, driver-native recovery, and Pertylizer project lowering |
| `audio` | WAV parsing, feature profiles, and A/B comparison support |
| `bin/main.rs` | Main `clap` CLI and all export dispatch |
| `bin/sid-corpus-scan.rs` | Bulk HVSC analysis to SQLite |
| `bin/sid-composer-census.rs` | Composer/driver-family coverage census |
| `bin/sid-re.rs` | Driver reverse-engineering toolkit |
| `bin/m7-*.rs`, `bin/sid-*-qualify.rs` | Development evaluation and native-extractor qualification tools |

Header-only RSID inspection is supported. Emulation and export refuse RSID by
default because the project has no complete Kernal/BASIC/IRQ host environment;
`--allow-rsid` exists only for explicitly unreliable diagnostics.

## Newtype Pattern (CRITICAL)

**NEVER use raw primitives** for domain concepts. ALWAYS wrap in a newtype. SID has dozens of bit-packed fields with overlapping numeric ranges (12-bit pulse width vs 11-bit cutoff vs 16-bit frequency vs 4-bit ADSR phases) — confusing them silently is the failure mode this pattern prevents.

```rust
// WRONG — raw primitives for domain values
fn note_for(freq: u16, clock_hz: u32) -> u8 { ... }

// RIGHT — newtypes
fn note_for(freq: SidFreq, clock: SystemClock) -> MidiNote { ... }
```

Search the codebase before introducing a new newtype — a suitable one likely exists. Suggested newtypes for this project:

| Domain                | Newtypes                                                                                   |
|-----------------------|--------------------------------------------------------------------------------------------|
| SID register values   | `SidFreq` (16-bit), `PulseWidth` (12-bit), `Cutoff` (11-bit), `Resonance` (4-bit), `Adsr`  |
| Pitch                 | `MidiNote`, `Cents`, `Hertz`                                                               |
| Timing                | `FrameIndex`, `CpuCycle`, `SubFrameOffset`, `SystemClock` (PAL/NTSC)                       |
| Identifiers           | `VoiceId` (1/2/3), `SubtuneIndex`, `LoadAddress`, `InitAddress`, `PlayAddress`             |

**Raw primitives OK for:** loop counters, intermediate arithmetic inside one function, FFI/serialization internals.

## Code Style

- Use `Self` in impl blocks, not the type name
- `thiserror` for error types — no manual `Display + Error` impls
- No `.unwrap()` / `.expect()` in library or CLI code — use `unwrap_or`, `?`, or `if let`. Tests may unwrap freely
- `pub(crate)` for internal types — minimize public API surface
- `#[must_use]` on newtypes and builder methods
- No `unsafe` code without discussion
- Prefer `for` loops over iterators in tight per-frame / per-cycle decode paths where it aids readability; iterators elsewhere
- Default to **no comments**; write one only when the *why* is non-obvious

## Pertylizer MCP — always log gaps and failures

The Pertylizer MCP integration is **under active development**. Whenever an
agent uses it, treat its rough edges as first-class output:

- **Report every gap, failure, or dead-end.** If a tool errors, returns nothing
  useful, lacks a capability you needed, is under-documented, or you simply fail
  to do the thing — surface it to the user *and* investigate the root cause
  (what tool, parameter, doc, or feature would have made it work).
- **Verify before declaring something "missing."** Try alternative tool/module
  names, `search_modules`, `add_module` with the short key, `get_module_type_info`,
  and `list_*` first. (Lesson learned: a "Pertylizer has no ring modulator"
  claim was wrong — `RingMod` existed under key `rng`; the real gap was that
  `search_modules` didn't surface it.) Distinguish a genuine gap from your own
  misuse, and report which it was.
- **Persist it locally.** Keep every finding, request, and concrete follow-up
  plan in this repository's running feedback memory
  (`pertylizer-mcp-feedback.md`) so they accumulate across sessions. Do not
  edit the Pertylizer repository unless the user explicitly asks for that.
- **Report scripting/engine improvements, not just tool friction.** Whenever the
  Mod Matrix (`mmx`), Script module (`scr`), AudioScript (`asc`), or the **YAMS**
  language could be *improved* to serve a task better — a missing function,
  operator, module capability, port, or a **missing context/static variable**
  (e.g. the audio sample rate we're rendering at, the note frequency, the
  transport rate) — surface it to the user and log it in
  `pertylizer-mcp-feedback.md`, so it can be fixed in Pertylizer. Treat "I had to
  hack around it" (e.g. hardcoding `48000/44100` because a script can't read the
  sample rate) as a first-class feature request, not a workaround to keep. Keep
  concrete implementation plans in `pertylizer-mcp-feedback.md` as well.
- The goal is a tight loop: every time the MCP makes a task harder than it
  should be, that friction becomes actionable feedback for improving the toolset.

## SID file format reference

The canonical references when implementing parsing or emulation are the
PSID/RSID specification (`SID_file_format.txt` from the HVSC project) and the
SID chip register map (`$D400-$D41C`). A SID file is a header (PSID/RSID magic,
version, load/init/play addresses, song metadata, speed bitmask, flags)
followed by 6502 machine code and data that drives the SID registers. In the
speed bitmask, bit 0 selects subtune 1, bit 1 selects subtune 2, and so on; a
set bit means CIA-timer playback and a clear bit means vblank timing.
