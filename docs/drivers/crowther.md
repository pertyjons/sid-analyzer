# Antony Crowther V3 playroutine — reverse-engineering notes

> **Status: driver reference.** Further coverage is prioritized only from the
> corpus-ranked follow-ups in [`PLAN.md`](../../plans/PLAN.md#4-corpus-ranked-follow-ups).

Status: **extractor SHIPPED for the Ark_Pandora generation** (`crowther.rs`:
`locate` + frame-synchronous `decode_song` + onset gate). Ark_Pandora decodes
at onset agreement **1.000 — 510 of 510 notes** against the emulator trace at
1500 frames, note-for-note on the first decode. The format below was read out
of the disassembly and confirmed by hand-decoding voice 0's stream bytes.

## HVSC sweep (start_song, 400f, `dbg_crowther_hvsc_sweep`)

**`total=88 located=53 locate_fail=32 emu_fail=3 timeout=0 PASS=49`** under
the production precision/recall/timing gate. History under the older onset-only
metric: Chain-only baseline `PASS=2` → +Table generation (Cobra
scaffold) `PASS=39` → +revision generalizations (early/mid table builds,
split-indexed duration counters, fetch-at-one) `PASS=50` → +ctrl gating
(silent lead-in sequencing) `PASS=52`. The split 16-bit duration fix keeps
Rolling Stoned and Way of the Tiger at 1.000 onset agreement. Remaining strict
failures:

- **Rastertime_Utopia (0.09) — known limitation, won't fix**: a demo whose
  play entry wraps the player in a 3-frame cycle (counter `$97FF`: two
  sequencer frames + one register-blast frame), so the effective tick is
  not the play rate the standard model assumes. The gate rejects it
  cleanly; `--format synth` covers it.
- **Blitzkrieg** decodes every traced onset but emits 26 extra rows
  (50.9% precision, 100% recall).
- **We M U S I C 3** remains a partial decode (75.9% precision, 43.1% recall).
- **Olli and Lissa** has 1.000 onset agreement but is rejected because its
  CIA-timed subtune does not program a timer period during init.
- **31/88 lack the fold entirely** — a later engine revision (the Apex-era
  catalogue: Creatures, Mayhem_in_Monsterland, Retrograde, Cyberdyne_Warrior,
  plus Daglish_01, Dead_Ringer, …). Separate RE effort.
- A few oddballs (Neon_Nights, Nitro, Shorty, Vikings) with neither drain
  shape.

The sweep uses a 30-second per-tune deadline and reports timeouts by name.
Earlier five-second measurements produced false timeouts on loaded machines.

### Ctrl gating (silent lead-in sequencing)

Gauntlet and Firelord author **silent intro voices**: the stream sets the
ctrl/waveform command to `$00` (gate bit clear) and sequences rows
inaudibly until a later set (`$41` etc.) turns the voice on. The decoder
tracks the per-voice ctrl byte — initial value from the located ctrl cell's
post-init state, updated by the classified `CtrlSet` command — and only
emits rows whose ctrl has the gate bit set. The ctrl cell is located by
chaining two anchors: the register-image blast (`LDA image,X / STA
$D400,X`) names the image base, and the note-on staging (`LDA ctrl,X /
STA image+4,Y`) names the cell. Gauntlet 0.75→1.000, Firelord 0.49→1.000.

### The Table revision spectrum

The Table generation is itself a spectrum of hand-evolved builds; everything
below is **detected per tune from the code**, never assumed:

- `cmd_max` varies (`$14`/`$1F`/`$20`/`$21` observed) — read from the
  drain's `CMP` immediate.
- Porta-prefix marker `$63` (early) vs `$FF` (late) — read from the
  dur-fetch's `CMP` immediate.
- Duration countdown: 8-bit fetch-at-one (`DEC/LDA/CMP #$01` — a dur byte
  of `$01` wraps to 256 frames, `$00` to 255), Cobra's 16-bit X-indexed
  `SBC` pair, Chicken_Song's mixed-index variant (lo `,X`, hi `,Y`), and
  Rolling_Stoned's split borrow (8-bit DEC + Y-indexed hi borrow with
  voice self-stop on underflow).
- Loop-end arm: `INC flag,X` (Cobra) vs `LDA #1 / STA flag,X` (Biggles).
- Additive commands (`CLC/ADC/STA`): transpose add (X-indexed) and
  duration-hi add (X- or Y-indexed, optionally mirrored into a second
  cell) — cross-confirmed against the note path's `ADC` operand and the
  located duration-hi cell respectively.

Triage tools: `dbg_crowther_tune` (per-tune layout + native-vs-trace miss
dump, `SID_DBG_TUNE`/`SID_DBG_FRAMES`) and `dbg_crowther_hvsc_sweep`
(`SID_HVSC_ROOT`), both `#[ignore]`/CI-safe.

