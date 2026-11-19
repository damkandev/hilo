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
| `hilo add DIR` | link another folder (or a single repo) |
| `hilo up [--read-only]` | in a sandbox's setup script: join, link, start, wait until in sync ([Cloud agents](cloud-agents.md)) |
| `hilo uninstall` | remove the service, hooks and config; your repos stay plain git repos |

`init`, `join` and `setup` link the folder you run them in, or `--folder DIR`. They also take
`--name NAME` and `--no-service` (don't install launchd or systemd).

## Day to day

| Command | |
| --- | --- |
| `hilo status [--latency]` | machines online, linked folders, leases, conflicts, waiting approvals, p50/p95 latency |
| `hilo approve [PATH \| --all \| --deny PATH]` | review a remote change to a path that runs code |
| `hilo run -- npm run dev` | run with the variables, without writing any file |

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
| `mirrors` | where mirrored branches go: `<repo>.worktrees/<branch>` next to the repo by default, a directory, or `off` |
| `env = strict` | never write `.env` files; variables only reach `hilo run` |
| `readonly` | machines that may not write here (`hilo trust`) |
| `bind` | listen on a different address than the one you advertise (NAT, containers) |
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
