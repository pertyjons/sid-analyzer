# Plan: sid-analyzer

This is the single active implementation plan for the project.
[`TODO.md`](../TODO.md) is an index pointing here, completed and superseded
plans live in [`done/`](done/), and `docs/` contains design, investigation,
format, and reverse-engineering reference material rather than a second
backlog.

## Current state

`sid-analyzer` parses and executes PSID v1/v2/v3/v4 files, captures ordered SID bus
activity, models digital chip state, derives musical and causal views, and
exports text, JSON, MIDI, lossless capture, reconstructed digi audio, and
Pertylizer projects.

The analyzer and export foundations are complete:

- PSID emulation, CIA/vblank timing, multi-SID tracing, SIDId identification,
  STIL and Songlengths metadata, note/effect/timbre analysis, and corpus tools;
- ordered lossless capture with resumable digital SID checkpoints and oracle
  coverage for envelope, oscillator, sync/ring, TEST, noise, and OSC3/ENV3;
- a target-neutral `AnalyzedSidProgram` with continuous physical state,
  semantic/native overlays, causal evidence, provenance, and uncertainty;
- deterministic Pertylizer candidate generation, state and forward-model
  gates, explicit fallbacks, structured pattern reuse, and census sidecars;
- strict native extraction and recovered structure for supported Hubbard,
  Crowther, Galway, Gremlin, Whittaker, and GoatTracker variants;
- a render adapter and offline fixture matrix with pinned Nemesis and 6581
  filter windows.

RSID remains outside normal export until a complete host environment exists.
Unsupported formats, drivers, target capabilities, and timing cases must keep
typed reasons; no path may silently fall back or claim exactness.

## Evidence snapshot

Regenerate the corpus evidence rather than editing its measurements by hand:

```bash
cargo run --release -p sid-analyzer --bin sid-corpus-baseline -- --full
```

The latest pinned run covers 25 tunes under `assets/music`:

- trace export succeeds for all 23 analysable tunes;
- 21 tunes have native decoded structure, one has native decoded notes without
  recovered placements, and one has a typed native timing rejection;
- Arkanoid and Last V8 are RSID and are skipped explicitly;
- all 45 generated projects are schema-valid;
- every tune's best available export has zero melodic forward-model failures.

The harness writes deterministic projects and `exports/baseline/baseline.json`
under the ignored `exports/` tree. Native decoding and native structure are
separate capabilities: never call output `native-structured` unless
`recovered_structure` is present.

The full-length composer baselines are regenerated separately:

```bash
cargo run --release -p sid-analyzer --bin sid-composer-census -- \
  --corpus /path/to/C64Music/MUSICIANS/H/Hubbard_Rob \
  --subject "Rob Hubbard" --full-length --subtunes all \
  --songlengths assets/Songlengths.md5 \
  --output /tmp/hubbard-author-full.json \
  --summary-output /tmp/hubbard-author-full-summary.json
```

The HVSC #84 snapshot covers 96 artist-directory files. All 488 PSID subtunes
resolve to full lengths and trace successfully, with eleven explicitly inexact
CIA schedules; the 18 RSID files remain typed metadata-only inputs. The ranked
representation pressure is shared ring modulation, vibrato shape/delay, hard
sync, waveform programs, dynamic filter topology, and combined waveforms. See
the census reference for counts and named windows. Native extraction recovers
validated structure for 179 subtunes.

```bash
cargo run --release -p sid-analyzer --bin sid-composer-census -- \
  --corpus /path/to/C64Music/MUSICIANS/G/Galway_Martin \
  --subject "Martin Galway" --full-length --subtunes all \
  --songlengths assets/Songlengths.md5 \
  --output /tmp/galway-author-full.json \
  --summary-output /tmp/galway-author-full-summary.json
```

