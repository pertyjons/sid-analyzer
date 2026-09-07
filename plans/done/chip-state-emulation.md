# Chip-state emulation — slice plan

*Emulate the SID's **digital** internal state — the envelope generator first,
then the oscillator accumulator and noise LFSR — with **one** implementation
that runs live under the emulation layer and is replayed for analysis. Today
[`analysis::voice`](../../crates/analyzer/src/analysis/voice.rs) and the
[forward-model gate](forward-model-gate.md) **infer** envelope phase, gate onset
and release from register writes and gate edges; the known ear-bugs
(release-tail pitch motion, hard-restart pollution, gate-edge note-on/off) are
inference failures. A cycle-clocked ADSR state machine replaces "guess the
phase" with "compute it." E3-style slices — each lands alone, gate green, with a
measurable exit.*

*Incorporates two review passes
([`chip-state-emulation-review.md`](chip-state-emulation-code-review.md), latest
supersedes): a shared live digital-SID core; an explicit timing contract before
the envelope work; a **separate host clock** distinct from `cpu.cycles`; a
**play-call** timeline primitive (not "video frame"); interval-aware envelope
activity and an ordered event log; corrected voice-3 routing audibility; and a
vblank-only Slice 0 with a loud CIA exclusion.*

## Scope: the digital chip, not the analogue audio

The SID splits cleanly, and only one half is worth reimplementing:

- **Digital, in scope.** The envelope generator (8-bit level + ADSR phase +
  internal counters), the 24-bit oscillator phase accumulator, and the 23-bit
  noise LFSR — deterministic integer state machines.
- **Analogue, out of scope.** Waveform DACs with combined-waveform bleed, the
  two-integrator filter, DC offsets, the C64 output RC network — reSID/sidera
  territory. We compute *state*, not *sound*.

Only the filter's analogue integrator state is excluded; its *register* state is
already decoded by [`analysis::filter`](../../crates/analyzer/src/analysis/filter.rs).

### Top-level exactness invariant

> The state machines are bit-exact **for a supplied cycle/event timeline**.
> Whole-tune reconstruction is cycle-accurate only to the fidelity of the
> **captured timeline** — bounded by instruction-start write stamping, assumed
> idle time, unavailable CIA timing, the init→first-play host policy, and the
> selected NTSC raster model.

This sentence belongs verbatim in the doc comments of the replay and snapshot
APIs, not only here. Slice 0 is the work of making the timeline as faithful as
it can be and documenting each remaining bound.

## Timeline vocabulary (the boundary contracts)

Every later correctness claim rests on four distinct clocks. Name them once:

### Play call is the scheduling primitive, not "video frame"

Today `FrameTrace` = one `play` invocation and `FrameIndex` = the play-call
index (`emu/mod.rs`). That resembles a raster frame **only** for vblank tunes;
CIA/multi-speed tunes issue several calls per video frame. Therefore:

- The scheduling/timeline primitive is the **play call**: `PlayCallIndex`,
  `cycles_per_call` (**replaces** `CYCLES_PER_FRAME` in the core API).
- `FrameTrace`/`FrameIndex` are retained but **documented as "one scheduled play
  call, not a raster frame."** A presentation-frame aggregation (bucketing calls
  into video frames) is deferred until CIA timing lands — and CIA tunes are
  excluded from Slice 0 anyway (§Timing contract, Option A).
- `release_frame`, `sound_end_frame`, note durations and `PlaybackTiming` all
  measure in **call** units until the aggregation layer exists.

### Audibility predicates (the old `gate/env/audible` triad, corrected)

The prior draft's `audible` rule was wrong: `$D418` bit 7 (`voice3_off`) removes
voice 3 only from the **direct/unfiltered** path — a filter-routed voice 3 is
still heard. Master volume 0 is a *chip-output* silence, not a *voice-content*
condition (digi/tremolo zero it deliberately). Split into three predicates:

- **`voice_signal_active`** — `envelope_active` (interval-aware, below) **and** a
  waveform bit set **and** test bit clear. Voice-local, pre-routing.
- **`voice_routed`** — the voice reaches output via the direct path **or** the
  filter (`RES_FILT` routing bits + `$D418`). For voice 3: routed if
  filter-routed, **or** (`voice3_off` clear **and** direct); voices 1/2 are on
  the direct path unless filter-routed.
