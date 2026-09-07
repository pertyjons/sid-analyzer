# Martin Galway corpus census

This is the measured HVSC #84 baseline for Martin Galway's artist directory. It
answers two separate questions:

1. Which musical and SID-programming traits recur across Galway's catalog?
2. Which analyzer, native-extraction, and Pertylizer representation gaps affect
   the most files?

The result is a technical priority list, not an aesthetic judgment about the
music. Trace traits can establish prevalence and locate source windows, but an
audible exporter change still needs a pinned SID-versus-Pertylizer A/B result.

## Reproduce the census

```bash
cargo run --release -p sid-analyzer --bin sid-composer-census -- \
  --corpus /path/to/C64Music/MUSICIANS/G/Galway_Martin \
  --subject "Martin Galway" \
  --full-length \
  --songlengths assets/Songlengths.md5 \
  --subtunes all \
  --workers 4 \
  --timeout-seconds 30 \
  --corpus-label HVSC-84-Martin-Galway-full-length \
  --output /tmp/galway-author-full.json \
  --summary-output /tmp/galway-author-full-summary.json \
  --markdown-output /tmp/galway-author-full.md
```

The JSON report contains every per-subtune outcome and representative source
window. The summary JSON omits those rows for cheap comparison. The Markdown
report is generated from the same aggregate and must not be edited as measured
source data.

All 40 files in the directory are identified as `Martin_Galway`. An
artist-directory run therefore measures the same file set as
`--driver-filter Martin_Galway` over that directory. A whole-HVSC driver-family
run may include files outside the artist directory and answers a different
extractor-engineering question.

## Coverage

The 2026-08-28 run selected all 40 artist-directory files:

| Coverage | Result |
|---|---:|
| PSID files | 35 |
| RSID files | 5 |
| Full-length PSID subtunes | 382 |
| Songlengths-resolved subtunes | 382 |
| Play calls | 799,598 |
| Derived notes | 111,472 |
| Trace-derived patches | 1,284 |
| Trace failures | 0 |
| Inexact-timing traces | 47 |

The five RSID files account for another 41 subtunes. They receive metadata and
typed coverage accounting only because the project deliberately has no RSID
host. Native validation is capped at 1,500 calls per subtune even when the
musical trace runs for the full HVSC duration.

The 47 inexact traces occur in seven files. They include CIA-marked tunes with
no init-time timer period, play-time CIA reprogramming, and MicroProse Soccer
V1's `play_address == 0` interrupt schedule, whose IRQ source and raster timing
the PSID host cannot reconstruct exactly. These traces remain useful for trait
prevalence, but they cannot qualify as exact native evidence.

## Common musical vocabulary

The denominator below is the 35 trace-analyzed PSID files. Counts are file
presence, so categories are non-exclusive.

| Trait | Files | Reading |
|---|---:|---|
| Pulse waveform | 35 | The universal oscillator foundation. |
| Portamento | 33 | Continuous pitch movement is nearly universal. |
| Lead role | 33 | Most files contain a distinct lead-like voice. |
| Bass role | 33 | A dedicated low-register function is equally persistent. |
| Sawtooth waveform | 30 | A much stronger catalog-wide color than in Hubbard's corpus. |
| Arpeggio | 30 | Rapid pitch cycling supplies harmony and timbral motion. |
| Stab role | 29 | Short chordal or accent gestures recur widely. |
| Percussive role | 29 | Transient programming is present in most files. |
| Vibrato | 28 | Sustained pitches commonly carry shaped motion. |
| Pad role | 28 | Long supporting layers are unusually prevalent for three voices. |
| PWM | 27 | Pulse-width motion remains a defining texture. |
| Sound-effect role | 25 | Musical and effect-like gestures often share the score. |
| Tremolo | 21 | Amplitude motion is a conspicuous Galway characteristic. |
| Triangle waveform | 20 | Common, but less dominant than pulse and sawtooth. |
| Noise waveform | 18 | Used for drums and transient color in about half the PSID files. |
| Ring modulation | 13 | Live cross-oscillator relationships are important but selective. |
| Hard sync | 10 | Another selective shared-oscillator technique. |
| Filter sweep | 10 | Long filter motion appears in a substantial minority. |

The common denominator is therefore a compact three-voice grammar built from
pulse and sawtooth, portamento and fast arpeggios, pervasive PWM, shaped
vibrato, and unusually frequent tremolo. Lead and bass roles appear in 33 files
each, while pad, stab, and percussion roles occur in at least 28. Galway's
catalog is not defined by one patch; it is defined by coordinated motion across
pitch, pulse width, amplitude, filter, and the three oscillator relationships.

The native path independently recovers complete instrument definitions and
authored effect blocks in six files and 19 accepted subtunes. That is useful
ground truth, but too small a sample to replace the trace-level catalog result.

## Ranked representation work

The census ranks exact-representation targets by affected files and then by
observed duration:

| Rank | Target | Files | Subtunes | Representative full-length window |
|---:|---|---:|---:|---|
| 1 | Vibrato shape and delay | 28 | 94 | Street Hawk, subtune 10, voice 1, 0–8 s |
| 2 | Shared ring modulation | 13 | 59 | Parallax, subtune 1, voices 2–3, about 558–684 s |
| 3 | Shared hard sync | 10 | 39 | Roland's Ratrace, subtune 1, voice 2, 8–22 s |
| 4 | Dynamic shared filter topology | 7 | 17 | Athena, subtune 1, first transition around 41 s |
| 5 | Combined-waveform fidelity | 6 | 18 | Yie Ar Kung Fu II, subtune 1, voice 3, about 0–63 s |
| 6 | `$D418` PCM detector | 0 | 0 | Ocean Loader 1 is a named negative control; a cycle-stamped packed-nibble fixture covers the Galway cadence |

This produces concrete implementation priorities:

- Preserve vibrato onset delay, rate, depth, and contour instead of replacing
  all pitch motion with one generic LFO.
- Model ring modulation and hard sync as shared live oscillator relationships.
  Independent note-local flags cannot preserve the source oscillator.
- Model one chip-global filter with dynamic voice routing and mode changes.
- Keep a measured triangle-plus-pulse fallback until model-specific audio
  fixtures qualify a better combined-waveform lowering. Only six files use it,
  but those spans total 132,025 frames and are often sustained.
- Keep sampler export provisional until named digi fixtures validate detection,
  reconstruction, bundle persistence, and rendering together.

The full-length run matters. A 1,500-call opening-window census found vibrato
in 24 files, dynamic filter topology in three, and combined waveforms in five;
full-song coverage raises those counts to 28, seven, and six. Parallax's longest
ring-modulation span begins around 558 seconds, far beyond the opening window.

Street Hawk is the broadest compact fixture family: its subtunes cover vibrato,
ring modulation, hard sync, filter transitions, and combined waveforms. Ocean
Loader 1 remains the cleaner native-exact control for portamento, PWM, tremolo,
and vibrato. Yie Ar Kung Fu II supplies the clearest long triangle-plus-pulse
window.

## Contrast with Rob Hubbard

The two full-length censuses use the same analyzer and therefore support a
controlled detector-level comparison. File counts are normalized by the 35
Galway and 78 Hubbard trace-analyzed PSID files.

| Trait | Galway | Hubbard | Reading |
|---|---:|---:|---|
| Pulse waveform | 35/35 | 78/78 | Universal in both catalogs. |
| Sawtooth waveform | 30/35 | 17/78 | Far more prevalent for Galway. |
| Triangle waveform | 20/35 | 73/78 | Far more prevalent for Hubbard. |
| Portamento | 33/35 | 76/78 | A shared near-universal device. |
| Arpeggio | 30/35 | 77/78 | Shared, but closer to universal for Hubbard. |
| Vibrato | 28/35 | 74/78 | Central to both catalogs. |
| PWM | 27/35 | 71/78 | Central to both catalogs. |
| Tremolo | 21/35 | 2/78 | The sharpest measured Galway/Hubbard contrast. |
| Ring modulation | 13/35 | 51/78 | More pervasive in Hubbard's catalog. |
| Hard sync | 10/35 | 46/78 | Also more pervasive for Hubbard. |
| Bass role | 33/35 | 64/78 | More consistently present for Galway. |
| Pad role | 28/35 | 39/78 | Galway more often spends a voice on sustained support. |

Both composers rely on pulse, pitch movement, arpeggiation, vibrato, PWM, and a
lead/bass/percussion division. The largest difference is how that shared SID
vocabulary is weighted: Galway favors sawtooth, tremolo, and sustained pad
roles, while Hubbard favors triangle plus more pervasive ring modulation and
hard sync. These are corpus tendencies, not rules for identifying authorship.

## Ranked extraction work

The native path attempted all 382 PSID subtunes. It accepted 45 subtunes,
recovered structure for all 45, accepted at least one subtune in nine of 35
PSID files, and accepted every attempted subtune in three files. Precision and
recall among accepted subtunes have medians of 0.990 and 1.0 and means of 0.970
and 0.988.

The largest gaps are:

| Rank | Typed gap | Files | Subtunes | Next action |
|---:|---|---:|---:|---|
| 1 | `locate_failed` | 22 | 227 | Cluster post-init code signatures and implement the highest-impact engine or layout family behind typed rejection. |
| 2 | `decode_empty` | 5 | 67 | Group failures within already located layouts and implement the highest-impact grammar variant. |
| 3 | `rsid_host` | 5 | 41 | Keep separate until Kernal, BASIC, CIA, and interrupt requirements are explicitly scoped. |
| 4 | `timing_inexact` | 5 | 31 | Recover a file-derived CIA or interrupt schedule where possible. |
| 5 | `decode_unreliable` | 5 | 12 | Correct the decoded grammar or alignment while retaining the existing validation threshold. |

