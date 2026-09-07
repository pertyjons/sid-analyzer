# Rob Hubbard corpus census

This is the measured HVSC #84 baseline for Rob Hubbard's artist directory. It
answers two separate questions:

1. Which musical and SID-programming traits recur across Hubbard's catalog?
2. Which analyzer, native-extraction, and Pertylizer representation gaps affect
   the most files?

The result is a technical priority list, not an aesthetic judgment about the
music. Trace traits can establish prevalence and locate source windows, but an
audible exporter change still needs a pinned SID-versus-Pertylizer A/B result.

## Reproduce the census

```bash
cargo run --release -p sid-analyzer --bin sid-composer-census -- \
  --corpus /path/to/C64Music/MUSICIANS/H/Hubbard_Rob \
  --subject "Rob Hubbard" \
  --full-length \
  --songlengths assets/Songlengths.md5 \
  --subtunes all \
  --workers 4 \
  --timeout-seconds 120 \
  --corpus-label HVSC-84-Rob-Hubbard-full-length \
  --output /tmp/hubbard-author-full.json \
  --summary-output /tmp/hubbard-author-full-summary.json \
  --markdown-output /tmp/hubbard-author-full.md
```

The JSON report contains every per-subtune outcome and representative source
window. The summary JSON omits those rows for cheap comparison. The Markdown
report is generated from the same aggregate and must not be edited as measured
source data.

The artist directory contains 96 SID files, but SIDId classifies them as 83
`Rob_Hubbard`, six
`Jason_Page/RobTracker`, five `SidTracker64`, and two `Companion`. A default
artist-directory run includes all four. A whole-HVSC run with
`--driver-filter Rob_Hubbard` instead selects the driver family, which includes
music by other authors and is useful for extractor engineering rather than an
artist study.

## Coverage

The 2026-08-28 run selected all 96 artist-directory files:

| Coverage | Result |
|---|---:|
| PSID files | 78 |
| RSID files | 18 |
| Full-length PSID subtunes | 488 |
| Songlengths-resolved subtunes | 488 |
| Play calls | 1,469,948 |
| Derived notes | 282,075 |
| Trace-derived patches | 2,748 |
| Trace failures | 0 |
| Inexact-timing traces | 11 |

The 18 RSID files account for another 93 subtunes. They receive metadata and
typed coverage accounting only because the project deliberately has no RSID
host. Native validation is capped at 1,500 calls per subtune even when the
musical trace runs for the full HVSC duration. Eleven CIA-marked subtunes do not
program a timer period during init; their duration and source seconds therefore
use the explicit vblank fallback and are marked inexact.

## Common musical vocabulary

The denominator below is the 78 trace-analyzed PSID files. Counts are file
presence, so the effect categories are non-exclusive.

| Trait | Files | Reading |
|---|---:|---|
| Pulse waveform | 78 | The universal oscillator foundation. |
| Lead role | 78 | Every file contains lead-like note behavior. |
| Arpeggio | 77 | Near-universal harmonic and timbral motion. |
| Portamento | 76 | Pitch movement is structural rather than exceptional. |
| Percussive role | 76 | Percussion/transient programming is almost universal. |
| Vibrato | 74 | Sustained notes commonly require shaped pitch motion. |
| Triangle waveform | 73 | Common low-frequency and rounded-tone complement to pulse. |
| PWM | 71 | Pulse-width motion is a defining texture. |
| Bass role | 64 | A dedicated low-register function is common despite three voices. |
| Sound-effect role | 59 | Music and effect-like gestures frequently share the same score. |
| Noise waveform | 55 | Central to drums and transient design. |
| Ring modulation | 51 | Live cross-oscillator relationships occur in most of the catalog. |
| Hard sync | 46 | Also common, but often appears well after a tune's introduction. |
| Filter sweep | 28 | Less universal, still a substantial shared behavior. |

Native decoding independently recovers authored effect definitions from 53
files: arpeggio tables in 47, PWM in 46, vibrato in 47, drum pitch drops in 38,
and upward chirps in 26. That agreement between trace behavior and authored
tables is stronger evidence than either view alone.

The common denominator is therefore not a single fixed patch. It is a compact
three-voice grammar: pulse/PWM as the base color, rapid arpeggiation and
portamento for motion, shaped vibrato on sustained notes, noise plus pitch-drop
percussion, and deliberate oscillator coupling through ring modulation and
sync. Mid-note waveform switching appears in 45 files and reusable waveform
programs in 41, so timbre is often a sequence rather than a static instrument.

## Ranked representation work

The census ranks exact-representation targets by affected files and then by
observed duration:

| Rank | Target | Files | Subtunes | Representative full-length window |
|---:|---|---:|---:|---|
| 1 | Shared ring modulation | 51 | 179 | Sanxion, subtune 1, voice 2, 293–335 s |
| 2 | Vibrato shape and delay | 49 | 100 | Nemesis, subtune 1, voice 1, 401–412 s |
| 3 | Shared hard sync | 46 | 113 | Delta, subtune 1, voice 1, 0–144 s |
| 4 | Waveform programs | 41 | 92 | Go Go Dash, subtune 1, voice 3, around 46 s |
| 5 | Dynamic shared filter topology | 27 | 33 | AWM, subtune 1, first transition around 30 s |
| 6 | Combined-waveform fidelity | 22 | 31 | IK+, subtune 1, voice 2, 276–283 s |

This changes the fixture strategy in concrete ways:

- Treat ring and sync as shared live oscillator relationships, not independent
  per-note flags. They affect 51 and 46 files respectively.
- Preserve vibrato onset delay and contour instead of reducing it to a generic
  sine LFO. Full-length coverage raises resolved vibrato from 35 to 49 files.
- Keep waveform programs and mid-note switches as time-varying timbre. A static
  patch loses behavior present in more than half of the PSID set.
- Model one chip-global filter with dynamic routing and mode changes. Per-voice
  filter copies cannot reproduce the 27 affected files exactly.
- Retain measured fallbacks for combined waveforms until model-specific audio
  fixtures prove a better lowering.

The full-length run matters. A 1,500-call opening-window census found hard sync
in 31 files and dynamic filter topology in 17; full-song coverage raises those
to 46 and 27. Routing changes alone rise from 13 to 23 files. Auf Wiedersehen
Monty's start subtune is the clearest correction: its hard-sync span begins
around 196 seconds, outside the earlier window, while filter transitions occur
around 30, 106, 109, and 186 seconds.

## Ranked extraction work

The native path attempted all 488 PSID subtunes. It accepted 179 subtunes,
recovered structure for all 179, accepted at least one subtune in 55 of 78 PSID
files, and accepted every attempted subtune in 29 files. The 13 non-Hubbard
PSID files all have typed `no_extractor` outcomes, so Hubbard-native coverage is
55 of 65 PSID files at file level.

The largest gaps are:

| Rank | Typed gap | Files | Subtunes | Next action |
|---:|---|---:|---:|---|
| 1 | `decode_unreliable` | 26 | 201 | Cluster failures by recovered layout and add the highest-impact grammar variant behind the existing validation gate. |
| 2 | `rsid_host` | 18 | 93 | Keep separate until Kernal, BASIC, CIA, and interrupt requirements are explicitly scoped. |
| 3 | `no_extractor` | 13 | 33 | Group the three alternate player families before deciding whether any warrants native support. |
| 4 | `decode_failed` | 7 | 65 | Extend bounded order/pattern grammar without weakening rejection. |
| 5 | `timing_inexact` | 4 | 7 | Recover a file-derived CIA or stall schedule where possible. |
| 6 | `locate_ambiguous` | 1 | 2 | Strengthen evidence for the Sanxion layout while retaining ambiguity rejection. |
| 7 | `locate_failed` | 1 | 1 | Investigate the Pygmies Revenge variant as a named locator fixture. |

The gate-continuation pass qualifies two bounded player dialects: only bit-7
note rows continue an open gate, or every note row following a sustained status
does. It retains the same recovered source rows, preserves the established
dialect whenever it passes, and admits the alternate only through the unchanged
one-to-one trace gate. This adds 18 subtunes across Action Biker, Dragon's Lair
Part II, Flash Gordon, Geoff Capes, International Karate, Las Vegas Video Poker,
Monty on the Run, Star Paws, Thrust, and Warhawk with no previously accepted
subtune lost.

Precision and recall among accepted native subtunes have medians of 1.0; their
means are 0.979 and 0.990. The immediate native opportunity is therefore not a
looser acceptance threshold. It is a new decoder grammar for the coherent
`decode_unreliable` cluster, verified by the same strict trace alignment.

## Limits

- The study follows the HVSC #84 artist directory and its attribution choices;
  it is not an independent discography.
- Heuristic effects can overlap and are evidence of register behavior, not a
  claim about compositional intent.
- File prevalence does not measure perceptual importance or audible error.
- RSID remains unexecuted, so claims about common musical traits apply to the
  78 PSID files only.
- The census identifies source windows and target pressure. It cannot declare a
  Pertylizer representation correct without rendered A/B evidence.
