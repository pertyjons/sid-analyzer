# Martin Galway playroutine — reverse-engineering notes

> **Status: driver reference.** Further extractor work is prioritized only from
> the corpus-ranked follow-ups in
> [`PLAN.md`](../../plans/PLAN.md#4-corpus-ranked-follow-ups).

Status: **three-sequencer discovery and native note decode implemented and
validated.** The table-driven generations use a faithful player simulation over
the post-`init` RAM image. Comic Bakery's older `$C0` command generation uses
the real 6502 player as its procedural control-flow engine while the extractor
recovers note bytes, duration counters, transposes, and frequency-table pitches
from located driver cells. Street Hawk and Yie Ar Kung Fu II now use the same
live-control-flow strategy through their related legacy `$C0` generation. All
supported generations emit recovered source patterns and placements. The
full-length artist census accepts at least one subtune in **nine of the 35 PSID
files**, 45 of 382 PSID subtunes in total, and every subtune in three files:

| tune                       | accepted subtunes          | generation |
|----------------------------|----------------------------|------------|
| Comic_Bakery               | 1, 3, 4                    | `$C0`      |
| Neverending_Story          | 1 (full file)              | `$95xx`    |
| Ocean_Loader_1             | 1 (full file)              | Ocean      |
| Helikopter_Jagd            | 1–7                        | Ocean      |
| Hyper_Sports               | 21, 22, 25–27, 30, 31, 35 | Ocean      |
| Rambo_First_Blood_Part_II  | 1, 3–6, 8–10, 20, 21      | `$3F`/`$C0` plus Comic |
| Street_Hawk_Prototype      | 1 (full file)              | Ocean      |
| Street_Hawk                | 1–4, 6, 7, 10, 11          | legacy `$C0` |
| Yie_Ar_Kung_Fu_II          | 1, 3–7                     | legacy `$C0` |

The measured coverage, common traits, and ranked follow-ups are in the
[full-length Galway corpus census](../galway-corpus-census.md). Locator failure
remains the dominant native gap: 22 files and 227 subtunes, before decoder
quality is considered.

The gate is **arpeggio-aware** (`arp_aware_agreement`): Galway's instrument
arpeggios cycle the chip through chord steps every frame (Ocean_Loader_1 V2:
base 70, chip cycles 74/77/70 with the *base on the gate-off closing step*),
and the gates are only 3-6 frames long — below `detect_effects`' arpeggio
thresholds and re-gated every cycle, so `detect_notes` reports the gate-time
upper step. A native note that fails the strict voice+pitch+frame match still
counts if the chip played its pitch on its voice within the onset window, on a
frame gated or adjacent to a gated one (per-frame trace fallback). Strict-only
scored 0.721 on Ocean_Loader_1 at 9000 frames for a note-perfect decode.

Most unsupported PSID tunes run **structurally different engines** — e.g.
Kong_Strikes_Back's 1984 player (single sequencer, `JSR $8E0B` byte reader)
and Highlander's later workspace-swapping engine (per-voice zp swap via
`LDA $10,X / STA $ADF1,X`) — each its own RE effort. They fall back cleanly:
`locate` returns `None` -> `LocateFailed` -> `--format synth`.

Comic Bakery's start song passes strict native validation for its full 9474
frames. Ocean Loader 1 passes for its full 10126 frames after production
validation was made arpeggio-aware: the authored base pitch must occur on the
same physical voice within the onset window before the detected gate-time pitch
is accepted as an equivalent arpeggio step. Alignment remains monotonic and
one-to-one.

Comic Bakery's runtime pointer discontinuities delimit its older procedural
patterns. Revisiting a source address reuses the same pattern number; the exact
two-byte rows, `$C0+` commands, transposes, placements, and repeat ordinals are
retained independently from render grouping.

Street Hawk and Yie Ar Kung Fu II form the largest conservative unsupported
cluster from the previous census: two files, 33 subtunes, and `0.717` pair
similarity. Their sequencers share `$C0+` command dispatch, `$60..$BF` folded
notes, `$5F` ties, table-indexed durations below `$60`, literal durations above
it, and consecutive per-voice pointer, duration, enable, and transpose cells.
The locator requires all three code-shaped sequencers and their shared
frequency/duration tables to agree. Street Hawk additionally requires the
`$8069` active mask with bits 1/2/4; Yie II's generation has no corresponding
shared active mask. The live 6502 player resolves their procedural commands,
while source addresses and two-byte note rows become recovered patterns and
placements.

