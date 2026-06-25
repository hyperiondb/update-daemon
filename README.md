# renew

A **self-healing deployment daemon**: a single Rust binary that keeps a
host's running `docker compose` stacks in sync with a git repository, on a
cron-like schedule, driven by **one JSON config file in the project root** and
nothing else.

It is a long-running, self-scheduling reimplementation of the classic
`systemd timer + sync.sh` pull-deploy loop. Drop `renew.json` in your
repo, point the daemon at it, and every service is reconciled on its own
schedule — pulling new images and recreating only the containers whose image
actually drifted, gated by a peer-health quorum so a cluster never restarts
itself into an outage.

Status: **in progress**

---

## Features

- **One config file, no other state.** Everything lives in a single
  `renew.json` at the repo root: the services, their schedules, health
  checks, and env files. There are no systemd timers, drop-in overrides, or
  `/etc/*.env` files to maintain. Edit the file and the running daemon
  **hot-reloads** it — no restart, no redeploy.

- **Cron + interval scheduling, per service.** Each service has its own
  schedule: a 5-field cron expression (`*/3 * * * *`), a staggered offset
  (`1-59/3 * * * *`), an interval (`@every 90s`, `@every 1h30m`), or a macro
  (`@hourly`, `@daily`, `@weekly`). Staggering lets a cluster update **one node
  at a time** so quorum is preserved throughout.

- **Image-drift reconciliation, not git-diff.** A git push is *not* a restart
  signal. The daemon advances the working tree, then compares the **image SHA a
  container is running** against the **image its compose file declares**. Only
  the services that actually drifted are recreated — with `up -d --no-deps`, so
  unrelated containers are never touched.

- **Env-file rotation detection.** The env file lives outside git (secrets), so
  it is hashed (SHA-256) and compared to a stored hash. A change escalates to a
  **full-stack restart** of that service, because rotated secrets affect every
  container that consumes them. The hash is only persisted on a clean run, so a
  failed tick safely retries.

- **Peer-health quorum gate with recovery bypass.** Before restarting, if the
  local service is **up**, the daemon requires a quorum of peers to be healthy —
  refusing (and retrying next tick) rather than dropping the cluster below
  quorum. If the local service is **down**, the gate is bypassed (restarting a
  dead node cannot reduce capacity), so a cluster-wide outage can always
  self-recover instead of deadlocking. Identical peer lists can be deployed to
  every node — each node filters itself out by hostname/IP.

- **Self-healing by construction.** A panicking or failing sync is isolated to
  its task, logged, and retried on the next tick — it never takes down the
  daemon or the other services. An **overlap guard** skips a tick if the
  previous run is still in flight; **graceful shutdown** drains in-flight syncs
  on SIGTERM/Ctrl-C; and the OS supervisor (`Restart=always`) is the outer loop.

- **Daemon or oneshot.** Run `renew run` for the built-in scheduler, or
  `renew sync` for a single reconcile pass that exits with the original
  `sync.sh` codes (`0` ok, `1` config/git, `2` compose op, `3` quorum refused) —
  drop-in for anyone who prefers an external systemd timer or cron.

- **Dry-run & validate.** `renew sync --dry-run` logs exactly what it
  would pull and recreate without touching git or docker;
  `renew validate` resolves the config and prints each service's
  schedule and next fire times.

- **Structured logging.** Human-readable text or line-delimited JSON
  (`"log_format": "json"`), filtered with `RUST_LOG`.

- **Fully testable, fully tested.** Every side effect — process exec, HTTP
  health probes, the clock — sits behind a trait, so the whole engine runs
  against fakes in unit tests, plus an end-to-end test that drives the **real
  binary** against a **real temporary git repo** and a mock docker.

- **Linux + systemd, packaged as a `.deb`.** The supported deployment target is
  Linux managed by **systemd** (the only supported service manager), shipped as
  a `.deb` for **amd64 and arm64** that installs the binary and registers the
  unit automatically. The binary also compiles on macOS/Windows for development,
  but those are not supported for running in production.

---

## Install (Debian / Ubuntu)

> **Only systemd is supported.** The package installs the binary to
> `/usr/bin/renew` and registers the `renew.service` systemd
> unit automatically. `git` and `docker` (with the compose plugin) must be on
> `PATH`.

