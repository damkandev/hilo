# Contributing

<p align="center">
  <b>English</b> · <a href="CONTRIBUTING.es.md">Español</a>
</p>

## Build and test

```sh
HILO_KEYCHAIN=0 HILO_NOTIFY=0 cargo test --offline   # includes the 400k-step simulation
cargo test -- --ignored keychain                      # touches the real login Keychain
cargo vet
cargo build --release && bash scripts/e2e-drive.sh target/release/hilo   # real daemons in a temp dir
```

Every crate is vendored in `vendor/`, so the build never touches the network. `scripts/e2e-drive.sh` starts an anchor, a
member and a scoped sandbox, each with its own `HOME` and `HILO_HOME` under one temp dir, and checks drive sync,
conflicts, history, scoped tokens and the drive safety guards end to end; it never touches your own setup.

### The Mac app

`app/` is the desktop app, built with [gpui](https://gpui.rs). It is its own cargo workspace so gpui's
~700 crates stay out of the CLI, `vendor/` and `cargo vet`: the CLI's rules below don't apply to it.
It does nothing on its own; it runs the `hilo` CLI (`status --json`, `setup`, `add`, `remove`, `log`,
`restore`, `trash`). Its build uses crates.io, so build it from outside the repo's cargo config:

```sh
(cd / && cargo run --manifest-path "$PWD/app/Cargo.toml")   # uses ~/.local/bin/hilo, or a hilo next to the binary
app/bundle.sh                                               # app/dist/hilo.app and hilo-macos.dmg
```

`bundle.sh` signs ad hoc unless `SIGN_ID` names a Developer ID; a public `.dmg` also needs notarizing
(`xcrun notarytool submit … --wait`, then `xcrun stapler staple`).

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
