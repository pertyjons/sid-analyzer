# GoatTracker native extraction

## Supported families

| `sidid.cfg` name       | HVSC #84 files | Extractor         |
|------------------------|----------------|-------------------|
| `GoatTracker_V2.x`     | 7550           | `goattracker-v2`  |
| `GoatTracker_V1.x`     | 1384           | `goattracker-v1`  |
| `GoatTracker_V2/Mini`  | 1              | none              |
| `GoatTracker_V2/Mini2` | 1              | none              |

`Mini` and `Mini2` are size-optimised players that share no signature with
either extractor. They are deliberately left unclaimed, so a tune using one
reports "no native extractor handles it yet" instead of being dispatched and
then failing to locate. One HVSC file each, and the `Mini` file does not even
survive `init` under the emulator.

Both extractors locate relocated operands rather than fixed addresses. When a
file embeds several players, the active play trampoline and code proximity
correlate the layout candidates; native note validation remains the final
selection gate. Files identified as one GoatTracker generation can therefore
use the other extractor when the selected subtune runs that generation.
Readers that resolve to the same operands are deduplicated before correlation.

They share the pointer-table reader matcher, per-call state sampling,
decoder-phase resolution, and song assembly; they differ in discovery and
grammar.

## Recovered data

Pitch is read from the player's own playing-frequency cell at each
trace-detected note onset — the value the chip saw, slides and vibrato
included. Gate timing and articulation stay trace-measured. Buffered players
are qualified at direct through four-call-delayed phases, and the selected
phase is recorded in native validation.

Where the pattern grammar is recognised, both families also recover:

- three orderlists for the selected subtune;
- pattern, repeat, transpose, loop and stop commands;
- packed pattern rows with instrument changes, effects and effect parameters;
- notes, key-off/key-on rows, rests, and packed multi-row holds;
- runtime-observed pattern placements with exact start frames.

Authored instrument tables are not decoded yet for either family.

### V2

The relocator emits split low/high frequency tables and a columnar three-voice
runtime state. Discovery anchors on four operands: the `SEC / SBC #$60 / STA
note_indices,X` that turns a pattern byte into an authored note index, the
`LDA playing_lo,X / STA $D400,X` mirror that names the playing-frequency cell,
and the song and pattern pointer-table readers. Anchoring the frequency on the
chip mirror rather than on the note-to-frequency table lookup is what makes the
locator survive the many relocator layouts in the wild.

The packed grammar follows GoatTracker's `greloc.c` `packpattern()` output and
the corresponding `player.s` sequencer. Compact optimised players use a reduced
grammar and remain native-decoded without claiming native structure.

### V1

V1 resolves an authored note number through split frequency tables. The
observed generations contain 96, 104, or 159 entries; these spacings separate
the real table pair from unrelated adjacent-cell stores. The standard pattern
reader has this shape:

```text
LDY pattern_number,X ; LDA ptr_lo,Y ; STA zp ; LDA ptr_hi,Y ; STA zp+1
LDY pattern_position,X ; LDA (zp),Y ; INY ; CMP #$60
```

A pattern row is one of three shapes:

| first byte | meaning                                                                 |
|------------|-------------------------------------------------------------------------|
| `< $60`    | note number, followed by `instrument << 3 \| command` and a command byte |
| `$60..$BF` | note number `byte - $60`, nothing else                                   |
| `>= $C0`   | hold the previous row for `256 - byte` further ticks                     |

Note numbers `$5E` and `$5F` are key-off and rest in either note shape, and a
`$FF` byte ends the pattern.

The orderlist reader is *not* one shape — three generations ship in HVSC:

| generation           | how the orderlist is reached                       | grammar                                            |
|----------------------|-----------------------------------------------------|----------------------------------------------------|
| indexed (`CMP #$D0`) | split pointer table, indexed by a per-voice song index `init` sets | pattern numbers, `$D0..$DF` repeat and `$E0..$FE` transpose prefixes, `$FF` + target loops |
| pointer (`CMP #$FF`) | address cached per voice on the first `play` call     | bare pattern numbers, `$FF` + target loops          |
| pointer (`CMP #$FE`) | address cached per voice on the first `play` call     | bare pattern numbers, `$FE` stops, `$FF` loops to 0 |

Because the cached-pointer generations fill their pointers on the first `play`
call rather than in `init`, V1 reads its tables from a RAM image taken *after*
the player has run. That also means subtune selection stays the player's
business: the extractor reads back whichever orderlists `init` chose instead of
reimplementing the subtune arithmetic.

## Qualification

Committed fixtures cover both V2 grammars (a compact player and a full player)
and all three V1 orderlist generations. The deterministic qualification binary
runs every identified tune, sorts every result by corpus-relative path, and
writes stable JSON with the selected extractor, validation metrics, structure
status, and separate timing/locator/decoder/emulation-stage outcomes:

```bash
cargo run --release -p sid-analyzer --bin sid-goattracker-qualify -- \
  --corpus /path/to/C64Music --frames 1500 --output goattracker.json
```

`--family v1|v2` restricts the family. `--limit N` is applied after
identification in sorted path order, so reduced runs are reproducible.

Over HVSC #84 at 1500 calls per tune:

| family | identified | accepted | with placements |
|--------|-----------:|---------:|----------------:|
| V2.x   | 7550       | 7062 (93.5 %) | 6770 (89.7 %) |
| V1.x   | 1384       | 1367 (98.8 %) | 1361 (98.3 %) |

Against the deterministic pre-change baseline, this adds 205 accepted tunes
and 161 placement-bearing tunes. The former 283 V2 locator failures are down
to 101, all 35 structure-decode failures are now pitch-preserving fallbacks,
and V1 has no remaining locator failure. Only 30 CIA-timed tunes remain
inexact because their init routines do not program a timer period: 28 V2 and 2
V1.

The remaining V2 outcomes are 101 locate failures, 5 empty decodes, 2
validation failures, and 118 init-emulation failures. Another 234 are explicit
unsupported configurations: 213 multi-SID files and 21 selected subtunes with
no trace-detected pitched notes. V1 has one validation failure, one
init-emulation failure, and 13 explicit unsupported configurations (12
multi-SID and one without pitched notes). Emulation errors retain their exact
stage, so init, native sampling, and trace failures no longer collapse into one
extractor bucket. `GoatTracker_V2/Mini` and `/Mini2` remain one unclaimed HVSC
file each because neither shares a signature with the V1 or V2 extractor.

Upstream format evidence:

- [GoatTracker 2.77 relocator source](https://sources.debian.org/src/goattracker/2.77%2Bds-1/src/greloc.c)
- [GoatTracker 2.77 player source](https://sources.debian.org/src/goattracker/2.77%2Bds-1/src/player.s)
