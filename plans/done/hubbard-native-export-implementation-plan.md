# Hubbard native export implementation plan

Companion review: [`hubbard-native-export-code-review.md`](hubbard-native-export-code-review.md)

## Goal

Turn the current Hubbard `synth-native` path from a successful hybrid prototype
into a trustworthy native extraction pipeline with one timing domain, typed and
complete driver decoding, bidirectional trace validation, explicit provenance,
and deterministic project output.

The work is complete when a supported Hubbard export can prove, for the requested
window, which notes, instruments, effects, and placements came from driver data;
which fields were corrected or supplied from the trace; and why the result passed
its validation gates. Unsupported or timing-inexact variants must fail precisely
without producing a misleading native project.

This plan addresses every finding in the companion review. It does not add new
driver families, implement RSID, or pursue general spectral improvements except
where the current native Hubbard lowering can violate SID voice semantics.

## Principles

- Correctness gates precede coverage expansion. A stronger gate may initially
  reduce the reported `Rob_Hubbard` pass count; that is expected.
- One source of timing truth flows through init, play calls, analysis, native
  pitch conversion, and export.
- Safety limits produce typed errors, never silently truncated native data.
- Notes, placements, and diagnostic structure consume one driver grammar walk.
- Native accuracy and final rendered fidelity are separate metrics.
- Trace correction is allowed, but must be visible in provenance and census.
- Driver-specific facts live in `export/native/hubbard.rs` or a Hubbard-specific
  submodule. General render correction remains in the common synth exporter.
- Every slice lands with focused tests and keeps the full workspace green.

## Non-goals

- RSID/Kernal/BASIC/IRQ support.
- CIA timer-period recovery. This plan makes CIA limitations explicit and safe;
  accurate CIA scheduling remains emulator work.
- Exact reconstruction of unsupported Hubbard command semantics by guessing.
- New Galway, Crowther, Gremlin, GoatTracker, or Whittaker coverage.
- Replacing Pertylizer's project schema or renderer.
- Tune-specific fixes that cannot be expressed as a Hubbard format invariant.

## Baseline

Before production behavior changes, capture machine-readable baselines for:

| Fixture | Subtune/window | Purpose |
|---|---:|---|
| Auf Wiedersehen Monty | 1 / 3000 calls | transpose, holds, authored instruments, structured lowering |
| Commando | 1 / 3000 calls | packed instruments, one-byte effects |
| Sigma Seven | 1 / 3000 calls | two-byte effects and note ties |
| Knucklebusters | 1 and 2 / 3000 calls | prescale timing variants |
| Warhawk | 1 / 3000 calls | whole-play stall gate |
| Human Race | 1 / 3000 calls | manual arpeggio collapse |
| Ikari Union | 1 / 1500 calls | six-bit duration, split frequency tables, embedded transpose |
| Shape Music 2 | 1 / 1500 calls | seven-bit embedded transpose |
| Nemesis the Warlock | 1 / 400 calls | known rejected Hubbard variant |

Record, per fixture and voice:

- trace timing eligibility and resolved call rate;
- native and trace note counts;
- current onset agreement;
- first/last decoded frame;
- patch/instrument counts;
- placement and pattern counts;
- degradation, silent-drop, and uncovered-frame counts;
- serialized project and census hashes;
- elapsed extraction/export time.

Add a checked-in compact fixture such as
`tests/fixtures/hubbard_native_baseline.json`; do not check in generated
`.ptz` files. Baselines describe the starting state, not permanent
acceptance thresholds.

### Baseline exit gate

- One command regenerates the baseline deterministically.
- AWM's current 479 native versus 449 trace source-note discrepancy is visible.
- Every supported asset and the known Nemesis rejection has a recorded outcome.
- No production behavior changes in the baseline commit.

## Target architecture

The completed path should have these boundaries:

```text
CLI
  -> ResolvedPlayback { clock, call_rate, cia_exactness }
  -> emulation snapshot + trace using that same timing
  -> SIDId candidates
  -> Hubbard locator candidates with evidence
  -> Hubbard IR
       order streams
       patterns/events
       instruments/effects
       resolved per-voice placements
       native note timeline with provenance
  -> native/trace alignment report
       note precision + recall
       onset/pitch/duration residuals
       structure coverage
       instrument-at-onset checks
  -> accepted NativeSong + validation/provenance
  -> render-oriented synth lowering
  -> final forward/render census
  -> atomic project write + explicit sidecar result
```

The native gate validates extraction. The existing forward model validates the
chosen Pertylizer pitch representation. Neither metric substitutes for the
other.

## Slice 1 — unify timing and remove discarded work

Addresses: H1, M1, M3, M8, M10.

### Implementation

1. Introduce a typed resolved timing value at the CLI/emulator boundary. Extend
   `PlaybackTiming` or add a wrapper that exposes:

   - `SystemClock`;
   - rational calls per second;
   - seconds per call;
   - whether call timing is exact;
   - why timing is inexact.

2. Change `NativeContext` to carry resolved timing rather than an independently
   supplied `SystemClock`.
3. In every native extractor, construct post-init emulators with
   `Emulator::with_timing(ctx.timing)`.
4. Replace Hubbard's `emu::run` with `run_with_timing` and pass the same timing to
   `analyze`.
5. Make `$02A6`/`TVSFLG`, init timing, play scheduling, SID frequency conversion,
   effect rates, and synth time conversion read from the same resolved value.
6. Expose rational call rate to the export layer. Replace temporal uses of exact
   `50.0`/`60.0` with the resolved calls-per-second value. Keep `SystemClock` for
   chip model/pitch constants, not elapsed-time conversion.
7. Dispatch `Format::SynthNative` before generic trace/state construction in
   `run_export`; native extraction must perform only the work it consumes.
8. Reject `--frames 0` for time-based exports with a CLI/export validation error.
   Header-only mode remains unchanged. Do not silently infer a full song length.
9. Refuse native validation for CIA-timed subtunes while CIA call timing is
   inexact. Return a typed error naming the timing limitation and suggesting
   `--format synth` only if that fallback's limitations are acceptable.

Expected files:

- `crates/analyzer/src/bin/main.rs`
- `crates/analyzer/src/emu/mod.rs`
- `crates/analyzer/src/analysis/mod.rs`
- `crates/analyzer/src/export/json.rs`
- `crates/analyzer/src/export/native/mod.rs`
- all native extractor implementations constructing an emulator
- `crates/analyzer/src/export/synth.rs`

### Tests

- Unit-test rational `calls_per_second` and `seconds_per_call` for PAL and NTSC.
- Integration-test AWM with `auto`, forced PAL, and forced NTSC; assert the same
  timing reaches init, trace, analysis, native conversion, and export metadata.
- Use a synthetic init routine reading `$02A6` to prove override propagation.
- Assert CIA-native extraction refuses to use an inexact trace as ground truth.
- Add a CLI test proving `synth-native --frames 0` fails before identification or
  output creation.
- Add instrumentation under tests to assert the native CLI path performs one
  reference trace run, not the discarded generic pass plus a native pass.
- Update time-base tests to compare against rational scheduler duration rather
  than nominal 50/60 Hz.

### Exit gate

- No `Emulator::new()`, header-derived `emu::run`, or hard-coded PAL timing remains
  in production native extraction.
- A forced clock override changes `$02A6`, scheduling, frequency interpretation,
  and exported real time coherently.
- AWM 3000-call project duration agrees with emulator schedule within one output
  tick.
- CIA-inexact input cannot pass the native gate.
- Native CLI elapsed time no longer includes the discarded full emulation pass.

### Commit boundary

One timing/dispatch commit. Do not mix native alignment or Hubbard grammar
changes into it; baseline differences should be explainable solely by corrected
timing and removed work.

## Slice 2 — replace onset agreement with bidirectional alignment

Addresses: H2, M6, L3.

