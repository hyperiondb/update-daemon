use crate::clock::Clock;
use crate::config::{ResolvedConfig, ResolvedService};
use crate::error::{Error, Result};
use crate::health::HealthChecker;
use crate::runner::CommandRunner;
use crate::sync::{SyncEngine, SyncOptions, SyncOutcome};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use time::OffsetDateTime;
use tokio::sync::{mpsc, Notify};
use tracing::{error, info, warn};

/// How often the loop wakes at minimum, bounding config-reload latency.
const POLL_CAP: Duration = Duration::from_secs(5);
/// Max time to wait for in-flight syncs to finish on shutdown.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug)]
struct SvcRuntime {
    next_fire: OffsetDateTime,
    schedule_expr: String,
    running: bool,
}

pub struct Daemon {
    runner: Arc<dyn CommandRunner>,
    health: Arc<dyn HealthChecker>,
    clock: Arc<dyn Clock>,
}

impl Daemon {
    pub fn new(
        runner: Arc<dyn CommandRunner>,
        health: Arc<dyn HealthChecker>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            runner,
            health,
            clock,
        }
    }

    /// Run a single pass over the selected services (or all) and return outcomes.
    /// Used by the oneshot `sync` CLI subcommand.
    pub async fn run_once(
        &self,
        cfg: &ResolvedConfig,
        filter: Option<&[String]>,
        dry_run: bool,
    ) -> Vec<SyncOutcome> {
        let engine = SyncEngine::new(
            self.runner.as_ref(),
            self.health.as_ref(),
            self.clock.as_ref(),
        );
        let mut out = Vec::new();
        for svc in &cfg.services {
            if let Some(names) = filter {
                if !names.iter().any(|n| n == &svc.name) {
                    continue;
                }
            }
            out.push(engine.sync_service(cfg, svc, SyncOptions { dry_run }).await);
        }
        out
    }

    /// Run the scheduler loop until `shutdown` is notified. Loads the config from
    /// `config_path` and hot-reloads it when the file changes on disk.
    pub async fn run(&self, config_path: PathBuf, shutdown: Arc<Notify>) -> Result<()> {
        let mut cfg = Arc::new(ResolvedConfig::load(&config_path)?);
        let mut last_mtime = file_mtime(&config_path);
        info!(
            services = cfg.services.len(),
            repo = %cfg.repo_dir.display(),
            "daemon starting"
        );
        for svc in &cfg.services {
            info!(service = %svc.name, schedule = %svc.schedule_expr, "scheduled");
        }

        let now = self.clock.now_utc();
        let mut runtimes = initial_runtimes(&cfg, now);

        let (done_tx, mut done_rx) = mpsc::unbounded_channel::<String>();

        loop {
            // 1. hot-reload on config change
            let mtime = file_mtime(&config_path);
            if mtime != last_mtime {
                last_mtime = mtime;
                match ResolvedConfig::load(&config_path) {
                    Ok(new_cfg) => {
                        info!("config changed — reloading");
                        cfg = Arc::new(new_cfg);
                        reconcile_runtimes(&mut runtimes, &cfg, self.clock.now_utc());
                        for svc in &cfg.services {
                            info!(service = %svc.name, schedule = %svc.schedule_expr, "scheduled");
                        }
                    }
                    Err(e) => error!("config reload failed, keeping previous config: {e}"),
                }
            }

            // 2. fire all due, not-running services
            let now = self.clock.now_utc();
            for name in fire_due(&mut runtimes, &cfg, now) {
                if let Some(svc) = cfg.services.iter().find(|s| s.name == name) {
                    self.spawn_sync(name.clone(), cfg.clone(), svc.clone(), done_tx.clone());
                }
            }

            // 3. sleep until the soonest future fire (bounded by POLL_CAP)
            let now = self.clock.now_utc();
            let soonest = cfg
                .services
                .iter()
                .filter_map(|s| runtimes.get(&s.name))
                .filter(|rt| !rt.running)
                .map(|rt| rt.next_fire)
                .min();
            let sleep_dur = match soonest {
                Some(t) => {
                    let ms = (t - now).whole_milliseconds();
                    let ms = ms.clamp(0, POLL_CAP.as_millis() as i128);
                    Duration::from_millis(ms as u64)
                }
                None => POLL_CAP,
            };

            tokio::select! {
                _ = shutdown.notified() => {
                    info!("shutdown requested");
                    break;
                }
                Some(name) = done_rx.recv() => {
                    mark_done(&mut runtimes, &name);
                }
                _ = tokio::time::sleep(sleep_dur) => {}
            }
        }

        drain(&mut runtimes, &mut done_rx).await;
        info!("daemon stopped");
        Ok(())
    }

    fn spawn_sync(
        &self,
        name: String,
        cfg: Arc<ResolvedConfig>,
        svc: ResolvedService,
        done_tx: mpsc::UnboundedSender<String>,
    ) {
        let runner = self.runner.clone();
        let health = self.health.clone();
        let clock = self.clock.clone();

        // Inner task does the work; an outer task awaits it so a panic surfaces as
        // a JoinError instead of silently leaving the service marked running.
        let inner = tokio::spawn(async move {
            let engine = SyncEngine::new(runner.as_ref(), health.as_ref(), clock.as_ref());
            engine
                .sync_service(&cfg, &svc, SyncOptions { dry_run: false })
                .await
        });

        tokio::spawn(async move {
            match inner.await {
                Ok(outcome) => {
                    if !outcome.status.is_success() {
                        warn!(service = %name, status = ?outcome.status, "sync finished with non-success");
                    }
                }
                Err(join_err) => {
                    error!(service = %name, "sync task panicked: {join_err} — will retry next tick");
                }
            }
            let _ = done_tx.send(name);
        });
    }
}