- **`chip_output_active`** — `voice_routed` **and** master volume `> 0`.

**Slice 2a's content mask is `voice_signal_active`, never `chip_output_active`** —
a note must not vanish because master volume is momentarily zero.

## Why

`VoiceState` carries `Adsr`, but that is only the four register nibbles
(`Adsr::from_bytes`). **We never compute the envelope's level or phase over
time.** Downstream reconstructs it heuristically — `gate_retrigger` from gate
edges (`analysis/mod.rs`), `detect_notes()` from gate edges (`analysis/note.rs`),
the forward gate's `release_tail_end` from *frequency motion* (`export/forward.rs`).
A real envelope generator turns "level/phase of voice *v* at call *c*?" into a
lookup, directly attacking the classes in `forward-model-gate.md` §slice-7.

## Architecture: one live core, replayed for analysis

An analysis-only model replaying a *completed* trace cannot be correct: a program
that does `LDA $D41C` and branches on it needs the envelope value **at the read**,
during execution (today `$D41C` returns raw RAM ≈ 0 — `PLAN.md` [MED risk]). One
implementation, two call sites (two *instances* are fine):

```text
emu::sid::DigitalSid
├── Envelope × 3            // slice 1
├── Oscillator × 3          // slice 3a/3b
├── Noise × 3               // slice 3c
├── register file (authoritative functional state)
├── clock_to(ChipCycle)
├── write(reg, value, ChipCycle)
└── read(reg, ChipCycle) -> u8     // OSC3/ENV3 computed at the read cycle
```

1. **Live in `Bus`** — reads computed at the read cycle, affecting CPU execution.
2. **Trace-replay for analysis** — same type, fed the write/read timeline, no CPU
   in the loop.

### Bus/core ownership + event ordering (locked)

- `DigitalSid` owns the **authoritative** functional register state; Bus RAM
  keeps a **mirror** only for compatibility/inspection.
- **On a SID write:** advance the core to the access `ChipCycle` → apply the write
  to the core → record the trace event → update the RAM mirror. Replay uses the
  **same advance-before-write ordering**.
- **`$D41B`/`$D41C` reads** come from the core after advancing to the read cycle,
  **never** from the RAM mirror.
- Writes to read-only SID registers have **explicitly documented** behaviour.
- **Same-instruction-start-timestamp writes must retain capture order** — do not
  sort by `SubFrameOffset` alone; preserve insertion order (or add a sequence
  number). Trace vectors already preserve insertion order; make it a **contract
  and a test**, not an accident.

## Timing contract (Slice 0 substance)

### A separate host clock — do not repurpose `cpu.cycles`

`cpu.cycles` backs the per-call `CYCLE_GUARD`, `SubFrameOffset`, the runner's
instruction timing, and the bus `total_cycles` mirror. Adding idle cycles to it
would corrupt all four. Use a dedicated timeline:

```rust
struct EmulationClock { call_origin: ChipCycle, cpu_start: u64, next_call: ChipCycle }
// current_chip_cycle = call_origin + (cpu.cycles - cpu_start)
```

At call return, advance `DigitalSid.clock_to(next_call)`, then move `call_origin`
to that boundary. `cpu.cycles` stays the library's executed-cycle counter.

Locked decisions:

- **Epoch:** `ChipCycle(0)` = **init entry**.
- **Init→first-play host policy (default):** SID state advances during every
  executed init instruction; after init returns, the first `play` occurs at the
  first complete scheduled call boundary after init (unless the chosen PSID
  convention requires an immediate first call); if init overruns that boundary,
  record the overrun and apply a documented catch-up. **Labelled a host policy,
  not hardware truth,** until verified against the PSID spec / oracle.
- **Non-integer call periods:** when `phi2_hz / call_rate` is fractional, the
  next boundary uses a **rational / fixed-point accumulator** carried across
  calls — not fresh rounding per call — so long tunes do not drift.
- **Overrun:** a routine running past its scheduled boundary is recorded, not
  silently truncated.

### Newtypes + read events

