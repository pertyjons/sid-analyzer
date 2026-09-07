# Export fidelity investigation — from special cases to a verified plan

> **Status: completed investigation.** Recommendations already delivered or
> still open are tracked only in [`PLAN.md`](../plans/PLAN.md).

*2026-07-05. A structural review of why the SID → Pertylizer export accumulates
special cases and what to build instead. Companion to [`export.md`](export.md)
(status + mapping) and [`extraction-methods.md`](extraction-methods.md) (RE
substrate). Grounded in a full survey of `export/synth.rs`, the analysis layer,
and the native extractors; the heuristic/ambiguity inventories are appended.*

**Goal restated:** melodies, notes, and timbres must be right; the sound does
not have to be perfect. The finding of this review is that timbre is
effectively solved (native `sid` module at the measurement floor) and that
everything that still "sounds weird" is a **note-decomposition artifact** — so
the architectural work belongs in how the export decides notes, glides, and
effects, not in the sound engine.

---

## 1. Diagnosis — why the special cases proliferate

The survey found ~30 magic thresholds in `synth.rs` and ~18 guess points in
`analysis/`. They are not independent problems; they share four structural
roots.

### Root 1: The generic path solves an ill-posed inverse problem

The driver *knows* note boundaries, instrument identity, and which effect is
intentional. The register trace flattens all of it, and every heuristic
(`pitch_plateaus`, the chirp window, drum-drop classification, `slide_through`,
`adopt_patch`) guesses the intent back. This cannot be done reliably even in
principle: a fast two-tone arpeggio and a real trill produce *identical*
register streams — only the driver's own data distinguishes them.

