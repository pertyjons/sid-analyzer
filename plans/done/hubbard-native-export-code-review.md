# Hubbard native export code review

Date: 2026-07-19

Scope: the complete production path used by `sid-analyzer --format
synth-native` for a SIDId-identified `Rob_Hubbard` tune, from CLI parsing and
PSID emulation through native table extraction, validation, timbre recovery,
structured Pertylizer lowering, and file output.

Primary concrete case:

```text
assets/music/Auf_Wiedersehen_Monty.sid
subtune 1
3000 play calls
PAL / MOS 6581
```

This is a code review, not an implementation plan. Findings are ordered by
severity and include a proposed correction, but no production code was changed
as part of the review.

## Executive assessment

The AWM PAL/auto-clock path works and is backed by unusually good driver-specific
reverse engineering. The strongest parts are the post-init extraction boundary,
relocation-independent table discovery, explicit support for several Hubbard
format variants, authored-instrument binding, and the final forward-model
degradation step. On the reviewed 3000-frame AWM window the command completes,
emits 84 patterns and 104 placements, and reports no final pitch failures.

It is not, however, a uniformly native or uniformly validated pipeline. It is a
hybrid:

- orderlists, pattern events, base pitches, instrument indices, and placement
  starts are native-decoded;
- gate/reference state, envelope activity, filter, pulse width, effect spans,
  waveform programs, amplitude automation, and several pitch corrections are
  trace-derived;
- the synth exporter may replace a native note representation with per-frame
  trace pitch runs when its forward model rejects the proposed rendering;
- the structured project preserves the rendered note timeline, not the exact
  identity and contents of the driver's pattern graph.

Four correctness issues should be fixed before expanding the claimed Hubbard
coverage:

1. The native path can use three different clock/timing configurations in one
   export.
2. The native acceptance gate is precision-only, non-unique, and has no recall
   requirement; incomplete or duplicated decodes can pass.
3. `resolve_placements` does not implement the repeat-count orderlist format
   that `decode_song` implements.
4. The no-tempo-anchor fallback says “one frame per tick” but encodes a reload
   value that produces one tick every two frames.

AWM subtune 1 with the default PAL clock does not trigger findings 1, 3, or 4,
but the gate weakness is active on every native export. The AWM native record
contains 479 source note events while the trace-derived path contains 449; the
current gate can accept that discrepancy because it does not establish a
one-to-one correspondence.

## Review method and evidence

The review followed the actual call graph rather than the module layout:

```text
main
  -> header::parse
  -> run_export
       -> emu::run_with_timing + analyze              (discarded for native)
       -> run_export_synth_native
            -> PlayerDb::identify
            -> native::extract_native
                 -> HubbardExtractor::extract
                      -> Emulator::load + call_init
                      -> hubbard::locate
                      -> emu::run + analyze
                      -> detect_effects
                      -> decode_song
                      -> arpeggio_survivors
                      -> onset_agreement
                      -> characteristics + native patches
                      -> resolve_placements
            -> EnrichedNote::enrich
            -> synth::write_synth
                 -> derive_time_base
                 -> build_track_plans
                 -> build_instruments
                 -> build_song_structured
                 -> forward-model census/degradation
                 -> serde_json project output
            -> census sidecar
```

The following checks were run locally:

```text
cargo run ... Auf_Wiedersehen_Monty.sid --subtune 1 --frames 3000 \
  --format synth-native
cargo run ... Auf_Wiedersehen_Monty.sid --subtune 1 --frames 3000 \
  --format synth
cargo run ... Auf_Wiedersehen_Monty.sid --subtune 1 --frames 1500 \
  --clock ntsc --format synth-native
cargo run ... Auf_Wiedersehen_Monty.sid --subtune 1 --frames 0 \
  --format synth-native
cargo test -p sid-analyzer --lib dbg_awm_pattern -- --ignored --nocapture
AB_ASSET=Shape_Music_2.sid AB_FRAMES=1500 cargo test ... dump_flat_and_structured
```

The Pertylizer sequencer implementation was also inspected to confirm how
placement length, note duration, transpose, and automation are evaluated. No
Pertylizer code or MCP state was changed.

## Severity model

- **High**: can accept or emit musically wrong output for an advertised input or
  makes validation materially untrustworthy.
- **Medium**: real correctness, fidelity, performance, or contract problem whose
  common path is constrained or whose failure is visible.
- **Low**: maintainability, diagnostics, test-quality, or API-design weakness
  unlikely to corrupt AWM by itself.

## Findings summary