- `ChipCycle` (absolute, `u64`), `CpuCycle`/`CpuCycles` (CLAUDE.md lists
  `CpuCycle`; none exist yet). `SubFrameOffset` is **redefined and tested** as a
  cycle offset at instruction start (its doc comment is stale), with the
  insertion-order contract above.
- **Timestamped reads** (the trace stores only a `voice3_reads` count today):

  ```rust
  struct RegisterRead { reg: SidRegister, value: u8, offset: SubFrameOffset }
  ```

  Covering at least `$D41B`/`$D41C`; `voice3_reads` becomes a **derived**
  convenience. Replay does not need reads to evolve the envelope (writes +
  boundaries suffice), but they are required to compare live vs replay at a
  checkpoint, distinguish OSC3 from ENV3 use, debug live-read trace changes, and
  build committed oracle fixtures. **Read-event infrastructure lands in Slice 0**;
  ENV3 *values* are populated live in Slice 1.
- The trace records per-call `start_cycle: ChipCycle` + `duration: CpuCycles`
  (min: `init_duration` + `play_duration`).

### Host-model honesty + CIA boundary (Option A)

- `cycles_per_call` for PAL vblank = **19 656** (985 248 / 50.1245).
  NTSC ≈ **17 095** is **one** configuration — a *selected host model*, not a
  universal constant (C64 NTSC variants differ; the PSID clock flag need not fix
  raster geometry).
- **Slice 0 is vblank-only.** CIA-timed subtunes are **marked unsupported for
  envelope ground truth**, emit a **loud diagnostic**, and are **excluded from
  exactness/census gates**. Faithful CIA scheduling (snoop `$DC04/$DC05`, the
  `PlaybackTiming` [HIGH] item) is a later prerequisite slice. Producing envelope
  values for a CIA tune on a known-wrong 50 Hz timeline and presenting them as
  ground truth is the specific failure this avoids.

## The envelope model (locked specification)

Canonical reSID algorithm — public, model-agnostic (6581 = 8580), ~150 lines.

```
RATE_PERIOD[0..16] = 9, 32, 63, 95, 149, 220, 267, 313,
                     392, 977, 1954, 3126, 3907, 11720, 19532, 31251
sustain_level(s)   = (s << 4) | s
exp_period(env)    = 1 @0xFF, 2 @0x5D, 4 @0x36, 8 @0x1A, 16 @0x0E, 30 @0x06, hold @0x00
```

**Rate counter width:** stored in a **`u16`**; the **logical** counter is
**15-bit** (`0x0000..=0x7FFF`). The increment can transiently set bit
`0x8000`; the delay-bug branch detects that and folds it back
(`counter = (counter + 1) & 0x7FFF`), reproducing the SID counter wrap. **Pin the
`0x7FFF → 0x8000` transition in tests.**

**Ordering (each an off-by-one trap; reference sidera `src/envelope.rs`,
`SID_ANALOG_SPEC.md` §8 — implemented from spec, not copied):**

- Increment rate counter (with the wrap) **first**, then compare `!= rate_period`
  → return on mismatch; on match reset to `0`.
- Increment exp counter each match; a step fires when
  `state == Attack || exp_counter == exp_period`, then exp counter resets to `0`;
  `hold_zero` short-circuits before any step.
- **Attack** increments `env`; at `0xFF` → `DecaySustain`, load `RATE_PERIOD[decay]`.
- **DecaySustain** decrements **only while `env != sustain_level(sustain)`** —
  never seeks *upward* toward a raised sustain.
- **Release** decrements toward `0`.
- Landing on `0` in **any** phase sets `hold_zero` (reSID's bottom
  value-switch) — including a DecaySustain decay to zero with the gate held,
  or a later sustain raise would wrap the decrement `0x00 → 0xFF`.
