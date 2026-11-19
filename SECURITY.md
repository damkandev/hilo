# Security

<p align="center">
  <b>English</b> · <a href="SECURITY.es.md">Español</a>
</p>

## Reporting a vulnerability

Please don't open a public issue. Report it privately through GitHub:
**Security → Report a vulnerability** on
[damkandev/hilo](https://github.com/damkandev/hilo/security/advisories/new).

## Status

**hilo has had no external audit, and neither has `snow`: its README says so.** Read what follows as
a description of the design and its tests, not as a guarantee.

## Network

- **Pairing:** a single-use, 10-minute code mixed into a `Noise_XXpsk3_25519_ChaChaPoly_SHA256` handshake (run by `snow`), tested against the official cacophony vectors. After pairing, only known keys connect, and each side pins the other's key.
- **The anchor** listens only on the private address you choose. A public one needs `--public`. Before a peer authenticates: a 1 MiB frame cap, a 5-second handshake timeout and at most 16 pending connections.

## Scoped tokens

- **A token made with `--path`** sees one folder of a drive (or the whole drive), or one whole repo: its branches, its announcement and its sealed `.env` values (the vault keys still need `--env`). The anchor filters every message it sends that sandbox (catch-up, live changes, re-sends, leases) through one check, and drops its changes, claims and releases outside the scope without answering, because an answer would carry the content. What it can still see: the names of the machines online and the anchor's change counter.
- **`--read-only`:** the anchor turns down each write with the current version and ignores its claims. **`--expires`** and **`hilo token revoke`:** the token leaves the handshake list, a sandbox that joined with it can't reconnect, and a connected one is cut within 30 seconds.

## Files

- **Every receiver checks every path itself.** It refuses `.git` in any case or Unicode disguise, opens each parent with `openat(O_NOFOLLOW)`, creates temp files with `O_EXCL`, never sets setuid, and refuses names that collide on a case-insensitive disk.
- **Paths that run code** (`.claude/`, `.mcp.json`, `.envrc`, `.vscode/`, `.husky/`, `.github/workflows/`, `Makefile`, `package.json` scripts, `CLAUDE.md`, `AGENTS.md`) wait for `hilo approve`, which shows the diff.
- **Secrets never sync as files.** That covers key files and known token formats (AWS, Stripe live, GitHub, private keys).

## Keys and envs

- **Keys live in the macOS Keychain,** readable only by the signed hilo binary. On Linux they live in `~/.hilo/secrets` with mode 0600. The local socket never returns a secret value.
- **`.env` values** live in a per-repo vault (ChaCha20-Poly1305). The written `.env` file is only a barrier for agents with hooks; see [Envs](docs/architecture.md#envs) for `env = strict` and `hilo env prod`.

## Cryptography

No primitive or protocol state machine is written in this repo. The Noise handshake and transport
run on `snow`; X25519, ChaCha20-Poly1305, SHA-256 and HKDF come from `x25519-dalek`/`curve25519-dalek`
and RustCrypto (`chacha20poly1305`, `sha2`, `hkdf`), all pinned and vendored. On top, hilo refuses
low-order public keys (`snow` doesn't), and the anchor tries each candidate pairing code by replaying
its side of the handshake.

Tests check the crates against FIPS 180-4, RFC 4231, RFC 5869 and the cacophony vector.
Deterministic fuzz tests check that tampered, truncated, reordered and replayed handshake and
transport messages are refused, that running out of nonces is an error and not a reuse, and that
the decoder survives 200,000 mutated inputs.

**`unsafe`** only appears in the system bindings (FSEvents, inotify, the Keychain, `openat`). Every
module that handles network input is `#![forbid(unsafe_code)]`.

## Supply chain

`cargo vet` passes. `sha2`, `hmac`, `subtle`, `zeroize` and some smaller building blocks are covered
by audits imported from other organizations (ISRG among them; see `supply-chain/imports.lock`). The
rest of the crypto crates pass as explicit exemptions, not audited in this repo:

| Crate | Audit |
| --- | --- |
| `snow` | none formal, per its README |
| `x25519-dalek`, `curve25519-dalek` | none documented |
| `chacha20poly1305`, `poly1305` | NCC Group, 2020, per their README |
| `hkdf`, `digest` | exempted |

`supply-chain/config.toml` says which is which. CI builds with `--offline --locked` from the
vendored crates and runs `cargo vet --locked`.

## Verifying a download

The installer checks the SHA-256, which catches a corrupt download but comes from the same release
as the binary. The proof of origin is the GitHub build attestation: the installer checks it when
`gh` is logged in, and `HILO_REQUIRE_ATTESTATION=1` makes it mandatory. By hand:

```sh
gh attestation verify hilo-<target>.tar.gz --repo damkandev/hilo
```
