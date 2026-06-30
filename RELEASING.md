# Releasing Telesthete

Current release: **0.2.0** (Telesthete wire **v1.2**, `PROTOCOL_VERSION = 3`).

Both reference implementations are pinned to the wire by shared conformance
vectors (`tests/vectors.json`). **Do not publish if the conformance tests fail** —
that is exactly the cross-impl drift the vectors exist to catch.

## Pre-flight (run everything green first)

```bash
# Python
python tests/test_vectors.py && python tests/test_negotiation.py \
  && python tests/test_replay.py && python tests/test_fragment.py \
  && python tests/test_end_to_end.py

# Rust
cd rust && cargo test && cargo build --release && cd ..
```

## PyPI (Python package `telesthete`)

```bash
python -m build            # produces dist/telesthete-0.2.0{.tar.gz,-py3-none-any.whl}
python -m twine check dist/*
python -m twine upload dist/*        # needs a PyPI token
```

Optional AES suite installs as an extra: `pip install telesthete[aes]`.

## crates.io (Rust crate `telesthete`)

```bash
cd rust
cargo publish -p telesthete --dry-run   # validate
cargo publish -p telesthete             # needs `cargo login`
```

`telesthete-c` (C ABI) and `telesthitium` (hub) can follow once `telesthete`
is up. The `examples/*` crates are not published.

## npm (browser/JS client) — not yet

Publishing a JS client to npm is **gated on building it first**: the
WebTransport (§9.6) + WebCodecs (§5.5) browser client described in the spec
does not exist yet. Once written, it should ship the baseline
ChaCha20-Poly1305 suite (e.g. via `@noble/ciphers`) so it interoperates with
the Python/Rust references, and validate against `tests/vectors.json`.

## Versioning

- The **doc/spec version** (`SPEC.md` title) and the **wire `PROTOCOL_VERSION`**
  are kept in lockstep (1.0→1, 1.1→2, 1.2→3). Bump `PROTOCOL_VERSION` only on a
  wire-breaking change.
- Package versions (`setup.py`, `rust/Cargo.toml`) track releases independently
  but should note the wire version they implement.