- `exp_period` reloads from the **new** `env` after the step.
- Gate `0→1` → `Attack` + `RATE_PERIOD[attack]` + clear `hold_zero`; `1→0` →
  `Release` + `RATE_PERIOD[release]`. AD/SR writes reload the **active** phase's
  `rate_period` in place (the delay bug's cause).

**Power-on state (part of the live contract — affects init-time ENV3 reads):**
`env = 0`, `phase = Release`, `rate_counter = 0`, `rate_period = RATE_PERIOD[0]`,
`exp_counter = 0`, `exp_period = 1`, `gate = false`, `hold_zero = true`, A/D/S/R
= 0.

## Representation

Grouped, not parallel arrays:

```rust
struct EnvelopeSnapshot   { level: EnvLevel, phase: EnvPhase }          // at the sampling point
struct EnvelopeFrameActivity {                                          // interval-aware, per call
    start_level: EnvLevel, end_level: EnvLevel, peak_level: EnvLevel,
    active_cycles: CpuCycles,
    first_nonzero: Option<SubFrameOffset>, reached_zero: Option<SubFrameOffset>,
}
struct DigitalVoiceState  { envelope: EnvelopeSnapshot, oscillator: OscillatorSnapshot /* slice 3 */ }
```

`FrameState` gains three `DigitalVoiceState`. **Content decisions use
`active_cycles > 0`, not `end_level > 0`** — the snapshot misses an envelope that
was active for part of the call and returned to zero before the sampling point.

**Ordered event log** (replaces lossy `entered_attack: u8` / booleans, which lose
count and order when several occur in one call):

```rust
enum EnvelopeEventKind { EnteredAttack, LeftZero, ReachedZero, EnteredDecaySustain, EnteredRelease }
struct EnvelopeEvent { offset: SubFrameOffset, kind: EnvelopeEventKind }
```

Booleans/counts for the census are **derived** from this list, so Slice 2b never
has to reconstruct event order.

**Sampling point, defined exactly:** after all of the call's writes *and* the
idle advancement, immediately before the next call boundary.

### Note-endpoint migration (compatibility)

`NoteEvent.end_frame` is today a two-meaning bug (`PLAN.md`). Replace with
explicit endpoints — and state what each consumer uses:

`NoteEvent.end_frame` remains the stored exclusive release endpoint so native
extractors can author it without an envelope trace. `release_frame()` names that
meaning, while `sound_end_frame(states)` computes the separate envelope-zero
endpoint and returns `None` when the trace censors the release.

- **MIDI** note-off uses `release` (unless a release tail is intentionally encoded).
- **Timbre** uses the `voice_signal_active` interval.
- **Synth export** may use `sound_end` plus release-phase pitch events.
- An unfinished note/release at trace end keeps `None` — **never silently
  closed** at the trace end. `sound_end_frame` names the call containing the
  transition to zero (subframe offset retained internally); if the release is
  still active at trace end, the endpoint is **unknown/censored**, not the trace
  end.

The old inclusive `frame_range()` ambiguity is removed; the stored endpoint is
release-only and sound duration is never inferred from that field.

## Slices

### Slice 0 — timing contract + digital-SID core (vblank-only) *(SHIPPED)*

- Introduce `ChipCycle`/`CpuCycle`; redefine + test `SubFrameOffset`; add the
  `EmulationClock`, per-call `start_cycle`/`duration`, and `RegisterRead` events
  (`voice3_reads` derived).
- Concrete init→first-play policy; rational call-boundary accumulator; emulation
  owns idle advancement.
- Create `emu::sid::DigitalSid` (register file + `clock_to`/`write`/`read`),
  **empty of generators**; establish bus/core ownership + event ordering.
- Thread `PlaybackTiming` through `analyze(trace, timing)` (touches CLI, tests,
  native extractors — no back-compat needed). Mark + exclude CIA subtunes with a
  loud diagnostic.

*Exit gate — structural, not just byte-identical* (an empty core can preserve
outputs on a *wrong* timeline):

- live and replay produce **identical absolute timestamps** for every captured
  SID write; call start/end cycles monotonic and non-overlapping; every write
  offset within its call duration; idle advancement ends exactly at the next
  boundary; **accumulated scheduling error bounded** over a long synthetic run;
  same-offset writes retain capture order; init writes replay at their original
  relative offsets.
- **Golden hashes** over deterministic outputs across `assets/music`
  (`.ptz`/JSON/MIDI; text normalised for env-dependent paths first).

### Slice 1 — live envelope + ENV3, report-only *(SHIPPED)*

- Implement `Envelope × 3` in `DigitalSid`; clock at reads, writes, and scheduler
  boundaries; wire **`$D41C` live** in `Bus`.
- **Leave the existing OSC3 model (`advance_v3_accum`/`osc3_output`) intact** —
  possibly behind a `DigitalSid` adapter; it is **not** replaced here (§Deletions).
- Reuse the same core in replay for `EnvelopeSnapshot` +
  `EnvelopeFrameActivity` + the `EnvelopeEvent` log on `FrameState`. Census only;
  **no downstream enforcement** (`detect_notes`, forward gate unchanged).

*Exit gate — by cohort:*

- **No-ENV3-read tunes:** trace **and** all normal outputs byte-identical.
- **ENV3-read tunes:** changes expected, but each must **originate from a
  captured ENV3 read-value difference** and be reviewed end-to-end (a changed
  ENV3 read can cascade through the rest of that tune's trace — do not claim the
  change is "confined").
- **Synthetic ENV3 fixture:** output matches the chosen oracle / committed golden.
- The census schema change is **intentional and tested**; **no change in existing
  fidelity classifications**; OSC3 behaviour **unchanged**.

### Slice 2a — release-tail + content migration *(SHIPPED)*

- Content mask = **`voice_signal_active` with `active_cycles > 0`** (interval-
  aware), replacing slice-7 `release_tail_end`'s frequency-motion *existence*
  test and sharpening `drop_ungated_notes` (parked, decayed row = `active_cycles
  == 0`).
- **Keep frequency-motion** for *how* the tail's pitch is represented — it only
  stops deciding *whether* the tail exists.
- Land the `release` / `sound_end` endpoint semantics above.

*Exit gate:* named tails (AWM V2 stab, drum-drop tails) extend to envelope zero;
`active_cycles == 0` parking rows removed; `synth_fidelity_budget` /
`render_fidelity_budget` hold or improve; every changed exported note classified.

### Slice 2b — retrigger + onset migration *(SHIPPED)*

- Replace `gate_retrigger` with **attack-entry events from the `EnvelopeEvent`
  log** (an intra-call attack→0xFF→decay is invisible to end-of-call phase).
- Keep an **exact envelope retrigger** separate from the driver-level
  **`HardRestart`** classification (gate/ADSR-prep/timing).
- **Preserve onset** at the gate/attack event; use `LeftZero` to drop gate blips
  that never produced a level; keep any subframe onset timestamp separate. Do
  **not** shift onset by requiring end-of-call `env > 0`.

*Exit gate:* no new ghost notes corpus-wide; intra-call attack onsets preserved;
Nemesis V1 hard-restart case + native-extractor gates revalidated.

### Slice 3a — basic oscillator core *(SHIPPED)*

Three 24-bit accumulators on the absolute clock; frequency writes; test-bit
semantics (verified or explicitly simplified); **saw/tri/pulse only**. Replaces
the old accumulator + simple-waveform path in `emu/bus.rs` **after parity**.

*Exit gate:* parity with the existing OSC3 model for the simple waveforms it
supports; oracle agreement for accumulator/saw/tri/pulse, freq changes, test
transitions; no change for tunes without OSC3 reads; reviewed changes for
OSC3-reading tunes. **No exactness claim for combined waveforms.**

### Slice 3b — sync + ring *(SHIPPED)*

Coupled clocking of all three oscillators (cyclic routing V1←V3, V2←V1, V3←V2);
MSB rising-edge detection; correct simultaneous-source ordering/suppression
(accumulator reset); ring-mod triangle = triangle modulated by the source
oscillator's MSB.

*Exit gate:* synthetic sync chains over all three source/dest pairs;
simultaneous-source edge cases; ring-mod changes output only for applicable
triangle configs; **effects analysis retains register intent and *adds* measured
activity** (reset counts, edge activity) — it does not replace register-bit
detection.

### Slice 3c — noise + combined-waveform policy *(SHIPPED)*

23-bit LFSR (exact seed, bit-19 edge clock, output-bit mapping, test-bit
interaction, waveform-switch tests); combined waveforms **modelled per chip
revision or explicitly reported unsupported** for exact OSC3.

*Exit gate:* exact LFSR seed/edge/output vectors; test-bit/noise recovery
vectors; an explicit supported-or-unsupported result for **every** combined-
waveform class; oracle agreement on named OSC3-noise fixtures. Retires the
approximate noise path in `emu/bus.rs`.

## Validation pyramid

1. **Hand-derived unit tests** — all 16 rate periods; writes before/at/after a
   match; `0xFE→0xFF`; every exp threshold; sustain change during decay/sustain
   (above and below level); gate-off during attack; gate-on during release;
   hold-zero + retrigger; multiple writes at one timestamp; the `0x7FFF/0x8000`
   wrap; power-on-state reads.
2. **Slow per-cycle reference** vs the optimised `clock_to` jump form on large
   deltas — this licenses the event-step optimisation.
3. **Property tests** — random write/clock-delta sequences, every internal field
   compared reference-vs-optimised.
4. **sidera/reSID oracle** via a **cycle-event stream** (not frame-level ENV3
   diffs, which miss transients / converging counters). Prefer a **committed
   dev-dependency or golden vectors with recorded generator version/commit** over
   an uncommitted scratchpad.
5. **In-situ captured `$D41B`/`$D41C` reads** as driver checkpoints.
6. **Named CI integration fixtures.**

## Census metrics

Calls with `gate == 0 && env > 0`; `gate == 1 && env == 0`; intra-call attack
entries; gate blips that never leave zero; release-tail duration p50/p95/max;
voices that never reach zero; `voice_signal_active && !voice_routed` calls;
`voice_signal_active` V3 calls while `voice3_off` set **and not filter-routed**;
`$D41C` reads currently receiving raw RAM; heuristic-tail-end vs envelope-zero
deltas.

## Deletions ledger

| Retired | Slice |
|---|---|
| PLAN.md linear-`$D41C` approximation (superseded before written) | 1 |
| `gate_retrigger` rising-edge heuristic (`analysis/mod.rs`) | 2b |
| slice-7 `release_tail_end` frequency-motion *existence* test (representation kept) | 2a |
| `advance_v3_accum` + simple-waveform `osc3_output` (`emu/bus.rs`) | **3a only** (after parity) |
| approximate noise branch of `osc3_output` (`emu/bus.rs`) | **3c only** |

The OSC3 model is **kept intact through Slice 1** — removing it there would
regress `$D41B` tunes.

## Relationship to `PLAN.md`

- **Resolve the strategy conflict:** PLAN.md proposes a **linear-decay** `$D41C`
  approximation and defers full ADSR "regression risk." This plan supersedes it
  (build the canonical machine directly); **update PLAN.md** on adoption.
- **Declared dependencies:** `PlaybackTiming` + CIA call-rate (PLAN.md [HIGH];
  Slice 0 excludes CIA rather than blocking on it); `$D41C` read capture
  (PLAN.md [MED]); note-endpoint semantics.
- **Do not conflate** `NoteEvent.end_frame`'s two-meaning bug (downstream note
  semantics) with SID-clock scheduling (an emulation concern) — coordinate,
  keep distinct.

