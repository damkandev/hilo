# Commands and configuration

<p align="center">
  <b>English</b> · <a href="commands.es.md">Español</a>
</p>

You only need `hilo`. Everything else is there when you want to look or change something.

## Setup

| Command | |
| --- | --- |
| `hilo` | the wizard; on a machine that's set up, a menu |
| `hilo setup HOST [--folder DIR]` | the wizard without questions: set up `HOST` over SSH and pair this machine with it |
| `hilo init [--listen IP:PORT] [--public] [--bind IP:PORT]` | make this machine the anchor and print a join code ([Deployment](deployment.md)) |
| `hilo join CODE@IP:PORT` | join an anchor |
| `hilo add DIR [--as LABEL]` | link another folder of repos, or a single repo. A folder with no repos in it becomes a [drive](architecture.md#drive-folders): its files sync as they are, matched across machines by `LABEL` (default: the folder's name, lowercased) |
| `hilo add --drive DIR [--as LABEL]` | link `DIR` as a drive even if it has repos in it (they are skipped: add each with `hilo add`) |
| `hilo up [--read-only] [--drive LABEL]` | in a sandbox's setup script: join, link, start, wait until in sync ([Cloud agents](cloud-agents.md)). `--drive LABEL` links the folder (created if missing) as that drive |
| `hilo uninstall` | remove the service, hooks and config; your repos stay plain git repos |

`init`, `join` and `setup` link the folder you run them in, or `--folder DIR`. They also take
`--name NAME` and `--no-service` (don't install launchd or systemd).

## Day to day

| Command | |
| --- | --- |
| `hilo status [--latency]` | machines online, linked folders, leases, conflicts, history, waiting approvals, p50/p95 latency; in a sandbox with a scoped token, its scope and the files kept there because they are outside it |
| `hilo approve [PATH \| --all \| --deny PATH]` | review a remote change to a path that runs code |
| `hilo log PATH` | every version of a file this machine kept, newest first: when, how (`local`, `remote`, `conflict`, `replaced`, `restored`), who, size |
| `hilo restore PATH [--at N \| --conflict]` | bring a version back: the newest one that differs from the file, line `N` of `hilo log`, or the one that lost the last conflict. It syncs as a new write |
| `hilo trash [DIR]` | files deleted under `DIR` (default: the whole repo) that hilo can bring back |

History starts with the first version hilo sees after you upgrade: a file nobody has touched since
has no history on this machine yet. If you delete one of those here, the other machines still keep
it, because the delete reaches them as a `replaced` version.
| `hilo run -- npm run dev` | run with the variables, without writing any file |
| `hilo grep PATTERN [DIR] [-i]` | literal search over the synced files under `DIR` (default: here); ignored files are skipped |

## Agents over MCP

`hilo mcp` is an MCP server on stdio for agents without hooks, and for sandboxes that should only
touch synced files. Paths are absolute or relative to the agent's working directory, and must be
inside a synced folder; anything else is refused. Without a daemon, the file tools answer "hilo
daemon is not running".

| Tool | |
| --- | --- |
| `claim` · `release` · `who` | take a file before editing it, let it go, see who holds it |
| `read` `path` `offset?` `limit?` | text of a file, cut at 256 KB (`offset`/`limit` in lines); binary files answer with their size |
| `write` `path` `content` | claims the file, writes it whole and atomically, syncs it at once; refused if another agent holds it, or on a read-only token |
| `list` `path?` `depth?` | files and folders with sizes, `depth` 1 to 4 |
| `search` `pattern` `path?` `ignore_case?` | same as `hilo grep`: `path:line: text`, at most 200 results |

## Environment variables

| Command | |
| --- | --- |
| `hilo env set KEY=value` · `unset KEY` | change a variable everywhere (`--file apps/web/.env` in monorepos) |
| `hilo env local KEY` | every machine keeps its own value, e.g. `DATABASE_URL` |
| `hilo env prod KEY` | only on this machine, only reachable through `hilo run` |
| `hilo env ls` · `history [KEY]` · `revert KEY [N]` | layers, the last 50 versions per file, going back |

## Machines and access

| Command | |
| --- | --- |
| `hilo invite` | a new single-use join code (anchor) |
| `hilo unpair MACHINE` | cut a machine off and rotate the vault key (anchor) |
| `hilo trust MACHINE read-only\|full` | stop (or allow again) a machine writing here |
| `hilo token create [--env] [--addr HOST:PORT] [--path P] [--expires 24h] [--read-only]` · `ls` · `revoke ID` | reusable tokens for cloud sandboxes, optionally limited to one folder or repo, for a while, without writing (anchor) |

## Configuration

`~/.hilo/config` is plain `key = value`. You rarely need to touch it.

| Key | |
| --- | --- |
| `folder` | a linked folder (one line each) |
| `drive` | a drive: `drive = <label> <absolute path>`, the path may have spaces |
| `mirrors` | where mirrored branches go: `<repo>.worktrees/<branch>` next to the repo by default, a directory, or `off` |
| `env = strict` | never write `.env` files; variables only reach `hilo run` |
| `readonly` | machines that may not write here (`hilo trust`) |
| `bind` | listen on a different address than the one you advertise (NAT, containers) |
| `history_days` | how long old versions stay in `~/.hilo/store` (default `30`); the newest version of each file always stays |
| `history_max_mb` | the most that history may take, in MB (default `5120`); the oldest versions go first |
| `scope_ro = 1` | this sandbox joined with a read-only token: its own tools refuse writes (`hilo up --read-only`) |

## Environment

| Variable | |
| --- | --- |
| `HILO_HOME` | use another directory instead of `~/.hilo` |
| `HILO_TOKEN` · `HILO_NAME` · `HILO_READONLY=1` | token, machine name and read-only for `hilo up` |
| `HILO_PROXY` | proxy to use, before `ALL_PROXY` and `HTTPS_PROXY` |
| `HILO_KEYCHAIN=0` | keep keys in `~/.hilo/secrets` instead of the macOS Keychain |
| `HILO_NOTIFY=0` | no desktop notifications |
| `HILO_VERSION` · `HILO_BIN_DIR` | installer: which release (default `latest`) and where to put it (default `~/.local/bin`) |
| `HILO_REQUIRE_ATTESTATION=1` | installer: refuse a download without a valid GitHub build attestation |