From the apt repository (amd64 and arm64):

```bash
curl -fsSL https://hyperiondb.github.io/renew/install.sh | sudo bash
sudo apt-get install -y renew
```

Then configure and start it — the unit reads `/etc/renew/renew.json`:

```bash
sudo mkdir -p /etc/renew
sudo cp /usr/share/doc/renew/renew.example.json /etc/renew/renew.json
sudo "$EDITOR" /etc/renew/renew.json
sudo renew --config /etc/renew/renew.json validate   # sanity-check
sudo systemctl enable --now renew                                    # start + run on boot
journalctl -u renew -f
```

`systemctl enable` makes the daemon start automatically on every reboot
(`Restart=always` keeps it alive); the daemon then hot-reloads the config file
in place, so edits never need a restart. If `--config` is omitted, the daemon
discovers `renew.json` by walking up from the current directory.

### Build the `.deb` yourself

```bash
bash packaging/build-deb.sh                 # -> dist/renew_<version>_<arch>.deb
sudo apt install ./dist/renew_*.deb
```

CI builds both arches and publishes the signed apt repo — see
[`.github/workflows/packages.yml`](.github/workflows/packages.yml).

### From source (development)

```bash
make release                                # target/release/renew
./target/release/renew --config ./renew.json validate
```

## Configuration

A minimal config:

```json
{
  "services": [
    { "name": "backend", "compose_files": ["docker-compose.yml"] }
  ]
}
```

Everything else has a default: `repo.dir` is the config's directory,
`repo.remote`/`repo.branch` are `origin`/`main`, the schedule is `*/3 * * * *`,
and state lives under `<repo>/.renew`. See
[`renew.example.json`](renew.example.json) for the full surface
(staggered schedules, peer-health quorum, per-service env files, defaults), and
[`docs/configuration.md`](docs/configuration.md) for a field-by-field reference.

| Field | Default | Meaning |
| --- | --- | --- |
| `repo.dir` | config's directory | git working tree to keep in sync |
| `repo.remote` / `repo.branch` | `origin` / `main` | what to fetch and reset onto |
| `state_dir` | `<repo>/.renew` | where env-file hashes are stored |
| `run_on_start` | `false` | fire every service once immediately on boot |
| `log_format` | `text` | `text` or `json` |
| `defaults.*` | — | values inherited by every service |
| `services[].name` | — | unique id (also the state-file key) |
| `services[].compose_files` | — | compose files to reconcile (relative to repo) |
| `services[].schedule` | `*/3 * * * *` | cron / `@every` / `@macro` |
| `services[].env_file` | — | env file to hash for rotation detection |
| `services[].compose_project_name` | — | `COMPOSE_PROJECT_NAME` (volume adoption) |
| `services[].health.local_url` | — | this host's own health endpoint |
| `services[].health.peer_urls` | `[]` | peer endpoints for the quorum gate |
| `services[].health.peers_required` | `1` if peers set | healthy peers needed to restart |
| `services[].clean_script` | — | best-effort script run after a restart |

## Commands

```
renew run                     # start the scheduler loop (default)
renew sync [--service NAME]   # one reconcile pass, then exit (oneshot)
renew sync --dry-run          # show intended actions, touch nothing
renew validate                # print resolved schedules + next fires
```

## Build & test

```bash
make build        # cargo build --workspace
make test         # unit tests + e2e (real git + mock docker)
make clippy       # cargo clippy -D warnings
make release      # optimized binary at target/release/renew
```

## How it works

```
                 ┌──────────────── renew (one process) ────────────────┐
   update-       │  scheduler: per-service cron, overlap guard, hot-reload      │
   daemon.json ──▶  ── tick(service) ─────────────────────────────────────────┐│
                 │     git fetch + reset --hard onto origin/branch             ││
                 │     hash env file → changed? → restart all services         ││
                 │     for each compose file:                                  ││
                 │        declared image SHA  vs  running container SHA         ││
                 │        drifted? → health gate (local up ⇒ need peer quorum)  ││
                 │        → docker compose pull + up -d --no-deps <drifted>     ││
                 │  panics/failures isolated per service, retried next tick ◀───┘│
                 └──────────────────────────────────────────────────────────────┘
```

## License

AGPL-3.0-only. See [LICENCE](LICENCE).
