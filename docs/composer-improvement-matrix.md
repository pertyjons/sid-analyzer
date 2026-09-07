# Three-composer improvement matrix

This reference joins the reproducible HVSC #84 censuses for Rob Hubbard,
Martin Galway, and Ben Daglish. It distinguishes corpus prevalence from an
implemented representation and from rendered proof. A high corpus count is a
priority signal, not evidence that a Pertylizer candidate is exact.

## Current full-length coverage

| Composer | Selected files | Trace-analyzed PSID subtunes | Trace failures | Native structured subtunes | Explicit host boundary |
|---|---:|---:|---:|---:|---|
| Rob Hubbard | 96 | 488 | 0 | 179 | 18 RSID files |
| Martin Galway | 40 | 382 | 0 | 45 | 5 RSID files |
| Ben Daglish | 89 | 527 | 0 | 294 | none |

All trace counts use resolved full song lengths. Inexact CIA or interrupt
schedules remain marked and are excluded from exact timing gates. Native
structure is counted only when decoded notes pass the existing trace-alignment
gate and recovered placements satisfy the structure contract.

## Improvement-pass outcome

All three artist-directory censuses were rerun at resolved full song lengths
with every subtune selected. The comparison is keyed by path and subtune, so a
gain cannot hide a previously accepted regression.

| Composer | Before | Current | Net gain | Previous accepted losses |
|---|---:|---:|---:|---:|
| Rob Hubbard | 161 | 179 | +18 | 0 |
| Martin Galway | 30 | 45 | +15 | 0 |
| Ben Daglish | 271 | 294 | +23 | 0 |
| **Total** | **462** | **518** | **+56** | **0** |

Hubbard now decodes the alternate sustained-gate continuation dialect only
when the established dialect rejects. Daglish adds bounded zero-page note,
absolute pointer-pair, orderlist, and third-voice guard layouts. Galway adds
Rambo's `$C0` command threshold, `$3F` dispatch mask, folded note/duration rows,
inline repeat stack, compact handlers, multi-player candidate selection, and a
`trace_corrected` fallback restricted to an otherwise empty voice reached by a
classified native call. It also recovers the related legacy `$C0` Street
Hawk/Yie Ar Kung Fu II family by strict three-sequencer code shape and live 6502
control flow, adding 14 structured subtunes. Every path still uses the unchanged
validation gate.

## Representation status

| Domain | Corpus pressure | Implemented state | Remaining qualification gate |
|---|---|---|---|
| Vibrato shape and delay | Hubbard 49 files; Galway 28; Daglish 64 | contour-derived onset delay, depth, rate, and sine/square/saw shape; delayed contours use exact track-pitch automation | AWM rejects a wrong-shape control; AWM and Trap retain zero-delay/shape resolution controls because the current aggregate audio features cannot distinguish every contour; structural exporter tests enforce delayed versus zero-delay lowering; Galway retains the Ocean presence/absence control |
| Shared ring modulation | Hubbard 51; Galway 13; Daglish 52 | destination SID oscillator receives the physical previous voice's live MSB source and frequency automation | moving-modulator render fixture for each SID model; quantify the remaining bright 6581 response |
| Shared hard sync | Hubbard 46; Galway 10; Daglish 50 | destination SID oscillator receives the physical previous voice as sync source | full-length named hard-sync windows and deliberate disconnected-source controls |
| Waveform programs | Hubbard 41; Daglish 57; material Galway use | per-frame one-shot programs, looped periods through 16 steps, held frames, per-step noise frequency, and explicit long-window fallback | continuation/restart render fixtures; a genuine period over 16 steps remains target-dependent |
| Combined waveforms | Hubbard 22; Galway 6; Daglish 49 | full waveform mask is rendered by the model-aware SID oscillator; the 6581 pulse+saw target is pinned at the floor, while triangle+saw has independent synthetic level and gain-normalized shape gates across A1/A2/A4 | Pertylizer is unchanged; triangle+saw needs a direct-module, frequency-dependent fit because one global trim cannot close the pinned shape residual |
| Chip-global filter | Hubbard 27; Galway 7; Daglish 10 | the exporter can serialize one shared return filter for stable state and has a measured per-instrument fallback for dynamic state | Pertylizer currently cannot construct `Filter` as a return-bus effect; static construction, then dynamic routing/mode automation, need capability fixtures |
| `$D418` PCM | named Galway negative control plus cycle-stamped synthetic positive controls | packed-nibble detection is separated from invariant volume housekeeping; PCM is reconstructed to WAV | sampler bundle persistence and rendering remain target-dependent |

## Native extraction priorities

| Family | Highest-impact typed gap | Baseline | Completion criterion |
|---|---|---:|---|
| Hubbard | `decode_unreliable` | 26 files / 201 subtunes | cluster the remaining coherent layouts, add bounded grammar variants, and pass unchanged precision/recall plus structure gates |
| Daglish / Gremlin and Crowther | `decode_unreliable` | 13 files / 57 subtunes | add the smallest coherent stream/order grammar variant without accepting inserted or missing notes |
| Daglish / Gremlin and Crowther | `locate_failed` | 10 files / 113 subtunes | identify the largest remaining signature family, require unique post-init evidence, then pass unchanged native validation |
| Galway | `locate_failed` | 22 files / 227 subtunes | implement the next conservative code-similarity family, preferring Hunchback II/Kong Strikes Back for similarity or MicroProse Soccer V1/indoor for pair size |
| Galway | `decode_empty` | 5 files / 67 subtunes | recover the next bounded pattern body for an already located layout and prove note/placement agreement |

`no_extractor`, `unsupported_configuration`, and inexact-timing rows remain
typed until corpus clustering identifies a coherent implementation. RSID is a
separate host project and is not absorbed into a PSID driver decoder.

## Public render acceptance

The checked-in matrix contains eight deterministic synthetic fixtures: a tonal
baseline, a low-cutoff 6581 filter case, and A1/A2/A4 controls for both 6581
pulse+saw and triangle+saw. Every fixture has an accepted target and a rejected
near-miss for the metric it gates. The tests also build a minimal NTSC PSID in
memory to verify clock selection and vblank execution.

Composer- and game-derived render windows are local qualification inputs, not
redistributed test fixtures. They can still be regenerated against a licensed
local corpus when cross-driver audio evidence is needed, while the public CI
gate remains self-contained and synthetic.

## Sources

- [`hubbard-corpus-census.md`](hubbard-corpus-census.md)
- [`galway-corpus-census.md`](galway-corpus-census.md)
- [`daglish-corpus-census.md`](daglish-corpus-census.md)
- [`export.md`](export.md)
- [`../plans/PLAN.md`](../plans/PLAN.md)