The Galway snapshot covers 40 files: 35 PSID and five RSID. All 382 PSID
subtunes resolve to full lengths and trace successfully; 47 have explicitly
inexact call timing. Its ranked representation pressure is vibrato shape and
delay, shared ring modulation, hard sync, dynamic filter topology, and combined
waveforms. See the census reference for counts, comparison
with Hubbard, and named windows. Native extraction recovers validated structure
for 45 subtunes after adding Rambo's `$3F` dispatch / inline-repeat dialect and
the legacy `$C0` Street Hawk/Yie Ar Kung Fu II family; the unchanged full-corpus
gate preserved every prior acceptance.

```bash
cargo run --release -p sid-analyzer --bin sid-composer-census -- \
  --corpus /path/to/C64Music/MUSICIANS/D/Daglish_Ben \
  --subject "Ben Daglish" --full-length --subtunes all \
  --songlengths assets/Songlengths.md5 \
  --output /tmp/daglish-author-full.json \
  --summary-output /tmp/daglish-author-full-summary.json
```

The Daglish snapshot covers 89 PSID files and 527 subtunes. Every subtune
resolves to a full length and traces successfully; 13 have explicitly inexact
call timing. Native extraction recovers validated structure for 294 subtunes.
Its ranked representation pressure is vibrato shape and delay, waveform
programs, shared ring modulation, hard sync, combined waveforms, and dynamic
filter routing. The cross-composer implementation and qualification status is
kept in [`composer-improvement-matrix.md`](../docs/composer-improvement-matrix.md).

## Active roadmap

Work in this order. Each slice must leave the normal repository gate green and
add a synthetic regression. Named real-tune checks use a licensed local corpus
and are not committed as public fixtures.

<a id="public-release-gate"></a>

### 0. Complete the public repository release gate — **in progress**

Technical cleanup already replaced redistributed SID/WAV fixtures with
synthetic coverage, made the local music corpus optional, stopped tracking
generated exports, removed personal absolute paths, synchronized public
documentation, and configured new commits to use the GitHub noreply identity.

The local repository now has one parentless root commit using the GitHub
noreply identity. Old local refs, reflogs, and unreachable objects have been
removed; ignored local music, exports, and Songlengths are preserved.

The remaining release work is:

- replace `origin/main` with the clean local root while the GitHub repository
  remains private, checking the remote tip before force-pushing;
- verify all remote branches and tags and a fresh clone, run the secret-history
  scan on that clone, and confirm hosted CI before changing visibility.

The 2026-09-07 review added CI for default and `corpus-scan` builds, contributor
instructions, dependency metadata, and a third-party inventory. The project
license is now GPL-3.0-or-later, with its full text in `LICENSE` and inherited
Cargo metadata. The reSID-derived tables use the upstream GPL-2.0-or-later
grant under GPL version 3 or later; no separate permission request is needed.
The existing PTZ scripts require no identified output exception. Per Jonsson
confirmed the hero and icon artwork as his own generated images. Provenance
and license notices are recorded in
[`THIRD_PARTY_NOTICES.md`](../THIRD_PARTY_NOTICES.md). The all-feature Cargo license
inventory resolves all 63 packages with no warnings or errors; its accepted
licenses and repeatable command are in `about.toml` and `CONTRIBUTING.md`.
Songlengths is now optional user-supplied metadata selected by `--songlengths`
or `HVSC_SONGLENGTHS`. Its former snapshot is ignored and no longer tracked.
Subtune timing now applies the correct header-specific policy through subtune
256, and malformed RSID/song-selection headers are rejected. Public CI includes
mandatory synthetic PTZ JSON Schema validation with a declared Python validator.
The final isolated source copy passes the default and `corpus-scan` gates
(546 tests each) and the two schema tests; dependency, license, and secret scans
pass. The reviewed source preparation is included in the clean local root.
Remote history replacement and publication checks remain outstanding.

Verification on 2026-09-07 used Linux and synthetic fixtures. Hosted GitHub
Actions has not run yet; confirm it after pushing. Optional local-corpus tests,
live Pertylizer rendering, oracle regeneration, and non-Linux execution were
outside this check. Existing corpus reports predate the timing fix.

