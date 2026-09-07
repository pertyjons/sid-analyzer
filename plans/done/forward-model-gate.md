# Forward-model gate — slice plan

*Implementation plan for the core recommendation of
[`export-fidelity-investigation.md`](../../docs/export-fidelity-investigation.md): make
export note plans testable against the trace, and degrade on a fixed ladder
instead of stacking classification guards. E3-style slices — each lands alone,
gate green, with a measurable exit.*

## The mechanism

For every note the exporter emits, **predict what Pertylizer will play per
frame** and compare against the trace (`states`, already in `write_synth`'s
hand). The emitted `Note` struct carries the full spec (pitch, `Glide {from,
time, interp}`, `NoteExpression.vibrato {depth, rate, delay, shape}`, `legato`,
duration) and the track carries the `Arpeggiator` processor — everything a
frame-domain forward model needs.

```
predicted_cents(frame) = base_pitch
                       + glide(frame)          Semitones offset → 0 over `time`
                       + vibrato(frame)        shape LFO × depth, after delay
                       + arp_offset(frame)     processor: phase restarts per onset
residual = |predicted_cents − trace_cents| over gated, non-noise frames
```

- **residual ≤ tolerance** → keep the structured representation.
- **otherwise** → degrade one step:

```
authored program → fitted expression → legato split → per-frame bake
```

The bake bottom (plateau/per-frame sub-notes clamped to the trace, the
`expand_arpeggio` idiom) passes by construction — the ladder cannot emit wrong
pitches, only flatter files.

**Design invariants:**

- The predictor models **Pertylizer's engine semantics**, not our intent —
  glide interp, vibrato shape/phase, Arpeggiator per-onset phase restart.
  Where semantics are uncertain, confirm once with an MCP render of a
  synthetic single-note project (the reSID-A/B method, minus reSID).
- One shared frame mask everywhere: gated frames only, `Waveform::is_noise_only`
  frames skipped, first `ARP_ATTACK_SKIP` onset frames skipped. Today each
  heuristic masks slightly differently; the gate unifies it.
- Verification happens **on the plan, once** — before both `build_instruments`
  and `build_plan_automation` — killing the "two build paths must take the same
  branch" fragility.
- Tolerances are two named consts with rationale, set in slice 1 from measured
  distributions (starting point: `RESIDUAL_TOL_CENTS = 50`,
  `RESIDUAL_OK_FRAME_FRACTION = 0.9`), not tuned per special case.

## Slices

### Slice 1 — predictor + report-only residual census

New module `crates/analyzer/src/export/forward.rs` (`pub(crate)`, keeps
`synth.rs` from growing):

- `predict_note(note, processors, ctx) -> PitchCurve` — per-frame cents for one
  emitted `Note` given its track's processors and the time base.
- `residual(curve, states, voice_index, mask) -> Residual { frames_checked,
  frames_over_tol, max_cents, mean_cents }`.
- The shared frame mask.

Wire into the census only: per-plan residual bands + a worst-notes list in the
sidecar JSON. **No behavior change.**

*Exit gate:* exports **byte-identical** on all assets; unit tests on synthetic
`FrameState` fixtures (existing test style) for each term — plain, glide,
vibrato (both shapes), arp processor incl. phase restart, legato ties; census
on Monty/Nemesis/Commando shows near-zero residual for known-good notes and
**lights up the known V1 Stab #5 stab-tail region** (the decode-drops-content
class becomes measurable — this validates the whole approach before anything
enforces).

### Slice 2 — the ladder for melodic pitch shapes *(SHIPPED)*

`push_melodic_event` wraps `push_expressive_notes`: the heuristic chain emits
its proposal, `forward::batch_passes` verifies every verifiable emitted note,
and a failing proposal is replaced by `bake_pitch_runs` — one legato-tied note
per run of equal nearest-MIDI pitch read off the trace (detune-compensated,
1-frame noise interleaves extend the run). Each longer note retains the 90%
frame tolerance internally, but a long correct run cannot hide a completely
wrong one-frame arpeggio step. Trace-faithful by construction.
Enforcement is off for percussion/drum-drop plans (slice 4) and behind
`--unstable-no-forward-gate` for A/B. Vibrato notes grant 2×depth slack (anti-phase
worst case), so the check verifies the carrier, never LFO phase.

