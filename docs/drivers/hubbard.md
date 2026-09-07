# Rob Hubbard playroutine — reverse-engineering notes

> **Status: driver reference.** Remaining variant failures are prioritized only
> from the corpus-ranked follow-ups in
> [`PLAN.md`](../../plans/PLAN.md#4-corpus-ranked-follow-ups).

Reference map for the **shipped** native extractor
(`crates/analyzer/src/export/native/hubbard.rs`, `--format synth-native`). Current
coverage is measured by `sid-hubbard-qualify`; historical HVSC signature counts
below describe locator potential, not native acceptance. Two parts:

1. **Note / pattern / sequence format** — the stepping loop, address-discovery
   recipe, and the Jeroen Tel relocation family (below).
2. **Instrument + effect decode** — recovering the driver's own instrument table
   and per-frame effect bytes (`## Instrument + effect decode`, at the end).

## Current trust boundary (2026-08-28)

Native export is strict. It resolves one rational `PlaybackTiming` value and
uses it for init-visible `$02A6`, play scheduling, trace analysis, SID-frequency
conversion, effect rates, and project time. CIA-driven subtunes are rejected
until the emulator can recover their timer period; a failed native request never
silently invokes `--format synth`.

The locator builds candidates around a common pattern-pointer anchor. Pattern
fetch, duration mask, sequence pointers, frequency reads, and SID writes must be
within the same relocated routine window. Bounded instruction reads cannot wrap
from `$FFFF` into zero page. Equally strong candidates produce a typed ambiguous
locator error, and the selected layout retains all core evidence addresses.

One typed order/pattern walk produces both notes and placements. Repeat ordinals,
transpose, order offsets, row ticks, pattern-byte offsets, holds, rests,
instrument changes, and slide bytes are retained in `RecoveredStructure` in the
census. Render placements are a separate derived view and are checked against
that source structure before export.

Native notes are aligned monotonically and one-to-one against trace notes per SID
voice. Acceptance requires both precision and recall of at least 0.75, with
per-pair limits of 8 calls onset, 75 cents pitch, and 64 calls duration. A bounded
global phase fit of ±8 calls is recorded in the report. Field-level provenance
distinguishes verified/decoded/partial authored data from trace measurements;
the Hubbard vibrato rate remains `authored_partial` because its high-nibble
semantics are not proven.

Run the deterministic qualification workflow with:

```bash
cargo run -p sid-analyzer --bin sid-hubbard-qualify -- \
  --assets assets/music --frames 1500 --all-subtunes \
  --output /tmp/hubbard-qualification.json
```

The checked fixture currently produces 120 outcomes: 35 accepted and 85
rejected. All 13 AWM subtunes pass. Ten of the 11 start-song fixtures pass:
AWM, Commando, Ikari Union, Knucklebusters, Last V8, Monty on the Run, Nemesis,
Shape Music 2, Sigma Seven, and Warhawk. Nemesis start-song precision/recall is
0.793/1.0 after locating its embedded 7-bit orderlist transpose. Human Race is
the only rejected start-song fixture because its timing remains inexact. Each
accepted entry also records deterministic project and census MD5 hashes.

The separate HVSC-wide production-path report uses the same typed schema as
the other driver-family qualifiers:

```bash
cargo run --release -p sid-analyzer --bin sid-hubbard-corpus-qualify -- \
  --corpus /path/to/C64Music --frames 400 \
  --output /tmp/hubbard-corpus-qualification.json
```

On HVSC #84 it accepts and structures 207 of 289 identified tunes. The typed
remainder is 37 native-validation rejections, 13 locator failures, five
locator ambiguities, ten decode failures, two empty decodes, ten init-emulation
failures, and five inexact-timing rejections. This corpus report measures
coverage; `sid-hubbard-qualify` remains the deterministic checked-fixture and
project/census hash report.

The complementary [`Hubbard corpus census`](../hubbard-corpus-census.md) scans
Hubbard's actual HVSC #84 artist directory rather than every tune sharing his
driver signature. Its full-length run covers all 488 PSID subtunes in 78 files,
including the alternate RobTracker, SidTracker64, and Companion players found
in that directory. It ranks musical representation pressure separately from
typed native-extraction failures. This distinction matters: the 289-file
`Rob_Hubbard` driver family below includes music by other authors, while the
96-file artist directory includes 13 PSID files built with other drivers.

Native timing resolution now carries an init-programmed CIA timer period into
the extractor and validation report. Human Race remains timing-inexact for a
more specific reason: its init does not program the host's CIA timer, so there
is no file-derived period the PSID host can adopt.

Relocated players also differ in gate continuation. Some reserve continuation
for bit-7 note rows; others consume every note row following a sustained status
without generating another gate edge. The note overlay remains gate-edge based;
the per-frame trace retains intervening pitch motion. In the alternate 5-bit
duration dialect, a duration-zero row is also an implicit continuation: the play
loop reloads the next row before reaching its gate-off branch, so there is no
intervening frame in which the gate can close. The extractor decodes both bounded
interpretations from the same source structure, preserves the established one
whenever it passes, and admits the alternate only through the unchanged
trace-alignment gate. In Hubbard's artist
directory this raises full-length native structure from 161 to 179 subtunes;
the whole-driver start-song qualification rises from 203 to 207.

The 2026-08-28 `decode_unreliable` follow-up began by clustering the pre-change
206 rejected subtunes in 28 Hubbard artist files into 22 signature groups. The
largest signature group
contained 53 subtunes across Geoff Capes Strongman Challenge, Gerry the Germ,
Monty on the Run, and Rasputin. Disassembly of Monty's stepping loop exposed the
duration-zero rule above; subtune 2 moved from 21.7% precision / 94.7% recall to
1.0 / 1.0 and strict native acceptance. A full 84-subtune run across those four
files moved from 15 to 16 accepted outcomes. The same bounded rule also passes
the unchanged gate for Dragon's Lair Part II subtune 7, Star Paws subtune 3,
and Warhawk subtunes 4 and 5. That leaves 201 `decode_unreliable` subtunes in 26
files. The remaining members are not one shared byte grammar: most rejected
late subtunes initialize sound-effect or dormant orderlists (fixed native walks
of 38/115/454 notes against traces with only a few notes), while Rasputin's late
entries leave zero order pointers. They stay rejected rather than being
admitted by a relaxed validation policy.

**Method.** `sid-playerid` identifies the driver by code signature (the
`Rob_Hubbard` entry in `assets/sidid.cfg`). The structures below come from
disassembling tunes from `assets/music/` with a small 6502 disassembler:
**Commando** (1985), **Nemesis the Warlock** (1987), **Knucklebusters** in
detail, plus a signature-cluster scan across all 7 PSID Hubbard tunes in
`assets/`. The SID-register operands cited are baked-in absolute addresses and
are reliable; some instruction *addresses* in the working notes drifted and
were re-derived.

## The stepping loop is present in all 7 assets/ Hubbard tunes

A whole-module scan for the loop signature — `AND #$1F` (note mask) within a
small window of `BIT abs`/`BVS` (command test), `INC abs,X` (sequence advance),
`ASL`/`TAY` (frequency-table index ×2), and `LDA (zp),Y` (pattern fetch) —
finds the cluster, with **all four secondary anchors present**, in every tune:

| Tune                  | play  | loop body | all anchors |
|-----------------------|-------|-----------|-------------|
| Commando              | $5012 | $50CA     | yes         |
| Nemesis the Warlock   | $F190 | $E0E6     | yes         |
| Knucklebusters        | $1ED4 | $050D     | yes         |
| Warhawk               | $1012 | $10DA     | yes         |
| Sigma Seven           | $8013 | $80E2     | yes         |
| Auf Wiedersehen Monty | $E40F | $E4F7     | yes         |
| Human Race            | $0986 | $0A33     | yes         |

The loop body is frequently far from the `play` address (Nemesis: play $F190 →
body $E0E6; Knucklebusters: play $1ED4 → body $050D), because `play` points at a
dispatch stub. The signature scan finds the body regardless — exactly what an
extractor's address-discovery step needs. **7/7 with the identical anchor set is
strong evidence the stepping loop is a stable, shared routine across the
corpus**, not just the three studied by hand.

### HVSC-wide confirmation: ~99 % of tunes carry the loop

The same signature scan over **all 289 `Rob_Hubbard` tunes in HVSC #84**
(257 PSID, 32 RSID):

| Result                                  | Count   | % of 289   |
|-----------------------------------------|---------|------------|
| Full signature (4/4 anchors)            | 261     | 90.3 %     |
| Partial (1–3 anchors)                   | 25      | 8.7 %      |
| No signature at all (`AND #$1F` absent) | 3       | 1.0 %      |
| **Loop present (full or partial)**      | **286** | **99.0 %** |

**Reading the partials.** The scan is deliberately exact (it wants all four
specific opcodes in one window), so a "partial" usually means a variant tweak,
not a different driver:

- The most common partial pattern is `(BIT/BVS=no, INC=yes, ASL/TAY=yes,
  LDA(zp),Y=yes)` — score 3, the command-test encoded slightly differently. This
  covers many of Hubbard's own later tunes (After 8, Mr Meaner, Off the Cuff,
  Pygmies Revenge, Chicken Song, …), several of them RSID.
