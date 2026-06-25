use crate::clock::Clock;
use crate::config::{ResolvedConfig, ResolvedService};
use crate::docker::{decide, Docker, DriftDecision};
use crate::envhash;
use crate::git::Git;
use crate::health::{check_peer_quorum, HealthChecker};
use crate::runner::CommandRunner;
use std::collections::HashSet;
use tracing::{info, warn};

/// Maps onto the original sync.sh exit codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncStatus {
    Success,
    ConfigError,
    CommandFailed,
    RefusedQuorum,
}

impl SyncStatus {
    pub fn exit_code(self) -> i32 {
        match self {
            SyncStatus::Success => 0,
            SyncStatus::ConfigError => 1,
            SyncStatus::CommandFailed => 2,
            SyncStatus::RefusedQuorum => 3,
        }
    }

    pub fn is_success(self) -> bool {
        matches!(self, SyncStatus::Success)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComposeOutcome {
    NoDrift,
    Restarted,
    RefusedQuorum,
    DryRun,
    NotFound,
    Failed(String),
}

#[derive(Debug, Clone)]
pub struct ComposeReport {
    pub file: String,
    pub declared: Vec<String>,
    pub in_sync: Vec<String>,
    pub skipped_build_only: Vec<String>,
    pub to_restart: Vec<String>,
    pub restarted: Vec<String>,
    pub outcome: ComposeOutcome,
}

impl ComposeReport {
    fn new(file: &str) -> Self {
        Self {
            file: file.to_string(),
            declared: Vec::new(),
            in_sync: Vec::new(),
            skipped_build_only: Vec::new(),
            to_restart: Vec::new(),
            restarted: Vec::new(),
            outcome: ComposeOutcome::NoDrift,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SyncOutcome {
    pub service: String,
    pub dry_run: bool,
    pub repo_advanced: bool,
    pub from_rev: Option<String>,
    pub to_rev: Option<String>,
    pub env_changed: bool,
    pub compose_reports: Vec<ComposeReport>,
    pub status: SyncStatus,
    pub error: Option<String>,
}

impl SyncOutcome {
    fn new(service: &str, dry_run: bool) -> Self {
        Self {
            service: service.to_string(),
            dry_run,
            repo_advanced: false,
            from_rev: None,
            to_rev: None,
            env_changed: false,
            compose_reports: Vec::new(),
            status: SyncStatus::Success,
            error: None,
        }
    }

    /// Record a non-success status, keeping the last one (sync.sh `rc` semantics:
    /// a later success never clears an earlier failure).
    fn record(&mut self, status: SyncStatus) {
        if !status.is_success() {
            self.status = status;
        }
    }

    fn fail(&mut self, status: SyncStatus, msg: String) {
        warn!(service = %self.service, "{msg}");
        self.error = Some(msg);
        self.status = status;
    }
}

#[derive(Debug, Clone, Copy)]
pub struct SyncOptions {
    pub dry_run: bool,
}

pub struct SyncEngine<'a> {
    pub runner: &'a dyn CommandRunner,
    pub health: &'a dyn HealthChecker,
    pub clock: &'a dyn Clock,
}

impl<'a> SyncEngine<'a> {
    pub fn new(
        runner: &'a dyn CommandRunner,
        health: &'a dyn HealthChecker,
        clock: &'a dyn Clock,
    ) -> Self {
        Self {
            runner,
            health,
            clock,
        }
    }

    pub async fn sync_service(
        &self,
        cfg: &ResolvedConfig,
        svc: &ResolvedService,
        opts: SyncOptions,
    ) -> SyncOutcome {
        let dry_run = opts.dry_run;
        let mut outcome = SyncOutcome::new(&svc.name, dry_run);
        let _ = self.clock.now_utc();
        info!(service = %svc.name, dry_run, "sync start");

        if !cfg.repo_dir.is_dir() {
            outcome.fail(
                SyncStatus::ConfigError,
                format!("repo dir not found: {}", cfg.repo_dir.display()),
            );
            return outcome;
        }

        // Preflight required binaries -> config error (exit 1), like sync.sh's
        // `command -v` check. `run` errors only on spawn/ENOENT, so a binary that
        // exists but exits non-zero on `--version` is not a false positive.
        for bin in [cfg.git_bin.as_str(), cfg.docker_bin.as_str()] {
            if let Err(e) = self
                .runner
                .run(bin, &["--version".to_string()], None, &[])
                .await
            {
                outcome.fail(
                    SyncStatus::ConfigError,
                    format!("required binary not found: {bin} ({e})"),
                );
                return outcome;
            }
        }

        let git = Git {
            runner: self.runner,
            bin: &cfg.git_bin,
            cwd: &cfg.repo_dir,
        };

        // 1. fetch
        if dry_run {
            info!(service = %svc.name, "[dry-run] would fetch {}/{}", cfg.remote, cfg.branch);
        } else if let Err(e) = git.fetch(&cfg.remote, &cfg.branch).await {
            outcome.fail(SyncStatus::ConfigError, format!("git fetch failed: {e}"));
            return outcome;
        }

        // 2. local vs remote revision
        let local_rev = git.rev_parse("HEAD").await.ok().flatten();
        outcome.from_rev = local_rev.clone();
        let remote_ref = format!("{}/{}", cfg.remote, cfg.branch);
        let remote_rev = git.rev_parse(&remote_ref).await.ok().flatten();

        // 3. advance working tree if drifted
        match (&local_rev, &remote_rev) {
            (Some(l), Some(r)) if l != r => {
                if dry_run {
                    info!(service = %svc.name, "[dry-run] would reset {l} -> {r}");
                    outcome.repo_advanced = true;
                    outcome.to_rev = Some(r.clone());
                } else if let Err(e) = git.reset_hard(r).await {
                    outcome.fail(SyncStatus::ConfigError, format!("git reset failed: {e}"));
                    return outcome;
                } else {
                    info!(service = %svc.name, "advanced repo {l} -> {r}");
                    outcome.repo_advanced = true;
                    outcome.to_rev = Some(r.clone());
                }
            }
            _ => {
                outcome.to_rev = remote_rev.clone().or_else(|| local_rev.clone());
            }
        }

        // 4. env-file hash (out-of-band secret rotation => full-stack restart)
        let env_status = match envhash::evaluate(svc.env_file.as_deref(), &svc.env_state_file) {
            Ok(s) => s,
            Err(e) => {
                warn!(service = %svc.name, "env hash check failed (treating as unchanged): {e}");
                envhash::EnvHashStatus {
                    current: None,
                    changed: false,
                }
            }
        };
        outcome.env_changed = env_status.changed;
        if env_status.changed {
            info!(service = %svc.name, "env file changed -> full-stack restart");
        }

        // 5. reconcile each compose file
        let probe = svc.health.local_url.is_some() && !svc.health.peer_urls.is_empty();
        let self_hosts = detect_self_hosts(self.runner, &svc.self_hosts, probe).await;

        let docker = Docker {
            runner: self.runner,
            bin: &cfg.docker_bin,
            cwd: &cfg.repo_dir,
            env_file: svc.env_file.as_deref(),
            project: svc.compose_project_name.as_deref(),
        };

        for file in &svc.compose_files {
            let mut report = ComposeReport::new(file);

            if !cfg.repo_dir.join(file).is_file() {
                report.outcome = ComposeOutcome::NotFound;
                outcome.compose_reports.push(report);
                outcome.record(SyncStatus::CommandFailed);
                warn!(service = %svc.name, "compose file not found: {file}");
                continue;
            }

            let services = match docker.compose_config(file).await {
                Ok(s) if !s.is_empty() => s,
                Ok(_) => {
                    report.outcome = ComposeOutcome::Failed("no services declared".into());
                    outcome.compose_reports.push(report);
                    outcome.record(SyncStatus::CommandFailed);
                    continue;
                }
                Err(e) => {
                    report.outcome = ComposeOutcome::Failed(e.to_string());
                    outcome.compose_reports.push(report);
                    outcome.record(SyncStatus::CommandFailed);
                    continue;
                }
            };
            report.declared = services.iter().map(|s| s.name.clone()).collect();

            // Determine which services to restart.
            let mut to_restart = Vec::new();
            if env_status.changed {
                to_restart = services.iter().map(|s| s.name.clone()).collect();
            } else {
                for s in &services {
                    let declared_image = s.image.as_deref();
                    let (desired_id, container_id) = match declared_image {
                        None => (None, None),
                        Some(img) => {
                            let desired = docker.image_id(img).await.unwrap_or(None);
                            let cid = docker.container_id(file, &s.name).await.unwrap_or(None);
                            (desired, cid)
                        }
                    };
                    let running_id = match &container_id {
                        Some(cid) => docker.running_image(cid).await.unwrap_or(None),
                        None => None,
                    };
                    match decide(
                        declared_image,
                        desired_id.as_deref(),
                        container_id.as_deref(),
                        running_id.as_deref(),
                    ) {
                        DriftDecision::InSync => report.in_sync.push(s.name.clone()),
                        DriftDecision::SkipBuildOnly => {
                            report.skipped_build_only.push(s.name.clone())
                        }
                        DriftDecision::Restart(reason) => {
                            info!(service = %svc.name, "{}: {reason} -> restart", s.name);
                            to_restart.push(s.name.clone());
                        }
                    }
                }
            }

            if to_restart.is_empty() {
                report.outcome = ComposeOutcome::NoDrift;
                outcome.compose_reports.push(report);
                info!(service = %svc.name, "{file}: no drift");
                continue;
            }
            report.to_restart = to_restart.clone();

            // Health gate. Only enforced when local is up; if local is down or
            // unconfigured, restarting cannot reduce capacity, so we proceed.
            if !dry_run {
                let local_healthy = match &svc.health.local_url {
                    Some(url) => self.health.check(url, svc.health.timeout).await,
                    None => false,
                };
                if local_healthy {
                    let quorum = check_peer_quorum(
                        self.health,
                        &svc.health.peer_urls,
                        &self_hosts,
                        svc.health.peers_required,
                        svc.health.timeout,
                    )
                    .await;
                    if !quorum.met() {
                        warn!(
                            service = %svc.name,
                            "{file}: refusing restart — peer quorum {}/{} < {} (retry next tick)",
                            quorum.healthy, quorum.total, quorum.required
                        );
                        report.outcome = ComposeOutcome::RefusedQuorum;
                        outcome.compose_reports.push(report);
                        outcome.record(SyncStatus::RefusedQuorum);
                        continue;
                    }
                    info!(service = %svc.name, "{file}: local up, peer quorum met");
                } else {
                    info!(service = %svc.name, "{file}: local down/unconfigured — bypassing peer gate (recovery)");
                }
            }

            if dry_run {
                info!(service = %svc.name, "[dry-run] would pull + up -d --no-deps: {}", to_restart.join(", "));
                report.restarted = to_restart.clone();
                report.outcome = ComposeOutcome::DryRun;
                outcome.compose_reports.push(report);
                continue;
            }

            if let Err(e) = docker.pull(file, &to_restart).await {
                report.outcome = ComposeOutcome::Failed(e.to_string());
                outcome.compose_reports.push(report);
                outcome.record(SyncStatus::CommandFailed);
                continue;
            }
            if let Err(e) = docker.up(file, &to_restart).await {
                report.outcome = ComposeOutcome::Failed(e.to_string());
                outcome.compose_reports.push(report);
                outcome.record(SyncStatus::CommandFailed);
                continue;
            }
            info!(service = %svc.name, "{file}: restarted {}", to_restart.join(", "));
            report.restarted = to_restart;
            report.outcome = ComposeOutcome::Restarted;
            outcome.compose_reports.push(report);

            // clean script is best-effort (non-fatal), like sync.sh.
            if let Some(clean) = &svc.clean_script {
                if clean.is_file() {
                    let args = vec![clean.display().to_string()];
                    match self
                        .runner
                        .run("bash", &args, Some(&cfg.repo_dir), &[])
                        .await
                    {
                        Ok(o) if o.success() => info!(service = %svc.name, "clean script ok"),
                        Ok(o) => warn!(service = %svc.name, "clean script exited {:?}", o.status),
                        Err(e) => warn!(service = %svc.name, "clean script error: {e}"),
                    }
                }
            }
        }

        // 6. persist env hash only on overall success (so a failed tick retries).
        if outcome.status.is_success() && !dry_run {
            if let Some(hash) = &env_status.current {
                if let Err(e) = envhash::write_state(&svc.env_state_file, hash) {
                    warn!(service = %svc.name, "could not persist env hash: {e}");
                }
            }
        }

        info!(service = %svc.name, status = ?outcome.status, "sync end");
        outcome
    }
}

/// Build the set of host identifiers that count as "self" so peer health lists
/// can be deployed identically to every node. Probing runs only when there are
/// peers to filter.
async fn detect_self_hosts(
    runner: &dyn CommandRunner,
    extra: &[String],
    probe: bool,
) -> HashSet<String> {
    let mut hosts: HashSet<String> = ["127.0.0.1", "::1", "localhost"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    for e in extra {
        let e = e.trim();
        if !e.is_empty() {
            hosts.insert(e.to_string());
        }
    }
    if probe {
        if let Ok(out) = runner.run("hostname", &[], None, &[]).await {
            if out.success() {
                for h in out.stdout.split_whitespace() {
                    hosts.insert(h.to_string());
                }
            }
        }
        #[cfg(unix)]
        if let Ok(out) = runner.run("hostname", &["-I".to_string()], None, &[]).await {
            if out.success() {
                for ip in out.stdout.split_whitespace() {
                    hosts.insert(ip.to_string());
                }
            }
        }
        #[cfg(unix)]
        if let Ok(out) = runner.run("hostname", &["-f".to_string()], None, &[]).await {
            if out.success() {
                for h in out.stdout.split_whitespace() {
                    hosts.insert(h.to_string());
                }
            }
        }
    }
    hosts
}

/// Worst (highest) exit code across a batch of outcomes, for oneshot CLI runs.
pub fn worst_exit_code(outcomes: &[SyncOutcome]) -> i32 {
    outcomes
        .iter()
        .map(|o| o.status.exit_code())
        .max()
        .unwrap_or(0)
}