*Shipped shape vs the original sketch:* a two-rung ladder (current proposal →
bake) rather than re-trying each heuristic branch as a separate proposal —
measurement showed nothing on the native path needed a middle rung (0
degradations, byte-identical exports), so the extra rungs wait for data that
wants them.

*Exit gate MET:* all 376 tests green (slide-through, chirp-swallow, structure
transparency, drum routing intact); native exports **byte-identical** (nothing
wrongly degraded); heuristic path degrades 1–8 events/tune with measured
improvement — Nemesis 233→279 ok notes (mean 221→201 ct), Knucklebusters
298→414 (mean 392→303 ct); remaining fails are arp-plan notes (slice 3 scope,
confirmed via the census worst-lists). reSID render A/B not run (Pertylizer
MCP absent this session; native output unchanged, heuristic changes are
per-frame chip pitches by construction) — ear-check the next time the MCP is
up.

### Slice 3 — arps under the verifier *(SHIPPED)*

`arp_processor_for` acceptance is measured: every event's predicted rendering
(held base note + processor offsets restarting at the onset, `arp_event_spec`)
must pass the forward model — `arp_plan_clean` deleted. Dirty-arp events go
through `push_arp_event`: `expand_arpeggio` (clamp deleted, now the middle
rung) → verify → `bake_pitch_runs`. Two refinements shipped with it:

- **Percussion exemption dropped** (slice-2 revision): verification is
  pitch-only, so `is_sid_percussion` events are enforced too — a faithful tom
  fall passes (its glide comes from the same trace) while a mis-decomposed
  stab degrades. Only `drum_drop` plans stay exempt (slice 4).
