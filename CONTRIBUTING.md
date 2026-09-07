# Contributing

The project is in active development. Public APIs and export schemas can change
without backward compatibility until the first tagged release. Check the
[active roadmap](plans/PLAN.md) before starting substantial work.

## Development setup

Install Rust through rustup, then build from the repository root. The pinned
toolchain includes rustfmt and Clippy. The default workspace needs no music
collection, audio device, Pertylizer process, or C++ oracle generator.

```sh
cargo build --workspace --locked
cargo run -p sid-analyzer --bin sid-analyzer -- --help
```

## Changes and verification

Keep code, comments, CLI messages, documentation, and commits in English.
Follow [AGENTS.md](AGENTS.md), including domain newtypes, typed errors, and no
`unwrap()` or `expect()` outside tests. Add synthetic regression coverage for
behavior changes; keep third-party music and generated exports out of commits.

Run the complete gate before committing:

```sh
cargo fmt --check
cargo build --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

When touching the optional SQLite scanner, also run build, Clippy, and tests
with `--features corpus-scan`. CI checks both feature configurations on Linux.

CI also requires full JSON Schema validation of synthetic PTZ exports. To run
that job locally, install its declared Python dependency in a virtual environment:

```sh
python3 -m venv /tmp/sid-schema-venv
/tmp/sid-schema-venv/bin/python -m pip install -r tools/schema-validation/requirements.txt
SID_SCHEMA_PYTHON=/tmp/sid-schema-venv/bin/python \
  cargo test --workspace --locked --features schema-tests --test synth_schema
```

The `schema-tests` feature checks faithful, modern-analog, and enhancement
exports under PAL and NTSC against the mirrored project schema. It fails if
Python or `jsonschema` is unavailable; there is no skip fallback. It also
checks that malformed output is rejected. The ordinary Rust gate does not
require Python. Schema validity does not prove that live rendering succeeds.

Tests behind `asset-tests` require a separately obtained, licensed corpus under
`assets/music/`. Some manual diagnostic tests are ignored even when that feature
is enabled. Do not enable all features in public CI or attach copyrighted SID
files to issues. Prefer a synthetic reproducer, command line, error output,
toolchain version, and relevant input metadata.

The [oracle generator](tools/digital-sid-oracle/README.md) is a separate GPL
development tool with its own pinned C++ compiler. Normal builds and tests
consume the checked-in JSON vectors without regenerating them.

## Licensing

Contributions to the project's own code and documentation are provided under
`GPL-3.0-or-later`, as described in [LICENSE](LICENSE), unless explicitly agreed
otherwise. Preserve existing third-party notices and record the source,
revision, license, and modifications when introducing external material.

Generate the Cargo license inventory with `cargo-about` installed:

```sh
cargo about generate --workspace --all-features --locked --offline --fail \
  --format json --output-file /tmp/sid-analyzer-licenses.json
```

[about.toml](about.toml) records the accepted dependency licenses. This checks
Cargo packages; manually review vendored data and tools using
[THIRD_PARTY_NOTICES.md](THIRD_PARTY_NOTICES.md). Binary releases must include
the applicable notices and provide corresponding source under GPL.