- A cluster of **Giulio Zicchi** tunes (Zone Z, Sweep, Electric, Armourdillo, …)
  scores 2 — consistent with `sidid.cfg` listing Zicchi as a *near-Hubbard*
  sub-variant: shares the core, diverges in pattern fetch. These are arguably a
  related driver, not pure Hubbard.
- Only **3** tunes lack the `AND #$1F` note-mask entirely (Budokan, PuPuPuPulsar,
  Megamania 64) — likely mis-attributions or heavily reworked remixes.

So the strong claim is: **99 % carry the stepping loop**, and ~90 % match the
*exact* four-anchor Commando-class signature. The remaining ~9 % are variant
encodings (often RSID, out of current scope) plus the Zicchi sub-family.

This supports the earlier hunch: a *parameterized* extractor keyed on the shared
loop is a sound bet for the bulk of the PSID Hubbard set, with a known tail of
variants to handle (or skip) explicitly.

## Corpus

Signature scan: **289 `Rob_Hubbard` tunes in HVSC #84**, 8 in `assets/music/`.
Header addresses (PSID, all PAL, all vblank, MOS6581):

| Tune                  | load   | init  | play  | subtunes |
|-----------------------|--------|-------|-------|----------|
| Commando              | $5000  | $5FB2 | $5012 | 19       |
| Nemesis the Warlock   | $E000  | $F160 | $F190 | 15       |
| Warhawk               | $1000  | $1F53 | $1012 | 18       |
| Knucklebusters        | $0400  | $1EC0 | $1ED4 | 11       |
| Sigma Seven           | $8000  | $800D | $8013 | 1        |
| Human Race            | $0980  | $0980 | $0986 | —        |
| Auf Wiedersehen Monty | $E000  | $E8E2 | $E40F | 13       |
| Last V8               | (RSID) | —     | —     | —        |

