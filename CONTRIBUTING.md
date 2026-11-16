# Contributing

<p align="center">
  <b>English</b> · <a href="CONTRIBUTING.es.md">Español</a>
</p>

## Build and test

```sh
HILO_KEYCHAIN=0 HILO_NOTIFY=0 cargo test --offline   # includes the 400k-step simulation
cargo test -- --ignored keychain                      # touches the real login Keychain
cargo vet
```

Every crate is vendored in `vendor/`, so the build never touches the network.

## Rules of the codebase

- **Only cryptography comes from crates.** JSON, the wire format, the terminal UI and the file
  watchers are written against the standard library and system calls. A new dependency needs a very
  good reason, a pinned `=` version, a vendored copy and a `cargo vet` entry.
- **No hand-written crypto.** Primitives and protocol state machines come from `snow`, dalek and
  RustCrypto.
- **`unsafe` stays in the system bindings.** Modules that handle network input are
  `#![forbid(unsafe_code)]`.
- **Sync changes run against the simulation.** Anything that touches ordering, conflicts or the
  wire should keep the 400,000-step simulation green.

Security issues go through [SECURITY.md](SECURITY.md), not public issues.

## CI

[`ci.yml`](.github/workflows/ci.yml) runs on every push to `main` and every pull request:

- `cargo test --release --offline --locked` on macOS, Linux x86_64 and Linux arm64
- `cargo vet --locked`

## Releases

Pushing a `v*` tag runs [`release.yml`](.github/workflows/release.yml):

1. builds `aarch64-apple-darwin`, `x86_64-apple-darwin`, `x86_64-unknown-linux-musl` and
   `aarch64-unknown-linux-musl`
2. signs and notarizes the macOS binaries when the Apple secrets are set
3. attests every tarball and `SHA256SUMS` with GitHub build provenance
4. publishes a GitHub release that [`install.sh`](install.sh) downloads

Bump `version` in `Cargo.toml` before tagging.