### Implementation

1. Replace `f64 onset_agreement` with a typed `NativeValidationReport`.
2. Align notes independently per physical SID voice using a monotonic one-to-one
   sequence alignment. A truth event may match at most one native event.
3. Preserve enough native evidence to compare:

   - native frequency-table index and raw SID frequency;
   - MIDI note plus cents;
   - start and exclusive end call;
   - instrument index;
   - source pattern/event location.

4. Define alignment costs from onset calls, pitch cents, and duration. Do not use
   a single broad Boolean tolerance during matching.
5. Report at least:

   - native count and truth count;
   - matched, inserted, and deleted events;
   - precision and recall;
   - onset median/p95/max residual;
   - pitch median/p95/max residual in cents;
   - duration median/p95/max residual;
   - metrics per voice and aggregate;
   - exact/inexact timing status.

6. Bootstrap acceptance thresholds from the supported fixture matrix. Thresholds
   must be named constants grouped in a `NativeValidationPolicy`, not scattered
   literals.
7. Make `DecodeUnreliable` carry a compact reason summary rather than only one
   percentage.
8. Attach the complete validation report to `NativeSong` and serialize it in the
   census sidecar. Keep it out of `.ptz` schema fields unless a stable
   metadata location exists.
9. Separate decoder acceptance from downstream forward-model results in census
   names and summaries.

Expected files:

- `crates/analyzer/src/export/native/mod.rs`
- `crates/analyzer/src/export/native/hubbard.rs`
- other native extractors adapting to the shared validation API
- `crates/analyzer/src/export/synth.rs` or a dedicated census module
- native integration tests and fixtures

### Tests

- Missing truth notes lower recall and fail even when every native note matches.
- Duplicate native notes cannot reuse one truth note.
- Same MIDI integer with wrong cents is measured and can fail.
- Correct notes with small monotonic onset drift align in order.
- Crossed/reordered events produce insertions/deletions rather than false matches.
- Wrong durations are visible even when starts and pitches match.
- Empty native and empty truth cases have explicit, non-vacuous outcomes.
- AWM, Commando, Sigma Seven, Knucklebusters, Warhawk, Human Race, Ikari Union,
  and Shape Music 2 establish reviewed thresholds.
- Nemesis remains rejected with a useful dominant reason.

### Exit gate

- No production decision depends on `onset_agreement` or `MIN_AGREEMENT`.
- Acceptance requires both minimum precision and minimum recall.
- Every match is one-to-one and monotonic per voice.
- The AWM 479/449 discrepancy is classified into concrete matches, insertions,
  and deletions.
- Validation output distinguishes native decoder accuracy from final rendered
  pitch fidelity.

### Commit boundary

Land the alignment engine and adversarial unit tests first, then migrate native
extractors and thresholds in a second commit if needed. Do not weaken thresholds
to preserve the previous pass count without explaining the alignment residuals.

## Slice 3 — introduce a typed Hubbard IR and one grammar walker

Addresses: H3, H4, M5, L1.

### Implementation

1. Split Hubbard code into focused modules if useful:

   ```text
   export/native/hubbard/
     mod.rs
     locate.rs
     ir.rs
     decode.rs
     validate.rs
   ```

   The exact file split is secondary; the IR and single-walker invariant are
   mandatory.

2. Introduce newtypes for at least:

   - `PatternNumber`;
   - `InstrumentIndex`;
   - `FrequencyIndex`;
   - `OrderOffset`;
   - `PatternOffset`;
   - `RowTick`;
   - `TempoReload`;
   - `RepeatCount`;
   - `Transpose`.

   Raw `u16` remains acceptable for local 6502 address arithmetic inside the RAM
   reader/locator boundary.

3. Decode orderlist bytes into a typed command stream such as:

   ```text
   Pattern { number, repeat }
   SetTranspose(value)
   DriverCommand { opcode, operands or Unsupported }
   Loop
   ```

