# SID → Pertylizer export

> **Status: architecture and mapping reference.** Active implementation work is
> consolidated in [`PLAN.md`](../plans/PLAN.md); the rollout notes below are
> retained as detailed historical context and may not be independently current.

How `analyzer::export::synth` turns an `AnalyzedSidProgram` from either trace
analysis (`--format synth`) or driver-native recovery (`--format synth-native`
and `--format synth-modern`) into a schema-valid Pertylizer `.ptz`. Direct file
emit, parallel to `text`/`json`/`midi` — no MCP, no running synth.

**Goal:** map musical *structure* faithfully, then chase *timbre* as far as the
Pertylizer schema allows. Approximate, not bit-accurate — Pertylizer's
oscillators/filter are not SID models.

**Non-goals:** bit-accurate SID reproduction; RSID; driving Pertylizer at runtime
via MCP.

This merges the former `sid-to-pertylizer.md` (exporter status) and
`pertylizer-sound-engine-plan.md` (the fidelity-ladder redesign, now mostly
shipped). Companion reference lives in `pertylizer/` (schema mirror + `PATCHES.md`
format dissection + `module-audit.md` capability review) and `drivers/` (which
notes each extractor recovers).
[`export-fidelity-investigation.md`](export-fidelity-investigation.md)
(2026-07-05) is the structural review of the note-decomposition special cases
and the **forward-model gate** proposal. The active render qualification work in
[`PLAN.md`](../plans/PLAN.md#active-roadmap) owns
the render-side twin:
`sid-abtest`, an automated note-aligned reSID-vs-render ear-proxy with a
classified failure report.

`sid-abtest render` drives `sidplayfp` and the external `pertylizer render`
protocol v1 directly. It pins lossless `32f` output with no post-transport tail,
stores and validates the Pertylizer receipt beside the cached WAV, and maps
`--voice N` to every exported track whose ownership prefix is `VN`.
`--start-seconds` selects a source-aligned comparison window. Both renderers
still run from song start, with a one-second guard after the window, so SID and
target oscillator, envelope, filter, and effect history remain intact before
only the requested samples are measured.
Pertylizer's integer WAV depths remain readable for offline `sid-abtest wav`
inputs, but live A/B rendering does not introduce their quantization error.
The live command retains whole-second windows: Pertylizer accepts fractional
seconds, but `sidplayfp` recording rejects sub-second windows and otherwise
requires a different time spelling, so fractions are not a shared protocol.

---

## Status (2026-08-08)

Both input paths now construct the same target-neutral `AnalyzedSidProgram`
before export. Native recovery attaches authored structure and validation as a
semantic overlay; its intermediate `NativeSong` is private to the extractor
registry. The synth exporter imports neither `Trace` nor `FrameState`.

Before serialization, the lowering compiler generates concrete ADSR, MSEG,
arpeggiator, glide, LFO, kinetic-modulator, direct-CV, Mod Matrix, Script,
SID-oscillator/sequence, shared-filter, automation, and sampler recipes. State
and capability gates reject invalid recipes. The render gate selects the
lowest-cost accepted recipe per required state domain; candidates without
render evidence cannot displace the pinned dense-automation baseline. The
selected coverage, costs, residuals, requirements, and rejection reasons are
recorded in the census.

Program debug schema 5 stores a complete digital SID checkpoint at every
continuous occurrence start. Physical voice regions form one ordered lineage
across silent gaps; overlapping waveform/texture views remain unlinked layers.
Lowering requires verified phase initialization and continuation behavior for
exact SID sequences, and treats `$D418` as digi only when analysis produced a
`DigiStream` region.

The lowering census now contains the chip-causal contract rather than generic
recipe labels. Each gate rise retains its pre-onset voice registers and ordered
SID writes; continuous occurrences carry explicit oscillator and noise start
policies; envelope recipes report history-sensitive retrigger residuals; and a
shared filter or live sync/ring graph requires verified target behavior. RAM,
table, timer, accumulator, OSC3, and ENV3 modulation lowers to a direct graph
only for a supported high-confidence single consumer. Random or ambiguous
flows remain measured automation. The pinned serializer is intentionally
fallback-only and records timing, state, and topology losses in the census.

`--format synth-native` and `--format synth-modern` are strict contracts:
identification, exact timing, locator, decode, or validation failure returns
non-zero and does not create or replace the requested project. `--format synth`
is the separate trace-derived choice. Project and census sidecar replacement use sibling temporary files,
flush/sync, and atomic rename; a sidecar failure warns but cannot corrupt the
previous sidecar or invalidate a successfully written project.

One resolved `PlaybackTiming` object supplies the rational call rate throughout
emulation, analysis, native pitch/effect conversion, and export metadata. Native
validation refuses CIA-driven subtunes while their timer period is unknown. The
native census records driver/extractor, the full one-to-one validation report,
bounded decoder phase, field-level provenance, exact recovered driver structure,
render placements, forward-model results, and explicit unstable debug options.

Track splitting preserves physical SID voice ownership: a following note on the
same hardware voice caps an older plan's release note and measured amplifier
lane. Census reports both pre-lowering release overlaps and post-choke rendered
overlaps. The checked start-song fixtures have zero rendered overlap; examples
of source tails that are deliberately choked are Knucklebusters (1 pair/75
calls), Sigma Seven (23/23), and Warhawk (14/3192) in a 1500-call window.

The exporter is built, schema-validated in `tests/synth_export.rs`, and
load-tested live in Pertylizer. Landed and stable:

- **Modern Analog profile** — `--format synth-modern` keeps driver-native notes,
  structure, expression, ADSR, cutoff, and pulse-width automation, then replaces
  SID modules with editable role-aware oscillator layers, filters, and effects.
  `export/synth/modern.rs` owns the profile boundary and recipes; adding a style
  does not fork decoding, track planning, or song construction. Pulse-width
  lanes are retargeted from `sid_oscillator.pw_reg` to
  `oscillator.pulse_width`.

- **Faithful enhancement** — `--enhance <1-10>` composes with `--format synth`
  or `--format synth-native`. It retains every SID oscillator, source module,
  connection, pattern, placement, note, expression, and automation lane, then
  adds level-scaled production effects selected from the analyzed instrument
  role. Bass and drums receive bounded tube saturation and parallel
  compression; melodic roles can also receive chorus, short delay, or reverb.
  A restrained low/body EQ and parallel compressor precede the existing master
  limiter. Level 5 is the balanced starting point; levels 6–10 add progressively
  heavier saturation, body, and level compensation while keeping ambience
  bounded. The option is deliberately rejected with `synth-modern`, which
  already replaces and processes the source instruments.

- **Structure & notes** — one track per `(voice, merged-shape)`; per-voice
  instrument merge; native extractors supply exact patterns/placements/structure.
- **Guarded patch adoption** (commit `09dd112`) — unassigned (sub-threshold
  cluster) notes only adopt the nearest same-voice patch when `timbre_compatible`
  (waveform family + filter routing + ADSR within tolerance) and within
  `ADOPTION_MAX_DISTANCE_FRAMES`; otherwise they fall to a raw plan instead of
  being reskinned with a lead's full patch. Fires only on the heuristic
  `--format synth` path — native binds every note to its authored instrument via
  `extract_patches_grouped`, so there are no unassigned notes to guard.
- **Native `sid` oscillator voices** — every voice source is now a native
  `sid_oscillator` (model from the header, clock PAL/NTSC), not generic
  osc/noise modules. This gives the chip's real combined-waveform mix, native
  RING (neighbour-voice carrier), hard sync, LFSR noise, and a looping per-frame
  waveform sequence — all in one module. Replaced the old dominant-waveform +
  two-osc-sum + `rng` approximations.
- **Expression** — zero-delay per-note vibrato / portamento glide / legato;
  arpeggio via the native `Arpeggiator` NoteProcessor (default on); onset
  attack chirp; static cents detune via instrument `transpose`.
- **Automation** — exact PWM + filter-cutoff lanes (`AutomationTarget::Module`)
  sampled per-frame; delayed vibrato as placement-relative track pitch;
  `$D418` master-volume contour (`Global` lane, digi-hammer rejected by a point
  cap).
- **Filter** — reSID-calibrated cutoff curve + resonance cap per SID model; the
  `acid` filter model (closest Pertylizer match, shipped 2026-07-03).
- **Master-bus coloring (§A9)** — one light tube `distortion` + 3-band `eq` +
  `-5 dB` look-ahead `limiter` on the master bus. The coloring was calibrated
  against reSID with `analyze_mix_bus`; the limiter was render-calibrated on
  the pinned Nemesis intro and preserves its reference RMS while reducing peak
  error from about 0.27 to 0.07.
- **Role-calibrated output** — analyzed lead/drum roles receive bounded channel
  trims; arpeggiated leads and percussion also receive a non-resonant output
  low-pass after the programmable SID graph. On the pinned Nemesis windows this
  cuts V1 percussion RMS/peak errors to about `0.003/0.022`, V3 percussion
  centroid error to about `27 Hz`, and melody peak error to about `0.04`.
- **Authored intent (E1)** — Hubbard's decoded `InstrumentEffects` thread into the
  export via `Patch.authored_effects`; consumed today for vibrato backfill and
  authored PWM programs (E3 slice 1, below).
- **Driver program as script (E3)** — authored PWM → a `scr` YAMS program
  (slice 1, trace-confirmed); waveform alternation → native `sid` seq (slice 2);
  fast arp → native processor (slice 3).

Tempo/grid: a musical BPM is derived per subtune (onset autocorrelation, folded
to `[75,150)`), with an exact integer frame→tick scale — see Reference.

---

## Historical rollout and fidelity notes

The active backlog is exclusively in [`PLAN.md`](../plans/PLAN.md). The entries
below retain implementation context; explicit completion markers take
precedence over the original grouping.

### Exporter follow-ups recorded by the investigation

- **Fidelity census** *(completed 2026-08-08).*
  `write_synth` prints a per-export `Census` to stderr: instrument/track/note
  counts, percussion / raw-low-fi split, authored-vs-inferred patch ratio (E1
  coverage), filter-routed + combined-waveform frame counts, `$D418` volume
  changes (digi-hammer flag), vibrato/portamento/arpeggio span counts, ring-mod /
  hard-sync notes. It is written as sidecar JSON next to the output for
  corpus-wide regression collection. Duration integrity is reported as both
  per-voice uncovered gated-frame totals and the ten longest contiguous spans,
  which makes trailing-content failures directly locatable from the sidecar.
- **Vibrato shape + onset delay** *(completed 2026-08-28).* The contour
  measurement recovers depth, rate, stable-prefix delay, and the nearest
  supported shape. Pertylizer's persisted per-note `delay` means depth fade-in,
  not an onset hold, so only zero-delay contours use `NoteExpression.vibrato`;
  delayed contours lower to a placement-relative `Track::Pitch` lane sampled
  from the SID frequency register. Auf Wiedersehen Monty and Trap render
  fixtures accept that path. AWM robustly rejects its square-shape control; the
  zero-delay and Trap shape candidates remain explicit accepted resolution
  controls because the current aggregate audio features cannot separate them.
  Unit tests enforce the structural lowering distinction directly.
- **Faithful raw fallback** *(completed 2026-07-19).* A timbre-incompatible
  note uses `raw_trace_instrument`, which derives waveform, pulse width, ADSR,
  and current filter routing from measured state rather than silently becoming
  a generic pulse patch.
- **High-risk regression matrix** *(completed at the available evidence layer
  2026-08-28).* Warhawk proves the physical ring-source cable and moving
  `sid-2.freq_reg` lane end to end; Shape Music 2 proves a three-class SID
  waveform sequence survives full export; forced NTSC proves clock selection
  reaches every SID module and the project timebase; PAL, NTSC, and exact CIA
  rates share the real-time round-trip test; and dense `$D418` PCM reaches the
  export census without becoming global master-volume automation. Hard sync
  retains structural disconnected-source coverage. A deterministic synthetic
  NTSC PSID now verifies header clock selection and vblank execution without a
  redistributability-sensitive source fixture. Digi audio rendering remains a
  separate capability gate.
- **`$D418` sample voice (§A7)** *(rendering deferred — narrowest win, real
  cost).* Detection, cycle-stamped nibble reconstruction, WAV encoding, sampler
  lowering requirements, and census coverage exist. No inline PCM storage in
  the schema (audio lives in a `.zip` bundle whose on-disk format is not pinned),
  so the continuous digi stem cannot yet be delivered as a self-contained
  rendered project.
- **Duration resolution** *(completed 2026-08-06).* All export formats,
  including `synth`, use the explicitly supplied local Songlengths input.
- **Smaller historical ideas:** per-voice stereo pan spread; optional top-level
  `author {name,email}`;
  cross-voice duplicate-instrument dedup; optional global reverb/delay flag.

### Schema-blocked — waiting on Pertylizer

- **§B1 — Tracker-style instrument table (wavetable/arp).** *Their Phase E.* Now
  largely subsumed by the native `sid` module's per-frame seq; a fuller
  tick-clocked table with a loop point would still generalize arp beyond the
  legato subset. The `seq_loop` flag shipped, but the `sid` seq is capped at 16
  steps. Short periods retain every held frame (`T T N N` stays four steps);
  longer representative windows such as Nemesis V2's 22-frame capture fall
  back to their distinct audible masks. Exact timing for a genuine period over
  16 steps still needs a longer / tick-clocked table.
- **§B2 — Shared / bus filter + `AutomationTarget::ReturnBus`.** *Their Phase D.*
  SID has **one** global filter all voices route through. The exporter can
  serialize a stable shared return filter, but Pertylizer's runtime currently
  cannot construct `Filter` as a return-bus `AudioEffect` and drops it with an
  `unsupported-module-type` warning. Dynamic cutoff/routing also lacks a return
  automation target. Per-instrument `flt.cutoff` automation remains the working
  partial substitute until both target capabilities have fixtures.
- **§B3 — `Note.param_overrides`.** *Their Phase E.* Per-note parameter tweaks;
  per-instrument overrides (lanes) exist, per-note do not.
- **Semver bump policy** — version + drift-guard exist; the written minor-vs-major
  rule is pending on us.

### Open fidelity gaps (measured, harder)

From the historical 48-case oscillator reSID experiment (reproducible with
`assets/fixtures/sound-engine-poc/mk_sid.py`; generated SID/WAV files are not
checked in; `log_spectral_distance` in dB, saw row = method floor). The native `sid` module is
**at the floor** for saw / pulse / tri / noise / pulse+tri / hard-sync / ring on
both chip models and **all four 8580 combos**. Remaining:

- **6581 pulse+saw (0x61) has a pinned calibration target.** A 2026-08-28
  synthetic re-run at A1, A2, and A4 confirmed that the reference stays at
  the quantization floor. A total bus collapse matches it; the former one-code
  MSB blip produces roughly `0.01` RMS and `0.323` peak and is rejected by all
  three controls. This repository records the target and does not change the
  Pertylizer implementation.
- **6581 tri+saw (0x31) has separate level and shape gates but remains open.**
  The A1/A2/A4 matrix reports `19.38`, `19.74`, and `21.95 dB` level error and
  `3.86`, `13.77`, and `16.30 dB` gain-normalized spectral error. PW `0x0200`,
  `0x0800`, and `0x0E00` produce identical candidate profiles, correctly
  proving that inactive pulse width is not the cause. The frequency-dependent
  level and shape residuals rule out treating one global trim as a DSP fix.
  Keep the existing option-C pulldown until a direct-module,
  frequency-dependent target fit improves all pinned controls.
- **Ring-mod** — sideband placement is exact, but the 6581 residual remains. A
  source-aligned exact-edge prototype worsened the sustained window from 7.896
  to 13.302 dB and the full window from 11.504 to 17.711 dB, so no target-engine
  change was retained. A future experiment must control phase initialization,
  level, and fold transfer together.
- **Filter response** — the 6581's low-cutoff dry leakage is now represented by
  a bounded parallel path that tapers from `0.40` at 420 Hz to zero at 2 kHz.
  The `$200` fixture moved from 12.49 to 9.25 dB LSD and from 105 to 8 Hz
  centroid error; the pinned matrix rejects the sealed low-pass. Raw trace
  instruments now retain filter routing as well. The `acid` high-cutoff
  resonance peak is bounded by a render-qualified `0.20` normalized ceiling:
  Nemesis voice 2 at 20–28 s moved from 9.73 to 4.63 dB LSD, 9.88 kHz to
  251 Hz centroid error, and 0.082 to 0.0008 RMS error. The pinned matrix
  rejects the former `0.65` ceiling.
- **3+-class per-frame waveform grit** — mostly covered by the native seq now;
  per-note envelope-shaped grit remains approximate.
- **Noise-percussion brightness (full-mix)** — measured 2026-07-05 (AWM drum
  section 38–44 s vs reSID), ATTACKED 2026-07-06 in two shipped slices
  (time-resolved LSD on that window: 9.87 → **9.18 dB**, centroid gap
  −2573 → −2136 Hz):
  1. *Noise accent pinned* — the percussion noise click clocks at the
     trace-measured noise-frame register (`NoiseAccent`; AWM V3 snare `$684C`,
     19× its body pitch) instead of tracking the note, and the click decay
     follows the measured noise-run length.
  2. *One-shot waveform programs* — the Hubbard per-frame wavetable
     (`T N T P N N N N N P P P` → hold) exports as a held sid-module seq
     (`seq_loop=0`), grouped **per note's own program** (a patch-mode program
     regressed 9.5 → 11.5 dB: minority members played noise where the chip
     held pulse) and gated on the noise frames clocking near the note pitch
     (`PROGRAM_NOISE_PITCH_MAX_RATIO`; V2-solo 36.6 → 29.0 dB with programs,
     while ungated V3 bass ticks *regressed* 9.9 → 12.6 — those need per-step
     frequency through Pertylizer's `seq_step_freq_i` + `seq_freq_mask` fields.
     Tonal steps inherit note pitch; measured noise steps carry the median raw
     SID frequency for that program position. The oscillator also receives the
     analyzer's deterministic `0x7ffff8` noise seed.
  Still open: (a) sid-seq restarts on legato splits (engine gap — the chip's
  wavetable free-runs from gate-on across retunes), (b) per-seq-step
  `freq_reg` for far-from-pitch noise ticks, (c) noise-step level vs reSID
  through the master tube (export ZCR overshoots ~40 % with programs on).
- **AWM V1 lead truncation (native decode, uncovered-tail class)** — fixed
  2026-08-08. The original pattern-52 hold was already handled, but the
  full-length census exposed a second variant in pattern 34: bit-7 note bytes
  reuse the instrument/envelope while still carrying a new frequency index,
  and retrigger when the preceding status row lets the gate expire. Treating
  every such byte as an unconditional hold left an 85-frame gap. The decoder
  now distinguishes a continuing five-bit-format sustain from a bit-7 note
  after gate expiry. Full-length uncovered content fell from `184/17/41` to
  `2/17/30`; every remaining span is one frame and the longest gap is pinned.
  Historical diagnosis: found
  2026-07-06 by per-voice solo A/B (sidplayfp `-u` refs vs track-muted export
  renders): the 15 s vibrato lead at frame 1356 is gated 581 frames in the
  trace but the native Hubbard decode emits 95 frames — rest rows after the
  note hold the gate and the extractor ends the note at the pattern row. V1
  is near-silent for ~13 s (V1-solo 49 dB, voice effectively missing — the
  dominant full-mix error on this tune, and the same class as the former census
  `uncovered gated frames` 486/0/571 and the Muso_64 queue entry).

---

## The fidelity ladder (design)

Treat a SID instrument as a *program* and map each feature to the **cheapest
faithful primitive**, escalating only when native modules structurally can't
reach it. This generalized the old special-case builder zoo
(`alternating_instrument`, `drum_drop_instrument`, `expand_arpeggio`) into one
ladder over a shared IR.

- **Tier 0/1 — native modules.** pitch, ADSR, waveform, PW, filter, cents; plus
  `sid` module (combined waveforms, LFSR, ring/sync, per-frame seq, DAC/model),
  `osc.sync`, `flt.cutoff_cv`, vibrato Expression `{shape,delay}`, Arpeggiator,
  glide/legato.
- **Tier 2 — `scr` Script.** The recovered driver program as a YAMS script +
  `arr` tables indexed by `age`/frame (arp/PWM/filter contour). One mechanism
  replaces the special-case builders (E3).
- **Tier 3 — `asc` AudioScript.** *Superseded by the native `sid` module* — a SID
  oscillator DSP the module now owns engine-side. The `asc` PoC proved the
  measurement harness and register mapping; E4 is skipped per its own exit clause.

**The analyzed program — `SidVoiceProgram`** (spec before writing backends; the
analysis layer already holds ~90% via `NoteCharacteristics` +
`PatchVoiceProfile` + native `InstrumentEffects`): timbral core (waveform set
incl. combined byte, ADSR, PW, ring/sync topology); program tables (per-tick
wavetable/pulse/arp + loop points, indexed by **tick** carrying the tick rate and
chip model); articulation (vibrato/glide/gate/arp); and a **provenance field per
parameter** (`Authored` from the driver vs `Inferred` from the trace — only
Hubbard yields `Authored` today; the census must report the split). A
chip-scoped companion owns the shared filter, routing, and mixer rather than
duplicating them into the voice programs. The lossless capture and full expanded
model are recorded in
[`plans/done/analyzed-sid-program.md`](../plans/done/analyzed-sid-program.md).

**Rollout (E1–E4), each with a measurable exit gate:**

1. **E1 — stop discarding authored intent.** *Shipped* (vibrato backfill +
   authored PWM). Exit: census reports zero authored fields dropped.
2. **E2 — adopt native Tier-1 primitives** (`sid` module, `osc.sync`, vibrato
   shape/delay, `flt.cutoff_cv`, digi sample import). *Mostly shipped via the
   `sid` module rewire.*
3. **E3 — `scr` Script backend for the driver program.** Slices 1–3 shipped
   (authored PWM / waveform alternation / fast arp). `expand_arpeggio` stays as
   the fallback until the clean-arp gate covers every shape. Filter-contour slice
   dropped after measurement (cutoff lanes are rare and have no authored rule —
   a YAMS replay is the same samples in another container).
4. **E4 — `asc` timbral-core prototype.** *Skipped* — the native `sid` module
   landed first (`e7f2f3a8`); Tier-3 work folded into the module spec.

**RE follow-up (E1):** the Hubbard `+5` vibrato byte is **not** a plain
right-shift count — Monty authors per-instrument *rates* (high nibble likely LFO
speed), so authored-first precedence waits on disassembling the vibrato block in
Monty's relocation and fixing `authored_patch_effects` (`hubbard.rs`).

---

## Reference

### Mapping (SID → Pertylizer)

| SID concept                    | Pertylizer target                                                          |
|--------------------------------|----------------------------------------------------------------------------|
| waveform bits (incl. combined) | native `sid_oscillator` waveform mask (one module)                         |
| 4-bit ADSR nibble              | `env.{attack,decay,release}` seconds (table below); `sustain = nibble/15`  |
| 12-bit pulse width             | `sid_oscillator.pw_reg` (raw 0–4095, linear)                               |
| `filter_routed` (per voice)    | chain `source → flt → amp` (routing from that voice's own `$D417`)         |
| filter mode                    | `flt.type` LP/BP/HP by priority; LP+HP → `notch`; model `acid`             |
| `HardwareTrick::RingMod`       | native RING bit + a `track_pitch`-off neighbour `sid` feeding `msb → ring` |
| hard sync                      | native SYNC bit + neighbour `sid` `msb → sync` (`sync_source_hz` capture)  |
| `voice 1/2/3`                  | one `SequencerTrack` per `(voice, merged-shape)` → bound instrument        |
| `NoteEvent`                    | one `Note`, `start_frame * ticks_per_frame → tick`                         |
| effect: FilterSweep / PWM      | `Module` automation lane on `flt.cutoff` / `sid.pw_reg`                    |
| effect: Vibrato                | zero-delay `NoteExpression.vibrato`; delayed `Track::Pitch` automation     |
| effect: Portamento             | per-note `Glide { from, time, interp }`                                    |
| effect: Arpeggio               | native `Arpeggiator` NoteProcessor (Custom offsets, MilliHz = frame rate)  |
| `$D418` master volume          | `Global` `MasterVolume` lane (§A5), or none if constant                    |
| onset pitch chirp              | fast per-note `Glide` (§A8), when no portamento opens the note             |
| static cents detune            | instrument `transpose` (fractional semitones), median per plan             |

Module ids follow the schema regex: `^osc-\d+$`, `^env-\d+$`, `^flt-\d+$`,
`^amp-\d+$`, `^nse-\d+$`, `^rng-\d+$`, `^mix-\d+$`, `^sid-\d+$`, `^scr-\d+$`.

### SID ADSR nibble → seconds

Attack times per nibble (ms); decay/release use **3×** the attack column
(canonical 6581/8580 table). Clamp each to `≤ 10.0` s (schema cap; the dec/rel
table reaches 24 s at nibble 15).

```
nibble:    0    1    2    3    4    5    6    7    8    9   10   11    12     13     14     15
attack ms: 2    8   16   24   38   56   68   80  100  250  500  800  1000   3000   5000   8000
dec/rel:   ×3 of the attack column
```

### Validated format idioms

Confirmed against the gold file (`SID Export v0 Test.json`); full dissection in
`pertylizer/PATCHES.md`.

- `Note` has **no `instrument` field**; `SequencerTrack.instrument` binds it.
- `song.patterns`/`tracks`/`arrangement` are **arrays of objects** with the id
  inside each element; song carries `next_pattern_id`/`next_track_id`.
- **Connections are 2-string tuples**: `{"from":["osc-1","out"],"to":["amp-1","in"]}`.
- **Pitch and gate are implicit** — the engine drives them from the active note;
  no pitch/gate wire, and the envelope module has no input ports.
- Pan uses `BipolarValue` (-1 L / 0 center / 1 R); velocity is `f32` 0–1.
- Enums are **string-only** (`"pulse"`, `"lowpass"`, `"exponential"`).
- Emit **all** parameter keys explicitly (blocks are `additionalProperties:false`
  but accept omitted keys as defaults).
- **Automation-lane values are normalized `0..1`** through the param's response
  curve; on-disk *parameter values* are real values in `[min,max]`. The exporter
  reads ranges + curves from the embedded `descriptors.json` (`export::descriptors`,
  drift-guarded by `descriptors_cover_exporter_params`).

### Time-base

Pertylizer uses 960 PPQN + BPM; vblank call rates are represented rationally
(PAL `985248/19656`, NTSC `1022727/17095` calls/s).
The time base is derived per subtune (§A6):

- `ticks_per_frame` (integer) is the absolute scale: `tick = sid_frame *
  ticks_per_frame`. Tempo follows: `bpm = ticks_per_frame * frame_rate / 16`, so
  **real-time playback is exact for any scale** — a musical BPM only relabels.
- `ticks_per_frame` comes from onset-autocorrelation tempo detection folded to
  `[75,150)` BPM; below `MIN_TEMPO_ONSETS` it falls to the clock default (PAL
  40 → 125 BPM, NTSC 33 → 123.75).
- `song.row_resolution.ticks_per_row = 240` (16th-note grid); cosmetic only.
- CIA-timed analysis adopts a complete `$DC04/$DC05` timer period programmed
  during init and schedules play calls at the resulting exact rate. Native
  export rejects a tune when init supplies no complete period. A player that
  later reprograms the timer is marked inexact instead of retaining a stale
  ground-truth rate.

### Exporter entry point

`synth::write_synth(program: &AnalyzedSidProgram, out: &mut dyn Write) ->
io::Result<Census>`, with option-bearing and quiet variants. Cached physical
frames and semantic/native views are owned by the program; no alternate
`FrameState`, trace, JSON-export, or native-song entry point exists.
CLI: `--format synth`, `--format synth-native`, or `--format synth-modern` (all
require `--output`). `--enhance <1-10>` is available for the two faithful synth
formats; omitting it leaves their output unchanged.

---

## Faithful enhancement

Enhancement is an opt-in post-pass over the faithful instrument graph. The
scalar coordinates wet mix, saturation drive, body EQ, compression, and level
compensation; it is not a simple output-gain control. Levels 1–5 run from subtle
to the original balanced recipe. Levels 6–10 follow a separate heavy curve that
adds harmonic density, low-mid body, and parallel-compressor makeup while
reducing the rate at which compression depth and ambience increase. The
exporter does not add notes, transpose voices, replace oscillators, or edit the
song object.

The role matrix keeps low-frequency parts focused: bass has no chorus, delay,
or reverb; kicks use only a very short low-level room, while leads, arpeggios,
pads, bells, and untagged melodic material receive progressively wider ambience.
The implementation lives in `export/synth/enhance.rs` and is separate from the
`ModernAnalog` resynthesis profile.

```console
sid-analyzer tune.sid --format synth-native --enhance 5 --output tune.ptz
```

`--enhance 1` and `--enhance 10` use the same module topology for a given role;
only bounded parameters change. Level 5 remains the stable boundary between the
balanced and heavy curves. Values outside `1..=10`, non-synth formats, and
`synth-modern` are rejected before export.

---

## Modern sound profiles

Modern sound design is deliberately separate from SID faithfulness. The first
profile, `ModernAnalog`, maps semantic roles to editable recipes: bass uses an
analog oscillator plus sub oscillator; leads and pads combine analog and
wavetable sources; drums combine pitched bodies with chip noise. Each recipe
also owns its filter, level trim, and ordered effect chain. The shared exporter
still owns musical timing and automation, so correcting a note or recovered
pattern benefits every profile while sound variants remain isolated.
The faithful default remains unchanged, and a profile adds no notes—that would
be composition rather than sound design.

To tune the shipped profile, edit the named `VoiceRecipe` constructor and its
`EffectRecipe` branch in `export/synth/modern.rs`; arrangement code is not
involved. To add a profile, add a `SynthStyle` variant, implement
`ModernProfile`, and add one style-dispatch arm in `write_synth_source`. A
profile receives analyzed `TrackPlan` roles and returns only synthesis/mix
choices, which keeps future variants from depending on driver implementations.

## Pattern reuse

Native extractors retain their recovered per-voice placements. The exporter
rebases each note block, moves leading silence, transpose, and uniform velocity
to the placement, then content-addresses the remaining canonical body across
tracks. Musical note identity is separate from note-local expression in the key;
vibrato and glide must still match exactly because the project schema has no
placement-local override for either. A truncated body may reuse a longer prefix
only when its `length_override` clips before the first extra note. Placements use
`loop_mode: "clip"`, so a longer source body cannot repeat into source silence.

Automation remains in full-length lane-only patterns and is counted separately
from user-facing musical patterns. Trace-only exports still use one full-length
pattern per track; inferring reusable form from a flattened trace remains future
MIR work.
