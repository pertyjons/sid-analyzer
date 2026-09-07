# Analysis completeness — audit & plan

> **Status: superseded.** The audit and provenance rationale remain useful;
> current work is consolidated in [`../PLAN.md`](../PLAN.md).
> Present-tense inventory claims describe the 2026-07-07 planning snapshot.

*Goal shift (Per, 2026-07-07): stop chasing individual export bugs; make the
**analyzer** as complete and general as possible. Are we capturing all the
information a SID tune contains? This doc is (1) an inventory of the
information layers and what we drop today, (2) two measurable completeness
censuses, (3) an audit-first work plan. Downstream consumers (synth export,
MIDI, future ML/transcription) then build on richer data instead of
compensating for missing data.*

Companion docs: [`export.md`](../../docs/export.md) (the main consumer),
[`forward-model-gate.md`](forward-model-gate.md) (decode-vs-trace gate — the
pattern the register census generalizes),
[`../PLAN.md`](../PLAN.md) (render-side verifier; note it becomes a *consumer* of
this work — a complete IR hands it note map and loudness for free).

## Principle: measure completeness, don't feel it

"Do we read everything?" gets two numbers, both census-shaped (the pattern
that already works: `uncovered_gated_frames`, `ResidualCensus`):

1. **Byte coverage** — classify every byte of the loaded image per tune:
   `executed code | table decoded by a native extractor | read during play,
   unexplained | never touched`. The rankable failure bucket is
   *read-but-unexplained*: information the driver uses that we do not
   understand. (The bus/taint layer already observes reads; this is
   bookkeeping, not new emulation.)
2. **Register-write explanation** — the forward-model gate does this for
   pitch. Generalize the question to every write: is this write explained by
   our model (note on/off, effect span, authored program, digi, filter
   contour, hard-restart idiom…)? Unexplained writes = phenomena we don't
   model. Per-register-class buckets, per tune, corpus-rankable.

Both run in `sid-corpus-scan` / a `sid-re` subcommand over HVSC, producing the
same kind of ranked next-work queue the note-fidelity census produces today.

## Layer inventory — what we capture vs. drop

### L1. File / static

| Have | Drop today |
|------|------------|
| PSID v2–v4 header, speed bitmask, subtune count | **STIL.txt** (HVSC composer/cover annotations — sits next to `Songlengths.md5` we already parse) |
| Driver ID (`sid-playerid`, SIDId port) | Byte-coverage classification of the image (metric #1) |
| Native table decode for 7 families (Hubbard, Galway, Crowther, Gremlin, Whittaker, GoatTracker V1 and V2 — the last two covering ~8,900 HVSC tunes) | Every other family; **authored instrument tables for GoatTracker**, which the format documents and which would give timbre and program, not just notes |

### L2. Execution / trace

| Have | Drop or assume today |
|------|----------------------|
| All `$D400–$D41C` writes, cycle-offset granular (`SubFrameOffset`) | **Nothing consumes the cycle offsets** outside `$D418` digi counting — write *order* semantics (gate-off→wave→gate-on hard-restart idiom, test-bit tricks) are invisible to analysis |
| `$D41B/$D41C` read **counts** per frame (M7 uses the count as a feature) | Read **semantics**: what value the bus returns, what the driver does with it (OSC3 noise as RNG, ENV3 modulation feedback) |
| One `play` per vblank frame | **CIA multi-speed**: speed-bitmask tunes still run at 1 play/frame (documented shortcut in `emu/mod.rs::run`). 2×/4× players get wrong wall-clock for *everything* downstream. Unmeasured how many tunes this corrupts |
| `$D418` digi *detection* (`Sample` effect spans) | Digi *reconstruction*: the PCM stream is real musical content, planned Tier 1 in export.md, never built. Same for pulse-width/test-bit digi variants |

### L3. Chip response — the largest gap

We record the machine's *inputs* and never model its *outputs*:

- **Envelope value per voice per frame.** We know ADSR registers and gate
  edges; we never compute the resulting 0–255 amplitude. Today's velocity is
  the static `velocity_for_envelope(adsr)` heuristic (`analysis/note.rs:130`)
  — same velocity for every note of a patch, no accents, no measured attack
  contour. The AWM "strength lags" session made it concrete: the export
  writes velocity 100 on *every* note because dynamics data does not exist in
  the IR. The envelope generator is a fully specified state machine (rate
  counter, exponential segment table, ADSR-delay bug that hard restart
  exploits) — emulate it per frame in `analysis::voice` and per-note
  loudness, accents, and contour shape become first-class data for **all**
  drivers retroactively.
- **Oscillator output**: combined waveforms, ring-mod and sync we record as
  flags, never as produced signal.
- **Filter response**: cutoff/res/routing raw; the 6581 curve is approximated
  only at export time (`SID_FILTER_MODEL="acid"`).

The end state for this layer: a cycle-approximate **SID model inside the
analyzer** (own Rust implementation, envelope first — it is the cheap and
most valuable third; oscillator/filter models can arrive later or stay
export-side), exposing internal state per frame: envelope counters, osc
accumulators, filter state. "What it actually sounds like" becomes analyzer
data instead of an export-time approximation.

### L4. Structure / musical form

All currently dropped: loop point (when does the tune's state cycle —
frame-state hashing gives it nearly free), section/form structure, the
driver's own row grid (we export a fixed 20 ms grid; gate-edge quantization
plus native speed tables recover the real one, incl. funktempo), subtune
sharing (which subtunes reuse patterns/instruments).

### L5. Generalization machinery