## Implementation order

1. Slice 0 — vblank scheduling + loud CIA exclusion.
2. Slice 1 — live ENV3 + replay snapshots + ordered envelope events.
3. Slice 2a — release-tail migration.
4. Slice 2b — retrigger + onset migration.
5. Slices 3a → 3b → 3c, each with its own exit gate.

## Risks / open questions

- **Timeline fidelity caps exactness** — instruction-start stamping, assumed
  idle, CIA (excluded), init→play policy, NTSC variant. Each documented and
  bounded. Investigate whether `mos6502` 0.9 can expose per-bus-cycle callbacks
  before promising exact CPU-observed register values (likely needs a fork —
  then the instruction-start bound stands).
- **Live-read cascade** — a changed `$D41C` read can legitimately alter the whole
  downstream trace of an ENV3-reading tune; the Slice 1 cohort gates account for
  this rather than assuming confinement.
- **`analyze(trace, timing)` blast radius** — CLI/tests/native extractors; done
  in Slice 0 while nothing depends on the new fields.
- **Scheduling drift** — mitigated by the rational/fixed-point call-boundary
  accumulator; the long-run bound is a Slice 0 exit criterion.
- **6581 vs 8580** — envelope and accumulator are model-agnostic; only the
  out-of-scope analogue stages differ.