4. Implement one stateful order walker yielding resolved pattern instances with
   voice, pattern, transpose, repeat ordinal, order offset, and row start.
5. Make native note decoding, placement resolution, and debug structure consume
   this walker. Delete their independent orderlist loops.
6. Correct the missing-tempo fallback: if no divider means one row per play call,
   represent it as `TempoReload(0)` and prove the resulting tick map.
7. Recover initial counter phase from post-init RAM when locator evidence includes
   the counter cells. Where phase is not known, represent it explicitly and fit
   one bounded global phase offset during validation; do not silently rebase the
   first tick to zero without provenance.
8. Replace vector-returning capped decoders with typed results:

   ```text
   Complete(value)
   Unterminated { address, limit }
   InvalidPointer { address }
   UnsupportedCommand { voice, order_offset, opcode }
   ```

9. Reject a native song when a pattern/order stream needed inside the requested
   frame window is incomplete. Unreached data beyond the window may remain lazy.
10. Preserve `$FE` commands in the IR. Continue only for command shapes proven to
    consume no operands and no row time; otherwise return `UnsupportedCommand`.

Expected files:

- `crates/analyzer/src/export/native/hubbard.rs` or new Hubbard submodules
- `crates/analyzer/src/export/mod.rs` for typed placements if shared
- `crates/analyzer/src/trace.rs` only if a shared row/frame newtype belongs there
- Hubbard unit and integration tests

### Tests

- Synthetic plain orderlist, separate transpose, embedded transpose, and loop.
- Synthetic repeat-count orderlist proving notes and placements use identical
  repeated instances.
- A repeat-count structured export flattens to the same timeline as flat export.
- No-divider layout produces one row tick per eligible play call.
- Prescale and stall fixtures retain their non-integer effective row schedules.
- Counter phase is either recovered or reported unknown and fitted within policy.
- Unterminated 512-byte pattern and 256-byte orderlist return typed failures.
- `$FE` with unknown operands fails rather than desynchronizing.
- AWM pattern 52 retains its one note, hold rows, and rest semantics.

### Exit gate

- Exactly one production order walker exists for Hubbard.
- `order_repeat`, transpose, command, and loop behavior cannot diverge between
  notes and placements.
- No cap can return silently truncated native data.
- No-tempo fallback tests demonstrate one call per row.
- All Hubbard decoder domain values crossing function boundaries use newtypes or
  a documented serialization/RAM exception.

### Commit boundary

Prefer three reviewable commits: newtypes/IR, unified walker, then timing/cap
behavior migration. Each commit must keep supported fixtures either passing or
failing with a more precise typed reason.

## Slice 4 — make locator candidates coherent and diagnosable

Addresses: M4 and the locator portion of L4.

### Implementation

1. Change `locate` from “assemble the first global matches” to candidate
   generation around a common Hubbard pattern-step anchor.
2. For each candidate, retain evidence addresses for:

   - pattern pointer loads and zero-page pointer;
   - pattern fetch and duration mask;
   - sequence pointer loads;
   - frequency-table reads and SID writes;
   - tempo/prescale/stall counters;
   - instrument ADSR loads/writes;
   - transpose/effect/repeat format signatures.

3. Require anchors to be locally or control-flow coherent. At minimum, constrain
   them to the same relocated routine region unless an explicitly supported
   dispatch relationship is found.
4. Use bounded address readers for instruction windows. Do not match an
   instruction shape by wrapping `$FFFF` into `$0000`.
5. Validate recovered pointer targets before decoding:

   - voice count is plausible without wrapping subtraction;
   - pattern pointers target loaded/post-init RAM;
   - frequency tables provide all referenced indices;
   - instrument table covers every referenced instrument;
   - counter reloads are semantically valid.

6. Score candidates with structural checks, then run Slice 2 alignment on viable
   candidates. Select one only when it clearly wins; otherwise return an ambiguous
   locator error with the candidate evidence.
7. Make `sid-re dis` labels derive from the selected candidate evidence rather
   than labeling every coincidental operand equal to a recovered address without
   context.