## The headline finding: it is a *family*, not one driver

Unlike GoatTracker (one open-source codebase), `Rob_Hubbard` is a **family of
hand-written, hand-relocated variants**. The signature matches the recognizable
core, but the layout around it differs per tune:

- **Commando** — the jump table sits *at* the load address; `play` ($5012)
  points just past it. The play routine writes SID registers **directly**
  (`STA $D400,Y`, `STA $D404,Y`, …) while stepping pattern data.
- **Nemesis** — padding bytes, then the jump table at $E009. `play` ($F190) is a
  thin **dispatch stub** keyed on a subtune/state byte ($F15F) that `JMP`s to
  the real play body at **$E0C6**. The most elaborate of the three (the
  digi-heavy tune).
- **Knucklebusters** — loads very low ($0400); `play` ($1ED4) is again a
  dispatch stub: it compares a counter ($1EBF) and either `JMP $0413` (a setup
  path that does `LDA #$1F / STA $D418` — master-volume init) or `JMP $1A43`
  (the main body), indexing a small order byte table at $1EF0.

So a native extractor cannot assume fixed offsets. It must **locate** the tables
dynamically per tune (the play address often points at a dispatch stub, not the
stepping loop).

### But the stepping loop itself is the *same routine*, relocated

The decisive finding: Nemesis's real play body ($E0C6) is **instruction-for-
instruction the same pattern-stepping loop as Commando's**, only at different
addresses. Side by side:

```
Commando ($C0BA…)              Nemesis ($E0D6…)
  LDY $54EF,X   ; seqpos         LDY $E4DB,X   ; seqpos
  LDA ($5F),Y   ; pattern        LDA ($E2),Y   ; pattern (ZP ptr $E2/$E3)
  STA $54F5,X   ; latch          STA $E4E1,X   ; latch
  AND #$1F      ; mask note      AND #$1F      ; mask note
  STA $54F2,X                    STA $E4DE,X
  BIT $5502 / BVS                BIT $E506 / BVS
  INC $54EF,X   ; advance        INC $E4DB,X   ; advance
  …ASL / TAY → freq table        …ASL / TAY → freq table ($E412,Y)
```

Same opcodes, same order, same `AND #$1F` note mask, same `BIT`/`BVS` command
test, same `INC seqpos,X` advance, same `ASL`/`TAY` → 16-bit frequency-table
lookup. Only the data addresses (the field-split per-voice arrays, the ZP
pattern pointer, the frequency table) differ. The variants are **relocations /
mild edits of one source**, not independent rewrites — which makes a
*parameterized* extractor (locate the addresses, run one decoder) much more
realistic than "one extractor per tune."

## Common skeleton (from Commando, the cleanest variant)

Despite the variation, the conceptual structure is consistent — and strikingly
close to GoatTracker's (which postdates it by ~15 years):

**Jump table** at the driver base — Hubbard's signature shape:

```
$5000: JMP $5F0C    ; entry 0   (init/reset helpers)
$5003: JMP $5F42    ; entry 1
$5006: JMP $5F48    ; entry 2
$5009: JMP $5F4E    ; entry 3
$500C: JMP $53CF    ; entry 4
$500F: JMP $5F56    ; entry 5
$5012: <play>       ; play address points here, mid-block
```

**Per-voice state as field-split parallel arrays, X-indexed** (X = voice 0/1/2):

```
$54EC,X   sequence position (orderlist index)
$54EF,X   pattern position  (Y-offset into pattern data)
$54F2,X   note/parameter latch        $54F5,X
$54FB,X   $54FE,X   $5520,X   $551A,X  $551D,X   ...
```

One array per field, all `,X`-indexed — the same memory idiom as GoatTracker's
field-split instruments. Cheap on the 6502.

**Zero-page pattern pointer:**

```
$5F/$60 = pointer to current pattern data;  read via  LDA ($5F),Y
```

**Pattern pointer tables (split lo/hi), ~45 entries:**

