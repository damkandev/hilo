# Deployment

<p align="center">
  <b>English</b> · <a href="deployment.es.md">Español</a>
</p>

What runs where, which address to use, and how it stays running. The wizard (`hilo`) does all of
this for the common case; this page is for when you want to know or do it by hand.

## Topology

```
  laptop  ──┐
            ├──►  anchor (server or VPS)  ◄──  cloud sandbox
  desktop ──┘
```

- **The anchor** is the one machine that orders changes. Pick one that's always on and that the
  others can reach: a home server, a devbox or a small VPS.
- **Every other machine** opens an outgoing connection to it. Nothing at home needs an open port.
- **No NAT traversal in v1.** A machine that can't reach the anchor's address can't join.

## The anchor's address

```sh
hilo init --listen 10.0.0.12:7710                              # the address the others will dial
hilo init --listen 203.0.113.10:7710 --public                  # a public address must be explicit
hilo init --listen 203.0.113.10:7710 --public --bind 0.0.0.0:7710   # behind NAT or in a container
```

| Flag | |
| --- | --- |
| `--listen` | the IP and port others dial. Defaults to this machine's first private IP on port 7710. Never `0.0.0.0`. |
| `--public` | required for a public IP. Private ranges, loopback and Tailscale's `100.64.0.0/10` don't need it. |
| `--bind` | where the socket actually listens, when it differs from the advertised address (port forward, Docker). Saved as `bind` in `~/.hilo/config`. |

Before a peer authenticates, the anchor caps frames at 1 MiB, gives the handshake 5 seconds and
holds at most 16 pending connections.

| Setup | Anchor address |
| --- | --- |
| Mac and home server on the same LAN | the server's LAN IP |
| Machines on Tailscale or WireGuard | the server's tailnet IP |
| Cloud sandboxes | a VPS IP with `--public`; tokens can advertise a domain with `--addr` |

## Background service

`hilo`, `setup`, `init` and `join` install a per-user service that runs the daemon:

| OS | Service | Logs |
| --- | --- | --- |
| macOS | launchd agent in `~/Library/LaunchAgents/`, starts at login | `~/Library/Logs/hilo.log` |
| Linux | systemd user unit `~/.config/systemd/user/hilo.service` | `journalctl --user -u hilo` |

On a Linux server, keep it running after you log out:

```sh
sudo loginctl enable-linger $USER
```

Without a service manager (containers, sandboxes), `hilo up` starts the daemon in the background and
logs to `~/.hilo/daemon.log`. `--no-service` skips the service on `setup`, `init` and `join`.

## Files

| Path | |
| --- | --- |
| `~/.hilo/config` | settings ([reference](commands.md#configuration)) |
| `~/.hilo/key` · `peers` · `tokens` | this machine's identity, pinned peer keys, cloud tokens |
| `~/.hilo/secrets` | Linux only, mode 0600: the keys that live in the Keychain on macOS |
| `~/.hilo/store` | the version history behind `hilo log`, `restore` and `trash`, mode 0600 (`history_days`, `history_max_mb`) |

`HILO_HOME` moves all of it. `hilo uninstall` removes the service, hooks and config; your repos stay
plain git repos. It also removes the version history in the store.

## Platforms

| | |
| --- | --- |
| macOS | Apple Silicon and Intel |
| Linux | x86_64 and aarch64, static (musl): any distro, Alpine and old glibc included |
| Windows | not in v1 |

## Updating

Run the installer again. It replaces the binary in `~/.local/bin` (or `HILO_BIN_DIR`); pin a release
with `HILO_VERSION=v0.3.0`.