| ID | Severity | Finding | AWM PAL/auto impact |
|----|----------|---------|---------------------|
| H1 | High | Native extraction mixes CLI, header, and hard-coded PAL timing | Latent; default AWM values coincide |
| H2 | High | Native onset gate has no one-to-one match or truth recall | Active validation weakness |
| H3 | High | Structured placement decode omits `order_repeat` | Not triggered by AWM |
| H4 | High | Missing-tempo fallback produces a two-frame, not one-frame, tick | Not triggered; AWM locates tempo 2 |
| M1 | Medium | Native CLI path emulates/analyzes the full window twice | Active; measurable duplicate work |
| M2 | Medium | Documented automatic synth fallback does not exist | Active on native failure |
| M3 | Medium | Inexact CIA timing still participates in the native gate | Not triggered; AWM is vblank |
| M4 | Medium | Locator combines unrelated global first matches | Latent false locate/reject risk |
| M5 | Medium | Decoder safety caps silently truncate valid-looking data | Latent; amplified by H2 |
| M6 | Medium | Validation ignores duration, cents, authored instrument, and structure | Active scope limitation |
| M7 | Medium | “Native structure” is rendered-note regrouping, not exact driver structure | Active semantic limitation |
| M8 | Medium | Frame-rate conversion is inconsistent with the rational emulator scheduler | Active small duration/rate drift |
| M9 | Medium | Hubbard authored effect decode exports a guessed vibrato rate | Active on affected instruments |
| M10 | Medium | `--frames 0` reaches extraction and fails as “decoded no notes” | Active CLI contract issue |
| M11 | Medium | Output replacement is non-atomic and sidecar failure is non-fatal | Latent I/O integrity issue |
| M12 | Medium | Trace-derived multi-track lowering can model one SID voice as overlapping synths | Fidelity risk, partly mitigated |
| L1 | Low | Hubbard domain values still use many interchangeable primitives | Maintainability risk |
| L2 | Low | Debug environment variables silently change project semantics | Reproducibility risk |
| L3 | Low | Tests bypass important production boundaries and miss adversarial gates | Coverage weakness |
| L4 | Low | Several code comments and claims are stale or stronger than behavior | Maintenance/documentation debt |

## Detailed findings

### H1 — Native extraction mixes clock domains

The CLI resolves one `SystemClock` from `--clock` and the header in
[`main.rs`](../../crates/analyzer/src/bin/main.rs). That resolved value is placed in
`NativeContext`, but the Hubbard extractor does not consistently use it:

1. The post-init locator uses `Emulator::new()`, which is always PAL. This also
   seeds C64 `TVSFLG` at `$02A6` to PAL before init.
2. The reference trace uses `emu::run`, which derives timing from the SID header
   and ignores the CLI override.
3. `analyze` is passed `PlaybackTiming::vblank(SystemClock::Pal)` explicitly.
4. Native frequency-table values, note detection, authored effect rates, and
   project lowering use `ctx.clock`, the CLI-resolved clock.

Relevant code:

- [`run_export_synth_native`](../../crates/analyzer/src/bin/main.rs)
- [`HubbardExtractor::extract`](../../crates/analyzer/src/export/native/hubbard/mod.rs)
- [`Emulator::new`](../../crates/analyzer/src/emu/mod.rs)
- [`PlaybackTiming::for_subtune`](../../crates/analyzer/src/emu/mod.rs)

This is more than a tempo-label issue. SID pitch is
`FREQ * phi2 / 2^24`; PAL and NTSC therefore interpret the same 16-bit register
value about 0.65 semitone apart. `$02A6` may also cause init to select different
frequency or timing data. Envelope and oscillator state then evolve against yet
another call schedule.

The reviewed `--clock ntsc` AWM export completed, but that does not prove it is an
NTSC emulation: its locator/init and trace remained tied to PAL/header behavior
while downstream pitch was interpreted as NTSC. The current onset gate compares
MIDI note numbers only and can mask much of the discrepancy.

**Recommendation:** construct one `PlaybackTiming` in the CLI and pass it through
`NativeContext`. Use `Emulator::with_timing(timing)` for post-init RAM, use
`run_with_timing` for the trace, and pass exactly the same `timing` to `analyze`.
Make the context carry timing, not a separately recomputed clock.

### H2 — The native acceptance gate is not a correspondence test

`onset_agreement` computes:

```text
native notes having any truth note with:
  same voice + same MIDI integer + onset within +/- 8 calls
divided by native note count
```

This has four important consequences:

1. **No recall:** one correct native note and hundreds of omitted truth notes can
   score 100%.
2. **No uniqueness:** multiple native notes can all match the same truth note.
3. **Coarse pitch:** cents and raw SID frequency are ignored.
4. **Wide ambiguity:** eight calls is 160 ms at nominal PAL and often spans
   several Hubbard row ticks.

See [`onset_agreement`](../../crates/analyzer/src/export/native/mod.rs).

The production comments call the emulator independent ground truth, but the gate
does not prove that the two timelines represent the same sequence. It proves only
that most decoded events resemble something nearby. Silent pattern/orderlist
truncation and duplicated order walking are therefore not reliably rejected.

Concrete AWM evidence at 3000 calls:

```text
native source events:       479
trace-derived source events: 449
native export accepted:     yes
```

The difference can include legitimate representation changes, but the gate does
not explain or bound it.