The evidence is already in: the native-vs-generic ear A/B on Commando was a
step change, and essentially every recent ear-test bug (phantom arp, onset
glide swallow, drum F#3, stab tail, slide-through) was a decomposition
artifact, not a timbre one.

### Root 2: Even the native path funnels through the trace heuristics

`NativeSong` supplies exact notes and structure, but expression still goes
through `push_expressive_notes` and its priority cascade (vibrato → drum-drop →
chirp → fall → legato split). Hubbard tunes with perfect note data still get
their fall/drum/glide decisions from thresholds — the stab-tail and
slide-through bugs both hit *native* exports.

### Root 3: Classify-and-patch instead of propose-and-verify

The failure pattern: a heuristic classifies ("this is a drum drop"), and when
it misclassifies, a new guard is stacked on top (`near_gate`,
`snap_drum_bodies_to_floor`, the `expand_arpeggio` trace clamp). Each guard is
a new threshold that can fail on the next tune. `MAX_ONSET_GLIDE_SEMITONES = 12`
currently carries **three unrelated decisions** (melodic glide clamp, drum-drop
classification, drum-body outlier snap); one tune violating "downward > octave
= percussion" breaks all three at once.

Crucially, the codebase has already invented the right pattern in three places
without making it architecture:

| Site | Mechanism |
|---|---|
| Native gate (`native/mod.rs`) | `onset_agreement ≥ 0.75` vs the trace, else fallback |
| `plan_authored_pwm` (`synth.rs`) | authored PWM must be **trace-confirmed** per note, else baked lane |
| `arp_plan_clean` (`synth.rs`) | arp processor only if every note covers the cycle **in the trace**, else bake |

All three are the same idea: *propose a structured representation, verify it
against the trace, degrade safely.* It is just not applied to the rest —
chirp, fall, drum-drop, legato split, and vibrato still guess without checking
the answer key.

### Root 4: Role-blind clustering + duplicated derivation

- The patch key `(adsr, dominant_waveform, role_tags)` is timbre-only, so a
  drum body clusters into the lead patch, a flat tone folds into an arp patch,
  and three downstream mechanisms exist purely to repair that (`adopt_patch`,
  the drum-drop split, the arp clamp).
- Analysis *classifies* effects; the export *re-measures* their magnitudes from
  the same `states`. There are two clustering layers (patch + `MergeShape`),
  and two build paths (`build_instruments` / `build_plan_automation`) must take
  the same branch — a fragility the code itself flags.

## 2. Timbre is (nearly) done — the notes are the frontier

Given the goal "melodies, notes, timbres must sit":

- **Timbre:** the native `sid` module measures at the reSID fixture floor for
  saw / pulse / tri / noise / hard-sync / ring and all 8580 combos. The
  remaining gaps (6581 tri+saw / pulse+saw combine, ring ~1.4 kHz bright,
  filter low-cutoff leakiness) are known, measured, and live in **Pertylizer**,
  not the exporter. No redesign needed — just the three listed polish passes.
- **Notes:** every recent ear-test bug was a note-decomposition artifact. This
  is where the architecture work goes.

## 3. The recommendation — one mechanism replaces the zoo

### Core proposal: a forward-model gate in the exporter

Make export note plans *testable*. Every plan primitive is deterministic, so
for each planned note the exporter can cheaply **predict what it will render
per frame** — pitch in cents (base + glide + vibrato + arp processor + fall)
and control byte (waveform seq) — and compare the prediction against the trace
(`states`, already passed into `write_synth`):

```
residual = per-frame |predicted_cents − trace_cents|
           over gated, non-noise frames
```

- **residual < tolerance** (e.g. 30–50 cents on ≥ 90 % of frames) → keep the
  structured representation (glide, vibrato expression, arp processor, fall).
- **otherwise** → degrade one step on a fixed ladder:

```
authored program → fitted program → legato split → baked per-frame replay
```

The baked end of the ladder is always correct, merely less structured.

Consequences:

1. **Every current heuristic becomes a proposal generator, not a judge.** The
   chirp window, the `slide_through` half-note rule, the drum-drop octave,
   `MIN_LEGATO_FRAMES` — they may guess wrong freely, because one shared
   verifier catches it. The failure mode changes from *"wrong notes"* to
   *"right notes, less elegant file"* — exactly the asymmetry the project goal
   demands.
2. **`plan_authored_pwm`, `arp_plan_clean`, the chirp settle check, and
   `near_gate` are subsumed** by one mechanism and can be deleted as separate
   special cases.
3. **The "two build paths must agree" fragility disappears:** the decision is
   made once, on the plan, before both the instrument and automation builds.
4. The per-note residual becomes a **census signal** ("N notes degraded, worst
   residual X cents"). The stab-tail class of bug (decode drops content the
   trace shows) becomes measurable instead of ear-discovered.

This is a refactor, not a rewrite: `push_expressive_notes` + the note-plan
build reorganized around a `verify_note_plan(plan, states) -> Residual`
function, with the already-specced `SidVoiceProgram`
([`export.md`](export.md), fidelity ladder) as the carrier. The slice plan
lives in [`done/forward-model-gate.md`](../plans/done/forward-model-gate.md).

### Supporting leg 1: fix ambiguity at the source (analysis layer)

Cheap fixes that remove *classes* of weirdness before they reach the export
(current work, when still applicable, is tracked in `PLAN.md`):

1. **`effects.rs` is noise-polluted** — the `is_noise_only` filter exists in
   timbre but not in the effect detectors; a drum voice still registers vibrato
   reversals and arpeggio switches. Feeds directly into note plans; should be
   treated as HIGH.
2. **`NoteEvent.end_frame` has two conflicting meanings** (inclusive vs
   one-past-end) — timbre characterizes a frame the exports never play,
   contaminating patch keys. Pure correctness bug.
3. **Frozen modulator pitch:** `ring_source_hz` / `sync_source_hz` capture the
   first non-zero value; a sweeping modulator (common) is frozen to a point.
   Make it a contour, not a scalar.
4. **Filter summary from the note's first frame only** — a note whose filter
   opens mid-note is characterized by its closed state.
5. **Vibrato/arp/portamento detectors overlap freely** (three independent
   detectors; priority resolved downstream). Less dangerous once the forward
   gate exists, but a mutually exclusive classification per gate region is
   cleaner.

### Supporting leg 2: move the fidelity boundary — more authored, less inferred

The special-case count is inversely proportional to how much of the driver's
own data is read. Each RE step deletes heuristics for its whole family:

1. **GoatTracker decoder** — ~8,900 tunes for one decoder; the locate design is
   already written. By far the best coverage-per-hour, and the format is
   *documented* (a tracker, not hand-written code) — instrument tables with
   wavetable/pulsetable/filtertable come for free, i.e. authored timbre + program,
   not just notes.
2. **Hubbard `+5` vibrato byte** (high nibble likely LFO rate) — disassemble
   the vibrato block, flip to authored-first precedence. Already filed as an RE
   gap in `export.md`.
3. **Galway/Crowther/Gremlin authored effects** — today only Hubbard authors
   `InstrumentEffects`; Gremlin binds no instruments at all. The E1 pattern
   applies per extractor.
4. **LLM-in-the-loop extractors (proposal F, `extraction-methods.md`)** — the
   gate architecture is built for exactly this: a proposed decoder can never
   ship garbage, the PASS gate rejects it. This amortizes the per-driver RE
   cost, which is the only expensive step left.

### Supporting leg 3: measure instead of ear-driven whack-a-mole

All the parts exist: the reSID A/B method, the fidelity census sidecar, the
fixture matrix, and the external sidplayfp/Pertylizer render adapter. The
remaining scale step is **corpus-wide ranking**: a batch that,
for N tunes, renders export vs sidplayfp, computes windowed log-spectral
distance + onset agreement, and writes a ranked list. "Sounds weird" becomes a
sorted queue instead of a bar-21 discovery, and every deleted special case gets
regression cover. An in-process renderer remains a corpus-ranked follow-up in
`PLAN.md`, not a blocker for the external adapter.

## 4. Priority order

| # | Effort | Effect | Size |
|---|---|---|---|
| 1 | **Forward-model gate** (verify note plans vs trace, degradation ladder) | removes the *mechanism* that breeds special cases; "wrong notes" → "right notes, flatter file" | medium — refactor of existing code, E3-style slices |
| 2 | **Analysis hygiene:** noise filter in `effects.rs`, `end_frame` semantics, modulator contour, filter contour | removes error sources *upstream* of the gate | small, bounded |
| 3 | **Corpus A/B batch** | turns remaining weirdness into a ranked list | small–medium (scripting + glue) |
| 4 | **GoatTracker decoder** | ~8,900 tunes get authored notes + timbres; the heuristic path becomes the exception, not the rule | medium–large RE, but documented format |
| 5 | Hubbard `+5` RE, Galway/Crowther authored effects, Pertylizer polish (6581 combine, ring brightness, filter leakiness) | the last timbre gaps | several small |

**The strategy in one sentence:** stop making the guesses smarter — make them
*testable*. The trace is the answer key and is already in the exporter's hand;
the native gate proved the pattern on the extraction side; the forward-model
gate is the same idea on the synthesis side, and it turns the whole
special-case zoo into replaceable proposal generators behind a single verifier.

---

## Appendix A — heuristic inventory (`export/synth.rs`, survey 2026-07-05)

Line numbers are from the 2026-07-05 working tree and will drift; anchor on the
named constants/functions.

| Site | Purpose | Fragility |
|---|---|---|
| `ADOPTION_MAX_DISTANCE_FRAMES = 500` | patch-adoption distance backstop | arbitrary ~10 s window |
| `ADOPTION_ADSR_SHAPE_TOL = 3` / `RATE_TOL = 5` | ADSR tolerance for adoption | hand-split shape-vs-rate slack |
| `timbre_compatible` | waveform-family + filter + ADSR gate for orphans | bitwise `& 0x70` family test |
| `MIN_LEGATO_FRAMES = 4` | plateau anchor min dwell | below → slide frames fabricate notes; above → fast melodies merge |
| `MIN_PITCH_EFFECT_SEMITONES = 0.05` | audibility floor for wobble | tuning-jitter cutoff |
| `MAX_CHIRP_FRAMES = 8`, `MIN_CHIRP_SEMITONES = 1.0`, `CHIRP_SETTLE_SEMITONES = 0.5` | onset-chirp window | boundary vs portamento; dip-and-return edge cases |
| `MAX_ONSET_GLIDE_SEMITONES = 12` | melodic-vs-percussive split | **overloaded 3×**: glide clamp + drum-drop classify + body snap |
| `ARP_ATTACK_SKIP = 2` | skip attack frames in arp spans | fixed transient width |
| `FALL_PLATEAU_FRAMES = 8` | post-gate fall bottoms out | accumulator-near-DC assumption |
| `MIN_GATE_OFF_FRAMES = 2` | gate flicker vs real note-off | driver-specific flicker widths |
| `GATE_ON_LEAD_MAX = 3` | max start snap-forward | > 3 assumed legato/artifact |
| `slide_through` (span·2 ≥ note) | porta covering ≥ ½ note → one glide | 50 % rule; cross-voice clash band-aid |
| `snap_drum_bodies_to_floor` | relocate outlier drum body | pure band-aid over clustering error |
| `PWM_CONFIRM_MIN_STEPS = 2` + `plan_authored_pwm` | trace-confirm authored PWM | conditional-effect check; two build paths must agree |
| `PWM_BOUNCE_LO = 2048` / `SPAN = 1536` | Hubbard `$800..$E00` PWM reflect band | hard-coded to one driver idiom |
| `RESONANCE_MAX_NORM = 0.65` | cap resonance | model-mismatch cap (Pertylizer self-oscillates) |
| `MAX_VOLUME_LANE_POINTS = 512` | reject `$D418` digi hammer | swell-vs-PCM discriminator |
| `BPM_FOLD_MIN/MAX = 75/150`, `MIN_TEMPO_ONSETS = 16` | tempo derivation window | half/double-time guesses |
| `MIX_HEADROOM = 0.5` | per-voice gain | correlated-voice worst case |
| §A9 tube/EQ consts, `SID_FILTER_MODEL = "acid"` | master coloring / filter model | ear-tuned + fixture-derived |
| `DRUM_CLICK_LEVEL = 1.5`, `DRUM_CLICK_ADSR`, `DRUM_BODY_ADSR` | 2-source percussion tuning | ear-tuned on Monty bar 21 |
| `alternation_seq` (≥ 2 distinct masks) | waveform-switch idiom → native seq | exact held-frame timing through 16 steps; longer representative windows collapse to distinct masks |
| `arp_plan_clean` | processor only if trace covers cycle | the good pattern — to be generalized |
| `expand_arpeggio` trace clamp | clamp sub-notes to traced span | band-aid over role-blind clustering |
| `authored_vibrato` backfill | driver-table vibrato on short notes | `+5` byte not fully RE'd |

## Appendix B — ambiguity inventory (`analysis/`, survey 2026-07-05)

| Site | Guess | Failure mode |
|---|---|---|
| `note.rs` `hertz_to_midi` | nearest-integer MIDI + cents field | detuned tunings snap; midpoint flips |
| `note.rs` velocity | loudness = sustain (or decay if 0) | ignores `$D418` envelope and gate length |
| `note.rs` note-off on gate fall only | held-gate re-pitch = same note | held-gate players → one monster note (PLAN §3 HIGH) |
| `effects.rs` vibrato (≥ 3 reversals, ≤ 100 ¢) | wobble = vibrato | > 100 ¢ vibrato silently dropped; trill overlap |
| `effects.rs` arpeggio (2–4 distinct, ≥ 3 changes) | pitch cycling = arp | fast arp ≡ real trill in the trace; > 4-note arps rejected |
| `effects.rs` portamento (monotonic, ≤ 1 hold) | run = slide | stepped/tabled glides fragment and vanish |
| `characteristics.rs` ≤ 1-semitone loop filter | jitter, not arp | genuine 1-semitone trill arps discarded |
| `dominant_gated_waveform` | longest-gated byte = the timbre | genuine waveform sweeps collapse to one byte |
| `is_noise_only` pitch skip | noise frame carries no pitch | hard-restart noise vs real noise percussion not distinguished |
| TestBitUsage flag | test bit seen ⇒ trick | restart-vs-reset semantics unmodelled; dropped by bytecode path |
| `is_percussive` cascade | length + attack + signal bands | hard-restart click leads vs real drums; idiom-specific guards |
| `filter_summary` first frame | filter is static per note | mid-note filter opening lost |
| `neighbour_source_frequency` | first non-zero neighbour freq | sweeping modulator frozen to a scalar |
| Patch key `(adsr, waveform, roles)` | exact match = same instrument | merges distinct instruments; splits on mis-tagged roles |
| `detect_loop` (offset ≤ 4, 5 % tol) | short-period cycle = loop | arps with long setup or jitter read as `Raw` |
| `Voice3LfoSource` | any `$D41B` read overlap ⇒ LFO target | binary; no target voice, depth, or curve |

## Appendix C — information the analysis layer drops (present in the trace)

- Sub-frame write order / multiple writes per register per frame (except the
  gate-retrigger edge count and the raw `$D418` write count).
- `SubFrameOffset` is an instruction counter, not CPU cycles — digi timing
  precision lost.
- `$D41B`/`$D41C` read *values* (only a per-frame count survives) — the
  voice-3-as-LFO modulation curve is never captured.
- Test-bit semantics (decoded, flagged, never modelled).
- Time-varying ring/sync source pitch (frozen scalar).
- Per-voice filter routing over time within a note (reduced to a boolean +
  first-frame mode).
- The `$D418` 4-bit PCM stream itself (only overlap counts).
- Master-volume envelope as a velocity source.
