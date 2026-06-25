# Configuration reference

The daemon is driven by a single JSON file, `renew.json`, normally at
the root of the repo it deploys. Unknown fields are rejected, so typos fail fast
at load and on hot-reload.

## Top level

| Field | Type | Default | Notes |
| --- | --- | --- | --- |
| `repo.dir` | string | the config file's directory | git working tree. Relative paths resolve against the config's directory. |
| `repo.remote` | string | `origin` | remote to fetch. |
| `repo.branch` | string | `main` | branch to track; the tree is `reset --hard` onto `<remote>/<branch>`. |
| `state_dir` | string | `<repo.dir>/.renew` | where per-service env-file hashes are stored. |
| `git_bin` | string | `git` | overridable; env `RENEW_GIT_BIN` wins. |
| `docker_bin` | string | `docker` | overridable; env `RENEW_DOCKER_BIN` wins. |
| `run_on_start` | bool | `false` | fire every service once immediately when the daemon starts. |
| `log_format` | string | `text` | `text` or `json`. |
| `defaults` | object | — | values inherited by every service that doesn't set its own. |
| `services` | array | — | at least one required. |

## `defaults` and `services[]`

A service inherits any of these from `defaults` when it does not set them
itself. `name` and `compose_files` are service-only.

| Field | Type | Default | Notes |
| --- | --- | --- | --- |
| `name` | string | — | unique; `[A-Za-z0-9._-]` only (it keys the state file). |
| `compose_files` | array | — | ≥1 compose file, relative to `repo.dir`. |
| `schedule` | string | `*/3 * * * *` | see **Schedules** below. |
| `env_file` | string | — | hashed for rotation detection; relative to `repo.dir`. A change restarts every service in the file. |
| `clean_script` | string | — | best-effort `bash` script run after a successful restart; failure is logged, not fatal. |
| `compose_project_name` | string | — | exported as `COMPOSE_PROJECT_NAME` (adopt existing volumes). |
| `self_hosts` | array | — | extra host/IP strings that count as "self" when filtering `peer_urls`. |
| `health` | object | — | the restart gate; see below. |

Health merge is **wholesale**: a service that sets `health` replaces
`defaults.health` entirely rather than merging field by field.

### `health`

| Field | Type | Default | Notes |
| --- | --- | --- | --- |
| `local_url` | string | — | this host's own endpoint. If unset, the health gate is skipped (restart always proceeds). |
| `peer_urls` | array | `[]` | peer endpoints. Self-referential entries are filtered out by host. |
| `peers_required` | int | `1` when `peer_urls` is non-empty, else `0` | healthy non-self peers required to restart **while local is up**. |
| `timeout_secs` | int | `5` | per-probe timeout. |

Gate logic, per compose file with drift:

- **local up** → require `peers_required` healthy peers; otherwise refuse and
  retry next tick (exit code `3` in oneshot mode).
- **local down or `local_url` unset** → bypass the peer gate and restart
  (a dead node restarting cannot reduce capacity).

## Schedules

A `schedule` string is one of:

- **5-field cron** — `minute hour day-of-month month day-of-week`
  - operators: `*`, `*/step`, `a-b`, `a-b/step`, `a/step`, lists `a,b,c`
  - names: `jan..dec`, `sun..sat`; day-of-week `0` and `7` are both Sunday
  - when **both** day-of-month and day-of-week are restricted, a day matches if
    **either** matches (standard cron rule)
  - evaluated in **UTC**, pinned to `:00` seconds
- **interval** — `@every <dur>`, where `<dur>` is `90s`, `3m`, `1h30m`, `500ms`,
  `2d`, …
- **macros** — `@hourly`, `@daily` / `@midnight`, `@weekly`, `@monthly`,
  `@yearly` / `@annually`

Examples:

| Schedule | Fires |
| --- | --- |
| `*/3 * * * *` | every 3 min at minutes 0,3,6,… |
| `1-59/3 * * * *` | every 3 min at minutes 1,4,7,… (staggered) |
| `@every 90s` | every 90 seconds |
| `30 14 * * *` | 14:30 UTC daily |
| `0 0 * * sun` | Sundays at 00:00 UTC |

## Environment overrides

| Variable | Effect |
| --- | --- |
| `RENEW_CONFIG` | default config path (same as `--config`). |
| `RENEW_GIT_BIN` | override `git` binary. |
| `RENEW_DOCKER_BIN` | override `docker` binary. |
| `RENEW_COMMAND_TIMEOUT_SECS` | hard timeout per `git`/`docker` child process (default 600). A hung process is killed so it cannot wedge a service. |
| `RUST_LOG` | log filter, e.g. `info`, `renew_core=debug`. |

## Notes

- Health URLs may be `http://` or `https://` (a rustls TLS backend is built in;
  private-CA peers need their CA in the system/web roots).
- An unsatisfiable cron (e.g. `0 0 30 2 *`, Feb 30) is rejected at load rather
  than silently never firing. `peers_required` greater than the number of
  `peer_urls`, and `timeout_secs: 0`, are also rejected.
- Service names are validated as `[A-Za-z0-9._-]`, compared case-insensitively
  (state files collide on NTFS/APFS), and may not be Windows device names
  (`con`, `nul`, `com1`, …).