/// Fire every due, not-running service: flip it to running and advance its
/// next fire. Returns the names fired so the caller can spawn their syncs.
fn fire_due(
    runtimes: &mut HashMap<String, SvcRuntime>,
    cfg: &ResolvedConfig,
    now: OffsetDateTime,
) -> Vec<String> {
    let mut due = Vec::new();
    for svc in &cfg.services {
        let Some(rt) = runtimes.get_mut(&svc.name) else {
            continue;
        };
        if !rt.running && rt.next_fire <= now {
            rt.running = true;
            rt.next_fire = svc.schedule.next_after(now).unwrap_or_else(|| parked(now));
            due.push(svc.name.clone());
        }
    }
    due
}

fn mark_done(runtimes: &mut HashMap<String, SvcRuntime>, name: &str) {
    if let Some(rt) = runtimes.get_mut(name) {
        rt.running = false;
    }
}

async fn drain(
    runtimes: &mut HashMap<String, SvcRuntime>,
    done_rx: &mut mpsc::UnboundedReceiver<String>,
) {
    let mut running = runtimes.values().filter(|rt| rt.running).count();
    if running == 0 {
        return;
    }
    info!("draining {running} in-flight sync(s)");
    let deadline = tokio::time::Instant::now() + DRAIN_TIMEOUT;
    while running > 0 {
        tokio::select! {
            _ = tokio::time::sleep_until(deadline) => {
                warn!("drain timeout — {running} sync(s) still running");
                break;
            }
            msg = done_rx.recv() => {
                match msg {
                    Some(name) => {
                        mark_done(runtimes, &name);
                        running = running.saturating_sub(1);
                    }
                    None => break,
                }
            }
        }
    }
}

/// Where to park a service whose schedule has no representable next fire (only
/// reachable at the year-9999 ceiling, since impossible crons are rejected at
/// parse time). Far in the future so it never busy-loops.
fn parked(now: OffsetDateTime) -> OffsetDateTime {
    now.checked_add(time::Duration::days(365)).unwrap_or(now)
}