Expected files:

- Hubbard locator module
- `crates/analyzer/src/export/native/mod.rs`
- `crates/analyzer/src/bin/sid-re.rs` and disassembly label helpers
- locator tests/fixtures

### Tests

- Existing supported Hubbard assets retain the intended table addresses.
- Synthetic RAM with unrelated pointer-load shapes cannot assemble a mixed
  layout.
- A false early candidate loses to a later coherent candidate.
- A deliberately ambiguous image returns `LocateAmbiguous`.
- Candidate reads near `$FFFF` do not wrap into zero page.
- Invalid sequence/frequency/instrument ranges fail before native song decode.
- Nemesis either remains a precise validation rejection or becomes a precise
  locator/unsupported-format rejection; it must not produce a project.

### Exit gate

- Every selected `HubbardLayout` carries inspectable evidence.
- Independent global first-match fields are gone.
- Ambiguity is a typed failure, not config-file-order behavior.
- Corpus locate/pass counts and dominant failure categories are regenerated.

### Commit boundary

Keep locator refactoring separate from adding new Hubbard variant grammar. A
coverage increase is welcome only when candidate evidence and alignment pass.

## Slice 5 — validate instruments, effects, and native provenance

Addresses: M6, M9, and the provenance portion of M7/L4.

### Implementation

1. Extend the Hubbard IR with authored instrument records and per-field evidence:

   - ADSR;
   - waveform/control;
   - pulse-width initialization;
   - vibrato depth;
   - vibrato rate;
   - PWM period/step;
   - one-shot pulse-width offset;
   - drum drop, chirp, and arpeggio flags.

2. Give every exported field a provenance/confidence state, for example:

   ```text
   AuthoredVerified
   AuthoredDecoded
   AuthoredPartial
   TraceMeasured
   TraceCorrected
   Inferred
   Unsupported
   ```

3. At aligned note onsets, compare authored ADSR, waveform, pulse-width setup,
   and instrument changes against ordered SID writes/state. Account explicitly
   for driver setup writes preceding gate-on.
4. Do not export a guessed Hubbard `+5` high-nibble vibrato rate as fully
   authored. Until RE proves its semantics, mark it partial and prefer measured
   rate when available.
5. Use `sid-re dis/watch` to finish the `+5` high-nibble reverse engineering
   across at least AWM, Commando, and one other supported relocation before
   promoting rate to `AuthoredVerified`.
6. Include per-field provenance counts and mismatches in the native validation
   report/census.
7. Replace the binary `patches_authored` interpretation with field-level counts.

Expected files:

- Hubbard IR/decode/validation modules
- `crates/analyzer/src/analysis/timbre/patch.rs`
- `crates/analyzer/src/export/synth.rs`
- census serialization/tests
- `docs/drivers/hubbard.md`

### Tests

- Packed and columnar layouts produce the correct provenance per field.
- A wrong instrument-table candidate fails onset-state validation.
- Sticky instrument selection survives holds, rests, and pattern boundaries.
- AWM and Commando authored ADSR/waveform setup matches trace writes at named
  onsets.
- Unknown vibrato rate is never labeled verified.
- Measured versus authored effect selection records which candidate won and why.

### Exit gate

- “Authored patch” is no longer an all-or-nothing claim.
- Every exported Hubbard effect parameter has field-level provenance.
- Instrument validation contributes to native acceptance or a clearly separate
  partial-support policy.
- Hubbard documentation matches the implemented and verified byte semantics.

## Slice 6 — separate recovered structure from render-oriented lowering

Addresses: M7 and the structural part of M6.

### Implementation

1. Keep the exact Hubbard IR available through the native boundary: order
   commands, pattern IDs, events/rests/holds, instruments, transpose, repeats,
   and loop points.
2. Rename the current project-facing structure to make its semantics explicit,
   e.g. `NativePlacementTimeline` or `StructuredRenderPlacements`.
