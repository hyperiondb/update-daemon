# Deployment

The daemon keeps a host's `docker compose` stacks in sync with a git branch. It
does its own scheduling, so the only thing the OS has to do is keep the process
alive.

> **Only systemd is supported.** Linux + systemd is the single supported
> deployment target (amd64 and arm64). The binary builds on other platforms for
> development, but running it in production is supported only under systemd.

## Prerequisites

- `git` and `docker` (with the `docker compose` plugin) on `PATH`
- the service user in the `docker` group
- a git clone of the repo you want to keep deployed, with `origin` pointing at
  the remote and the working branch checked out

## 1. Install

### From the apt repository (recommended)

```bash
curl -fsSL https://hyperiondb.github.io/renew/install.sh | sudo bash
sudo apt-get install -y renew
```

This installs `/usr/bin/renew`, drops the systemd unit at
`/lib/systemd/system/renew.service`, and runs `systemctl daemon-reload`.
It does **not** enable or start the service yet — it has no config.

### Or build the `.deb` and install it directly

```bash
bash packaging/build-deb.sh                 # -> dist/renew_<version>_<arch>.deb
sudo apt install ./dist/renew_*.deb
```

## 2. Configure

The unit runs `renew run --config /etc/renew/renew.json`,
so put your config there:

```bash
sudo mkdir -p /etc/renew
sudo cp /usr/share/doc/renew/renew.example.json /etc/renew/renew.json
sudo "$EDITOR" /etc/renew/renew.json
renew --config /etc/renew/renew.json validate
```

`validate` prints each service's resolved schedule and its next three fire
times — confirm the staggering looks right before going live.

## 3. Enable + start

```bash
sudo systemctl enable --now renew
journalctl -u renew -f
```

`enable` makes it start on every boot; `--now` starts it immediately. There is
**no `.timer`** — the daemon does its own cron-style scheduling internally, and
`Restart=always` is the supervisor that keeps the process alive. Edits to the
config are hot-reloaded in place, so you never need to restart the unit after a
config change.

## Manual / dry runs

`renew sync` does a single reconcile pass and exits (codes: `0` ok,
`1` config/git, `2` compose op, `3` quorum refused); `--dry-run` shows exactly
what it would pull and recreate without touching git or docker. These are for
testing and ad-hoc runs — the supported unattended mode is the systemd service
above.

```bash
renew --config /etc/renew/renew.json sync --dry-run
```

## Multi-host clusters

Two options for updating a cluster one node at a time:

1. **One daemon per host, staggered schedules.** Give each host its own service
   entry with an offset schedule (`*/3`, `1-59/3`, `2-59/3`) and a shared
   `peer_urls` list. Each node filters itself out of its own peer list, so the
   config can be identical across hosts.

2. **Quorum gate only.** Use the same schedule everywhere but set
   `peers_required` so a node refuses to restart unless enough peers are
   healthy. Staggering minimizes contention; the quorum gate is the safety net.

If the whole cluster is down, every node's local health check fails, the peer
gate is bypassed, and all nodes recover together instead of waiting on each
other.

## Rollback

```bash
# from any laptop — no host access needed
git revert <bad-commit>
git push origin main
# hosts pick it up within one schedule interval
```

## Migrating volumes from another orchestrator

If containers were previously managed under a different compose project name,
set `compose_project_name` on the service so `docker compose` adopts the
existing named volumes instead of creating fresh ones.

## Building and publishing packages

`packaging/build-deb.sh` builds a `.deb` for the host architecture.
`.github/workflows/packages.yml` builds both amd64 and arm64 on native runners
and publishes a GPG-signed apt repository to GitHub Pages
(`packaging/build-apt-repo.sh`). The Pages job needs a `GPG_PRIVATE_KEY` repo
secret and GitHub Pages enabled for the repository.