Exit gate: a clean checkout contains only redistributable source and attributed
assets, passes the public CI gate without a local music corpus, exposes no
personal credentials or paths, and has one reviewed public root commit.

### 1. Make the public render gate self-contained — **complete**

The checked-in schema-2 matrix contains eight deterministic synthetic cases:
tonal and low-cutoff-filter baselines plus A1/A2/A4 pulse+saw and triangle+saw
controls. Each behavior-changing fixture has an accepted target and a rejected
near-miss. A minimal in-memory NTSC PSID covers header-clock selection without
shipping a third-party source file. Historical composer-derived profiles are
not part of the public fixture set; cross-driver campaigns run against a local
licensed corpus and produce disposable reports.

Exit gate: public CI is deterministic and self-contained, rejects all known-bad
synthetic candidates, and requires no commercial SID or rendered WAV fixture.

### 2. Close measured, buildable fidelity gaps — **next product work**

Only change rendering behavior after a fixture from step 1 exposes and pins the
gap. Current candidates that do not require a Pertylizer schema extension are:

- delayed vibrato is complete: zero-delay contours use per-note expression,
  while a measured stable prefix uses exact placement-relative track-pitch
  automation because Pertylizer's serialized `delay` is a depth fade-in;
- the isolated 6581 ring-edge timing experiment was rejected after worsening
  the pinned sustain window; phase initialization, level, and fold transfer
  must be measured together before another DSP change;
- the 6581 pulse+saw calibration target is closed: all three checked-in pitches
  remain at the reSID floor after total bus collapse and reject the former
  one-code residue; Pertylizer is intentionally unchanged by this work;
- 6581 triangle+saw qualification now separates absolute level from
  gain-normalized spectral shape at A1, A2, and A4. Pulse-width controls are
  identical, as expected without the pulse bit. The synthetic candidate is
  `19.38..21.95 dB` too loud and remains `3.86..16.30 dB` away in normalized
  shape. No single exporter gain or target-engine fit is safe from this
  frequency-dependent evidence, so rendering is
  intentionally unchanged pending a direct-module, frequency-dependent fit;
- layered structural regressions cover digi reconstruction/export census, hard
  sync, moving-modulator ring, forced NTSC, exact CIA multispeed, and waveform
  sequences; digi bundle rendering remains a separate capability gate;
- a synthetic NTSC PSID verifies header clock selection and vblank execution;
  a future local-corpus campaign should restore the end-to-end rendered-audio
  comparison without checking its source into the repository.

Exit gate: every accepted change improves its pinned render residual, keeps all
named controls inside budget, preserves schema validity, and introduces no new
physical-voice overlaps or forward-model failures.

### 3. Qualify target-dependent exact representations

The analyzer already preserves the following state, but exact lowering depends
on Pertylizer behavior. Keep the measured module/automation fallback and census
reason until a serialized, rendered, and reloaded capability fixture proves:

- sub-frame note/transient scheduling without destructive pattern
  fragmentation;
- initial oscillator phase and phase/LFSR continuation;
- one chip-global filter with dynamic voice routing and mode transitions;
- a shared live oscillator relationship for sync/ring sources;
- longer or tick-clocked SID sequences with loop and continuation semantics;
- sampler bundle writing/loading for reconstructed `$D418` PCM and bounded
  rendered one-shots;
- gate-wired LFO retrigger and automatable master/return filter routing;
- per-note parameter overrides where reusable exact occurrences require them;
- a documented semver policy for pinned Pertylizer schema and descriptor
  mirrors.

Target work may unblock one row at a time. A schema field alone is not evidence
that runtime semantics are correct.

Exit gate: each enabled exact representation has a target capability test, a
state-valid candidate, matching render evidence, reload coverage, and a removed
fallback count in the corpus census.

### 4. Corpus-ranked follow-ups

Start these only from reproducible corpus evidence rather than individual tune
preference:

- extend native extraction to high-impact unsupported variants or new driver
  families while preserving typed early rejection and exact structure rules;
  for Hubbard, start with the `decode_unreliable` cluster affecting 29 artist
  files and 207 subtunes, not by lowering the validation threshold; for Galway,
  the Street Hawk/Yie Ar Kung Fu II cluster is complete with 14/33 accepted and
  structured subtunes under the unchanged gate. Take Hunchback II/Kong Strikes
  Back next for the strongest remaining shared-code evidence (0.898 Jaccard
  similarity), or MicroProse Soccer V1/indoor for the largest remaining
  conservative pair (31 subtunes);
- retain the named Ocean Loader 1 negative control and cycle-stamped Galway
  packed-nibble detector fixture when changing `$D418` analysis; the former
  Neverending Story result was invariant master-volume housekeeping, not PCM;
- add an in-process SID renderer and feature-gated spectral regression tests,
  retaining reSID/libsidplayfp as an external oracle;
- build a stable cross-corpus patch/program codebook after render fidelity has
  remained stable for at least two weeks;
- revisit driver-agnostic semantic mapping, grammar induction, and LLM-assisted
  decoder drafting behind the same validation gates as hand-written extractors;
- implement an RSID host only as a separately scoped project with ROM and C64
  environment requirements made explicit.

## Completed milestones

The following roadmap areas are closed and should not be copied back into the
active backlog:

- reproducible corpus baseline and typed skip/rejection reporting;
- timing correctness, digital SID state, external oracle vectors, and
  `play_address == 0` IRQ handling;
- supported native extraction and exact recovered structure;
- structured export compaction and safe cross-track reuse;
- chip-causal export, continuous-region semantics, and deterministic
  representation selection;
- target-neutral analyzed program and removal of transitional synth inputs;
- multi-SID analysis, digi reconstruction, STIL, lossless capture JSON, and
  expressive MIDI export;
- live `sid-abtest` process integration plus the initial Nemesis and low-cutoff
  6581 render qualification.

Detailed implementation records are indexed in [`done/README.md`](done/README.md).

## Verification

Before every commit:

```bash
cargo fmt --check
cargo build --workspace
cargo clippy --workspace --all-targets
cargo test --workspace
```

Additional gates by change type:

- native work: qualification fixture plus full-length named tune;
- export work: schema validation, census comparison, and
  `synth_fidelity_budget`;
- digital SID work: synthetic transition tests plus oracle vectors;
- render-facing work: pinned short-window A/B result;
- corpus work: deterministic sorted output and explicit skipped-input reasons.

## Reference map

- [`done/analysis-completeness.md`](done/analysis-completeness.md): historical
  information-layer audit and provenance rationale.
- [`done/analyzed-sid-program.md`](done/analyzed-sid-program.md): implemented
  target-neutral program design and delivery record.
- [`done/export-fidelity-next.md`](done/export-fidelity-next.md): superseded
  corpus-driven export plan and its original fixture priorities.
- [`../docs/export.md`](../docs/export.md): SID-to-Pertylizer architecture and
  format reference.
- [`../docs/export-fidelity-investigation.md`](../docs/export-fidelity-investigation.md):
  root-cause analysis for heuristic export failures.
- [`../docs/extraction-methods.md`](../docs/extraction-methods.md): taint, probe,
  and decoder-RE methodology.
- [`../docs/hubbard-corpus-census.md`](../docs/hubbard-corpus-census.md):
  full-length HVSC #84 artist census, common traits, and ranked extraction and
  representation work.
- [`../docs/galway-corpus-census.md`](../docs/galway-corpus-census.md):
  matching Martin Galway census, Hubbard comparison, and named fixture
  priorities.
- [`../docs/daglish-corpus-census.md`](../docs/daglish-corpus-census.md):
  full-length Ben Daglish artist census and Gremlin/Crowther priorities.
- [`../docs/composer-improvement-matrix.md`](../docs/composer-improvement-matrix.md):
  shared implementation, render-qualification, and native-extraction status
  across Hubbard, Galway, and Daglish.
- [`../docs/drivers/`](../docs/drivers/): driver-specific format maps and
  evidence.
- [`done/`](done/): completed implementation plans, reviews, and closure
  records.