The accepted set spans Comic Bakery (subtunes 1, 3, and 4), Helikopter Jagd
(1–7), Hyper Sports (21, 22, 25–27, 30, 31, and 35), The Neverending Story
(1), Ocean Loader 1 (1), Rambo: First Blood Part II (1, 3–6, 8–10, 20, and 21),
Street Hawk Prototype (1), Street Hawk (1–4, 6, 7, 10, and 11), and Yie Ar Kung
Fu II (1 and 3–7). The immediate opportunity is not a looser acceptance gate.
Locator failure remains the largest single gap, so the next extractor work
should first identify coherent engine or layout families inside that cluster.

The first conservative cluster follow-up adds Street Hawk (subtunes 1–4, 6, 7,
10, and 11) and Yie Ar Kung Fu II (1 and 3–7). Their 14 accepted subtunes all
carry recovered structure; the remaining 19 family subtunes stay typed
`decode_empty` or `decode_unreliable` rejections. The implementation recovers
the legacy `$C0` sequencer cells from strict code shapes and uses the live 6502
player for procedural command flow, so the production validation threshold is
unchanged. The full-corpus comparison preserved all 31 earlier acceptances.

Rambo subtune 1 is the bounded `decode_empty` recovery from this improvement
pass. Its embedded player uses `$C0` as the command threshold, masks dispatch
with `$3F`, folds `$60..$BF` note rows, and encodes inline repeats as
`DA <count> ... DC`. The file retains multiple complete player copies, so the
locator groups strict three-voice candidates, deduplicates byte-identical code
copies, and selects the unique relocation nearest the initialized stream
pointers. The pattern decoder supplies 188 authored notes in the 1,500-call
qualification window. Voice 1 is generated by an explicitly classified native
call and has no pattern notes, so its 136 trace notes are added only after the
initial native validation fails; those notes carry `trace_corrected`
provenance, and the combined 324-note result must pass the unchanged gate. The
older Comic-based Rambo candidates remain preferred whenever the new layout
does not pass its deterministic code check. Full-corpus comparison found no
previously accepted status loss.

### `locate_failed` code families

`sid-re cluster` now consumes the census rows directly, initializes each
selected file, follows documented 6502 control flow from the resolved play/IRQ
entry, and compares relocation-neutral four-instruction shingles. Immediate
values and SID-register operands remain significant; relocated code/data
addresses do not. Complete-link clustering prevents a weak similarity chain
from joining a family whose members do not all meet the threshold.

```bash
cargo run -p sid-analyzer --bin sid-re -- cluster \
  --census /tmp/galway-author-full.json \
  --root /path/to/C64Music/MUSICIANS/G/Galway_Martin
```

Before the legacy `$C0` implementation, the conservative default threshold
(`0.70`) accounted for all 24 files and 260 failed subtunes with no fingerprint
failures. The Street Hawk/Yie II row was selected as the largest bounded family
and now locates; the remaining locator gap is 22 files and 227 subtunes:

| Files | Subtunes | Pair similarity | Members |
|---:|---:|---:|---|
| 2 | 31 | 0.732 | MicroProse Soccer V1; MicroProse Soccer indoor |
| 2 | 24 | 0.898 | Hunchback II; Kong Strikes Back |
| 1 | 24 | — | Insects in Space |
| 1 | 21 | — | Green Beret |
| 4 | 20 | 0.711–0.901 | Highlander; Miami Vice; Mikie; Parallax |
| 1 | 20 | — | Roland's Ratrace |
| 1 | 19 | — | Ping Pong |
| 1 | 19 | — | Yie Ar Kung Fu |
| 2 | 15 | 0.707 | Short Circuit; Terra Cresta |
| 1 | 9 | — | Athena |
| 1 | 8 | — | MicroProse Soccer outdoor |
| 1 | 7 | — | Match Day |
| 1 | 6 | — | Rastan |
| 1 | 2 | — | Swag |
| 1 | 1 | — | Commando High-Score |
| 1 | 1 | — | Ocean Loader 2 |

At the relaxed `0.45` lineage threshold, the four-file Highlander family joined
Street Hawk/Yie Ar Kung Fu II in the previous baseline, and all three MicroProse
Soccer variants joined. The conservative families remain better extractor
boundaries. After the selected Street Hawk/Yie II recovery, the strongest
shared-engine candidate is Hunchback II/Kong Strikes Back (`0.898`), while the
largest remaining conservative pair by subtunes is MicroProse Soccer V1/indoor.

## Limits

- The study follows the HVSC #84 artist directory and its attribution choices;
  it is not an independent discography.
- Heuristic effects can overlap and are evidence of register behavior, not a
  claim about compositional intent.
- File prevalence does not measure perceptual importance or audible error.
- RSID remains unexecuted, so claims about common musical traits apply to the
  35 PSID files only.
- Forty-seven PSID traces have explicitly inexact call timing. They contribute
  trait evidence but cannot qualify native timing or exact source seconds.
- The earlier `$D418` result for The Neverending Story was four identical
  master-volume writes, not PCM. The detector now requires varying low-nibble
  values and keeps the full Ocean Loader 1 trace as a named negative control.
  A synthetic cycle-stamped fixture covers Galway's later packed-nibble cadence;
  the measured artist snapshot contains no detected `$D418` PCM.
- The census identifies source windows and target pressure. It cannot declare a
  Pertylizer representation correct without rendered A/B evidence.