```
$5711,Y → lo byte      $573E,Y → hi byte
```

`play` loads `$5F/$60` from these per pattern — directly analogous to
GoatTracker's `mt_patttbllo/hi`.

**Control-byte stream** (orderlist / sequence):

```
CMP #$FF  → end of sequence (loop / restart)
CMP #$FE  → command escape (JSR into a jump-table entry, e.g. $5003)
AND #$1F  → mask note/parameter out of a packed status byte
```

**Note→frequency table** (16-bit entries, Y doubled):

```
LDA $5428,Y / $5429,Y → V1FreqLo / V1FreqHi
```

Equivalent to GoatTracker's `mt_freqtbl{lo,hi}`.

## Implications for a native Hubbard extractor

**Feasible, but harder than GoatTracker, and per-variant.**

1. **Dynamic location, not fixed offsets.** The reliable anchors are the same as
   the GoatTracker plan: find a lo/hi pointer-table pair whose combined
   addresses point at `$FF`-terminated streams (patterns / sequences), and the
   16-bit frequency table. The locator heuristic is partly reusable across
   drivers — a shared `native` building block.
2. **Self-modifying code.** Variants write into their own code/data
   (`STA $5525`, `STA mt_initsongnum+1`-style), so static analysis alone is
   fragile. Emulating `init` first (we already do) and reading the **post-init
   RAM image** is the right substrate; a read-trap pass during `play` (the
   backlog idea) would pin the actual base addresses the player indexes.
3. **Packed/embedded commands.** The `$FE` escape calls jump-table routines, so
   pattern decoding is not GoatTracker's clean 4-byte rows — command semantics
   must be recovered per variant.
4. **Raster/digi harness (Nemesis-class).** Some tunes wrap play in raster
   timing and ZP-shadow→SID bulk copies; the extractor must see through the stub
   (`JMP`) to the real play body, and the ZP-shadow indirection means the
   register-write trace is the more robust ground truth there.

**Confidence.** The skeleton (jump table, field-split per-voice state, pattern
pointer tables, frequency LUT, `$FF`/`$FE` control bytes) is **high**, and the
cross-tune finding raises confidence in a *parameterized* decoder: the stepping
loop is shared, so the hard part is **address discovery**, not writing several
decoders. A single-variant proof against a committed fixture (Commando is the
cleanest) is clearly realistic; generalizing across the Hubbard corpus is now
**supported by measurement** — the loop is present in 99 % of the 289 HVSC tunes,
with ~90 % matching the exact four-anchor signature (see the HVSC-wide scan
above). The ~9 % tail (variant command encodings, the Zicchi sub-family, RSID)
is a known, bounded set rather than an open question.

## Address-discovery recipe (validated on assets/)

Concrete enough to build against:

1. Confirm the driver via `sid-playerid` (`Rob_Hubbard`).
2. Emulate `init` for the subtune; work on the post-init RAM image.
3. **Find the stepping loop** by scanning for the signature cluster (`AND #$1F`
   near `BIT`/`BVS`, `INC abs,X`, `ASL`/`TAY`, `LDA (zp),Y`). 7/7 in `assets/`.
4. From the loop, read off the data addresses by operand:
    - the `LDA (zp),Y` zero-page operand → the **pattern pointer** ($5D/$5F-style);
    - the `INC abs,X` operand → the **sequence-position array** base;
    - the `ASL`/`TAY` then `LDA abs,Y` → the **frequency table** base;
    - nearby `STA abs,X` operands → the field-split per-voice arrays.
5. Walk the sequence/pattern streams (`$FF` end, `$FE` command escape, `AND
   #$1F` note mask) to recover notes, honoring the frequency table for pitch.

## Implemented locator (validated on all 11 `assets/` Hubbard tunes)

`export::native::hubbard::locate` recovers the layout from the post-init RAM
image with no hardcoded addresses, and succeeds on **11/11** Rob_Hubbard tunes in
`assets/music/`. Two of those load high at
`$E000` (RAM under the KERNAL), so the scan range is `$0200..$FFFF`. Two
independent consistency checks hold across all eight: the `AND #$1F` note mask
sits **exactly 8 bytes** after the pattern fetch (`pat_read + 8`), and for
Nemesis the recovered `freq_table = $E412` matches the hand-disassembled value
in the variant diagram above.

Exact byte signatures, as confirmed against the Commando disassembly (load base
`$5000`):

- **Pattern pointer load** — `B9 ll hh / 85 zz / B9 mm nn / 85 zz+1`
  (`LDA lo,Y / STA zp / LDA hi,Y / STA zp+1`). Note the operand offsets: the
  second `STA`'s zero-page operand is at **+9**, not +7 (+7/+8 are the second
  `LDA`'s high byte and the `STA` opcode). Candidate selection requires the
  pattern fetch, mask, sequence pointers, frequency reads, and SID write to form
  one locally coherent routine. In Commando this is
  `$50AB` → `pat_ptr_lo=$5711`, `pat_ptr_hi=$573E`, `zp_ptr=$5F`.