**Recommendation:** perform a monotonic one-to-one alignment per voice. Report
precision, recall, insertion count, deletion count, onset residual, pitch residual
in cents/raw register units, and duration/gate residual. Require both precision
and recall. A dynamic-programming sequence alignment is inexpensive at these note
counts and makes repeated truth reuse impossible. Refuse inexact traces as gate
truth.

### H3 — Placement resolution does not implement repeat-count orderlists

`decode_song` has explicit `layout.order_repeat` handling. It treats the byte
after a pattern as the repeat count for the next pattern and emits repeated event
runs. `resolve_placements`, which is supposed to walk the orderlist “exactly as
`decode_song` does”, has no corresponding branch. It reads every count byte as a
pattern number and emits no repeated placements.

Compare:

- [`decode_song`](../../crates/analyzer/src/export/native/hubbard/mod.rs)
- [`resolve_placements`](../../crates/analyzer/src/export/native/hubbard/mod.rs)

This is a production correctness issue for any layout where `locate` sets
`order_repeat = true`. Note extraction may pass H2 because it uses the correct
walker, after which the synth exporter receives incorrect structure and takes the
structured branch. Native note validation does not validate structure.

`decode_structure`, currently test-only, independently lacks repeat handling as
well. This duplication is the underlying design problem: three walkers encode
partially different Hubbard grammars.

**Recommendation:** implement one typed orderlist iterator/state machine yielding
`PatternPlacement { pattern, transpose, repetition }`, and have `decode_song`,
`resolve_placements`, and structure diagnostics consume it. Add an
`order_repeat=true` production fixture to the structured-vs-flat round-trip test.

### H4 — The absent-tempo fallback is off by a factor of two

When no separate tempo divider is found, `locate` says the song tick is one frame
and chooses:

```rust
let tempo = tempo_addr.map_or(1, r);
```

But `tempo` is a counter **reload value**, and `row_frames` emits a row every
`tempo + 1` divider runs. A reload of 1 therefore means one row every two frames.
A one-frame row requires reload 0.

See [`locate`](../../crates/analyzer/src/export/native/hubbard/mod.rs) and
[`row_frames`](../../crates/analyzer/src/export/native/hubbard/mod.rs).

The comment that a wrong guess “can only lower the decode gate, never pass it” is
not valid because H2 has no recall requirement; a sparse subset can pass.

**Recommendation:** change the fallback to 0 if “one call per row” is the intended
model. Add a unit test that runs the actual `locate -> row_frames` fallback, not
only synthetic layouts constructed with `tempo: 0`.

### M1 — The production native path performs a discarded emulation pass

`run_export` builds `trace` and `states` before matching on `Format`. The
`SynthNative` arm then returns into `run_export_synth_native`, discarding both.
The Hubbard extractor separately performs init for its RAM image and calls
`emu::run` again for the usable trace.

For `N` requested calls the CLI therefore performs approximately:

```text
init + N play calls          generic run_export pass, discarded
init                         locator image
init + N play calls          Hubbard reference trace
```

See [`run_export`](../../crates/analyzer/src/bin/main.rs).

This doubles the dominant CPU cost and increases the chance that a slow driver
hits a guard on a pass whose result is never consumed.

**Recommendation:** dispatch `SynthNative` immediately after preflight/clock
resolution, before generic trace construction. A larger refactor could share a
single initialized emulator snapshot between locator and reference trace, but
early dispatch alone removes the full discarded pass.

### M2 — “Falls back to synth” is documentation, not behavior

CLI help, module comments, and extractor comments repeatedly describe a fallback
to `--format synth`. On `Unidentified`, `LocateFailed`, `DecodeEmpty`, or
`DecodeUnreliable`, production code prints the native error and exits with code 4.
The error text asks the user to rerun manually.

See [`run_export_synth_native`](../../crates/analyzer/src/bin/main.rs) and
[`NativeError`](../../crates/analyzer/src/export/native/mod.rs).

The reviewed zero-frame run demonstrates this contract:

```text
synth-native: ... decoded no notes (use --format synth)
exit code: 4
no output project created
```

**Recommendation:** choose one explicit contract. Either implement fallback and
emit a prominent diagnostic recording that the project is non-native, or replace
all “falls back” language with “fails without creating output”. For a format named
`synth-native`, explicit failure is arguably the safer contract.

### M3 — CIA-inexact traces still gate native output

`PlaybackTiming::for_subtune` marks CIA tunes `timing_exact = false` but still
schedules calls at vblank because CIA period capture is unavailable. The warning
says such state is excluded from ground-truth gates. Hubbard extraction nevertheless
runs `onset_agreement` unconditionally.

Additionally, its `analyze` call uses `PlaybackTiming::vblank(Pal)`, which clears
the CIA flag passed into analysis; `digital_state_exact` remains false only
because the trace itself carries `timing_exact = false`.