3. Add structure validation before lowering:

   - every native note maps to exactly one source event or documented collapse;
   - every requested-window pattern instance has a placement;
   - placement starts and ends match the shared row clock;
   - transpose round-trips;
   - no required instance disappears before render-plan splitting.

4. Keep render-oriented transformations explicit and counted:

   - empty/rest-only placements omitted;
   - leading silence lifted into placement start;
   - native patterns split across timbre tracks;
   - distinct pattern numbers content-deduplicated;
   - trace expression preventing reuse.

5. Avoid naming a deduplicated project block after only the first native pattern
   when several IDs map to it. Store all source pattern IDs in census metadata or
   use a neutral rendered-block name.
6. Extend the structured round-trip test to evaluate actual Pertylizer sequencer
   semantics, including placement transpose, note overhang, legato boundaries,
   automation, and looped reuse—not only a note multiset.

Expected files:

- Hubbard IR and native shared types
- `crates/analyzer/src/export/mod.rs`
- `crates/analyzer/src/export/native/mod.rs`
- `crates/analyzer/src/export/synth.rs`
- `crates/analyzer/tests/synth_structure.rs`

### Tests

- Exact IR snapshot for a short AWM order/pattern window.
- Plain, transposed, repeated, rest-only, and empty pattern instances.
- Same native ID rendered under different trace expression.
- Different native IDs with identical rendered content.
- Long held note crossing a native pattern boundary.
- Placement transpose clamp/fallback round-trip.
- Structured and flat projects produce equivalent sequencer note events and
  automation over the tested window.

### Exit gate

- Documentation never calls rendered-block deduplication exact native pattern
  identity.
- Exact recovered structure remains inspectable independently of project
  lowering.
- Every structural loss/transformation has a census count.
- A repeat-count fixture and AWM pass engine-semantic structured round-trip tests.

## Slice 7 — preserve physical SID voice semantics in split-track lowering

Addresses: M12.

### Investigation first

Before changing the representation, add diagnostics for each physical voice:

- simultaneous active Pertylizer notes across plans derived from that voice;
- overlap length and amplitude estimate;
- whether overlap corresponds to a measured SID release, retrigger, or patch
  switch;
- waveform/instrument changes during overlap;
- named worst windows for AWM, Commando, Warhawk, and Knucklebusters.

Confirm with Pertylizer renders whether current measured amplifier automation
already removes the suspected overlap in practice. Do not refactor tracks based
only on topology.

### Implementation

If diagnostics show false polyphony:

1. Define a physical-voice ownership model in track plans.
2. Ensure a new note/instrument on one SID voice chokes or transfers the prior
   plan exactly when the SID envelope/oscillator changes ownership.
3. Prefer explicit voice bus/choke semantics if Pertylizer supports them. If not,
   enforce mutually exclusive amplifier automation at the shared voice boundary.
4. Preserve genuine measured release only until the next SID attack or waveform
   ownership change; never leave an independent old synth releasing underneath a
   new hardware voice state.
5. Add amplitude/envelope coverage to the forward census. Pitch-only success is
   insufficient for this gate.

Expected files:

- `crates/analyzer/src/export/synth.rs`
- `crates/analyzer/src/export/forward.rs` or a new amplitude validation module
- Pertylizer schema/capability tests if choke/bus support is used
- render fixtures and fidelity budgets

### Exit gate

- Every project plan maps to one physical SID voice owner.
- False same-voice overlap is zero in symbolic diagnostics.
- Named patch-change windows match trace attack/release timing and rendered peak
  level.
- Existing pitch/percussion budgets remain green.
- If investigation finds no audible/symbolic defect, document the proof and keep
  the current lowering rather than adding machinery.

## Slice 8 — make CLI/output contracts deterministic and durable

Addresses: M2, M11, L2, and remaining L4 contract drift.

### Decisions

- `--format synth-native` remains a strict native request. It fails when native
  extraction or validation fails; it does **not** silently fall back.