`sid-re taint` (file byte → register write provenance) and `sid-re probe`
(mutate → replay → diff → role) exist as *manual RE aids*. The general form:
an automatic **semantic map** for any tune regardless of driver — which bytes
are read with which access pattern (stride, indexing depth, indirection) and
which register class they influence. That map is (a) the classifier feeding
byte-coverage census bucket "explained", (b) the scaffold that turns
per-family hand-written extractors into a guided, semi-automatic process —
the actual path to "general extractor", vs. hand-RE per family forever.

## IR consequence: provenance levels

Everything above lands in one rule for the IR (`NoteCharacteristics` /
`PatchVoiceProfile` / authored effects today): each phenomenon carries its
provenance — **Authored** (native tables) > **Register** (trace-derived) >
**Modeled** (chip-response layer). Consumers take the richest level present.
This is already implicitly true (authored vibrato backfills heuristic misses);
the audit makes it explicit per field so gaps are visible instead of silent.

## Plan

Audit first — measure where information leaks before building anything.

### Phase A — audit (report, no behavior changes)

- **A1 multi-speed exposure.** Count HVSC tunes with CIA speed bits (header
  scan, trivial) and measure what 1-play/frame does to a known 2× tune
  (trace at 1× vs. 2× manually — is our note/effect data garbage or merely
  time-stretched?). *Exit: a number ("N% of HVSC affected") and a verdict
  (fix now / defer with justification).*
- **A2 write-order semantics.** Inventory real drivers' within-frame write
  sequences (we already store order + cycle offsets): how common are
  hard-restart idioms, same-register double writes, test-bit pulses per
  frame? *Exit: ranked idiom list with frequencies; decision which get
  modeled fields in `VoiceState`.*
- **A3 OSC3/ENV3 read semantics.** What does the bus return on `$D41B/$D41C`
  today, and what do reading drivers do with the value (taint the read)?
  *Exit: list of read-consuming tunes + whether a modeled OSC3 (needs the L3
  envelope/osc model) changes their output.*
- **A4 digi census.** How many tunes trip `Sample` spans; what would PCM
  reconstruction take per variant ($D418, pulse, test-bit). *Exit: sized
  backlog item, not code.*
- **A5 byte-coverage prototype.** Wire read-tracking (bus already sees every
  fetch; taint already attributes) into the four buckets on our assets.
  *Exit: coverage table for the ~12 asset tunes; the Hubbard ones should show
  high "explained" (native decode) — if not, the census or the extractors are
  lying, either finding is valuable.*

### Phase B — the two censuses as permanent instruments

- **B1** Byte-coverage census in `sid-corpus-scan` (SQLite column + stderr
  block like `note_fidelity`). *Exit: HVSC-ranked "most unexplained bytes"
  queue; top-10 eyeballed and classified (new driver family? digi? code?).*
- **B2** Register-explanation census: extend `export/forward.rs`'s
  span-coverage idea from pitch to wave/pw/filter/ADSR register classes,
  report-only. *Exit: per-tune unexplained-write buckets; Nemesis/Monty/AWM
  numbers quoted as baseline in this doc.*

### Phase C — envelope model (first chip-response slice)

`analysis::envelope`: per-frame envelope value per voice from gate edges +
ADSR registers (rate periods, exponential segments, ADSR-delay bug —
constants from the SID datasheet/reSID literature, unit-tested against
published measurements). Feed into: per-note velocity (replaces the static
heuristic), accent detection, measured attack/decay contour in
`NoteCharacteristics`, hard-restart recognition (A2's idioms become
detectable as *intent*). Export consumes velocity + per-note level.
*Exit: AWM V3 bass notes get differentiated velocities matching the reSID
per-note loudness ranking (validated once against sidplayfp stems — the
render-abtest slice-1 features double as the validator); budget tests green;
census B2 "unexplained ADSR writes" drops.*

### Phase D — semantic map (generalization slice)

Promote taint+probe into `sid-re semanticmap <file>`: access-pattern
classification per byte range, register-class attribution, dumped as JSON.
Feeds B1's "explained" bucket for non-native tunes and becomes the standard
first tool on any new driver family (Laxity resume, Gremlin sub-gen #2 are
the live test cases). *Exit: on a Hubbard tune, the map re-discovers
orderlists/patterns/instrument table locations that the native extractor
already knows (blind validation); on one un-RE'd family it produces a table
map a human confirms plausible.*

### Phase E — census-driven backlog (ordered by B1/B2 data, not gut)

Expected front-runners, to be confirmed by the censuses: GoatTracker native
decoder (largest authored-data ROI) · multi-speed support if A1 says so ·
digi PCM reconstruction · loop-point/form detection (frame-state hash) ·
STIL parser (`songlengths`-style, trivial) · OSC3-consuming tunes if A3
found any that matter.

## Non-goals / boundaries

- Not RSID, not a full C64 environment (unchanged).
- Not bit-exact audio emulation in the analyzer: L3 models chip *state*
  (envelope counters, osc phase), not filtered analog output; the export and
  the abtest harness own audible-signal concerns.
- The render-abtest plan is unchanged but re-sequenced: its slice 1 doubles
  as Phase C's validator, the rest follows once the data side is trustworthy.

## Risks

- **Envelope constants**: getting rate/exponential tables right matters;
  mitigate with unit tests against published reSID/datasheet measurements,
  and the sidplayfp-stem validation in Phase C's exit.
- **Byte-coverage noise**: players read padding/garbage speculatively; the
  "unexplained" bucket needs a de-minimis threshold before it ranks fairly.
- **Scope creep in D**: the semantic map is a scaffold, not an auto-extractor;
  its exit gate is deliberately "rediscovers known + one plausible unknown",
  nothing more.
- **A1 could invalidate corpus history**: if multi-speed corruption is broad,
  earlier HVSC-wide numbers (census, pass rates) carry a caveat — that is a
  finding, not a reason to skip the audit.