## The two generations

The `Antony_Crowther_V3` signature spans (at least) two sequencer
generations sharing the stream grammar (2-byte records, the note encoding,
the loop mechanics, the shared octave fold):

|                  | Chain (Ark_Pandora `$A000`)             | Table (Cobra `$F900`)                                                            |
|------------------|-----------------------------------------|----------------------------------------------------------------------------------|
| Dispatch         | `CPY`-chain, hardwired numbers          | jump table (`$FF90`, word/cmd), self-mod `JSR`                                   |
| Commands         | `$01..$7E`                              | `$01..$21` (`CMP #$21` drain bound)                                              |
| Porta prefix dur | `$63`                                   | `$FF`                                                                            |
| Durations        | 8-bit × global divider (`$0E` self-mod) | **16-bit** (`$FF61`/`$FF1B`), play frames, no divider                            |
| Song end         | voice 0 reaches voice 1 (`$A1AE`)       | **explicit stop cmd** (`$1E` → `$FB88`)                                          |
| Transpose        | `$14` set                               | `$14` set **+ `$1A` add** (`CLC/ADC/STA` — key-change sections accumulate)       |
| Long rows        | —                                       | `$19` adds `value*256` to the running countdown                                  |
| Tempo extra      | —                                       | base-7 fractional accumulator (`$F96B`) drives a global timer, NOT the sequencer |
| SID writes       | staged cells per tick                   | 25-byte register image `$FFD2` blasted per frame                                 |
| Gate-off         | `$A549` hard-restart flag               | early gate-off threshold `$FF8D,X` (cmd `$05` ≥ `$64`)                           |

Table-generation command semantics are **classified from handler code**
(`classify_handler`): loop ends/starts pair via their shared pointer-save
tables, transpose/duration-add handlers cross-confirm against the note
path's `ADC` operand and the 16-bit countdown's hi cell, stop = `LDA #0 /
STA abs` at entry. Hardwired numbers are never assumed, so rebuilt/reordered
command tables across the 55-tune sub-family still decode.

Cobra layout (for reference): seq lo/hi `$FF67`/`$FF64`, freq table `$FF75`,
cmd table `$FF90`, song base zp `$60/$61`, voice remap `$FF6D = [0,7,14]`,
song header format identical (v0 at base+4, `[v1_off][v2_off]` words).

Family size (HVSC #84, `sid-playerid` + `assets/sidid.cfg`, measured
2026-06-08): **Antony_Crowther_V3 = 88 tunes HVSC-wide** (40 inside
`Daglish_Ben/`, the rest Antony Crowther / Ratt and others; the whole Crowther
V1+V2+V3 family is 111). This is Hubbard-class volume. The other Daglish engine
(Ben_Daglish/Gremlin, 60 tunes) is a separate driver with its own extractor and
qualification report.

The permanent production-path qualification command is:

```bash
cargo run --release -p sid-analyzer --bin sid-crowther-qualify -- \
  --corpus /path/to/C64Music --frames 400 \
  --output /tmp/crowther-qualification.json
```

On HVSC #84 it reports 49 accepted and structured tunes out of 88. The typed
remainder is 32 locator failures, three native-validation rejections, three
init-emulation failures, and one inexact-timing rejection. The three validation
gaps are Blitzkrieg, We M U S I C 3, and Rastertime Utopia; they remain strict
failures rather than silently falling back to trace-derived output.

Representative: `assets/music/Ark_Pandora.sid` (init `$B4C0`, play `$A007`,
player at `$A000-$A6xx`). `720_Degrees.sid` is the Gremlin representative.