- `--format synth` remains the explicit trace-derived path.
- The `.ptz` file is the required artifact. The census sidecar remains
  best-effort unless a future CLI flag makes it required.

### Implementation

1. Replace all “falls back” wording with the strict contract in CLI help, errors,
   module docs, and driver docs.
2. Include extractor, resolved timing, native validation summary, provenance
   summary, and trace-correction counts in the census.
3. Replace direct `File::create` project output with sibling temporary-file
   write, flush, optional sync, and atomic rename.
4. Preserve the existing output if serialization, disk write, or flush fails.
5. Write the sidecar through its own temporary file and atomic rename. Keep its
   failure non-fatal but unambiguously warn and remove/replace no valid prior
   sidecar until the new one is complete.
6. Move `SID_NO_FORWARD_GATE` and `SID_NO_ARP_PROCESSOR` behind explicit unstable
   CLI/debug configuration or compile-time test helpers. Record any enabled
   non-default switch in census metadata.
7. Construct `NativeSong` through a validating constructor that checks parallel
   note/patch/characteristic/provenance lengths. Do not let `EnrichedNote::enrich`
   silently hide internal native array mismatches.
8. Remove library `.expect()` usage encountered on this path and replace manual
   invariant panics with typed errors where input can influence them.

Expected files:

- `crates/analyzer/src/bin/main.rs`
- native error and song construction code
- synth/census serialization
- CLI integration tests
- README and relevant docs

### Tests

- Unsupported native driver exits non-zero, recommends explicit `--format synth`,
  and creates no output.
- A failed write leaves an existing project byte-identical.
- Successful write atomically replaces the project.
- Sidecar failure warns without corrupting a previous valid sidecar or changing
  project success.
- Internal parallel-array mismatch is rejected at construction.
- Default exports are unaffected by unrelated process environment variables.
- Help text and error snapshots state strict native semantics consistently.

### Exit gate

- Native failure behavior is identical in code, help, errors, and documentation.
- Project writes are atomic on the target filesystem.
- Debug switches cannot silently alter a production export.
- Census identifies timing, extractor, native validation, provenance, and
  corrections for every successful native project.

## Slice 9 — corpus qualification and documentation closure

Addresses: L3, L4, and final verification of all findings.

### Implementation

1. Add a repeatable Hubbard qualification command using existing binaries/tests,
   not an ad-hoc script. It must emit a compact JSON and human summary.
2. Run every asset subtune that is practical, not only subtune 1. At minimum,
   exercise all 13 AWM subtunes and all current supported-variant fixture
   subtunes named in the baseline.
3. For an available HVSC root, regenerate:

   - SIDId Hubbard count;
   - locate success/failure/ambiguity;
   - validation pass/failure by dominant reason;
   - timing-inexact exclusions;
   - instrument/structure support levels;
   - elapsed time and peak memory where available.

4. Pin per-asset validation minima and final forward-model maxima only after the
   stronger gates are stable.
5. Update:

   - `docs/drivers/hubbard.md` with the typed grammar and verified effects;
   - `docs/export.md` with native-versus-trace provenance;
   - `docs/PLAN.md` with measured coverage, not the old agreement percentage;
   - CLI/README examples with strict failure and nonzero frames.

6. Re-review every H/M/L item from the companion report and mark it resolved,
   deliberately deferred with owner, or superseded with evidence.

### Exit gate

- All AWM subtunes have recorded native outcomes and no unexplained acceptance.
- Supported fixtures meet native alignment, structure, and final fidelity gates.
- Nemesis and other unsupported variants fail with stable, precise reasons.
- Corpus coverage claims name the validation policy and timing eligibility.
- `cargo fmt --check`, `cargo build --workspace`,
  `cargo clippy --workspace --all-targets`, and `cargo test --workspace` pass with
  zero warnings or errors.
- The implementation plan and review contain no known behavior claims contradicted
  by production code.

## Finding-to-slice traceability

