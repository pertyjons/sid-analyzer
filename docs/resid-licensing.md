# reSID licensing under GPL-3.0-or-later

The project license selected on 2026-09-07 is **GPL-3.0-or-later**. The full
license text is in [LICENSE](../LICENSE), and Cargo uses the same SPDX
identifier. This resolves the earlier MIT compatibility question for the
reSID-derived filter tables through their existing GPL-2.0-or-later grant.
No additional upstream permission or special output exception is used.

## Material and provenance

The Rust exporter contains two tables in `crates/analyzer/src/export/synth.rs`:

| Local table | Upstream table | Local points |
|---|---|---|
| `F0_6581` | `f0_points_6581` | 27 |
| `F0_8580` | `f0_points_8580` | 17 |

The source is
[`filter.cc` at revision `02afcc5cefac34bd0c665dc0fa6b748d238c1831`](https://github.com/simonowen/resid/blob/02afcc5cefac34bd0c665dc0fa6b748d238c1831/filter.cc).
Its header names Copyright (C) 2004 Dag Lem and GPL-2.0-or-later. Per Jonsson
adapted the tables for sid-analyzer in 2026, removing duplicate endpoint knots
and using piecewise-linear interpolation. The tables convert SID cutoff
register values into filter frequencies in Pertylizer exports.

The upstream grant permits use under GPL version 3 or later. The project
retains the original attribution and GPL-2.0-or-later notice and distributes
the combined exporter under GPL-3.0-or-later. This does not relicense upstream
reSID as a whole or claim exclusive ownership of its measurements. See the
[GNU license compatibility guidance](https://www.gnu.org/licenses/license-list.html#GPLv2)
and the [third-party inventory](../THIRD_PARTY_NOTICES.md).

The existing grant supplies the distribution basis; no conclusion about
whether the numerical tables independently qualify for copyright protection
is needed for this licensing choice. The earlier MIT permission-request draft
has been superseded. No request was sent.

## Separate oracle tool

Neither reSID nor libresidfp is linked into the Rust analyzer. The separate
C++ oracle generator links libresidfp and retains its GPL-2.0-or-later notice
and [COPYING](../tools/digital-sid-oracle/COPYING). Its source revision and
build settings remain pinned in
[source.lock](../tools/digital-sid-oracle/source.lock).

## Exported projects and data

Running the analyzer does not by itself place its JSON, MIDI, WAV, or PTZ
output under GPL. The current exported YAMS template was inspected together
with an actual native PTZ export; no concrete need for an output exception was
identified. The template is ten lines of parameters and elementary arithmetic
for pulse-width modulation, and no reSID implementation or filter table is
embedded in it.

The 2026-09-07 inspection identified one script-producing path:
`authored_pwm_script` in `crates/analyzer/src/export/synth.rs`, serialized into
the `scr-1` module's `scripts["1"]` field. It emits six numeric parameter
declarations and four arithmetic statements for staircase triangle PWM.
The reSID-derived curves are evaluated inside the exporter and appear only
as numeric filter parameters and automation values. No reusable runtime or
third-party program is copied into the script.

All three script unit tests passed. A local Monty on the Run native export
contained one script matching that template; the checked Commando and Nemesis
exports contained none. These music-derived artifacts were kept outside the
repository. This source inspection identified no substantial third-party code
in the output; it does not establish the legal protectability of every template.

See the [GNU output FAQ](https://www.gnu.org/licenses/gpl-faq.en.html#GPLOutput) for the
distinction between data conversion and copying substantial program text into
output. Future embedded runtimes or substantial templates would need a new
review. Rights in the input music remain separate.

Pertylizer retains its own MIT license. Opening an exported project in that
separate application does not by itself change its license.

## Distribution requirements and remaining release work

Keep the root GPL license, original notices, and dependency notices with the
appropriate distributions. When distributing analyzer binaries, provide the
corresponding source in a manner permitted by GPLv3. Source releases should
include the build inputs and scripts needed for the supplied version.

The removed HVSC Songlengths snapshot and music assets have been purged from
local Git history. Remote history must also be replaced before publication;
choosing GPL does not supply missing rights to those materials. Songlengths is now supplied
locally by users. The hero and icon artwork has been confirmed by Per Jonsson
as his own generated images.