Complexity sits **between Hubbard and Galway**: a clean uniform stream format
(easier than both), but with two levels of nested repeat loops and three
self-modifying tempo/slide commands (harder than Hubbard's flat orderlist, far
easier than Galway's gosub stack).

## Method

Same loop as the other drivers — `sid-re` on the post-`init` RAM image:

```
cargo run -p sid-analyzer --bin sid-re -- dis assets/music/Ark_Pandora.sid --range a296:a3ec
cargo run -p sid-analyzer --bin sid-re -- dump assets/music/Ark_Pandora.sid --range a586:a5a0
```

## Player architecture (Ark_Pandora addresses)

- `$A000` = music-on flag; entry jump table at `$A001`; PSID play `$A007` →
  `$A028`. Music init = `$A1D8`.
- The player keeps its per-voice working state in **zero page `$32..$AA`**
  during the tick and persists it at `$A51A+` between frames (save/restore
  around the tick in `$A028`). The sequencer state is *also* shadowed in
  absolute cells (`$A586+` etc.), which is what an extractor reads.
- Frame flow (`$A028`): zp restore → command drain `$A296` for X=0..2 →
  outer divider (`INC $A528` vs immediate at **`$A07F`**, self-modified by
  cmd `$0E`) → effect engine `$A08B` (vibrato/portamento/pulse/arp-ctrl,
  per-voice arrays indexed by SID offset 0/7/14) → note engine `$A21F` per
  voice → song-end check `$A1AE` → zp save → hard-restart writes (`$A04A`:
  voices flagged in `$A549,X` get ctrl=0).
- **Voice remap `$A58F` = `[00 07 0E]`** (confirmed in RAM): sequencer arrays
  (`$A586`, `$A577`, `$A57A`, …) index by voice 0/1/2; effect/staging arrays
  (`$A52B+`, `$A54A+`, `$A55B+`, …) index by SID register offset 0/7/14.

## Song header & voice streams

Song base pointer in zp `$FA/$FB` (set per subtune by the outer init
`$B4C0`). Header (confirmed in RAM: base `$A800`, v0 `$A804`, v1 `$A8F8`,
v2 `$A942`):

```
base+0  u16  voice-1 stream offset  (relative to base+4)
base+2  u16  voice-2 stream offset  (relative to base+4)
base+4       voice-0 stream starts here
```

Post-init the three stream pointers sit in `$A586..$A588` (lo) /
`$A589..$A58B` (hi) — the easiest thing for `locate` to read. **Song end**:
voice 0's pointer reaching voice 1's stream start stops the music
(`$A1AE`; voice 0's stream ends where voice 1's begins).

## Stream format — uniform 2-byte records

Every record is exactly two bytes; the pointer always advances by 2. At each
position: zero or more **command pairs** `[cmd $01..$7E][value]`, then one
**row pair** `[note][duration]`. The command drain (`$A296`) consumes pairs
until it sees a byte `>= $7F` or `$00` (both note bytes); the note engine
(`$A21F`) consumes the row when the previous row's duration expires.

### Note byte

| value    | meaning                                                                                                                                               |
|----------|-------------------------------------------------------------------------------------------------------------------------------------------------------|
| `$00`    | rest — stages frequency **zero**                                                                                                                      |
| `$7F`    | tie — re-gate the staged pitch with the new duration                                                                                                  |
| `>= $80` | pitched note: `n = byte + cmd-$14-transpose`, folded down by octaves (`while n >= $8C: n -= 12, oct++`), top-octave freq table lookup, freq `>>= oct` |

The note→freq conversion output cell (`$A5DB/$A5DC`) is a single **global**
register: every fetched row rewrites it (a rest stages zero, a gate-off row
stages its pitch with the gate cleared), and a tie row re-gates whatever it
holds — even across voices. Bytes `$01..$7E` in the note position leave it
untouched (the `CMP #$80 / BCC -> RTS` path), same as a tie.

Freq table at **`$A5C2`**: 12 × 2 bytes, **hi byte first** (`$A5C2+2i` = hi,
`$A5C3+2i` = lo), top octave, halved per octave (`LSR/ROR` in `$A4D6`).

### Duration byte

| value | meaning                                                                                                                                                |
|-------|--------------------------------------------------------------------------------------------------------------------------------------------------------|
| `$63` | this pair is a **portamento-target prefix**: the note's freq becomes the slide target (`$A547/$A548,X`) and the *next* pair is the actual sounding row |
| `$00` | gate-off row (clears bit 0 of the staged ctrl byte)                                                                                                    |
| other | row length in sequencer ticks (× outer divider `$A07F`)                                                                                                |

### Command dispatch (`$A2CD`, Y=cmd, A=value)

Commands `$05..$09, $0B, $0F` re-index X through `$A58F` (voice → SID offset)
before storing. Unknown command bytes are consumed as no-ops.

| cmd   | target                 | meaning                                                                                                                                                                                                        |
|-------|------------------------|----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `$01` | `$A573` (global)       | global param — value `$0F` in Ark_Pandora; likely volume (unverified)                                                                                                                                          |
| `$02` | `$A580,X`              | per-voice param → staged `$A560,X` at note-on (semantics TBD)                                                                                                                                                  |
| `$03` | `$A57A,X`              | **SID ctrl/waveform byte** ("instrument"): staged straight into `$A55F,X` at note-on                                                                                                                           |
| `$04` | `$A57D,X`              | → `$A55E,X` at note-on (effect rate, TBD)                                                                                                                                                                      |
| `$05` | `$A52E,X`*             | slide/portamento mode (1 = to-target, 2 = 16-bit add)                                                                                                                                                          |
| `$06` | `$A52F,X`*             | vibrato type (1/2 = dispatch in `$A132`)                                                                                                                                                                       |
| `$07` | `$A530,X`*             | ctrl-effect index into waveform table **`$A599`** (`41 21 11 81…`) — per-frame waveform cycling/arp-ish                                                                                                        |
| `$08` | `$A531,X`*             | pulse-effect flag (`$A4A1`)                                                                                                                                                                                    |
| `$09` | `$A54A,X`*             | row-effect duration (frames until auto-slide / gate event)                                                                                                                                                     |
| `$0A` | `$A583,X`              | → staged `$A561,X` at note-on (TBD)                                                                                                                                                                            |
| `$0B` | `$A54C,X`*             | slide step (added/subtracted in `$A3EC`)                                                                                                                                                                       |
| `$0C` | self-mod `$A0A2`       | second divider immediate (effect-engine /N)                                                                                                                                                                    |
| `$0D` | self-mod `$A446/$A457` | portamento step bounds inside `$A42B`                                                                                                                                                                          |
| `$0E` | self-mod `$A07F`       | **outer speed divider** immediate (frames per sequencer tick)                                                                                                                                                  |
| `$0F` | `$A546,X`*             | auto-slide mode after `$A54A,X` frames (1 = to-target, 3 = down, 4 = up)                                                                                                                                       |
| `$10` | loop 1 **end**         | value = total plays (`DEC $00 -> $FF` on the 6502, so value `$00` = 256 plays); first hit arms `$A540,X` + count `$A5A7,X`; jumps back to saved ptr `$A5AD/$A5B0,X` while count > 0 (PLA/PLA + re-enter drain) |
| `$11` | loop 1 **start**       | saves ptr+2 into `$A5AD/$A5B0,X` (value byte unused)                                                                                                                                                           |
| `$12` | loop 2 **end**         | same mechanism, count `$A5AA,X`, flag `$A543,X`, ptr `$A5B3/$A5B6,X` — **loops nest two deep**                                                                                                                 |
| `$13` | loop 2 **start**       | saves ptr+2 into `$A5B3/$A5B6,X`                                                                                                                                                                               |
| `$14` | `$A574,X`              | **note transpose** in semitones (added to the note byte before the freq fold)                                                                                                                                  |

\* = SID-offset-indexed (0/7/14).

The two-level loop nesting is the structure/reuse mechanism → maps to
`NativePlacement` repeats for the structured export (like Galway's
goto-loops), with loop 2 typically the outer "song section" repeat.

### Confirmed hand-decode (voice 0 @ `$A804`)

```
0E 01  0D 02  0C 02  0B 00  01 0F  02 29  0A 2C  04 0E  05 02  09 14   setup
13 01  11 01                                                            loop2/loop1 start
03 41                                                                   ctrl = pulse+gate
98 0A                                                                   note $98, 10 ticks
08 00  B0 63  B3 0A                                                     pulse off, porta target $B0, row $B3
08 01  9B 0A  08 00  B3 0A  …                                           alternating cmd/row
```

## locate() anchor candidates (relocation-independent shapes)

Operand wildcards on addresses; these opcode shapes are distinctive:

1. **Octave fold** in note→freq (`$A4E6`): `C9 8C 90 06 E9 0C CA 4C ?? ??`
   (`CMP #$8C / BCC +6 / SBC #$0C / DEX / JMP`) — also yields the freq-table
   base from the following `B9 ?? ?? 8D ?? ??` pair (`LDA $A5C2,Y`).
2. **Command-drain head** (`$A296`): `8E ?? ?? A0 00 BD ?? ?? 85 ?? BD ?? ??
   85 ?? B1 ?? F0 ?? C9 7F B0` — yields the per-voice stream-pointer tables
   (`$A586`/`$A589`) and the zp pointer pair.
3. **Tie check** (`$A25C`): `85 ?? C9 63 D0` — confirms the note engine.
4. **Loop-end PLA/PLA** (`$A318`): `68 68 4C` after two pointer restores —
   confirms the repeat-loop mechanism and yields the loop state cells.

Cross-confirm by requiring (1) + (2) and reading all cell addresses out of
the matched operands, hubbard/galway style.

## Open questions (not blocking decode_song)

- cmd `$01` (`$A573`) semantics — likely master volume; check `$D418` writes.
- cmd `$02`/`$04`/`$0A` staged params (`$A560/$A55E/$A561`) — effect knobs
  used by the per-frame engine; matter for instrument/timbre fidelity, not
  note decode.
- The instrument half (ADSR/pulse staging and SID write sites beyond the
  `$A04A` hard-restart) — needed for native instruments later, not for notes.
- Gate fine-print: `$A549,X` hard-restart (ctrl=0 *after* the tick) and the
  dur-`$00` gate-off row interact; ground-truth against the trace when the
  onset gate is up.
- Whether all 88 HVSC tunes share this exact dispatch (V3) or straddle
  V1/V2 variants — the sweep (task 4) answers this.
