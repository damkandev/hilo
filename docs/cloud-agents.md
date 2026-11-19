# Cloud agents

<p align="center">
  <b>English</b> · <a href="cloud-agents.es.md">Español</a>
</p>

Agents that run in someone else's sandbox (Claude Code on the web, Codex, Cursor background agents,
Codespaces, Gitpod, Coder, dev containers) can join too. Each sandbox joins as its own short-lived
machine, and the branch it works on shows up on your Mac **while it works**, before anything is
pushed.

## Setup

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
service manager needed) and waits until it's in sync. `HILO_NAME` sets the machine's name
(`cloud-<host>` by default).

## One folder, for a while, read-only

A token can see less than everything:

```sh
hilo token create --addr hilo.example.com:7710 --path ~/notes/plans --expires 24h --read-only
```

- `--path P` limits the sandbox to one place, resolved on the anchor: inside a drive, that folder (or
  the whole drive); inside a repo, the whole repo (a repo is shared whole). Nothing outside reaches
  it: not the files, not the leases, not the history it catches up on. Its writes outside are dropped.
- `--expires 30m|24h|7d` ends the token: no new sandbox joins with it, and the connected ones are cut
  off within 30 seconds.
- `--read-only`: the anchor turns down every write from the sandbox (its version stays in its own
  history as a conflict and the file goes back). Run `sh -s -- up --read-only` (or set
  `HILO_READONLY=1`) so the sandbox's own tools refuse the write first.

`hilo token ls` shows what each token sees, when it expires and whether it's read-only.

## Proxies

Sandboxes that can only reach the internet through a proxy work as they are: hilo goes through
`HILO_PROXY`, `ALL_PROXY` or `HTTPS_PROXY` (HTTP CONNECT or SOCKS5) and honors `NO_PROXY`, so
Tailscale in userspace mode works too.

## What to know

| | |
| --- | --- |
| **Reachability** | The sandbox has to reach the anchor: use a VPS as anchor, a port forward, or a domain, and allow it in the platform's network settings. If the platform cuts the network once setup ends, hilo only syncs during setup. See [Deployment](deployment.md). |
| **Same repo, any clone** | A repo is its first commit plus `owner/repo`, so a clone made through the platform's git proxy is still the same repo. |
| **Secrets** | Without `--env` a sandbox never gets the vault keys. With it, it gets the shared layer; `hilo env prod` keys never leave your machine. |
| **Cleanup** | Sandboxes that haven't connected in 7 days are forgotten. `hilo token revoke ID` cuts every sandbox that used the token at once; an expired token does the same on its own. |
