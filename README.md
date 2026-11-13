<p align="center">
  <img src="assets/icon.svg" width="72" height="72" alt="">
</p>

<h1 align="center">hilo</h1>

<p align="center">
  The same code, live on all your machines, while your agents work.<br>
  No push, no pull, no copying <code>.env</code> files around.
</p>

<p align="center">
  <b>English</b> · <a href="README.es.md">Español</a>
</p>

<p align="center">
  <img src="assets/wizard.svg" width="678" alt="The hilo wizard pairing a Mac with a server called devbox">
</p>

An agent on your Mac and another on your server each work on their own copy of the repo. What one
changes, the other doesn't see until someone commits, pushes and pulls. Meanwhile they step on each
other, work on stale code, and every `.env` has to be copied by hand to every machine and worktree.

hilo sits underneath your agents (Claude Code, Codex, T3 Code, Cursor, OpenCode, anything that writes
files) and keeps the machines in step at the level of files and git.

## Get started

```sh
curl -fsSL https://raw.githubusercontent.com/damkandev/hilo/main/install.sh | sh
hilo
```

`hilo` asks which folder to keep in sync and where your other machine is (it lists the hosts in
`~/.ssh/config`). Then it does the rest over SSH:

1. installs hilo on the other machine and starts it as the **anchor**
2. pairs both machines (the join code travels over SSH; you never copy it)
3. links the folder on both sides and waits until every repo is in sync
4. installs a background service, the Claude Code hooks and the MCP server, on both

From then on, every repo in that folder syncs, including the ones you create later.

<details>
<summary>Without the wizard</summary>

```sh
hilo setup devbox --folder ~/code            # same thing, no questions

# or by hand, from the folder you want to link:
hilo init --listen 10.0.0.12:7710      # on the machine the others can reach
hilo join HILO-7KQ2-M9PA-XR4T@10.0.0.12:7710   # on every other machine
```

</details>

## What you get

| | |
| --- | --- |
| **Every write, everywhere** | A file an agent writes appears on the other machine in about 50 ms on a home Wi-Fi. |
| **Nothing is lost** | If two machines change the same file at once, one wins and the other's version is kept on a `hilo/conflict/<branch>/<date>` branch. |
| **Agents hear about it first** | Before an agent edits a file another agent is editing, Claude Code blocks the edit and tells it who has the file, where, and for how long. |
| **Branches and commits** | Commits travel with the files, branches move on every machine, and a new worktree on one machine appears on the others. |
| **New repos on their own** | `git init` a repo in a linked folder, make the first commit, and it shows up on the other machine with its history and uncommitted files. |
| **`.env` without copying** | Variables are encrypted, synced key by key, and written into every worktree that needs them. Keys can stay per machine or production-only. |

## Cloud agents

Agents that run in someone else's sandbox (Claude Code on the web, Codex, Cursor background agents,
Codespaces, Gitpod, Coder, dev containers) can join too. Each sandbox joins as its own short-lived
machine, and the branch it works on shows up on your Mac **while it works**, before anything is
pushed.

On the anchor, make a token (or run `hilo` and pick **Let cloud agents in**):

```sh
hilo token create --addr hilo.example.com:7710        # add --env to share .env values with them
```

Then, on the platform:

1. add the token as a secret: `HILO_TOKEN=HILO-CLOUD-…@hilo.example.com:7710`
2. add one line to its setup script:

   ```sh
   curl -fsSL https://raw.githubusercontent.com/damkandev/hilo/main/install.sh | sh -s -- up
   ```

`hilo up` joins with the token, links the repo it's run in, starts the daemon in the background (no
service manager needed) and waits until it's in sync. Sandboxes that can only reach the internet
through a proxy work as they are: hilo goes through `HTTPS_PROXY` or `ALL_PROXY` (HTTP CONNECT or
SOCKS5), so Tailscale in userspace mode works too.

| | |
| --- | --- |
| **Reachability** | The sandbox has to reach the anchor: use a VPS as anchor, a port forward, or a domain, and allow it in the platform's network settings. If the platform cuts the network once setup ends, hilo only syncs during setup. |
| **Same repo, any clone** | A repo is its first commit plus `owner/repo`, so a clone made through the platform's git proxy is still the same repo. |
| **Secrets** | Without `--env` a sandbox never gets the vault keys. With it, it gets the shared layer; `hilo env prod` keys never leave your machine. |
| **Cleanup** | Sandboxes that haven't connected in 7 days are forgotten. `hilo token revoke ID` cuts every sandbox that used the token at once. |

## How it works

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/how-dark.svg">
    <img src="assets/how-light.svg" width="760" alt="A Mac and a server; the server runs the anchor that orders every change">
  </picture>
</p>

**One anchor.** One machine, usually the server, gives every change a sequence number. The others
open an outgoing, encrypted connection to it, so nothing needs an open port at home. With a single
orderer every machine sees changes in the same order and converges, without clocks or CRDTs.

