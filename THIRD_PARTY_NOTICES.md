# Third-party notices and provenance

sid-analyzer's own code and documentation are Copyright (C) 2026 Per Jonsson
and licensed under `GPL-3.0-or-later`; see [LICENSE](LICENSE). Third-party
material retains the notices and terms below. User-supplied Songlengths and
music files are not distributed or relicensed by this project.

## SIDId signature database

`assets/sidid.cfg` is the SIDId playroutine-signature database by Cadaver
(Lasse Oorni) and contributors. Its BSD-style three-clause terms and attribution
are reproduced in [assets/sidid.cfg.NOTICE](assets/sidid.cfg.NOTICE).

The database is embedded in analyzer binaries. Include that notice with binary
distributions as well as source distributions. The upstream source header was
checked against [SIDId](https://github.com/cadaver/sidid/blob/master/sidid.c)
on 2026-09-07.

## Optional local HVSC Songlengths database

No Songlengths database is included in the source tree or analyzer binaries.
Users may keep a separately obtained copy at the ignored
`assets/Songlengths.md5` path and supply it with `--songlengths` or
`HVSC_SONGLENGTHS`. Download instructions are in [README](README.md#hvsc-songlengths).
Its format is documented in the
[HVSC Songlengths FAQ](https://www.hvsc.c64.org/download/C64Music/DOCUMENTS/Songlengths.faq).
The file contains duration metadata and tune paths, not music payloads.

The reviewed FAQ and
[HVSC documentation](https://www.hvsc.c64.org/download/C64Music/DOCUMENTS/HVSC.txt)
did not establish an explicit redistribution license for the previously
tracked HVSC #84 snapshot. It has been removed from the Git index while the
local copy is preserved. Its historical copies have been removed from the
local repository; remote history must also be replaced before publication. No database license is inferred
from the separate terms for SID music files.

## reSID filter measurements

`F0_6581` and `F0_8580` in `crates/analyzer/src/export/synth.rs` reproduce
cutoff measurement points from Dag Lem's `f0_points_6581` and `f0_points_8580`,
with piecewise-linear interpolation and duplicate endpoint knots removed. The originating
[reSID filter.cc](https://github.com/simonowen/resid/blob/02afcc5cefac34bd0c665dc0fa6b748d238c1831/filter.cc)
carries Copyright (C) 2004 Dag Lem and GPL-2.0-or-later terms.

The exporter distributes these adaptations under GPL version 3 or later,
as permitted by the upstream "or later" grant. The original attribution and
GPL-2.0-or-later grant are retained; the GPL version 2 text is available in
[tools/digital-sid-oracle/COPYING](tools/digital-sid-oracle/COPYING), and version
3 is in [LICENSE](LICENSE). The tables and interpolation were adapted for
sid-analyzer by Per Jonsson in 2026; the source is modified, not an unmodified
copy of reSID's filter implementation.

The license decision and exact scope are recorded in
[docs/resid-licensing.md](docs/resid-licensing.md). No separate MIT grant is
needed or claimed for this GPL release.

## Digital SID oracle generator

`tools/digital-sid-oracle/generate.cpp` declares `GPL-2.0-or-later`. The generator
builds against libresidfp and is separate from the Cargo workspace and analyzer
runtime. The full GPL version 2 text is in
[tools/digital-sid-oracle/COPYING](tools/digital-sid-oracle/COPYING); the source's
"or later" option remains applicable.

The exact upstream version, revision, archive checksum, and compiler settings
are in [source.lock](tools/digital-sid-oracle/source.lock). The committed JSON
vectors record generated observations; their provenance and regeneration
procedure are documented in the [tool README](tools/digital-sid-oracle/README.md).

## Pertylizer schemas and descriptors

`docs/pertylizer/` includes mirrored schemas and module descriptors from
[Pertylizer](https://github.com/pertyjons/pertylizer). The documented mirror
baseline is commit `c89f672d`, refreshed on 2026-07-27. Pertylizer's upstream
license checked on 2026-09-07 is MIT, Copyright (c) 2026 Per Jonsson.
The full notice is retained in
[docs/pertylizer/LICENSE-MIT](docs/pertylizer/LICENSE-MIT).

The analyzer embeds `descriptors.json`, so preserve that notice in binary
distributions too. The mirror baseline documents provenance, not a guarantee
of compatibility with every later Pertylizer version.

## Cargo dependencies

`Cargo.lock` pins the dependency graph, including optional SQLite support.
Dependency notices must accompany distributed binaries as required by their
individual licenses. A dependency-only manifest inventory cannot account for
the vendored assets or the reSID-derived table above.

The accepted Cargo dependency licenses are recorded in [about.toml](about.toml).
Use the inventory command in [CONTRIBUTING.md](CONTRIBUTING.md#licensing) for
release packaging and investigate unresolved license files. This automated
inventory does not include the manually documented assets and tools above.

On 2026-09-07, `cargo-about` 0.9.1 resolved all 63 Cargo packages with all
features enabled, offline and locked, with no warnings or errors. The project
was identified as GPL-3.0-or-later. This inventory does not cover removed
third-party material in remote history that has not yet been replaced.

## Music and artwork

The current tracked tree contains no `.sid`, `.wav`, or `.ptz` music/project
payloads. Local experiments belong under the ignored `assets/music/`,
`assets/fixtures/sound-engine-poc/`, and `exports/` paths. Older reachable Git
commits containing those payloads have been removed locally. The remote
repository must also use the clean history before publication.

The hero and icon artwork under `images/` was generated by Per Jonsson, who
confirmed on 2026-09-07 that these are his own generated images supplied for
this project. No third-party image source is claimed. No rights to third-party
tunes are granted by this repository.