- **Glide shape envelope**: Pertylizer's `GlideState` renders
  `f(t) = from·(to/from)^t` (cents-linear — confirmed in the engine source),
  the chip slides Hz-linear; both are the same musical gesture, so the
  per-frame tolerance inside a glide window is widened by the distance
  between the two curves. A genuine chip fall passes; a glide emitted over a
  *held* note (Monty's V2 short figure: B3+glide over a held F#5) exceeds the
  envelope and degrades.

*Exit gate MET* (378 tests green): Galway assets **byte-identical** (short
stabs still bake, now for the measured reason); Commando 15→18 processors,
+12 ok notes, no regressions; **Monty native 164 fail → 0** (mean 126→9.4 ct)
— the known V2 short-figure class degrades to the chip's pitches, and the
same plan's *genuine* falls (past frame 3000) keep their verified glides;
Ikari percussion 1750→1940 ok. The `synth_export` glide fixture moved to
6000 frames — its old 3000-frame assertion was satisfied only by the
mis-decomposed stabs. *Owed:* the Commando proc-vs-reSID ≈9.2 dB re-run and
an ear pass on the degraded percussion falls, next time the Pertylizer MCP
is up (with `--unstable-no-forward-gate` as the A/B lever).

### Slice 4 — percussion decisions under the verifier *(SHIPPED)*

Drum-drop plans are enforced like everything else (`push_melodic_event`
loses its `enforce` flag; `--unstable-no-forward-gate` is the only exemption);
`snap_drum_bodies_to_floor` + `drum_floor_pitch` deleted — a body is judged
by the trace, not by the plan's modal floor. `MAX_ONSET_GLIDE_SEMITONES`
survives only inside the proposal generators.

What the measurement forced: deleting the snap resurrected 8 stray F#3s —
but the trace showed they were **not** wrongly-pitched bodies. They were
1-frame notes sliced onto a drum row's *release* frames (the driver parks
`$0CA8` in the freq register while the gate is off), contributing zero
verifiable frames and therefore receive no batch verdict. The
fix is the emission-side twin of the census coverage mask, per note:
**`drop_ungated_notes`** — a proposal note whose whole span the chip never
gates is not a note (no attack to reproduce; the retuned release tail is
covered by the previous note's own release). Counted as `events_silent` in
the census.

*Exit gate MET* (378 tests green): Monty drop-pattern stray 53–55 = **0**,
now by measurement instead of a modal snap (40 silent-dropped events);
percussion buckets all-ok on every asset (Monty 796/796, Ikari 2000/2000,
Knucklebusters 724/724, Commando 38/38); Ocean_Loader_1's only change is 7
silent authored rows (Galway parks registers the same way); Neverending
Story byte-identical. The exit-gate's original "Commando has no drum drops"
premise was already obsolete (the `end_frame` fix gave Commando its authored
zap drums). *Owed:* the Monty bar-21 reSID ear A/B (MCP down), stacked with
the slice-2/3 ear pass.

### Slice 5 — authored-effects precedence by residual *(SHIPPED)*

The forward model gained a **PW dimension**, phase-blind like the vibrato
treatment (the script restarts its staircase at note-on while Commando's
driver free-runs the sweep across notes — phase is unknowable and benign):
`forward::pw_band_residual` (the traced register must live in the script's
`$800..$E00` bounce band) + `forward::pw_step_rate` (measured per-frame
movement within `PW_RATE_RATIO` of the program's `step/period`). They
replace `plan_authored_pwm`'s min-range confirm (`PWM_CONFIRM_MIN_STEPS`
deleted). **Vibrato precedence** is decided by residual: when both a
measured span and an authored table value exist, `vibrato_mean_residual`
scores each against the trace and the lower mean wins (ties to measured) —
when the Hubbard `+5` RE lands, authored wins automatically wherever it fits.

What the measurement found — the gate caught two real E3 mis-renders the
old confirm could not see:

- **Auf Wiedersehen Monty (0 scripts, was 6):** its driver variant sweeps
  `$400..$E80` at ~22 units/frame, while the decoded `+6` gave step 32 /
  period 17 (~1.9/frame) folded into the Commando band — the emitted
  scripts were rendering the wrong duty band at the wrong rate. **RE
  follow-up filed:** the `+6` byte, like `+5`, decodes differently in this
  relocation.
- **Commando (0 scripts, was 1):** its script plan contains wrap-around
  per-note pw-offset events (`$3A0 → $020 → $F40`) that leave the band —
  one static program cannot serve the plan.
- **Monty on the Run (2 scripts):** the tune whose V1 `$840..$E60`
  +224/frame sweep the band was RE'd from keeps its programs — the
  machinery accepts what actually fits.

All rejected plans fall back to the exact baked `pw_reg` lane (the same
samples, more points). *Exit gate:* the E3 dB numbers belong to Monty on the
Run, which keeps its programs; AWM/Commando rejections are measured
fidelity improvements, re-render A/B stacked on the owed MCP ear pass. 380
tests green.

### Slice 6 — corpus rollout + residual budget *(SHIPPED)*

Two pieces:

- **Per-asset budget test** (`tests/synth_fidelity_budget.rs`, plain
  `cargo test`): pins the census ceilings the export achieves today —
  Monty native `fail 0 / degraded 88 / silent 8 / uncovered 1057`, Commando
  native `0 / 8 / 0 / 95`, Nemesis heuristic `0 / 35 / 0 / 0` (@3000
  frames). Lowering a budget is progress; raising one demands exit-gate-level
  justification.
- **Corpus census tool** (`measure_note_fidelity_corpus`, `#[ignore]`,
  `SID_HVSC_ROOT` + `SID_LIMIT`/`SID_FRAMES`): the heuristic pipeline over an
  HVSC tree with a per-tune timeout, printing the distribution and two ranked
  worst lists (melodic fails / uncovered gated frames).

**Measured distribution (HVSC sample, 771 PSID tunes × 1500 frames,
heuristic path, 2026-07-05):**

- **250 479 ok notes · 691 fail = 0.28 %** melodic-fail rate; 49 368 events
  degraded by the ladder (the enforcement working at scale), 0
  silent-dropped (silent rows are a native-decode phenomenon — consistent).
- **562/771 tunes (73 %) fully clean** (0 fails, 0 uncovered frames).
- 34 064 uncovered gated frames concentrate in a short tail.

The ranked lists are the next-bug queue, replacing ear-discovery:

- *Worst fails:* topped by `Hallows_Eve_2SID` (71 fails, 179 ct) — a 2SID
  tune, i.e. **the then-untrapped multi-SID write gap** surfacing as measured
  note damage; the rest are small-fail demo
  tunes worth a batch triage.
- *Worst uncovered:* `Muso_64` (4464 frames), `Ghostbusters_Theme` (2168) …
  — heuristic-path tunes should have near-complete coverage by
  construction, so these are a distinct dropped-content class (digi/gate
  idioms the emission drops) to investigate.

*Exit gate MET:* distribution documented here; budget test green in the
default suite (381 tests); the ranked lists identified the capture-layer gap
that was subsequently closed.

### Slice 7 — release-tail pitch motion is content *(SHIPPED)*

Found by the first MCP ear-pass A/B (2026-07-05, reSID vs gate on/off, Auf
Wiedersehen Monty): full-mix windows were gate-neutral, but on the V2-solo
stab window (39–42 s) **gate OFF 55.8 dB beat gate ON 66.1 dB** — reSID
plays *descending* figures through the **release phase** (gate off, envelope
ringing, driver still stepping the frequency register), which the old
fall-glide proposals approximated and the gated-only bakes replaced with a
held F#5. The model's mask treated all gate-off frames as non-content.

The fix, in both the model and the emission:

- **`forward::release_tail_end`** — the *moving release tail*: from a
  release run's first gate-off frame, follow the frequency register from its
  last pre-release value; a tonal, non-silent frame that changes it is a
  move, and the tail ends after the last move (stasis), after
  `TAIL_STASIS_FRAMES` (8) frames without one, at silence, or at gate-on.
  Fewer than `TAIL_MIN_MOVES` (2) moves is no tail at all — the
  discriminator against the parking class slice 4 kills (parking writes one
  value and holds it; a retuned tail steps at least twice).
- **Verification mask** (`note_residual`): post-gate frames inside the tail
  verify like gated ones; beyond it they stay masked. Parking still cannot
  fail a note.
- **`bake_pitch_runs`** extends through the tail, so a degraded event
  renders the release descent as per-frame pitch runs — for a *stepped*
  figure that is the faithful rendering (the chip's own register steps).
- **`drop_ungated_notes`** keeps a note inside a moving tail (bake runs are
  content); parking rows still die.

Measured (native path):

- **The stab class renders its descent**: AWM V2 window f1961 was `60 held
  (3 f)` → now `60 (3 f, legato) → 54 (7 f)`; stab 78 → 76 tails; drum
  drops carry the full `57→52→49→34` sweep. Monty stray-F#3 parking rows
  stay dead (silent-dropped 40, unchanged; drop-pattern test green).
- **Commando @3000 f**: degraded 8 → 18 — ten drum events whose zap sweep
  continues through the release now bake the sweep's real pitches; the
  percussion bucket went 12 → 185 verified, all ok. 0 melodic fails,
  mean 3.8 ct. Budget re-pinned with this justification.
- **AWM @6000 f**: notes ok 538 → 683 (tails now measured), percussion
  796 → 1069 all-ok, degraded/silent unchanged (332/40). Monty on the Run:
  ok 1163 → 1523, percussion 6 → 124 all-ok, 0 fails everywhere.

*Exit gate: gate ON ≥ OFF within measurement noise, and strictly more
trace-faithful.* The A/B on this window proved **build-sensitive** — the
history matters more than any single number:

- **2026-07-05 (old engine)**: gate ON 73.3 dB beat OFF 80.1 and
  pre-slice-7 ON 87.3 (voiced-frame log-spectral RMS, envelope-aligned).
- **2026-07-06 morning (new engine, Pertylizer @809ac925)**: absolute
  distances halved and the ordering *flipped* — OFF 33.2 beat ON 36.7.
- **2026-07-06 night (after the legato-flag fix, f887883)**: the flip was
  mostly *our* bug — the legato flag sat on the predecessor note, so the
  baked tail continuations never tied. Re-measured
  (`compare_spectra time_resolved`, 3 s window, reSID reference
  `sidplayfp -b39 -t3 -u1 -u3` vs V2-track-muted export renders): LSD
  OFF 33.45 vs ON 33.91, mel-L2 **ON 202.6 vs OFF 204.2** — the sign flips
  between metrics and with alignment choice, i.e. a statistical tie.

Why the tie resolves in ON's favor anyway: per-frame diffing shows ON's
rendered pitches are trace-exact (the F#5 stab renders 78 → 76 legato =
the register's 740 → 668 Hz), while OFF plays a fabricated continuous
glide (700 → 325 Hz) the register never wrote — and at the drum-drop
tails (41.9 s) OFF is fully silent where reSID plays content (per-frame
delta −126 dB in ON's favor). The residual scalar difference is at this
window's method-noise floor: the export runs 20.0 ms rows against PAL's
50.1245 Hz vblank (≈98 ms accumulated drift at 39 s), and the material is
240 ms-periodic, so envelope alignment quantized to 20 ms hops dominates
sub-dB differences. Method notes: a single 3 s aggregate `compare_spectra`
window saturates on this material (`target_voiced: false` — sparse stabs
read as unvoiced and the voicing penalty pegs the distance at 110.3 for
every candidate); short 500 ms windows flip ordering with alignment
jitter — use `time_resolved: true` over the full window.

## Deletions ledger

Code this plan retires (kept until its slice's gate is green):

| Retired | Slice |
|---|---|
| `slide_through` ½-note rule as judge | 2 |
| chirp-settle / `near_gate` as judges | 2 |
| `arp_plan_clean` (bespoke) | 3 ✓ |
| `expand_arpeggio` per-frame trace clamp (+ `arp_trace_span`, the clamp-divergence census tool) | 3 ✓ |
| `snap_drum_bodies_to_floor` + `drum_floor_pitch` | 4 ✓ |
| `plan_authored_pwm` min-range confirm (`PWM_CONFIRM_MIN_STEPS`) | 5 ✓ |
| measured-over-authored vibrato precedence as a fixed rule | 5 ✓ |

### Slice 7 (proposed) — release-tail pitch motion is content

Found by the first MCP ear-pass A/B (2026-07-05, reSID vs gate on/off,
`--unstable-no-forward-gate` lever, Auf Wiedersehen Monty):

- Full-mix windows: gate-neutral (opening 8.21 dB both; drums window
  67.05027 vs 67.05028 — the changed notes are below full-mix resolution).
- **V2-solo stab window (39–42 s): gate OFF 55.8 dB beats gate ON 66.1 dB.**
  Pitch tracks show why: reSID plays *descending* figures (735→668→604 Hz)
  — the driver keeps stepping the frequency down through the **release
  phase** (gate off, envelope ringing), which the old B3+glide fall
  proposals approximated and the gated-only bakes replaced with a held F#5.

The model's mask treats all gate-off frames as non-content, but a
high-release envelope rings audibly while the driver retunes it — release
-phase pitch *motion* is musical content. The discriminator against the
stray-F#3 parking class (which slice 4 correctly kills) is motion vs
stasis: parking holds one register value; the stab tail steps monotonically
(exactly what `fall_extent` already detects for falls). Proposed fix:
extend the verifiable region (and the note's own span, as `fall_note`
already does for slide-through falls) through post-gate frames while the
frequency keeps moving, stopping at stasis — then the fall-style proposals
verify against the tail and survive, parking rows still die, and the bake
bottom can render the tail as pitch runs.

Until then: the slice-3 degradation of this class trades an audible descent
for a pitch-correct held note — strictly gated-trace-faithful, but the ear
(and the solo A/B) prefers the descent. `--unstable-no-forward-gate` reproduces
the old rendering for comparison.

## Risks / open questions

- **Engine-semantics drift:** if the predictor mis-models Pertylizer (glide
  curve, vibrato phase), verification systematically rejects good plans →
  over-degradation. Mitigation: slice-1 synthetic-project MCP renders pin the
  semantics; degradation counts are census-visible from day one.
- **Vibrato + glide composition:** predicted as a sum; confirm Pertylizer
  composes them the same way (one MCP render).
- **Tolerance choice:** 50 cents/90 % is a starting point; slice 1's measured
  distribution on known-good assets sets the real value. Detuned tunes
  (fractional-semitone transpose) must not mass-fail — the instrument
  `transpose` term belongs in the prediction.
- **Performance:** O(frames) per note, trivially cheap next to emulation; the
  corpus sweep already parallelizes.
- **`end_frame` semantics bug** (PLAN §3): fix it *before or during* slice 1 —
  an off-by-one in the verified range would bias every residual.