**The rule.** A write travels with the hash of the version its writer had. The anchor accepts the
first write on a version and rejects the rest. hilo never merges two edits: the machine that lost
saves its version on a conflict branch, with `git commit-tree` so the worktree isn't touched, and
takes the winner. A deterministic simulation (3 machines, 400,000 random steps with dropped
connections) checks that nothing is ever overwritten before it's saved.

**Live branches, not folders.** A worktree is identified by its repo and branch, not its path:
`~/code/x` on one machine and `/srv/x` on another are the same repo if they share their first commit
and `origin`. Commits travel as git bundles, after their files, and the other machines move the
branch with `git reset --mixed`, so the worktree stays clean.

**Leases.** Claude Code's `PreToolUse` hook takes a 120-second lease on the file. The anchor broadcasts
the lease table, so the hook answers from the local daemon in a few milliseconds. Agents without
hooks use `claim`, `release` and `who` over MCP. Leases only warn; the rule above is what keeps
writes safe.

**Envs.** `.env` and `.env.*` never travel as files. Each key lives in a per-repo vault
(ChaCha20-Poly1305) with a shared layer and a layer that never leaves the machine. hilo writes the
merged file only where git ignores it, and agents can't read it through their tools.

## Commands

You only need `hilo`. Everything else is there when you want to look or change something.

| Command | |
| --- | --- |
| `hilo` | the wizard; on a machine that's set up, a menu |
| `hilo status [--latency]` | machines online, linked folders, leases, conflicts, waiting approvals, p50/p95 latency |
| `hilo add DIR` | link another folder (or a single repo) |
| `hilo env set KEY=value` · `unset KEY` | change a variable everywhere (`--file apps/web/.env` in monorepos) |
| `hilo env local KEY` | every machine keeps its own value, e.g. `DATABASE_URL` |
| `hilo env prod KEY` | only on this machine, only reachable through `hilo run` |
| `hilo env ls` · `history [KEY]` · `revert KEY [N]` | layers, the last 50 versions per file, going back |
| `hilo run -- npm run dev` | run with the variables, without writing any file |
| `hilo approve [PATH \| --all \| --deny PATH]` | review a remote change to a path that runs code |
| `hilo trust MACHINE read-only` | that machine may read from this one but not write to it |
| `hilo invite` · `hilo unpair MACHINE` | a new join code · cut a machine off and rotate the vault key |
| `hilo token create` · `ls` · `revoke ID` | reusable tokens for cloud sandboxes |
| `hilo up` | in a sandbox's setup script: join, link, start, wait until in sync |
| `hilo uninstall` | remove the service, hooks and config; your repos stay plain git repos |

## Security

- **Pairing:** a single-use, 10-minute code mixed into a `Noise_XXpsk3_25519_ChaChaPoly_SHA256` handshake, tested against the official cacophony vectors. After pairing, only known keys connect, and each side pins the other's key.
- **The anchor** listens only on the private address you choose. A public one needs `--public`. Before a peer authenticates: a 1 MiB frame cap, a 5-second handshake timeout and at most 16 pending connections.
- **Every receiver checks every path itself.** It refuses `.git` in any case or Unicode disguise, opens each parent with `openat(O_NOFOLLOW)`, creates temp files with `O_EXCL`, never sets setuid, and refuses names that collide on a case-insensitive disk.
- **Paths that run code** (`.claude/`, `.mcp.json`, `.envrc`, `.vscode/`, `.husky/`, `.github/workflows/`, `Makefile`, `package.json` scripts, `CLAUDE.md`, `AGENTS.md`) wait for `hilo approve`, which shows the diff.
- **Secrets never sync as files.** That covers key files and known token formats (AWS, Stripe live, GitHub, private keys).
- **Keys live in the macOS Keychain,** readable only by the signed hilo binary. On Linux they live in `~/.hilo/secrets` with mode 0600. The local socket never returns a secret value.
- **Dependencies:** cryptography is the only thing that comes from crates, and those are vendored and checked with `cargo vet`.

## Configuration

`~/.hilo/config` is plain `key = value`. You rarely need to touch it.

| Key | |
| --- | --- |
| `folder` | a linked folder (one line each) |
| `mirrors` | where mirrored branches go: `<repo>.worktrees/<branch>` next to the repo by default, a directory, or `off` |
| `env = strict` | never write `.env` files; variables only reach `hilo run` |
| `readonly` | machines that may not write here (`hilo trust`) |
| `bind` | listen on a different address than the one you advertise (NAT, containers) |

## Develop

```sh
HILO_KEYCHAIN=0 HILO_NOTIFY=0 cargo test --offline   # includes the 400k-step simulation
cargo test -- --ignored keychain                      # touches the real login Keychain
cargo vet
```

One Rust binary of about 1.5 MB. SHA-256, HMAC, HKDF, Noise, JSON, the wire format, the terminal UI
and the FSEvents and inotify bindings are written against the standard library and system calls.
`x25519-dalek` and `chacha20poly1305` are the only crates.

**Not in v1:** Windows, NAT traversal (the anchor needs an address the others can reach), and syncing
the agent's conversation.

MIT licensed.