- **Pattern fetch** — first `B1 zz` (`LDA (zp),Y`) with `zz == zp_ptr`. Commando:
  `$50C2`.
- **Note mask** — the first `29 1F` (`AND #$1F`) within 32 bytes *after* the
  pattern fetch (the mask always follows the fetch). Commando: `$50CA`.
- **Frequency table** — the play loop writes voice frequency with **indexed**
  stores (`99 00 D4` / `99 01 D4` = `STA $D400,Y` / `STA $D401,Y`), not absolute
  `8D ..`, and reads the 16-bit table as two interleaved `abs,Y` loads
  (`B9 ll hh` … within 12 bytes … `B9 ll+1 hh`). The real base is read once per
  voice, so the locator counts every such pair and picks the **mode** —
  coincidental consecutive reads each appear once. Commando: `$5428` (5 votes;
  three coincidental bases got 1 each). A `$D400`/`$D401` store must also be
  present in the scan range as a sanity gate.

## The Jeroen Tel relocation (Ikari Union and kin)

A sizeable slice of the 289 `Rob_Hubbard`-identified tunes are not by Hubbard at
all — they are Jeroen Tel relocations of the same routine that the player-ID
matches on. They share the skeleton but diverge on **four** axes, all
auto-detected by `locate` (RE'd against `Ikari_Union.sid`, play `$1003`, RAM
dump `/tmp/lf_Ikari_Union.bin`):

1. **6-bit duration mask** — the status byte is masked with `AND #$3F` (`$10CC`),
   not `#$1F`. Already read from the `AND` immediate into `HubbardLayout::dur_mask`.
2. **Interleaved pattern-pointer table** — the play loop does `ASL A / TAY`
   (`0A A8` at `$10A8`) before `LDA $1673,Y / STA $FC / LDA $1674,Y / STA $FD`, so
   the lo/hi bytes are *one* `lo hi lo hi …` table indexed by `pattern * 2` (the
   two operands are adjacent). Detected by the `0A A8` prefix plus
   `pat_ptr_hi == pat_ptr_lo + 1` → `pat_stride = 2`.
3. **One-byte embedded orderlist transpose** — a bit-7 orderlist byte carries the
   transpose in its *own* low bits (`AND #$80` then `AND #$1F` / `STA $191B,X`
   at `$1093`), where Auf Wiedersehen Monty uses a `BPL` test and a *separate*
   value byte. Detected by the `29 80 … 29 1F 9D` shape → `embedded_transpose_mask
   = Some($1F)`. The Magnar / Shape Music player does the same with a 7-bit mask
   (a `CMP #$FE / BEQ / AND #$7F / STA $E0E1,X` orderlist-command dispatcher, the
   `C9 FE … 29 7F 9D` shape → `Some($7F)`); reading either as Monty's 2-byte form
   consumes a pattern byte and offsets every note by a per-voice constant.
4. **Separate lo/hi frequency tables** — `LDA $14E3,Y / … / LDA $1543,Y` with the
   raw note index `Y` (stride 1), the two bases differing by the table length
   (`$60`) rather than 1. Detected by the `B9 / 8D / B9 / AC / 99 01 D4` write
   shape with `hi_base != lo_base + 1` → `freq_hi = Some($1543)`.

`Ikari_Union.sid` (added to `assets/music/` as the fixture) now decodes to **1.00**
onset agreement. HVSC-wide this lifts the gate-pass rate from **148/289** to
**163/289** (and the exact-1.00 bucket from 107 to 133) with no regression on the
already-passing variants — the family generalizes far beyond the one fixture.

The brittle parts remain the **command semantics** behind the `$FE` escape
(per-variant jump-table routines) and self-modifying setup — but the spine
(notes, sequence, pitch) is recoverable from the shared loop alone.

## Relationship to existing work

- This is the "composer's own driver" end of the spectrum; GoatTracker
  (`../export.md` "song-structure decomposition", and the `native::goattracker`
  doc) is the shared-editor end. Both feed the same `--format synth-native` path
  via a `DriverExtractor`.
- The default `--format synth` (derive-from-trace) already handles these tunes;
  a native extractor adds **exact** pattern/structure recovery on top.
- The Nemesis per-frame modulation already measured (PW changes on 72 % of
  frames, freq 80 %, 9467 V2 waveform switches) is *consistent* with this
  driver: the timbre lives in the wavetable / pattern-command stream this map
  describes.

`crate::export::native`: ../../crates/analyzer/src/export/native/mod.rs

---

## Instrument + effect decode

Status: **static half wired into the export; dynamic half decoded but
deliberately not wired (the trace is more correct — see the mapping section).**
This covers the next axis of the native extractor: recovering the
**instrument definitions and their per-frame effects** from the driver's own
tables, instead of inferring timbre and effects from the emulated register trace.

**Wired (the static half):** `locate` recovers the instrument-table base
([`HubbardLayout::inst_table`], from the `STA $D405`/`STA $D406` ADSR register
writes the table feeds), `decode_instruments` decodes the five static bytes per
record into a `NativeInstrument`, `decode_song` carries a per-note authored
instrument index, and `HubbardExtractor::extract` binds each note to its authored
instrument via `timbre::extract_patches_grouped` — replacing the heuristic trace
clustering with the driver's real instrument set (one patch per authored
instrument, authored ADSR + waveform; per-voice filter/PW still trace-derived).