See [`run_inner`](../../crates/analyzer/src/emu/mod.rs),
[`analyze`](../../crates/analyzer/src/analysis/mod.rs), and
[`HubbardExtractor::extract`](../../crates/analyzer/src/export/native/hubbard/mod.rs).

**Recommendation:** reject native self-validation when timing is inexact, or use
a gate that is explicitly call-index-based and proven independent of the missing
CIA period. The current warning must match actual control flow.

### M4 — The locator assembles a layout from globally unrelated anchors

The locator scans nearly all RAM several times and generally takes:

- the first pattern-pointer-shaped sequence;
- the first sequence-pointer-shaped sequence;
- the first matching pattern dereference;
- the first tempo-divider shape;
- the globally most-voted adjacent absolute-load pair as the frequency table;
- any SID frequency write anywhere as confirmation.

These anchors are not required to be in the same code region, reachable from the
play routine, or mutually consistent. Reads near `$FFFF` use wrapping address
arithmetic, so a candidate may even be assembled across the `$FFFF->$0000`
boundary.

See [`locate`](../../crates/analyzer/src/export/native/hubbard/mod.rs).

The downstream gate catches many false layouts, but H2 is weaker than the locator
assumes. The result today is more likely false rejection than silent corruption,
but both are possible.

**Recommendation:** generate layout candidates around a common pattern-step
anchor, score local control/data-flow consistency, validate pointer targets and
table ranges, then run sequence alignment on each viable candidate. Avoid
wrapping code-window reads unless the candidate instruction itself can legally
cross the address boundary.

### M5 — Decoder caps truncate without surfacing an error

`decode_pattern` stops after 512 bytes and `orderlist` after 256 bytes even if no
`$FF` terminator was found. Both return normal vectors with no completeness flag.
The outer decoder then applies additional work budgets based on `frames`.

See [`MAX_PATTERN_BYTES`, `MAX_ORDER_LEN`](../../crates/analyzer/src/export/native/hubbard/mod.rs).

Safety caps are appropriate for hostile or mislocated RAM, but silent truncation
is not. With H2, a partial prefix may still pass.

**Recommendation:** return a typed result distinguishing `Terminated` from
`LimitReached`/`InvalidPointer`. Native extraction should reject any truncated
structure used within the requested window and include the address/pattern/voice
in the error.

### M6 — Validation covers only coarse onsets

Even if H2 is made one-to-one, the current gate does not validate:

- note end or gate duration;
- hold/rest semantics;
- frequency cents or raw frequency-table value;
- slide command interpretation;
- sticky instrument selection;
- decoded ADSR, waveform, pulse width, or effect bytes;
- pattern number, transpose state, repeat count, or placement boundary.

The later synth forward model is stronger for rendered pitch and uncovered gated
frames, but it is a degradation/reporting system, not a native-decoder acceptance
gate. It can replace a bad proposal with trace pitch runs, which makes the final
audio safer while concealing native decode defects.

**Recommendation:** define separate gates for notes, instruments, and structure.
The native result should carry validation evidence, and the census should state
which layers are native-verified, trace-corrected, or unverified.

### M7 — The structured project does not preserve exact Hubbard structure

`resolve_placements` recovers useful native boundaries, but
`build_song_structured` transforms them substantially:

- one SID voice becomes several timbre/instrument tracks;
- placements with no notes for a given track disappear;
- leading silence is removed and added to placement start;
- block identity is keyed by serialized rendered notes plus track id, not Hubbard
  pattern number;
- different native pattern numbers with byte-identical rendered notes may merge;
- one native pattern can become several project patterns on different tracks;
- trace-derived expression can prevent reuse of an otherwise identical native
  pattern.

See [`build_song_structured`](../../crates/analyzer/src/export/synth.rs).

This is a valid render-oriented lowering, but comments describing “the real
reused blocks” and “exact pattern boundaries” are too strong. The existing
round-trip test proves a multiset of emitted notes matches the flat exporter. It
does not prove native pattern identity or empty/rest structure survives.

**Recommendation:** name this layer `native placements` or `structured lowering`,
not exact recovered project structure. If exact structure is a product goal,
introduce a driver-level intermediate representation containing order commands,
pattern IDs, rows, rests, instruments, and effects, then lower that IR separately.

### M8 — Scheduler and exporter disagree on frame rate

The emulator schedules rational PAL calls using `985248 / 19656`, approximately
50.1245 Hz, and NTSC calls using `1022727 / 17095`, approximately 59.8261 Hz.
`SystemClock::frame_rate`, used throughout time-base, vibrato, glide, PWM, and
automation conversion, returns exactly 50.0 or 60.0.

See [`CallRate::vblank`](../../crates/analyzer/src/emu/mod.rs),
[`SystemClock::frame_rate`](../../crates/analyzer/src/analysis/mod.rs), and
[`derive_time_base`](../../crates/analyzer/src/export/synth.rs).

