# Digital SID oracle generator

This development-only tool regenerates the committed digital SID oracle
vectors from the exact libresidfp release pinned in `source.lock`. It is not a
Cargo workspace member or a sid-analyzer runtime dependency. Normal CI consumes
the committed JSON without a C++ compiler or network access.

`generate.cpp` is GPL-2.0-or-later (see [COPYING](COPYING)) because it builds against and inspects the
GPL libresidfp implementation. No generator code or linked libresidfp object is
part of the Rust workspace or shipped analyzer runtime; only generated facts in
JSON are consumed by normal builds.

See the repository [third-party inventory](../../THIRD_PARTY_NOTICES.md) for
the separate reSID-derived filter measurements used by the Rust exporter.

The generator uses libresidfp's public digital clock, register, `OSC3`, and
`ENV3` APIs. It also copies the pinned build's documented opaque save-state into
the matching `State` layout to emit diagnostic accumulator, shift-register, and
envelope fields. Public reads own compatibility; internal fields only locate a
divergence.

## Check committed vectors

```bash
tools/digital-sid-oracle/regenerate.sh --check
```

The command downloads the release into a validated temporary directory,
verifies its SHA-256, builds a static library, runs the generator twice, checks
that both outputs are byte-identical, validates the temporary fixtures through
the strict Rust loader and reviewed policy, and compares every generated file
with the committed fixture. `source.lock` also pins the GCC compiler version;
the libresidfp and generator compiler flags are recorded separately. The build
runs with a sanitized environment, so caller-provided `CXXFLAGS`, `CPPFLAGS`,
`LDFLAGS`, include paths, and library paths cannot alter fixture provenance.
Regeneration fails instead of silently changing compiler-dependent provenance.

An already downloaded archive can be supplied explicitly:

```bash
tools/digital-sid-oracle/regenerate.sh --check --archive /path/to/libresidfp-1.1.2.tar.gz
```

To review newly defined vectors before the handwritten policy covers them,
write a verified, byte-stable candidate set to a new directory:

```bash
tools/digital-sid-oracle/regenerate.sh --output-dir /tmp/sid-oracle-review
```

The command refuses an existing destination. Candidate output deliberately
skips policy validation; `--write` still requires the strict Rust test to pass
before it replaces committed fixtures.

## Update committed vectors

```bash
tools/digital-sid-oracle/regenerate.sh --write
```

`--write` replaces only the files named by the generator after source
verification, two deterministic runs, JSON parsing by the Rust oracle test, and
successful generation. Review both the immutable oracle JSON and the separate
handwritten `manifest.json`; a changed reference value must not be accepted by
editing the golden to match `DigitalSid`.

Required host tools are Bash, `curl`, `sha256sum`, `tar`, `make`, `diff`, `cmp`,
Cargo, standard POSIX/GNU file utilities, and the GCC C++23 compiler version
pinned in `source.lock`. The release archive supplies its generated `configure`
script but still needs the normal host build tools. Set `CXX` to the matching
compiler binary when it is not available as `c++`; regeneration deliberately
fails when the reported compiler version differs from the lock.