The unchanged 1,500-call production gate accepts 14 of the family's 33
subtunes: Street Hawk 1–4, 6, 7, 10, and 11; Yie II 1 and 3–7. The other 19
remain typed rejections rather than partial native output. A full HVSC #84
artist census raised Galway coverage from 31 to 45 accepted and structured
subtunes without losing any prior acceptance.

Rambo subtune 1 uses a third bounded dialect of the same three-sequencer
family. Commands begin at `$C0` and dispatch through `(event & $3F)`; note rows
in `$60..$BF` fold down by `$60` and carry a literal duration. Its `DA <count>`
and `DC` handlers push and drain an inline repeat stack, while return, gosub,
gosub-plus-transpose, and direct poke use compact Y-indexed handler forms. All
are classified from handler code and simulated in their real RAM cells. The
SID retains multiple full player copies, including byte-identical code aliases,
so discovery first builds strict three-voice candidates, deduplicates identical
layouts, and selects the unique relocation closest to the post-init stream
pointers. If that layout fails its deterministic handler check, the established
Comic locator remains the fallback; the full 382-subtune comparison preserved
every prior acceptance.

Voice 1 of Rambo subtune 1 is produced by an explicit native-code call rather
than pattern notes. If and only if such a voice has no decoded notes and the
initial native validation rejects, its trace notes may supplement the native
timeline. They carry `trace_corrected` provenance and the combined song must
pass the unchanged monotonic validation gate. In the 1,500-call qualification
window this combines 188 native pattern notes with 136 corrected voice-1 notes.