The comment that real-time playback is exact is consequently false. A 3000-call
PAL window represents about 59.85 seconds on the emulator schedule but is lowered
as 60.0 seconds. The drift is small (~0.25%) but systematic and also affects LFO
and glide rates.

**Recommendation:** expose the rational call rate or seconds-per-call from
`PlaybackTiming` and use it everywhere temporal values leave the frame domain.
Do not derive temporal physics from `SystemClock` alone, especially for future
CIA support.

### M9 — Authored Hubbard vibrato is partly guessed

The native instrument decoder knows that `+5` participates in vibrato depth but
does not know the high nibble's rate semantics. It exports a fixed
`clock.frame_rate() / 8` triangle rate and lets the trace-derived/forward-model
path choose between measured and authored candidates.

See [`authored_patch_effects`](../../crates/analyzer/src/export/native/hubbard/mod.rs)
and the vibrato selection in [`push_expressive_notes`](../../crates/analyzer/src/export/synth.rs).

This is responsibly documented in the code, but the census label “authored
patches” can be read as stronger than it is. Other decoded effect fields are also
based mainly on the Commando routine and assumed to generalize across relocations.

**Recommendation:** represent authored-effect confidence per field. Do not mark a
patch simply authored/un-authored when ADSR is exact, waveform is exact,
vibrato depth is partly understood, and vibrato rate is guessed.

### M10 — Zero frames is accepted too far into the native path

`--frames` defaults to zero. Header-only behavior makes sense without a format,
but `synth-native` requires a trace and native timeline. The CLI permits the
combination, initializes and locates the driver, then reports `DecodeEmpty`.

**Recommendation:** require `--frames > 0` for `synth`, `synth-native`, MIDI, and
other time-based formats, or define zero as “use resolved song length”. Return a
CLI validation error rather than a misleading driver-decoder error.

### M11 — Project output is not atomic

Extraction completes before the output is opened, which correctly prevents an
unsupported driver from creating a partial file. Once writing starts,
`File::create` truncates an existing project before serialization and flush have
succeeded. A serialization, disk-full, or flush error leaves a partial/truncated
project. Census sidecar creation is separately non-fatal, so command success does
not mean both requested artifacts are durable.

See [`write_export`](../../crates/analyzer/src/bin/main.rs) and
[`write_census_sidecar`](../../crates/analyzer/src/bin/main.rs).

**Recommendation:** write project and census to sibling temporary files, flush
(and optionally sync), then rename atomically. Decide whether the census is part
of the success contract or explicitly best-effort metadata.

### M12 — One physical SID voice becomes multiple potentially overlapping synths

Track planning splits a monophonic SID voice by patch, waveform program, and
percussion role, giving each plan a dedicated Pertylizer instrument and track.
This prevents automation collisions, but the hardware has one oscillator and one
envelope per voice. On the SID, a new instrument changes that same signal path;
it does not create a second independently releasing oscillator.

Measured envelope lanes and attack-boundary handling mitigate this for many AWM
notes, but static-envelope and censored-tail cases can still overlap across
project tracks in a way the SID cannot. Pitch-only forward validation will not
detect excess amplitude, doubled release tails, or two simultaneous timbres.

**Recommendation:** add an amplitude/timbre coverage gate per physical voice and
render tests around patch changes with non-zero release. Long term, consider one
voice bus or explicit voice-stealing/choke groups so split tracks retain SID
monophony.

### L1 — Hubbard domain primitives remain easy to mix

The repository's newtype rule is valuable, but the Hubbard layer still passes
many domain concepts as interchangeable `u8`, `u16`, and `u32`: pattern number,
frequency-table index, instrument index, duration reload, transpose, repeat
count, absolute address, row tick, and frame. Raw addresses are locally justified
against the 64 KiB image, but the other domains are exactly where accidental
mixing causes silent musical errors.

The duplicated walkers and the `tempo` fallback mismatch demonstrate the cost.

**Recommendation:** introduce at least `PatternNumber`, `InstrumentIndex`,
`FrequencyIndex`, `RowTick`, `TempoReload`, and `Transpose`. Keep raw arithmetic
inside the decoding boundary and convert to `FrameIndex`/`NativePlacement` once.

### L2 — Environment variables alter production output invisibly

`SID_NO_FORWARD_GATE` and `SID_NO_ARP_PROCESSOR` change the emitted project when
present in the process environment. They are useful A/B switches, but are not CLI
arguments and are not recorded in the project/census.

See [`push_melodic_event`](../../crates/analyzer/src/export/synth.rs) and
[`arp_processor_for`](../../crates/analyzer/src/export/synth.rs).

**Recommendation:** confine these switches to tests/debug builds or surface them
as explicit unstable CLI flags and record their state in export metadata.

### L3 — Test suite gaps align with production risks

The test suite is substantial, but several important boundaries are skipped:

- integration helpers call `extract_native` directly, so they do not exercise
  the discarded first pass, CLI clock override, preflight, exit codes, or output
  atomicity;