**Decoded but not wired (the dynamic half):** the three effect bytes (`+5/+6/+7`)
are decoded into `InstrumentEffects` (vibrato / PWM / pitch effects), but the
*sound* still comes from the emulated trace. The PWM→LFO mapping was built and
A/B'd, and the trace-baked lane won (more correct + simpler) — see below.

All addresses and byte values below are from **Commando** (load base `$5000`),
the cleanest variant, read from the post-init RAM image.

## Motivation

The `--format synth-native` path remains an explicitly documented hybrid:

- **Notes and exact source structure** come from driver tables and must pass
  bidirectional trace alignment. Render placements are a checked derived view.
- **ADSR, waveform, pulse width, vibrato, PWM, and flags** use field-level
  authored/trace provenance. Instrument evidence is currently report-only.
- **Remaining dynamic effects and per-voice detail** come from the emulated trace
  via `detect_effects` + timbre analysis — heuristic (thresholds, tolerances),
  variant-agnostic, and the source of three known weaknesses:
    1. **Diffuse character** — trace-detected modulation is baked into per-frame
       automation points / expanded sub-notes rather than authored LFO/envelope
       parameters (see the synth-export ear-test note).
    2. **Over-segmentation** — clustering trace output produced **34 ad-hoc tracks**
       for International Karate (3 SID voices), because the timbre layer invents a
       patch per cluster instead of reading the driver's fixed instrument table.
    3. **Bloat** — baked automation makes large project files.

The driver contains the *authored* instrument definitions and the per-frame
**programs** that produce vibrato / PWM / arpeggio / filter sweeps. Decoding them
natively yields the composer's exact intent and lets the exporter emit real
Pertylizer **instruments + LFOs/envelopes** instead of baked automation.

This is a bigger, per-variant RE job than the note format, and it does not escape
the Pertylizer representation ceiling (below). The trace stays as the validation
oracle, exactly as it does for notes.

## What the dump confirms: the instrument table (the static half — easy)

Commando holds an instrument table at **`$5591`**, **8 bytes per instrument**,
indexed by `instrument_index << 3`. The note's instrument index is *already*
recovered today as `PatternEvent.instrument` (read in `decode_pattern`) but
currently discarded.

```
offset  field   meaning
  +0     PWlo    pulse width low byte   ─┐ 12-bit pulse width
  +1     PWhi    pulse width high nibble ┘
  +2     ctrl    SID control register: waveform (bits 4-7), ring (bit2),
                 sync (bit1), gate (bit0)
  +3     AD      attack (hi nibble) / decay (lo nibble)
  +4     SR      sustain (hi nibble) / release (lo nibble)
  +5     p5      vibrato depth (right-shift count; 0 = off)        ─┐ effect
  +6     p6      PWM rate/step, or one-shot PW offset (if +7 bit3)  │ params —
  +7     p7      effect-enable mask (drum / chirp / arp / PW-offset)┘ see below
```