Galway wrote his own player (reportedly after disassembling Hubbard's). It is
**not** a Hubbard variant — different structure, hard-restart gating, a work area
relocated under I/O, and later `$D418` packed-nibble playback associated with
his routines. So this is a from-scratch effort, separate from `hubbard.rs`.

## Method

Disassemble straight from the post-`init` RAM image with the `sid-re` toolkit
(no dump step; SID register operands and located driver cells are annotated):

```
cargo run -p sid-analyzer --bin sid-re -- dis <tune.sid> --range <lo:hi>
```

`sid-re dump`/`watch`/`scan`/`cluster` cover the rest of the RE loop (RAM dumps,
frame-by-frame cell watching, signature counting, code-family grouping) — see `sid-re
--help`. The `#[ignore]` triage test `galway::tests::dbg_galway` (dumps to
`/tmp/galway_<name>.bin`, readable by `sid-re dis` too) remains for the
voice-level decode comparisons. Same loop that cracked the Hubbard player.

```
SID_HVSC_ROOT=/path/to/C64Music \
SID_DBG_TUNES=MUSICIANS/G/Galway_Martin/Neverending_Story.sid \
cargo test -p sid-analyzer --lib dbg_galway -- --ignored --nocapture
```

## Scope (HVSC #84, 40 tunes)

- 35 PSID (in scope), **5 RSID** (out of scope — Kernal/BASIC ROM).
- 39 PAL, 1 NTSC.
- 10 carry a non-zero speed bitmask (some CIA-timed subtune — harder timing).
- 31 are multi-subtune; **7 are clean single-subtune PSID vblank** starting
  targets: Commando_High-Score, Highlander, Kong_Strikes_Back, Neverending_Story,
  Ocean_Loader_1, Ocean_Loader_2, Street_Hawk_Prototype.

## Sequencer cracked — Neverending_Story (`init=$95E8 play=$95F4`, PSID, PAL)

PSID load address is embedded (header load = `$0000`); the player sits around
`$95xx`, work tables at `$9F00+`, and a work area relocated to `$F000+`.

```
play $95F4:  LDX #$35 / STX $01      ; bank RAM under I/O ($A000-$FFFF visible, IO at $D000)
             JSR $9695               ; the real per-frame play
             LDX #$37 / STX $01      ; restore default bank
             RTS

$9695 (core): JSR $9680              ; master-volume fade ($F083 += $F084 -> $D418 ramp)
              JSR $979D              ; VOICE 1 sequencer
              JSR $9A8C              ; VOICE 2 sequencer
              JMP $9CF2              ; VOICE 3 sequencer
```

Init-time (`$9600`) copies `$A000..$F7FF` to `$F000..` (self-modifying high-byte
loop, ends when the source page reaches `$F8`) — relocating data/work area under
the I/O space the player banks in.

### Three per-voice sequencers (one model, three instances)

Each voice N has its own pattern pointer, duration counter, and command jump
table, gated by a bit in the active-voice mask `$19`:

| Voice | routine | ptr (zp)  | dur ctr | transpose | cmd table | reg image | dur table | stack (idx, lo/hi)     | active bit |
|-------|---------|-----------|---------|-----------|-----------|-----------|-----------|------------------------|------------|
| 1     | `$979D` | `$10/$11` | `$1A`   | `$9F46`   | `$9913`   | `$9EF3`   | `$9F15`   | `$1D`, `$9F26`/`$9F36` | `$19 & 1`  |
| 2     | `$9A8C` | `$12/$13` | `$1B`   | `$9F9A`   | `$9BEF`   | `$9F47`   | `$9F69`   | `$1E`, `$9F7A`/`$9F8A` | `$19 & 2`  |
| 3     | `$9CF2` | `$14/$15` | `$1C`   | `$9FEE`   | `$9E31`   | `$9F9B`   | `$9FBD`   | `$1F`, `$9FCE`/`$9FDE` | `$19 & 4`  |

(The dur table is always reg image + `$22`; the earlier "≈`$9FA1`" guess for
V3's reg image was wrong — `$9DDF`'s `STA $9F9B,X` pins it at `$9F9B`.)

All three were disassembled and are byte-for-byte the same routine with these
per-voice addresses substituted — so one decoder model covers all three.

Per-voice loop (voice 1 shown): if the voice's active bit is set, `DEC` its
duration counter; while still counting, run the ongoing-note path; on underflow,
advance the pattern pointer by **3 bytes** (`ADC #$03`) and read the next event
with `LDA ($10),Y`.

### Pattern-stream byte encoding — **fully pinned** (`$97B4` / micro-trace)

A within-frame micro-trace (`galway::tests::dbg_galway_micro`, single-steps `play`
and logs every individual `$10/$11` move) plus a full disassembly of voice 2
(`$9A8C..$9C0D`, jump table `$9BEF`) nail the grammar. Two earlier guesses were
**wrong** and are corrected here: the command table is *not* 16 entries, and note
duration is *not* `idx*6`.

**bit 7 clear -> NOTE, 2 bytes `[note][dur-idx]`** (advance +2). `$00..$5E` are note
indices; `$5F` = tie, `$60` = rest (`$97D8`/`$9AC6`). A note adds the per-voice
transpose -> `$18`; pitch = stride-1 semitone tables **`$F08E[idx]` = freq hi ->
`$D401`, `$F0EA[idx]` = freq lo -> `$D400`**. Duration index is the 2nd byte;
**note length in frames = `dur-table[idx]`**, where dur-table is the **register
image at offset `$22`** (V1 `$9F15`, V2 `$9F69`, V3 `$9FBD`). It is **populated
lazily at runtime** by the block-load/poke commands, *not* fixed: post-init it
reads `[15,0,0,…]` but a few frames in V1/V2 hold
`[15,6,12,18,24,30,36,42,48,54,60,108]` (≈ `6*idx` for the common range, but with a
special `idx 0 = 15` and a non-linear tail, and V3 differs per instrument). So the
decoder must **read this table from the simulated image at note time** — `idx*6` is
only an approximation. Exact counter semantics (`$97A2`): each play-frame an
active voice does `DEC ctr / BEQ read-next` — the value written by a note is the
exact frame count it occupies, and a **zero table entry wraps the 8-bit counter
to a 256-frame note**. Tie and rest reload the counter the same way (their
duration byte indexes the same table); rest does not gate-off immediately — the
ongoing-note path handles the envelope. After a note, the pointer rests on the
*next* event; several commands can therefore execute back-to-back within one
frame until a note/tie/rest reloads the counter.

**bit 7 set -> COMMAND.** `(byte & $7F)` is a **byte offset** into the per-voice
word table (`$9913`/`$9BEF`/`$9E31`). **Important: the three voices' tables are not
the same map.** Voice 2 uses only even opcodes (so its table is a clean word array,
fully decoded below); voice 1's table mixes odd+even opcodes (`$81`,`$83`,`$87`,…)
at overlapping byte offsets and assigns them to *different* handlers (e.g. `$8C` =
poke on V1 but = stop on V2). So a general decoder must read each voice's own table
and map handler **addresses** to semantics (by recognising the handler code), rather
than assume V2's opcode map for all three. Commands are **3 bytes** `[cmd][op-lo][op-hi]`
*except* the two transpose-carrying forms which are **4 bytes**. The next 2 bytes go
to `$16/$17` before dispatch. Command set (voice-2 handlers shown; V1/V3 are the
same logic with per-voice cells):

All block-load handlers share one descending copy loop parameterised by two
immediates — `LDY #last_src / LDX #last_dst`, then `regimg[X] = src[Y]` with
both counting down — i.e. **copy `last_src+1` bytes from the operand address
into the reg image *ending* at offset `last_dst`**. (The first cut of this
table guessed "5-byte blocks" from the `$82` instance only; the real lengths
below come from each handler's immediates.)

| cmd               | len    | handler | meaning                                                                                                            |
  |-------------------|--------|---------|--------------------------------------------------------------------------------------------------------------------|
| `$80`             | jump   | `$9B6B` | **return / next orderlist**: `$1E`++ (stack idx), reload ptr from `$9F7A[idx]`(lo)/`$9F8A[idx]`(hi), re-read event |
| `$82`             | 3      | `$9B7D` | copy 5 bytes from `($16)` into reg-image `+$1A..$1E` (Y=`$04`, X=`$1E`)                                            |
| `$84`,`$8C`,`$94` | 3      | `$96A1` | stop / silence: `$19 <- $38` (all voice bits clear), zero `$D400-$D417`                                            |
| `$86`             | 3      | `$9B8D` | copy 35 bytes into reg-image `+$00..$22` (Y=X=`$22` — full instrument incl. dur-table base)                        |
| `$88`             | 3      | `$9B99` | copy 15 bytes into reg-image `+$00..$0E` (Y=X=`$0E`)                                                               |
| `$8A`             | 3      | `$9B9F` | copy 11 bytes into reg-image `+$0F..$19` (Y=`$0A`, X=`$19`)                                                        |
| `$8E`             | jump   | `$9BAC` | **goto** operand addr (no push)                                                                                    |
| `$90`             | jump   | `$9BC0` | **gosub**: push `ptr+3` to `$9F7A/$9F8A[$1E]`, `$1E`--, goto operand                                               |
| `$92`             | 3      | `$9B93` | copy 51 bytes into reg-image `+$00..$32` (Y=X=`$32` — covers the whole dur table)                                  |
| `$96`             | jump,4 | `$9BE4` | **gosub + transpose**: 4th byte -> transpose; push `ptr+4`; goto operand                                           |
| `$98`             | 3      | `$9BD6` | **poke** reg-image: `reg[byte1] = byte2`                                                                           |
| `$9A`             | jump,4 | `$9BA5` | **goto + transpose**: 4th byte -> transpose; goto operand                                                          |
| `$9C`             | 3      | `$9BB7` | **call native code** at operand (`JMP ($16)`), returns to `ptr+3`                                                  |

Voice 1's table maps the same opcodes differently (e.g. `$8C` -> `$98CB` =
copy 16 bytes into reg-image `+$23..$32` — **the dur-table load** — while
`$92`/`$94`/`$96`/`$9A` are stop), and its gosub+transpose handler (`$98E5`)
hides the plain-gosub `LDA #$03` entry inside a `BIT $03A9` skip-trick at
`$98EF`. This is why the decoder **classifies handler code instead of
trusting opcode numbers** (`classify_handler` in `galway.rs`) — the handler
shapes are shared across voices and embed every per-voice cell address
(stack index/lo/hi, reg image, transpose) in their operands.

The "orderlist" `$9F26/$9F36`(V1) / `$9F7A/$9F8A`(V2) is really the **gosub
return-pointer stack**: `$90`/`$96` push + decrement the index, `$80` pops +
increments. Song structure is procedural (goto/gosub/return inside the streams),
not a flat orderlist.

### Data layout (confirmed from the post-init image)

- **Frequency table**: standard semitone table, note 0 = `$0112`, each step
  ~x2^(1/12) (note index = semitones). Lo `$F0EA[idx]`, hi `$F08E[idx]`.
- **Pattern data + freq/work tables live in the relocated `$F000+` region** (copied
  from `$A000+` at init). Post-init voice pointers read from zero page: V1 = `$F250`,
  V2 = `$F5F3`, V3 = `$F6BE`. `$19` = `$3F` (all three voices active). Transposes 0.
- **Register image / instrument**: the per-voice reg image (`$9EF3`/`$9F47`/≈`$9FA1`)
  is pushed to `$D400..$D417`; instrument/param sub-blocks are loaded by the
  `$82/$86/$88/$8A/$92` copy commands and the `$98` poke. Master volume `$F082` ->
  `$D418`.

### Micro-trace evidence (Neverending_Story)

V1 frame 0 (single-stepped) — six 3-byte commands then a 2-byte note, all confirmed:

```
$F250 8C 69 F1  poke         $F25C 9C 00 F4  call $F400
$F253 82 E8 F1  load instr   $F25F 88 ED F1  load block (instr ptr)
$F256 98 18 00  poke         $F262 30 10     NOTE idx $30 (=48), dur-idx $10
$F259 98 19 07  poke
```

V2 shows the control flow live: `$F5F6 90 B2 F5` gosub -> `$F5B2`; `$F5B5 96 5F F5 ..`
gosub+transpose -> `$F55F` (return pushed = `$F5B9`); the `$80` at the pattern end
later pops back to exactly `$F5B9`. Note pitch confirmed: V1 idx 48 -> freq `$1128`,
trace opens V1 on **C-4 = `$1125`** (the `$1125/$159A/$19B1` cycling is the
instrument's arpeggio, one held note).

### decode_song (implemented — `galway.rs`)

`decode_song` simulates the player over a **mutable copy of the whole post-init
RAM image**: the zero-page pattern pointers and duration counters, the
active-voice mask, the gosub stacks, the reg images (including the lazily-loaded
dur tables) all live at their real addresses, and commands mutate them exactly
as the handlers do — so the dynamic data dependencies come out right by
construction. Commands are dispatched by reading the per-voice word table at
byte offset `(cmd & $7F)` and **classifying the handler code**
(`classify_handler`), which also recovers the stack/transpose/reg-image
addresses from the handler operands. A wedged stream (goto self-loop) is cut by
a per-frame command budget; an unrecognised handler silences the voice.

**Validation (dbg_galway_decode):** Neverending_Story scores onset agreement
**1.000** (384 native vs 386 trace notes, 1500 frames, every voice spot-checked
frame- and pitch-exact). The other clean PSID targets decode 0 notes against the
fixed `$95xx` layout — they are relocations, blocked on `locate`. Arkanoid's
trace emulation itself fails (`CycleGuardTripped`, a digi tune) — that is the
trace side, not the decoder.

## The Ocean-loader generation (Ocean_Loader_1, Helikopter_Jagd, Hyper_Sports, Street_Hawk_Prototype)

The same sequencer model with an **evolved command set** (RE'd from
Ocean_Loader_1, V1 handlers `$A34E..$A441`). Differences from `$95xx`:

- **Opcode map shifted**: `$80` = *end voice* (if the return stack is back at
  its initial index `$0F`, clear the voice's mask bit; anything else hits the
  error trap), `$82` = return/pop (range-checked: `BMI ok / CPX #$0F / BCS
  err`), `$84..$8E` = block loads (incl. `$86` = 4 bytes -> `+$1F..$22` and
  `$8E` = the 16-byte dur-table load -> `+$23..$32`), `$90` = goto, `$92` =
  gosub (overflow-checked `BMI`), `$94` = 51-byte load, **`$96` = standalone
  2-byte transpose `[cmd][value]`** (advance +2), `$98` = gosub+transpose (4
  bytes), `$9A` = range-checked poke (two hand-assembled variants: `CMP
  #limit` before `TAX`, or `TAX / CPX #limit`), `$9C` = goto+transpose.
- Block-copy heads enter the shared loop via `JMP` (the `$95xx` generation
  uses `BNE`/fall-through).
- **No tie byte**: the note path checks only `$60` = rest; `$5F` is an
  ordinary note index (`GalwayLayout::has_tie` = false, detected by `locate`
  from the `CMP #$5F` presence).
- An **error trap** (`$A437`: `$19 <- 0`, error code in zp) replaces silent
  garbage on stack over/underflow and out-of-range pokes.

`classify_handler` covers both generations by shape (the checks are optional
pattern elements), so `decode_song` is generation-agnostic.

## locate (implemented — `galway.rs`)

Scans the RAM image for the three sequencers' **command-dispatch shape**
(`AND #$7F / TAX / [STX zp] / LDA tbl,X / STA … / LDA tbl+1,X / STA … / INY /
LDA (ptr),Y / STA $16`), which yields the jump table + pattern pointer, then
reads the rest out of the surrounding code: duration reload (`LDA (ptr),Y /
TAX / LDA dur,X / STA durctr`), transpose add (`ADC abs / STA $18`), pitch
lookup (`LDX $18 / LDY hi,X / LDA lo,X`), routine head (`LDA mask / LSR` or
`AND #bit`), and the tie check (`CMP #$5F`). Demands exactly three hits
agreeing on freq tables/mask with voice bits `{1,2,4}`. The remaining cells
(gosub stacks, reg images) come out of handler classification at dispatch
time. `extract` then double-checks `layout_matches` (offset-0 handler must
classify as Return/StopVoice) before simulating, and the onset gate guards the
result.

## Roadmap (the RE ahead)

1. ~~Implement `decode_song`~~ **done** (player simulation, both generations).
2. ~~Derive a `locate`~~ **done** for the two known generations (dispatch-shape
   scan; asset fixtures and CI tests for both generations).
3. ~~Export gap: instrument arpeggios lost in the synth export~~ **fixed**
   (`split_arpeggio_patches` in `galway.rs`). The per-note loop *detection*
   was never the problem — `extract_characteristics` finds the chord cycles —
   but the trace clustering (ADSR + waveform + role tags) can't tell a flat
   stab from an arpeggiating one, and the patch-level `arpeggio_loop` is taken
   from the cluster's *first* member, flattening (or mis-chording) the rest.
   The Galway extract now gives every distinct chord body its own patch:
   Ocean_Loader_1's export grows 2838 -> 6422 notes (the chord expansions) and
   Neverending_Story's arpeggios land on per-chord patches instead of the
   first member's shape. Hubbard exports verified byte-identical (the change
   is Galway-extract-local). Still trace-derived; native instrument decode
   (the reg-image blocks) would make it authored ground truth.
4. ~~Cluster the 24-file, 260-subtune `locate_failed` census group~~ **done**.
   `sid-re cluster` builds relocation-neutral post-init code fingerprints and
   uses complete-link Jaccard clustering. At the conservative `0.70` threshold,
   five multi-file families cover 123 subtunes; the largest is Street_Hawk plus
   Yie_Ar_Kung_Fu_II (33), while Hunchback_II plus Kong_Strikes_Back is the
   strongest match (`0.898`). See the corpus census for all 17 groups.
5. Implement one conservative family behind typed rejection. Prefer the
   33-subtune Street_Hawk/Yie_Ar_Kung_Fu_II family for recall, or the much
   tighter Hunchback_II/Kong_Strikes_Back family for the lowest-risk first
   locator/decoder slice.
6. Native instrument/patch decode (the reg-image blocks) — timbre detail; the
   note timeline does not need it.
7. ~~Separate Galway `$D418` PCM from ordinary Ocean-loader volume writes~~
   **done**. The detector requires varying low-nibble values. A cycle-stamped
   packed-nibble cadence is the positive fixture, while the full Ocean Loader 1
   trace is a named negative control. The former Neverending_Story result was
   four identical `$0F` writes and is no longer classified as sampled audio.