- Hubbard tests overwhelmingly hard-code PAL;
- no adversarial test proves the onset gate rejects missing truth notes,
  duplicate native notes, or same-MIDI/wrong-cents notes;
- the structured round-trip test excludes an `order_repeat=true` fixture;
- its `flatten` helper ignores empty patterns and native pattern identity;
- supported-variant gate tests generally use only 400 calls and subtune 1, which
  may not reach later orderlist commands or effects;
- AWM has 13 subtunes, but the flagship production assertions focus on subtune 1.

**Recommendation:** test the public CLI path for a small matrix, add adversarial
unit tests for alignment, and generate per-subtune native coverage for every
asset. Keep fidelity budgets, but pair upper-bound budgets with minimum coverage
and exact invariant checks.

### L4 — Comments and behavior have drifted

Examples:

- “falls back” versus actual hard failure;
- “one-frame tick” versus reload 1;
- “exact pattern boundaries/real reused blocks” versus render-oriented regrouping;
- “inexact CIA excluded from gates” versus unconditional onset validation;
- “real-time playback is exact” versus 50/60 approximations;
- `HubbardExtractor` is still described as “locator stage only” immediately
  before its full extraction implementation.

These are not cosmetic in a reverse-engineered binary-format project: comments
serve as part of the format specification and future changes will be based on
them.

**Recommendation:** treat driver documentation and invariants as executable
specifications. Where possible, assert the claim (tick interval, correspondence,
structure round-trip) rather than restating it in prose.

## Step-by-step technical review

### 1. CLI, header, and preflight

Strengths:

- The CLI selects a typed `SubtuneIndex` and clock enum.
- RSID is refused by default, which is correct for an emulator without Kernal,
  BASIC, IRQ, and full C64 memory-map behavior.
- Multi-SID loss is disclosed before export.
- Native extraction completes before opening the project file, so identification
  and decode failures do not leave a new partial output.

Concerns:

- `--frames 0` is not rejected for time-based formats (M10).
- `SynthNative` is dispatched after generic emulation (M1).
- Multi-SID is only a warning even though native table decode may appear complete
  while secondary-chip musical material is absent.
- The parser contains an `.expect()` in library code for a fixed-length slice;
  it is logically safe after the preceding size check, but conflicts with the
  repository's stated no-`expect` rule and is easy to replace with direct array
  construction.

### 2. PSID load and 6502 call model

Strengths:

- Embedded little-endian load addresses are handled correctly.
- Init receives zero-based subtune in A, as required by PSID.
- A synthetic return address lets ordinary RTS-based init/play routines execute
  naturally.
- Init SID writes seed later state analysis.
- A rational cycle scheduler avoids cumulative integer frame-boundary drift.
- Writes and OSC3/ENV3 reads retain intra-call ordering and cycle offsets.

C64/SID limitations relevant to the trust model:

- RAM is flat; CPU port banking, ROM visibility, VIC, CIA, and IRQ behavior are
  not emulated. This is acceptable for supported PSID but explains the RSID ban.
- SID write-only registers are mirrored into RAM, so reads from `$D400-$D418`
  return the last stored byte rather than real-chip open-bus behavior. Correct
  PSID drivers should keep shadows in RAM, but this can make an invalid/nonportable
  driver work differently from hardware.
- SID writes are stamped at instruction start rather than the exact 6502 bus
  phase. The code documents this bound.
- Play calls start with A/X/Y = 0 and preserve other CPU state such as flags and
  stack between calls. This is a deterministic host convention, not a full C64
  interrupt-call environment.
- The production native path has no wall-clock deadline even though a deadline
  implementation exists for corpus tooling. The per-call one-million-cycle guard
  bounds CPU cycles but can still be expensive over large frame counts.

For AWM these constraints are reasonable: it is PSID, vblank-driven, single-SID,
and its high `$E000` payload benefits from flat RAM under the normally visible
Kernal ROM.

### 3. SIDId dispatch

Strengths:

- The signature database is vendored and embedded, avoiding runtime path drift.
- Signature matching intentionally mirrors SIDId's wildcard/`AND` behavior.
- Extractor selection is explicit and failures are typed with `thiserror`.

Concerns:

- `identify` returns the first matching player, even if multiple signatures
  match. `identify_all` exists but native dispatch does not use it to diagnose
  ambiguity.
- Identification scans the complete SID file buffer, including header metadata,
  rather than an explicitly loaded payload/RAM range. This mirrors existing
  SIDId behavior but should be stated at the trust boundary.
- The registry allocates boxed trait objects for every extraction. This is
  negligible beside emulation, but a static registry would be simpler.

AWM is identified as `Rob_Hubbard` and dispatched to `HubbardExtractor` as
intended.

### 4. Post-init RAM and Hubbard locator

Strengths:

- Locating after init is the correct boundary for self-modifying or initialized
  pointer tables.
- High-loaded players are scanned through `$FFFF`, which is necessary for AWM
  and Nemesis.
