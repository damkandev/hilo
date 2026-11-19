# hilo drive: design (phases 1 and 2)

Internal design note for building the drive. It fixes the interfaces so the pieces can be built in
parallel. Read `docs/architecture.md` first.

Rules that do not change: one anchor orders everything; the first write on a base wins; nothing is
overwritten before it is saved; the only crates are for cryptography; `#![forbid(unsafe_code)]`
outside FFI; no wire format changes unless written here.

## Units

Today a unit of sync is a worktree, id `<repo id>:<branch>`. The drive adds one kind:

| Unit | Id | Where it comes from |
| --- | --- | --- |
| Git worktree | `<repo id>:<branch>` | unchanged |
| Drive folder | `drive:<label>` | `hilo add --drive PATH`, or `hilo add PATH` on a folder with no repos in it |

- `label` is the folder's name lowercased (`daemon::folder_label`), or `--as LABEL`. It must pass
  `sync::safe_name`. Machines match drives by label, the same way they match linked folders.
- Config line: `drive = <label> <absolute path>` (the path is the rest of the line, it may have
  spaces). Existing `folder =` and `repo =` lines keep their meaning: a folder of repos stays a
  folder of repos.
- A drive only syncs between machines that added it (v1). It does not appear on its own on other
  machines.
- Roots never overlap: adding a drive inside a linked folder, a repo or another drive is refused,
  and the other way round.