| Review finding | Owning slice |
|---|---|
| H1 mixed clock domains | Slice 1 |
| H2 weak onset gate | Slice 2 |
| H3 repeat placement divergence | Slice 3 |
| H4 tempo fallback | Slice 3 |
| M1 discarded emulation | Slice 1 |
| M2 nonexistent fallback | Slice 8 |
| M3 CIA trace used as truth | Slices 1–2 |
| M4 unrelated locator anchors | Slice 4 |
| M5 silent decoder truncation | Slice 3 |
| M6 incomplete validation scope | Slices 2, 5, 6 |
| M7 rendered grouping called exact structure | Slice 6 |
| M8 nominal versus rational frame rate | Slice 1 |
| M9 guessed authored vibrato rate | Slice 5 |
| M10 zero-frame native request | Slice 1 |
| M11 non-atomic output | Slice 8 |
| M12 split-track false polyphony risk | Slice 7 |
| L1 primitive domain values | Slice 3 |
| L2 invisible environment switches | Slice 8 |
| L3 production-boundary test gaps | All slices, closed in Slice 9 |
| L4 stale/overstated documentation | Updated per slice, audited in Slice 9 |

## Implementation order and dependencies

```text
Baseline
  -> Slice 1: timing/dispatch
     -> Slice 2: alignment gate
        -> Slice 3: Hubbard IR/walker
           -> Slice 4: locator candidates
           -> Slice 5: instrument/effect provenance
           -> Slice 6: structure/lowering boundary
              -> Slice 7: physical-voice semantics
  -> Slice 8: output contracts (may begin after Slice 2 APIs stabilize)
  -> Slice 9: corpus qualification and documentation closure
```

Do not start locator coverage expansion before Slice 2 can reject insertions and
deletions. Do not refactor structured rendering before Slice 3 supplies one
authoritative placement stream. Slice 7 begins with measurement and may conclude
without a representation change. Slice 8's atomic writer is independent, but its
census metadata should wait for the validation/provenance types from Slices 2
and 5.

## Recommended logical commits

1. Add baseline command and fixture; no behavior change.
2. Introduce resolved rational timing and propagate it end to end.
3. Dispatch native early and validate nonzero frame requests.
4. Add one-to-one alignment engine and adversarial tests.
5. Migrate native extractors to typed validation reports.
6. Add Hubbard newtypes and IR.
7. Replace duplicated order walkers and fix repeat/tempo/cap behavior.
8. Introduce coherent locator candidates and evidence diagnostics.
9. Add instrument/effect field provenance and validation.
10. Separate exact Hubbard IR from structured render placements.
11. Add same-voice overlap diagnostics; fix lowering only if the gate proves a
    defect.
12. Make project/sidecar writes atomic and formalize strict native CLI behavior.
13. Run corpus qualification, pin policies, and close documentation.

Every commit must pass the repository's four pre-commit commands. Changes that
alter a baseline must state whether the difference is a corrected decoder,
stronger rejection, trace correction, rendering change, or serialization-only
change.

## Final acceptance criteria

The plan is complete when all of the following are true:

- one resolved timing object controls the entire native export;
- native validation uses one-to-one per-voice alignment with precision and
  recall, pitch, onset, and duration evidence;
- timing-inexact traces cannot authorize native output;
- one typed Hubbard grammar walk drives notes and placements;
- repeat, transpose, loop, hold/rest, effect width, and tempo variants are
  covered by tests;
- safety limits and unsupported commands are typed failures;
- locator selection is coherent, bounded, and evidence-bearing;
- authored instrument/effect fields carry granular provenance;
- exact recovered Hubbard IR is distinct from render-oriented project grouping;
- false physical-voice polyphony is either eliminated or disproven by symbolic
  and render evidence;
- native failure is strict and documented; output replacement is atomic;
- debug switches are explicit and recorded;
- all AWM subtunes and supported Hubbard fixtures have qualification results;
- final census separates native extraction accuracy, trace corrections, and
  rendered fidelity;
- the full pre-commit command set passes cleanly.