- The locator recognizes meaningful 6502 idioms rather than tune filenames or
  fixed relocation offsets.
- Variant fields are explicit: pointer stride, split/interleaved frequency table,
  duration mask, effect width, transpose encoding, repeat format, prescale,
  stall gate, voice count, and instrument layout.
- The instrument locator uses actual `$D405/$D406` dataflow anchors rather than
  assuming the table follows pattern data.

AWM recovered anchors observed in the post-init disassembly include:

```text
sequence pointer low/high: $EE98 / $EE9B
pattern pointer low/high:  $EEEC / $EF51
pattern zero-page pointer: $04/$05
pattern read:              $E4EF
duration mask:             $E4F7, AND #$1F
frequency table:           $E816, interleaved
instrument table:          $ECB0
tempo reload:              2
voices:                    3
```

The primary concern is not the known AWM layout but candidate cohesion (M4).
Returning a `Located<HubbardLayout>` with evidence addresses and confidence would
make both diagnostics and validation more robust.

### 5. Pattern and orderlist grammar

Strengths:

- `$FF` terminators and `$FE` order commands are distinguished.
- Status duration, tie/rest, extra byte, sticky instrument, slide byte width, and
  bit-7 note hold are decoded explicitly.
- AWM's separate-byte order transpose is supported.
- Frequency-table addressing correctly distinguishes interleaved and split lo/hi
  layouts.
- Note ends use half-open frame intervals.

Concerns:

- Three order walkers have already diverged (H3).
- `$FE` is skipped as a single command byte. If any Hubbard sub-family gives it
  operands or timing effects, the native structure cannot represent them.
- Transpose uses wrapping `u8` addition. That may match the driver's ADC path,
  but the required carry state/`CLC` invariant is not encoded or validated.
- Holds extend a previous note, while rests clear it, but duration agreement is
  not checked against the trace (M6).
- Hard caps do not report unterminated data (M5).

The AWM pattern-52 diagnostic is a good concrete regression fixture: one note
with instrument 12 and duration 31 is followed by hold rows and then rests. It
exercises the exact distinction between a sustained gate and a released row.

### 6. Row timing

Strengths:

- Simulating counters frame by frame is superior to a rounded
  frames-per-row formula.
- Prescale and whole-play stall gates are modeled separately.
- The tick map is shared by note decode and placement resolution in ordinary
  variants.

Concerns:

- H4 makes the no-divider branch inconsistent.
- Counter phase is initialized to reload values and the first observed tick is
  then rebased to frame 0. This intentionally discards the true post-init phase.
  The eight-call onset tolerance absorbs it, but native notes can be globally
  shifted relative to trace expression and gate state.
- Timing validation observes only note starts, not tick-to-tick residual or
  duration drift.

**Recommendation:** recover counter values from post-init RAM where addresses are
known, or align the decoded row clock to the trace using a single global phase
fit. Record maximum and cumulative row residual rather than hiding it inside a
wide per-note tolerance.

### 7. Native note construction and arpeggio collapse

Strengths:

- SID frequency conversion uses the physically correct `FREQ * phi2 / 2^24`.
- Native and trace notes share typed `NoteEvent` values.
- Manual gate-held arpeggios are collapsed before export to avoid representing
  the same effect as both retriggered notes and pitch modulation.
- Instrument indices are collapsed in lockstep with notes.

Concerns:

- Arpeggio collapse is driven by trace effect detection, so it is another place
  where native structure is changed by a heuristic trace classifier.
- It keeps the earliest decoded note in an effect span and drops all others,
  assuming exactly one gated truth note. That assumption is described but not
  asserted.
- Native `slide` bytes are parsed only to keep stream alignment; exported slides
  remain trace-derived.

### 8. Self-validation

The idea is excellent: a native decoder should not silently trust a located
binary grammar. The implementation is the weakest correctness boundary in the
current design because it reduces the comparison to H2.

A robust gate should emit a validation object such as:

```text
timing_exact
native_count / truth_count
matched / inserted / deleted
onset p50 / p95 / max calls
pitch p50 / p95 / max cents
duration p50 / p95 / max calls
per-voice metrics
structure coverage through requested frame
instrument/control agreement at matched onsets
```

That evidence belongs in the census and should decide whether native structure,
native instruments, or neither are safe to use.

### 9. Timbre and authored patches

Strengths:

- Per-note trace characteristics are computed on native note spans.
- Voice-3-as-LFO detection uses captured `$D41B/$D41C` reads.
- Native instrument indices replace heuristic patch clustering when the table is
  available.
- Packed versus columnar instrument layouts are differentiated.
- Authored ADSR and waveform are combined with trace-derived per-voice profiles
  rather than pretending every static instrument byte describes all runtime
  behavior.

Concerns:

- “Authored” is a mixed-confidence label (M9).
- Instrument-table location and contents have no independent onset-state
  validation. A wrong table can still group notes and stamp plausible ADSR.