fn initial_runtimes(cfg: &ResolvedConfig, now: OffsetDateTime) -> HashMap<String, SvcRuntime> {
    let mut map = HashMap::new();
    for svc in &cfg.services {
        let next_fire = if cfg.run_on_start {
            now
        } else {
            svc.schedule.next_after(now).unwrap_or_else(|| parked(now))
        };
        map.insert(
            svc.name.clone(),
            SvcRuntime {
                next_fire,
                schedule_expr: svc.schedule_expr.clone(),
                running: false,
            },
        );
    }
    map
}

fn reconcile_runtimes(
    runtimes: &mut HashMap<String, SvcRuntime>,
    cfg: &ResolvedConfig,
    now: OffsetDateTime,
) {
    // Drop entries for services that no longer exist and are not running.
    let live: std::collections::HashSet<&String> = cfg.services.iter().map(|s| &s.name).collect();
    runtimes.retain(|name, rt| live.contains(name) || rt.running);

    for svc in &cfg.services {
        match runtimes.get_mut(&svc.name) {
            Some(rt) => {
                if rt.schedule_expr != svc.schedule_expr {
                    rt.schedule_expr = svc.schedule_expr.clone();
                    if !rt.running {
                        rt.next_fire = svc.schedule.next_after(now).unwrap_or_else(|| parked(now));
                    }
                }
            }
            None => {
                let next_fire = svc.schedule.next_after(now).unwrap_or_else(|| parked(now));
                runtimes.insert(
                    svc.name.clone(),
                    SvcRuntime {
                        next_fire,
                        schedule_expr: svc.schedule_expr.clone(),
                        running: false,
                    },
                );
            }
        }
    }
}

fn file_mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).ok().and_then(|m| m.modified().ok())
}

