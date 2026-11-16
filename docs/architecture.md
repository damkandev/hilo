# How hilo works

<p align="center">
  <b>English</b> · <a href="architecture.es.md">Español</a>
</p>

<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="../assets/how-dark.svg">
    <img src="../assets/how-light.svg" width="760" alt="A Mac and a server; the server runs the anchor that orders every change">
  </picture>
</p>

## One anchor

One machine, usually the server, gives every change a sequence number. The others open an outgoing,
encrypted connection to it, so nothing needs an open port at home. With a single orderer every
machine sees changes in the same order and converges, without clocks or CRDTs.

## The rule

A write travels with the hash of the version its writer had. The anchor accepts the first write on
a version and rejects the rest. hilo never merges two edits: the machine that lost saves its version
on a `hilo/conflict/<branch>/<date>` branch, with `git commit-tree` so the worktree isn't touched,
and takes the winner.

```mermaid
sequenceDiagram
    autonumber
    participant M as Mac (agent A)
    participant A as Anchor
    participant S as Server (agent B)
    Note over M,S: both have app.ts at version v1
    M->>A: write app.ts, base v1 → v2a
    S->>A: write app.ts, base v1 → v2b
    Note over A: base v1 is current: accept v2a as change 42
    A-->>M: change 42 applied (it's ours, nothing to do)
    A-->>S: change 42 applied (its own write is still pending: wait)
    Note over A: base v1 is no longer current: reject v2b
    A-->>S: rejected v2b, current is v2a (with its content)
    Note over S: save v2b on hilo/conflict/main/20261116-143733-…<br/>write v2a to app.ts<br/>tell agent B to re-read the file
```

The machine that lost does three things, in this order:

1. **Saves its version** with `git commit-tree` on `hilo/conflict/<branch>/<date>`, without touching
   the worktree or the branch. If the content is already in `HEAD` there's nothing to lose and it
   skips this. If saving fails, it leaves the file alone: it never overwrites what it couldn't save.
2. **Writes the winner** to the file.
3. **Tells the agent** that wrote the losing version, in its session: who won, where its version is
   (`git show <branch>:<path>`), and to re-read the file before editing again.

`hilo status` counts the saved branches; `git branch --list 'hilo/conflict/*'` lists them. At 500
saved branches a repo stops saving new ones (and so leaves conflicting files alone) until you delete
old ones.

A machine that was offline works the same way. When it reconnects it tells the anchor the last change
number it saw, receives everything after it, and then resends the writes the anchor never got. Each
one still carries its base, so a write made offline on a file that changed meanwhile ends up on a
conflict branch, not on top of the newer version.

A deterministic simulation (3 machines, 400,000 random steps with dropped connections) checks that
nothing is ever overwritten before it's saved.

## Live branches, not folders

A worktree is identified by its repo and branch, not its path: `~/code/x` on one machine and `/srv/x`
on another are the same repo if they share their first commit and `origin`. Commits travel as git
bundles, after their files, and the other machines move the branch with `git reset --mixed`, so the
worktree stays clean.

A new worktree on one machine appears on the others, by default in `<repo>.worktrees/<branch>` next
to the repo (see [`mirrors`](commands.md#configuration)). A repo created with `git init` in a linked
folder shows up on the other machines after its first commit, with its history and uncommitted
files.

## Leases

Claude Code's `PreToolUse` hook takes a 120-second lease on the file. The anchor broadcasts the lease
table, so the hook answers from the local daemon in a few milliseconds. Agents without hooks
(Codex, Cursor, OpenCode) use `claim`, `release` and `who` over MCP.

```mermaid
sequenceDiagram
    participant B as Agent B (server)
    participant DS as Server daemon
    participant A as Anchor
    participant DM as Mac daemon
    participant MA as Agent A (Mac)
    B->>DS: PreToolUse: Edit app.ts
    DS-->>B: allowed (local table: free)
    DS->>A: claim app.ts for session B
    A-->>DS: lease: B on server, 120 s
    A-->>DM: lease: B on server, 120 s
    MA->>DM: PreToolUse: Edit app.ts
    DM-->>MA: blocked: agent B on server has app.ts, for 2 min
```

The local daemon grants a free file at once and tells the anchor; the anchor has the last word and
broadcasts the result. Each edit (`PostToolUse`) renews the lease, and the session's leases are
released when the agent stops. If the daemon is down, the hooks let the edit through.

Leases only warn; [the rule](#the-rule) is what keeps writes safe.

## Receiving a change

Every machine decides on its own whether to apply what the anchor sends:

```mermaid
flowchart TD
    R[change from the anchor] --> P{safe path?<br/>no .git, no symlinks,<br/>no case collisions}
    P -- no --> X[refused]
    P -- yes --> L{unsent local edit<br/>to the same file?}
    L -- yes --> C[save it on a conflict branch]
    L -- no --> H
    C --> H{runs code, or the sender<br/>is trusted read-only?}
    H -- yes --> W[held until hilo approve]
    H -- no --> F[written: temp file with O_EXCL, then rename]
```

Paths that run code are listed in [SECURITY.md](../SECURITY.md#files). For `package.json`, only a
change to `scripts` waits for approval.

## Envs

`.env` and `.env.*` never travel as files. Each key lives in a per-repo vault (ChaCha20-Poly1305)
with two layers:

- **shared:** synced to every machine, and to cloud sandboxes whose token has `--env`
- **local:** never leaves the machine (`hilo env local KEY`, `hilo env prod KEY`)

hilo writes the merged file only where git ignores it. Claude Code's hooks stop agents from reading
it with their tools, but that's a barrier, not a guarantee: a shell can still print it, and agents
without hooks (Codex, Cursor) have no barrier at all. When that matters, set `env = strict` so no
file is ever written and variables only reach processes started with `hilo run`, and keep
production keys in `hilo env prod`.

## The binary

One Rust binary of about 1.6 MB. JSON, the wire format, the terminal UI and the FSEvents and inotify
bindings are written against the standard library and system calls. The only crates are for
cryptography: `snow`, `x25519-dalek`, `chacha20poly1305`, `sha2` and `hkdf`. See
[SECURITY.md](../SECURITY.md) for how they're pinned and checked.

| Module | |
| --- | --- |
| `anchor.rs` | orders changes, accepts or rejects writes, broadcasts leases |
| `daemon.rs` | watches linked folders, applies remote changes, mirrors branches |
| `sync.rs` · `git.rs` | file and commit transfer, conflict branches, bundles |
| `watch.rs` · `scan.rs` · `fsops.rs` | FSEvents and inotify, scanning, safe path handling |
| `net.rs` · `noise.rs` · `wire.rs` · `crypto.rs` | connections and proxies, the Noise channel, framing |
| `vault.rs` · `secret.rs` | the `.env` vault, the Keychain and `~/.hilo/secrets` |
| `lease.rs` · `hooks.rs` · `mcp.rs` | leases, Claude Code hooks, the MCP server |
| `setup.rs` · `install.rs` · `tui.rs` | the wizard, background services, the terminal UI |