(The `+5/+6/+7` semantics below were originally a guess — "three program-table
references" — but the disassembly proved otherwise; see the dynamic-half section.)

Decoded first twelve instruments (raw bytes, then the interpreted static fields):

```
inst  raw (8 bytes)              PW     ctrl  waveform        ADSR (A D S R)   prog(p5 p6 p7)
  0   00 09 41 29 5F 02 E0 00    $0900  $41   pulse           2  9  5 15        02 E0 00
  1   80 01 41 06 4B 00 00 05    $0180  $41   pulse           0  6  4 11        00 00 05
  2   80 01 41 09 9F 00 16 08    $0180  $41   pulse           0  9  9 15        00 16 08
  3   00 02 81 0A 09 00 00 05    $0200  $81   noise           0 10  0  9        00 00 05
  4   00 02 43 0F C4 00 00 03    $0200  $43   pulse+sync      0 15 12  4        00 00 03
  5   80 08 41 05 A9 00 02 0D    $0880  $41   pulse           0  5 10  9        00 02 0D
  6   00 08 41 38 7A 02 E0 00    $0800  $41   pulse           3  8  7 10        02 E0 00
  7   80 01 15 0D FB 01 00 05    $0180  $15   triangle+ring   0 13 15 11        01 00 05
  8   00 08 41 49 5B 02 03 08    $0800  $41   pulse           4  9  5 11        02 03 08
  9   00 08 21 04 6F 03 00 05    $0800  $21   sawtooth        0  4  6 15        03 00 05
 10   00 03 41 09 6B 02 01 0D    $0300  $41   pulse           0  9  6 11        02 01 0D
 11   00 02 43 07 09 01 00 01    $0200  $43   pulse+sync      0  7  0  9        01 00 01
```

The control bytes decode to a clean, varied palette (pulse, noise, sawtooth,
triangle+ring, pulse+sync). These five static bytes map 1:1 to Pertylizer and are
a small, safe first step.

## The dynamic half: the three effect bytes (`+5/+6/+7`) — RE'd, model corrected

**The original hypothesis above (three program tables with an opcode grammar) is
wrong for Commando.** Disassembling the per-frame instrument tick
(`$51A3..$53A0`, reachable from the play loop; emulate `init`, dump the post-init
RAM, disassemble) shows there are **no program tables and no bytecode**. The three
bytes are *scalar effect parameters plus a flag mask* that drive a **fixed palette
of hardwired effects** the driver runs every frame:

- **`+5` → vibrato depth.** A right-shift count: the per-frame pitch wobble is
  `(freq[note+1] − freq[note]) >> p5`, applied by a triangle LFO folded from a
  free-running frame counter (`$5525`). `+5 == 0` disables vibrato
  (`$51BF BEQ`). Depth grows with smaller shifts.
- **`+6` → pulse-width modulation**, *unless* `+7` bit 3 is set:
    - bit 3 clear (`$524C`): continuous PWM. Low 5 bits = rate (period − 1 in
      frames, via the `$550D,X` counter), high 3 bits (`& $E0`) = the step added to
      the pulse width each period, bounced between high-nibble limits `$08`/`$0E`.
    - bit 3 set (`$5230`): the `+6` byte is instead a **one-shot pulse-width
      offset** added once to the instrument's PW at note setup.
- **`+7` → an effect-enable bitmask** (the `$5523` cell, tested bit by bit):
    - bit 0 (`$01`, `$52FA`): downward pitch sweep over the note's early frames —
      a drum/snare drop.
    - bit 1 (`$02`, `$5336`): upward pitch chirp on alternate frames.
    - bit 2 (`$04`, `$535E`): alternate between the note and an offset note (a fast
      hardware arpeggio / octave jump).
    - bit 3 (`$08`): selects the one-shot-offset interpretation of `+6` above.

So the dynamic half is decoded directly from the three bytes — no per-program
table walk, no `Hold`/`Loop`/`End` grammar. Other Hubbard relocations are
expected to share this engine (it is hand-written and relocated, like the note
format), but only Commando is RE-confirmed; validate each the same way before
trusting it.

## Rust data model (implemented)

`NativeInstrument` carries the decoded effects instead of raw program bytes:

```rust
struct NativeInstrument {
    pulse_width: PulseWidth,   // +0/+1
    control: ControlBits,      // +2 (waveform / ring / sync / gate)
    adsr: Adsr,                // +3/+4
    effects: InstrumentEffects, // decoded from +5/+6/+7
}

struct InstrumentEffects {
    vibrato_depth: Option<u8>, // +5 shift count (None when 0)
    pwm: Option<Pwm>,          // +6 continuous PWM (rate = low 5, step = high 3)
    pw_offset: Option<u8>,     // +6 one-shot offset when +7 bit 3 set
    drum_drop: bool,           // +7 bit 0
    chirp_up: bool,            // +7 bit 1
    arp: bool,                 // +7 bit 2
}

struct Pwm {
    rate: u8,
    step: u8
}
```

## Decode flow (parallel to note decoding)

1. **New anchor in `locate`** — the instrument-table base (Commando `$5591`),
   found from the play routine's `LDA table,Y` indexing of the instrument fields,
   the same technique used for the frequency table today. Add it to
   `HubbardLayout`.
2. `decode_instruments(read, layout) -> Vec<NativeInstrument>` — the five static
   bytes plus the decoded `InstrumentEffects`, directly. **Done.**
3. `decode_effects(p5, p6, p7) -> InstrumentEffects` — the hardwired-effect
   decode (replacing the abandoned `decode_program` / `ProgramStep` idea).
   **Done for Commando**; generalise per relocation as each is RE-confirmed.
4. Bind each note to its instrument via the already-recovered
   `PatternEvent.instrument` index — no clustering needed. **Done** (the static
   half binds the patch; the effects are not yet wired to the export — see below).

## Mapping to Pertylizer — A/B'd, and why the trace wins

| SID source             | Pertylizer target     | Status                                      | 
|------------------------|-----------------------|---------------------------------------------|
| ADSR (+3/+4)           | instrument envelope   | **wired** — exact authored values           |
| waveform (+2)          | oscillator waveform   | **wired** — exact                           |
| ring / sync (+2)       | ring / sync (or skip) | partial — see ceiling                       |
| pulse width (+0/+1)    | `pulse_width`         | **wired** — per-voice, trace-derived        |
| **`+5/+6/+7` effects** | LFO / automation      | **decoded, deliberately NOT wired** (below) |

The static-half payoff (real instruments, reused — ~12 authored instruments
replace the over-segmented trace clusters; the note's instrument index is the
direct binding) **landed**. The dynamic half stops at the *decode*: the effects
are decoded into `InstrumentEffects` (exact, tested) but **not mapped into the
export**, on purpose.

**The PWM → LFO mapping was built and A/B-tested, then reverted.** `+6` PWM was
wired to a real Pertylizer `lfo` module on the oscillator's `pwm` input (vibrato
can't be a pitch LFO — the oscillator has only `fm`/`pwm` CV inputs — so it would
have stayed `NoteExpression.vibrato` anyway, which the trace path already emits).
The A/B (Commando, via an `lfo`-vs-baked-lane comparison project) was close, but
the **trace-baked `pulse_width` automation lane won, on both axes that matter**:

- **More correct.** The baked lane samples the *actual per-frame pulse width from
  the emulated trace* — it is ground truth. The LFO had to *guess* parameters (the
  `$08..$0E` bounce limits, an idealised triangle, a free-running phase), so it was
  a sterile approximation. By ear the baked lane had slightly *more variation* —
  the real chip nuances (per-note reset from different bases, bounce irregularity)
  that a perfect triangle lacks.
- **Simpler.** The baked lane is the existing default; the LFO was the *added*
  machinery (a new module + connection + a per-instrument PWM→LFO conversion). The
  only LFO win was file size (~280 KB on Commando), which does not justify a less
  correct, more complex path.

So for `+6` PWM the export keeps the trace-baked lane. The same logic applies to
`+5` vibrato (already `NoteExpression.vibrato` from the trace) and the `+7`
drum/chirp/arp pitch effects (the trace's per-frame pitch motion is the ground
truth). The decode stays as **documented reference** — the exact authored knobs,
should a future need arise (e.g. a representation the trace genuinely cannot
recover) — but the *sound* comes from the trace, which is both simpler and more
correct.

## Validation and ceiling (honest framing)

- **Validation:** the decode is unit-tested against Commando's instrument table
  (static fields + the three effect bytes). The PWM→LFO ear A/B was run and the
  trace-baked lane won (more correct + simpler), so the trace stays ground truth
  for the effect *sound* — the decode is reference, not a render source.
- **Unmappable in Pertylizer v0** (see `synth.rs` skip list): hard-sync detail,
  combined waveforms, `$D418` sample voice, fast per-frame waveform sequences.
  Perfect native extraction of these still has nowhere clean to go.
- **Scope:** the static half (5 bytes) landed and replaced the clustered patches.
  The dynamic half is **decoded** (the three effect bytes → `InstrumentEffects`);
  what remains is (a) confirming the same engine across other Hubbard relocations
  and (b) the deferred, A/B-gated Pertylizer mapping above.

### Cross-relocation status (instrument-table locate)

`locate` finds `inst_table` in **all 7** supported asset variants. Six of them —
Commando, Sigma Seven, Auf Wiedersehen Monty, Knucklebusters, Warhawk, Human
Race — use the **packed** 8-byte-record layout ([`InstrumentTable::Packed`]).
**Ikari Union (the Jeroen Tel relocation)** uses a **columnar** layout
([`InstrumentTable::Columnar`]), located by `locate_columnar_instruments` when the
packed confirm fails:

- **Columnar (struct-of-arrays), not packed records.** Commando packs 8 bytes per
  instrument and indexes `base + index*8`. Ikari stores one *field-table per
  field*, each indexed by the raw instrument number at stride 1, with the
  field-tables spaced by the instrument count (6). Decoded from `$1141..$1163`:
  PWhi-table `$1633`→`$D403`, AD-table `$163F`→`$D405`, SR-table `$1645`→`$D406`
  (so `SR_base − AD_base == 6`, the instrument count — not `+1` as in a packed
  record). Detection anchors on the AD write `LDA $163F,X / STA $D405,Y`, then
  confirms the SR (`$D406`) and PWhi (`$D403`) field-table writes nearby. PWlo is
  forced to `0` at note setup (no table).
- **Only ADSR is statically authored.** The fuller disassembly corrects the
  earlier sketch: the byte at `$162D,X` (read just before AD) is *not* the control
  byte — it is stored to a per-voice runtime cell (`$1932,X`), not `$D404`. The
  control byte / waveform are written from `$1900,X` (loaded from `$192A`), driven
  by a *separate* per-voice program pointer (`$1906,X → TAY`, the `$164B`/`$1651`
  analog) — the indexed program/wavetable the original sketch imagined, its own RE
  job. So `decode_instruments` authors **ADSR only** for the columnar layout;
  the waveform and effects stay trace-derived (the grouped patch builder takes the
  trace's first waveform when the authored one is absent).

**Status: done.** Ikari now binds authored instruments (grouped by its real
instrument index, with authored ADSR) instead of trace clustering; note timing is
unaffected (the gate keys on onset agreement, not instruments). The Jeroen-Tel
per-frame waveform/effect program remains an unstarted, separate RE job.

## Relationship to other threads

- Complements the **structure-preserving export** (pattern/placement) sketch —
  together they turn one flat trace-derived track into the driver's real
  instruments *and* its real pattern structure.
- Makes the per-instrument **§A9 coloring** redundant for the modulation it
  currently approximates, though a master-bus coloring stage may still help the
  residual chip character.

`crate::export::native::hubbard`: ../../crates/analyzer/src/export/native/hubbard.rs