/// Convenience for the binary: discover the config path from an explicit override
/// or by walking up from the current directory.
pub fn resolve_config_path(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(p) = explicit {
        if p.is_file() {
            return Ok(p.to_path_buf());
        }
        return Err(Error::Config(format!(
            "config file not found: {}",
            p.display()
        )));
    }
    let cwd = std::env::current_dir()?;
    ResolvedConfig::discover(&cwd).ok_or_else(|| {
        Error::Config(format!(
            "no {} found in {} or any parent directory",
            crate::config::CONFIG_FILE_NAME,
            cwd.display()
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::FixedClock;
    use crate::config::{LogFormat, ResolvedHealth, ResolvedService};
    use crate::error::Result as CoreResult;
    use crate::health::FakeHealthChecker;
    use crate::runner::{CommandOutput, CommandRunner};
    use crate::schedule::Schedule;
    use std::time::Duration as StdDuration;
    use time::macros::datetime;

    fn svc(name: &str, expr: &str) -> ResolvedService {
        ResolvedService {
            name: name.into(),
            compose_files: vec!["dc.yml".into()],
            env_file: None,
            clean_script: None,
            compose_project_name: None,
            schedule_expr: expr.into(),
            schedule: Schedule::parse(expr).unwrap(),
            health: ResolvedHealth {
                local_url: None,
                peer_urls: vec![],
                peers_required: 0,
                timeout: StdDuration::from_secs(1),
            },
            self_hosts: vec![],
            env_state_file: PathBuf::from(format!("{name}.sha256")),
        }
    }

    fn cfg(services: Vec<ResolvedService>, run_on_start: bool) -> ResolvedConfig {
        ResolvedConfig {
            config_path: PathBuf::from("renew.json"),
            repo_dir: PathBuf::from("."),
            remote: "origin".into(),
            branch: "main".into(),
            state_dir: PathBuf::from(".state"),
            git_bin: "git".into(),
            docker_bin: "docker".into(),
            run_on_start,
            log_format: LogFormat::Text,
            services,
        }
    }

    #[test]
    fn run_on_start_fires_immediately() {
        let now = datetime!(2026-06-25 12:00:30 UTC);
        let c = cfg(vec![svc("a", "*/3 * * * *")], true);
        let rt = initial_runtimes(&c, now);
        assert_eq!(rt["a"].next_fire, now);
        assert!(!rt["a"].running);
    }

    #[test]
    fn without_run_on_start_uses_next_schedule() {
        let now = datetime!(2026-06-25 12:00:30 UTC);
        let c = cfg(vec![svc("a", "*/3 * * * *")], false);
        let rt = initial_runtimes(&c, now);
        assert_eq!(rt["a"].next_fire, datetime!(2026-06-25 12:03:00 UTC));
    }

    #[test]
    fn reconcile_adds_new_and_drops_removed() {
        let now = datetime!(2026-06-25 12:00:00 UTC);
        let mut rt = initial_runtimes(&cfg(vec![svc("a", "*/3 * * * *")], false), now);
        assert!(rt.contains_key("a"));

        reconcile_runtimes(&mut rt, &cfg(vec![svc("b", "*/3 * * * *")], false), now);
        assert!(!rt.contains_key("a"), "removed service should be dropped");
        assert!(rt.contains_key("b"), "new service should be added");
    }

    #[test]
    fn reconcile_preserves_running_and_next_fire() {
        let now = datetime!(2026-06-25 12:00:00 UTC);
        let c = cfg(vec![svc("a", "*/3 * * * *")], false);
        let mut rt = initial_runtimes(&c, now);
        rt.get_mut("a").unwrap().running = true;
        let pinned = datetime!(2026-06-25 12:09:00 UTC);
        rt.get_mut("a").unwrap().next_fire = pinned;

        reconcile_runtimes(&mut rt, &c, now);
        assert!(rt["a"].running);
        assert_eq!(
            rt["a"].next_fire, pinned,
            "unchanged schedule keeps next_fire"
        );
    }

    #[test]
    fn reconcile_recomputes_next_fire_on_schedule_change() {
        let now = datetime!(2026-06-25 12:00:00 UTC);
        let mut rt = initial_runtimes(&cfg(vec![svc("a", "*/3 * * * *")], false), now);

        reconcile_runtimes(&mut rt, &cfg(vec![svc("a", "0 0 * * *")], false), now);
        assert_eq!(rt["a"].schedule_expr, "0 0 * * *");
        assert_eq!(rt["a"].next_fire, datetime!(2026-06-26 00:00:00 UTC));
    }

    #[test]
    fn reconcile_keeps_removed_service_while_running() {
        let now = datetime!(2026-06-25 12:00:00 UTC);
        let c = cfg(vec![svc("a", "*/3 * * * *")], false);
        let mut rt = initial_runtimes(&c, now);
        rt.get_mut("a").unwrap().running = true;

        reconcile_runtimes(&mut rt, &cfg(vec![svc("b", "*/3 * * * *")], false), now);
        assert!(
            rt.contains_key("a"),
            "running service must survive reconcile"
        );
        assert!(rt.contains_key("b"));
    }

    #[test]
    fn fire_due_fires_and_advances() {
        let c = cfg(vec![svc("a", "*/3 * * * *")], false);
        let mut rt = initial_runtimes(&c, datetime!(2026-06-25 12:00:00 UTC));
        let due = fire_due(&mut rt, &c, datetime!(2026-06-25 12:03:00 UTC));
        assert_eq!(due, vec!["a".to_string()]);
        assert!(rt["a"].running);
        assert_eq!(rt["a"].next_fire, datetime!(2026-06-25 12:06:00 UTC));
    }

    #[test]
    fn fire_due_respects_overlap_guard() {
        let c = cfg(vec![svc("a", "*/3 * * * *")], false);
        let mut rt = initial_runtimes(&c, datetime!(2026-06-25 12:00:00 UTC));
        rt.get_mut("a").unwrap().running = true;
        let pinned = rt["a"].next_fire;
        let due = fire_due(&mut rt, &c, datetime!(2026-06-25 12:03:00 UTC));
        assert!(due.is_empty(), "must not fire a still-running service");
        assert_eq!(rt["a"].next_fire, pinned);
    }

    #[test]
    fn fire_due_skips_not_yet_due() {
        let c = cfg(vec![svc("a", "*/3 * * * *")], false);
        let mut rt = initial_runtimes(&c, datetime!(2026-06-25 12:00:00 UTC));
        let due = fire_due(&mut rt, &c, datetime!(2026-06-25 12:01:00 UTC));
        assert!(due.is_empty());
        assert!(!rt["a"].running);
    }

    #[test]
    fn fire_due_catches_up_after_overrun_without_backfill() {
        let c = cfg(vec![svc("a", "*/3 * * * *")], false);
        let mut rt = initial_runtimes(&c, datetime!(2026-06-25 12:00:00 UTC));
        // simulate an overrun: due time already passed while it was running
        rt.get_mut("a").unwrap().next_fire = datetime!(2026-06-25 12:03:00 UTC);
        let due = fire_due(&mut rt, &c, datetime!(2026-06-25 12:05:00 UTC));
        assert_eq!(due, vec!["a".to_string()]);
        assert_eq!(rt["a"].next_fire, datetime!(2026-06-25 12:06:00 UTC));
    }

    #[test]
    fn mark_done_clears_and_ignores_unknown() {
        let c = cfg(vec![svc("a", "*/3 * * * *")], false);
        let mut rt = initial_runtimes(&c, datetime!(2026-06-25 12:00:00 UTC));
        rt.get_mut("a").unwrap().running = true;
        mark_done(&mut rt, "a");
        assert!(!rt["a"].running);
        mark_done(&mut rt, "nonexistent"); // must not panic
    }

    #[tokio::test]
    async fn drain_returns_when_all_complete() {
        let c = cfg(
            vec![svc("a", "*/3 * * * *"), svc("b", "*/3 * * * *")],
            false,
        );
        let mut rt = initial_runtimes(&c, datetime!(2026-06-25 12:00:00 UTC));
        rt.get_mut("a").unwrap().running = true;
        rt.get_mut("b").unwrap().running = true;
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        tx.send("a".to_string()).unwrap();
        tx.send("b".to_string()).unwrap();
        drop(tx);
        drain(&mut rt, &mut rx).await;
        assert!(!rt["a"].running && !rt["b"].running);
    }

    #[tokio::test(start_paused = true)]
    async fn drain_times_out_when_sync_never_finishes() {
        let c = cfg(vec![svc("a", "*/3 * * * *")], false);
        let mut rt = initial_runtimes(&c, datetime!(2026-06-25 12:00:00 UTC));
        rt.get_mut("a").unwrap().running = true;
        let (_tx, mut rx) = mpsc::unbounded_channel::<String>();
        // Never send; paused clock auto-advances past DRAIN_TIMEOUT so drain returns.
        drain(&mut rt, &mut rx).await;
        assert!(rt["a"].running, "sync never reported done");
    }

    struct PanicRunner;

    #[async_trait::async_trait]
    impl CommandRunner for PanicRunner {
        async fn run(
            &self,
            _program: &str,
            _args: &[String],
            _cwd: Option<&Path>,
            _env: &[(String, String)],
        ) -> CoreResult<CommandOutput> {
            panic!("intentional test panic — verifying self-heal");
        }
    }

    #[tokio::test]
    async fn panicking_sync_still_reports_done() {
        // A panicking sync must still send its done signal so the service is
        // un-wedged and retried — the crux of the self-healing guarantee.
        let mut c = cfg(vec![svc("a", "*/3 * * * *")], false);
        c.repo_dir = std::env::temp_dir(); // exists, so preflight runs and panics
        let c = Arc::new(c);
        let daemon = Daemon::new(
            Arc::new(PanicRunner),
            Arc::new(FakeHealthChecker::new()),
            Arc::new(FixedClock::new(datetime!(2026-06-25 12:00:00 UTC))),
        );
        let (tx, mut rx) = mpsc::unbounded_channel::<String>();
        let service = c.services[0].clone();
        daemon.spawn_sync("a".to_string(), c.clone(), service, tx);
        assert_eq!(rx.recv().await, Some("a".to_string()));
    }
}