- Grouping by instrument index can combine runtime variants if self-modifying
  tables or voice-specific effect state exist.
- Trace characteristics use native durations that the native gate does not
  validate.

### 10. Structured synth lowering

Strengths:

- The common synth backend avoids duplicating instrument and automation logic.
- Plans isolate automation targets so one patch's lanes do not control another
  instrument instance.
- Native placement transpose is lifted out of note pitches only when the
  round-trip is lossless.
- Full-length automation carriers solve the problem of reused note blocks needing
  absolute trace-derived lanes.
- Master `$D418` volume is represented once globally.
- The forward model checks each proposed pitch representation and degrades a
  rejected event to trace-derived pitch runs.

Concerns:

- The common backend is an 8700-line module with many coupled heuristics. It is
  difficult to establish which transformations apply to native versus heuristic
  input.
- Native pattern identity is not retained (M7).
- Physical voice monophony is distributed across tracks (M12).
- The forward-model module header calls itself report-only while downstream
  exporter code uses its `batch_passes` result to mutate output. The slice
  distinction is understandable internally but easy to misread at module level.
- Pitch verification grants modulation-dependent slack and skips attack frames;
  it is not an audio, amplitude, filter, waveform, or envelope equivalence test.

### 11. Serialization and census

Strengths:

- Serde-owned project types make schema drift visible at compile/test time.
- The census is detailed and printed on every export.
- Sidecar naming is predictable.
- Enriched notes safely use indexed optional lookups rather than panicking if a
  parallel array is unexpectedly short.

Concerns:

- Parallel array mismatch is silently omitted by `EnrichedNote::enrich`, while
  patch extraction itself asserts equality. A `NativeSong` constructor should
  enforce all parallel-length invariants once.
- Output is non-atomic (M11).
- Census success and project success are not one transaction.
- Several census categories count final render representations, not native
  decoder accuracy; the distinction should be explicit.

## AWM concrete result

The reviewed default-clock export completed with:

```text
3000 calls
14 instruments
14 tracks
479 native source notes
273 source notes classified as percussion
9/9 patches carrying authored data
109 vibrato spans
31 portamento spans
191 arpeggio spans
84 project patterns
104 project placements
2 note graphs
4192 automation points
```

Final pitch census:

```text
206 melodic notes OK
0 melodic notes failed
88 events degraded to a safer representation
0 events silently dropped
mean residual 14.2 cents
415/415 percussion checks OK
2 uncovered gated frames on voice 1
```

For comparison, the trace-derived `--format synth` path over the same window
contained 449 source notes, 13 instruments/tracks, 93 degraded events, mean
residual 10.6 cents, and no uncovered gated frames.

Interpretation: the native export is successful and final pitch safety is good,
but “zero failed” is partly the result of an active trace-based degradation
ladder. It is not evidence that all 479 native events, durations, instruments,
and placements were independently decoded exactly.

## Recommended remediation order

1. **Unify timing.** Carry one `PlaybackTiming` through init, trace, analysis,
   native pitch conversion, and synth lowering. Add PAL, NTSC, override, and CIA
   contract tests.
2. **Replace onset agreement.** Add monotonic one-to-one alignment with recall,
   cents, duration, per-voice metrics, and exact-timing eligibility.
3. **Unify Hubbard walkers.** One orderlist iterator must drive notes,
   placements, and diagnostic structure; cover repeat-count format.
4. **Make truncation and malformed data typed errors.** Never return a normal
   native song from an unterminated pattern/orderlist.
5. **Fix the tempo fallback and recover initial counter phase.** Test tick maps
   through locator output.
6. **Dispatch native before generic emulation.** Remove the discarded pass.
7. **Clarify product semantics.** Decide whether failure or actual fallback is
   desired, and label structured lowering/authored-field confidence accurately.
8. **Strengthen downstream fidelity.** Add physical-voice monophony and
   amplitude/envelope checks, then rendered audio A/B gates for named Hubbard
   windows.
9. **Make output atomic and self-describing.** Record extractor, validation
   metrics, clock/timing, corrections, and debug switches in project/census
   metadata.

## Exit criteria for a trustworthy Hubbard native export

A Hubbard native export should be called verified only when all of the following
hold for the requested window:

- one timing configuration is used throughout;
- the trace is timing-eligible;
- native/truth alignment is one-to-one with explicit precision and recall;
- onset, pitch, and duration residuals meet per-voice thresholds;
- no pattern/orderlist decoder hit a safety cap;
- every placement through the window comes from the same grammar walk as notes;
- native instrument fields are checked against SID writes at matched onsets;
- trace corrections are counted and labeled rather than conflated with native
  accuracy;
- the structured project round-trips under actual sequencer semantics;
- output project and census are committed atomically;
- the result passes both symbolic gates and targeted rendered-audio A/B tests.

Under those criteria, the current AWM export is a strong prototype with good
rendered pitch safety, but not yet a fully verified native reconstruction.