- Inside a drive, a directory that has a `.git` entry is skipped (logged once: "add it with `hilo
  add`"). `.env*` files and secret files are never synced from a drive (no vault for drives).
- Ignore rules in a drive: `.gitignore` files, same syntax and engine (`git check-ignore` with
  `GIT_DIR` pointing to an empty bare repo under `~/.hilo/tmp` and `GIT_WORK_TREE` = the drive),
  plus built-in excludes in that repo's `info/exclude`: `.DS_Store`, `node_modules/`, `.hilo-*`.
- Ops on `drive:*` are ordinary file ops: same `Op`, same anchor rule, same `validate`. No `REF`, no
  `META`, no vault.

## Version store (`src/store.rs`)

Every version of a file that passes through hilo on this machine is kept, so any file can be
recovered after hilo or anyone else overwrites or deletes it. Per machine, under `~/.hilo/store/`:

- `objects/<hex sha256>`: the content, written once (temp file + rename, 0600).
- `journal`: append-only records, `wire::W` encoding:
  `t_ms u64 · unit str · path str · hash Hash · kind u8 · origin str`.
  `hash == sync::NONE` records a deletion.

| kind | Meaning | Written when |
| --- | --- | --- |
| 0 `Local` | this machine produced it | `D::local_changes`, before `send_op` |
| 1 `Remote` | it arrived from `origin` | `D::apply_file`, `Effect::Write`, before writing the file |
| 2 `Conflict` | it lost the race | `D::apply_file`, `Effect::Save` |
| 3 `Replaced` | it was on disk and hilo overwrote it | `D::apply_file`, before `fsops::write`, only if that hash has no record yet for this path |
| 4 `Restored` | `hilo restore` brought it back | the restore command |

API (the daemon is the only writer; the CLI reads through the daemon):

```rust
pub enum Kind { Local, Remote, Conflict, Replaced, Restored }
pub struct Entry { pub t_ms: u64, pub unit: String, pub path: String, pub hash: Hash, pub kind: Kind, pub origin: String }
pub struct Store { /* index of the journal in memory */ }
impl Store {
    pub fn open() -> io::Result<Store>;                       // reads the journal, cuts a torn tail
    pub fn put(&mut self, unit: &str, path: &str, data: Option<&[u8]>, kind: Kind, origin: &str) -> io::Result<Hash>;
    pub fn get(&self, h: &Hash) -> io::Result<Option<Vec<u8>>>;
    pub fn history(&self, unit: &str, path: &str) -> Vec<&Entry>;   // newest first
    pub fn deleted(&self, unit: &str, prefix: &str) -> Vec<&Entry>; // paths whose newest entry is a deletion
    pub fn prune(&mut self, max_age_days: u64, max_bytes: u64) -> io::Result<(usize, u64)>; // (entries, bytes) dropped
}
```

- `put` must succeed before hilo overwrites or deletes a file. If it fails, hilo leaves the file
  alone and logs it, exactly like a failed conflict branch today.
- Applies to repos too (uncommitted files get a trash). In repos, `Effect::Save` still makes the
  `hilo/conflict/*` branch as today, and also records `Conflict` in the store.
- Vault values, `REF` and `META` are not stored here.
- Retention: config `history_days` (default 30) and `history_max_mb` (default 5120). `prune` drops
  the oldest entries first until both hold, never the newest entry of a path, then deletes objects
  no entry references. The daemon runs it at start and every hour.
- Conflict notice in a drive (to the losing session, same channel as today): "while you were editing
  PATH, an agent on WHO saved it first. Your version is in hilo's history (`hilo log PATH`,
  `hilo restore PATH --conflict`); the file now has theirs. Re-read it before editing again."

CLI (all through the daemon's IPC; `PATH` absolute or relative to the cwd, must be in a synced unit):

- `hilo log PATH`: versions of a file, newest first: time, kind, origin, size, short hash.
- `hilo restore PATH`: writes back the newest version that differs from what is on disk now.
  `--at N` (Nth line of `hilo log`), `--conflict` (newest `Conflict`). The restored content goes
  through the normal scan, so it syncs as a new write; recorded as `Restored`.
- `hilo trash [DIR]`: files deleted under DIR (default: the unit of the cwd) whose previous content
  is in the store, with when and who.

## File tools over MCP (`src/mcp.rs`, `src/grep.rs`)

`hilo mcp` keeps `claim`, `release`, `who` and adds:

| Tool | Arguments | Does |
| --- | --- | --- |
| `read` | `path`, `offset?`, `limit?` (lines) | Text content. Binary files (NUL in the first 8 KB) answer with their size and type, not bytes |
| `write` | `path`, `content` | Claims the file, writes it atomically, tells the daemon (`touch`) so it syncs at once. Returns notices (a lease held by someone else is refused with the holder) |
| `list` | `path?`, `depth?` (default 1) | Entries with size and type, skipping `.git` and hilo temp files |
| `search` | `pattern`, `path?`, `ignore_case?` | Literal match, `path:line: text`, at most 200 results |

- Paths must be inside a synced unit (`Shared::locate`); anything else is refused. That is what keeps
  a scoped sandbox inside its folder.
- `search` and `hilo grep PATTERN [DIR] [-i]` run in the daemon over the files the scanners already
  track (the synced set: ignored files are never searched), through IPC `["grep", pattern, dir, flags]`.
- If the machine joined with a read-only token (config `scope_ro = 1`), `write` is refused locally.
- Without a daemon every tool answers "hilo daemon is not running".

## Scoped tokens (`src/config.rs`, `src/anchor.rs`, `src/main.rs`)

`hilo token create [--name N] [--env] [--addr H:P] [--path P] [--expires 24h|7d|30m] [--read-only]`

- `CloudToken` gains `scope: Option<Scope>`, `expires: u64` (unix seconds, 0 = never) and `ro: bool`.
  The line format appends `scope expires ro` (scope `-` = none); old lines still parse.
- `Scope { unit: String, prefix: String }`, resolved on the anchor machine from `--path`:
  inside a drive → `unit = drive:<label>`, `prefix` = the path inside it (`""` for the whole drive);
  inside a repo → `unit = repo:<repo id>`, `prefix = ""`.
- `fn in_scope(s: &Scope, wt: &str, path: &str) -> bool`:
  - `drive:L` matches `wt == "drive:L"` and (`prefix == ""` or `path == prefix` or `path` starts with `prefix + "/"`)
  - `repo:R` matches `wt` starting with `R:`, `wt == "vault:R"`, `wt == "repo:R"`
- The anchor learns a client's token from `Peer::cloud_token()` at `Connect` and keeps
  `scope`, `ro` and `expires` on `Client`. A scoped client:
  - receives only in-scope keys: catch-up (`Hello`), broadcasts in `accept`, `Resync`, `NeedFull`,
    and `Lease` messages;
  - has out-of-scope `Op`, `Claim` and `Release` dropped silently (no `Reject`: it would leak content);
  - with `ro`, an in-scope `Op` gets a `Reject` with the current version (its edit ends in its own
    store as a conflict and the file goes back);
  - receives vault keys only with `--env`, as today.
- Expired tokens are left out of the handshake PSKs, and the anchor kicks connected clients whose
  token expired (check every 30 s).
- `hilo token ls` shows scope, expiry and read-only.
- On join, the sandbox stores `scope_ro = 1` in its config when the token is read-only (the anchor
  does not tell it; `hilo up` learns it from a `--read-only` flag or from `HILO_READONLY=1`).
  The anchor's check is what enforces it.

## Tests

- `cargo test --release --offline --locked`, with `HILO_KEYCHAIN=0 HILO_NOTIFY=0` and `HILO_HOME` set
  to a temp dir. Never run a daemon or the CLI against the real `~/.hilo`.
- Each piece leaves its own focused tests; the simulation in `sync.rs` must keep passing.
- End to end (after merge): two daemons with separate `HILO_HOME`s on another port, Mac and acer.
