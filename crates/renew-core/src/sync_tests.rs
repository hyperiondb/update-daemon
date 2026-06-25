use crate::clock::FixedClock;
use crate::config::{LogFormat, ResolvedConfig, ResolvedHealth, ResolvedService};
use crate::health::FakeHealthChecker;
use crate::runner::FakeRunner;
use crate::schedule::Schedule;
use crate::sync::{ComposeOutcome, SyncEngine, SyncOptions, SyncStatus};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use time::macros::datetime;

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn tmpdir() -> PathBuf {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let c = COUNTER.fetch_add(1, Ordering::SeqCst);
    let mut p = std::env::temp_dir();
    p.push(format!("renew-sync-{n}-{c}"));
    std::fs::create_dir_all(&p).unwrap();
    p
}

fn config(repo: &Path) -> ResolvedConfig {
    ResolvedConfig {
        config_path: repo.join("renew.json"),
        repo_dir: repo.to_path_buf(),
        remote: "origin".into(),
        branch: "main".into(),
        state_dir: repo.join(".state"),
        git_bin: "git".into(),
        docker_bin: "docker".into(),
        run_on_start: false,
        log_format: LogFormat::Text,
        services: vec![],
    }
}

fn health_none() -> ResolvedHealth {
    ResolvedHealth {
        local_url: None,
        peer_urls: vec![],
        peers_required: 0,
        timeout: Duration::from_secs(1),
    }
}

fn service(
    name: &str,
    files: &[&str],
    health: ResolvedHealth,
    state_dir: &Path,
) -> ResolvedService {
    ResolvedService {
        name: name.into(),
        compose_files: files.iter().map(|s| s.to_string()).collect(),
        env_file: None,
        clean_script: None,
        compose_project_name: None,
        schedule_expr: "*/3 * * * *".into(),
        schedule: Schedule::parse("*/3 * * * *").unwrap(),
        health,
        self_hosts: vec![],
        env_state_file: state_dir.join(format!("{name}.env.sha256")),
    }
}

const COMPOSE_JSON: &str =
    r#"{"services":{"server":{"image":"img:server"},"builder":{"build":{"context":"."}}}}"#;

fn clock() -> FixedClock {
    FixedClock::new(datetime!(2026-06-25 12:00:00 UTC))
}

#[tokio::test]
async fn drift_triggers_pull_and_up() {
    let repo = tmpdir();
    std::fs::write(repo.join("dc.yml"), "x").unwrap();
    let runner = FakeRunner::new()
        .on(&["fetch"], 0, "")
        .on(&["rev-parse", "HEAD"], 0, "rev1")
        .on(&["rev-parse", "origin/main"], 0, "rev1")
        .on(&["config", "--format"], 0, COMPOSE_JSON)
        .on(&["{{.Id}}"], 0, "sha:new")
        .on(&["ps", "-q", "server"], 0, "c1")
        .on(&["{{.Image}}"], 0, "sha:old")
        .on(&["pull"], 0, "")
        .on(&["up", "-d"], 0, "")
        .with_default(0, "", "");
    let health = FakeHealthChecker::new();
    let clock = clock();
    let cfg = config(&repo);
    let svc = service("backend", &["dc.yml"], health_none(), &cfg.state_dir);

    let engine = SyncEngine::new(&runner, &health, &clock);
    let outcome = engine
        .sync_service(&cfg, &svc, SyncOptions { dry_run: false })
        .await;

    assert_eq!(outcome.status, SyncStatus::Success);
    assert!(!outcome.repo_advanced);
    let r = &outcome.compose_reports[0];
    assert_eq!(r.outcome, ComposeOutcome::Restarted);
    assert_eq!(r.restarted, vec!["server".to_string()]);
    assert_eq!(r.skipped_build_only, vec!["builder".to_string()]);
    assert!(runner.ran_matching(&["pull", "server"]));
    assert!(runner.ran_matching(&["up", "-d", "--no-deps", "server"]));
}

#[tokio::test]
async fn no_drift_skips_pull() {
    let repo = tmpdir();
    std::fs::write(repo.join("dc.yml"), "x").unwrap();
    let runner = FakeRunner::new()
        .on(&["fetch"], 0, "")
        .on(&["rev-parse", "HEAD"], 0, "rev1")
        .on(&["rev-parse", "origin/main"], 0, "rev1")
        .on(&["config", "--format"], 0, COMPOSE_JSON)
        .on(&["{{.Id}}"], 0, "sha:same")
        .on(&["ps", "-q", "server"], 0, "c1")
        .on(&["{{.Image}}"], 0, "sha:same")
        .with_default(0, "", "");
    let health = FakeHealthChecker::new();
    let clock = clock();
    let cfg = config(&repo);
    let svc = service("backend", &["dc.yml"], health_none(), &cfg.state_dir);

    let engine = SyncEngine::new(&runner, &health, &clock);
    let outcome = engine
        .sync_service(&cfg, &svc, SyncOptions { dry_run: false })
        .await;

    assert_eq!(outcome.status, SyncStatus::Success);
    assert_eq!(outcome.compose_reports[0].outcome, ComposeOutcome::NoDrift);
    assert_eq!(
        outcome.compose_reports[0].in_sync,
        vec!["server".to_string()]
    );
    assert!(!runner.ran_matching(&["pull"]));
    assert!(!runner.ran_matching(&["up", "-d"]));
}

#[tokio::test]
async fn advances_repo_when_remote_ahead() {
    let repo = tmpdir();
    std::fs::write(repo.join("dc.yml"), "x").unwrap();
    let runner = FakeRunner::new()
        .on(&["fetch"], 0, "")
        .on(&["rev-parse", "HEAD"], 0, "oldrev")
        .on(&["rev-parse", "origin/main"], 0, "newrev")
        .on(&["reset", "--hard"], 0, "")
        .on(&["config", "--format"], 0, COMPOSE_JSON)
        .on(&["{{.Id}}"], 0, "sha:same")
        .on(&["ps", "-q", "server"], 0, "c1")
        .on(&["{{.Image}}"], 0, "sha:same")
        .with_default(0, "", "");
    let health = FakeHealthChecker::new();
    let clock = clock();
    let cfg = config(&repo);
    let svc = service("backend", &["dc.yml"], health_none(), &cfg.state_dir);

    let engine = SyncEngine::new(&runner, &health, &clock);
    let outcome = engine
        .sync_service(&cfg, &svc, SyncOptions { dry_run: false })
        .await;

    assert_eq!(outcome.status, SyncStatus::Success);
    assert!(outcome.repo_advanced);
    assert_eq!(outcome.from_rev.as_deref(), Some("oldrev"));
    assert_eq!(outcome.to_rev.as_deref(), Some("newrev"));
    assert!(runner.ran_matching(&["reset", "--hard", "--quiet", "newrev"]));
}

#[tokio::test]
async fn refuses_when_quorum_not_met() {
    let repo = tmpdir();
    std::fs::write(repo.join("dc.yml"), "x").unwrap();
    let runner = FakeRunner::new()
        .on(&["fetch"], 0, "")
        .on(&["rev-parse", "HEAD"], 0, "rev1")
        .on(&["rev-parse", "origin/main"], 0, "rev1")
        .on(&["config", "--format"], 0, COMPOSE_JSON)
        .on(&["{{.Id}}"], 0, "sha:new")
        .on(&["ps", "-q", "server"], 0, "c1")
        .on(&["{{.Image}}"], 0, "sha:old")
        .with_default(0, "", "");
    let health = FakeHealthChecker::new()
        .up("http://local/")
        .up("http://p1/")
        .down("http://p2/");
    let clock = clock();
    let cfg = config(&repo);
    let mut hc = health_none();
    hc.local_url = Some("http://local/".into());
    hc.peer_urls = vec!["http://p1/".into(), "http://p2/".into()];
    hc.peers_required = 2;
    let svc = service("backend", &["dc.yml"], hc, &cfg.state_dir);

    let engine = SyncEngine::new(&runner, &health, &clock);
    let outcome = engine
        .sync_service(&cfg, &svc, SyncOptions { dry_run: false })
        .await;

    assert_eq!(outcome.status, SyncStatus::RefusedQuorum);
    assert_eq!(
        outcome.compose_reports[0].outcome,
        ComposeOutcome::RefusedQuorum
    );
    assert!(!runner.ran_matching(&["pull"]));
}

#[tokio::test]
async fn local_down_bypasses_peer_gate() {
    let repo = tmpdir();
    std::fs::write(repo.join("dc.yml"), "x").unwrap();
    let runner = FakeRunner::new()
        .on(&["fetch"], 0, "")
        .on(&["rev-parse", "HEAD"], 0, "rev1")
        .on(&["rev-parse", "origin/main"], 0, "rev1")
        .on(&["config", "--format"], 0, COMPOSE_JSON)
        .on(&["{{.Id}}"], 0, "sha:new")
        .on(&["ps", "-q", "server"], 0, "c1")
        .on(&["{{.Image}}"], 0, "sha:old")
        .on(&["pull"], 0, "")
        .on(&["up", "-d"], 0, "")
        .with_default(0, "", "");
    let health = FakeHealthChecker::new()
        .down("http://local/")
        .down("http://p1/")
        .down("http://p2/");
    let clock = clock();
    let cfg = config(&repo);
    let mut hc = health_none();
    hc.local_url = Some("http://local/".into());
    hc.peer_urls = vec!["http://p1/".into(), "http://p2/".into()];
    hc.peers_required = 2;
    let svc = service("backend", &["dc.yml"], hc, &cfg.state_dir);

    let engine = SyncEngine::new(&runner, &health, &clock);
    let outcome = engine
        .sync_service(&cfg, &svc, SyncOptions { dry_run: false })
        .await;

    assert_eq!(outcome.status, SyncStatus::Success);
    assert!(runner.ran_matching(&["up", "-d", "--no-deps", "server"]));
}

#[tokio::test]
async fn env_change_restarts_all_services() {
    let repo = tmpdir();
    std::fs::write(repo.join("dc.yml"), "x").unwrap();
    let env_file = repo.join(".env");
    std::fs::write(&env_file, "SECRET=new\n").unwrap();
    let state_dir = repo.join(".state");
    std::fs::create_dir_all(&state_dir).unwrap();
    // Seed an old (different) hash so the env file reads as changed.
    std::fs::write(state_dir.join("backend.env.sha256"), "0000oldhash\n").unwrap();

    let runner = FakeRunner::new()
        .on(&["fetch"], 0, "")
        .on(&["rev-parse", "HEAD"], 0, "rev1")
        .on(&["rev-parse", "origin/main"], 0, "rev1")
        .on(&["config", "--format"], 0, COMPOSE_JSON)
        .on(&["pull"], 0, "")
        .on(&["up", "-d"], 0, "")
        .with_default(0, "", "");
    let health = FakeHealthChecker::new();
    let clock = clock();
    let cfg = config(&repo);
    let mut svc = service("backend", &["dc.yml"], health_none(), &cfg.state_dir);
    svc.env_file = Some(env_file);

    let engine = SyncEngine::new(&runner, &health, &clock);
    let outcome = engine
        .sync_service(&cfg, &svc, SyncOptions { dry_run: false })
        .await;

    assert_eq!(outcome.status, SyncStatus::Success);
    assert!(outcome.env_changed);
    let restarted = &outcome.compose_reports[0].restarted;
    assert!(restarted.contains(&"server".to_string()));
    assert!(restarted.contains(&"builder".to_string()));
    // env hash persisted on success
    let persisted = std::fs::read_to_string(cfg.state_dir.join("backend.env.sha256")).unwrap();
    assert_ne!(persisted.trim(), "0000oldhash");
}

#[tokio::test]
async fn missing_compose_file_is_command_failure() {
    let repo = tmpdir();
    // note: dc.yml intentionally not created
    let runner = FakeRunner::new()
        .on(&["fetch"], 0, "")
        .on(&["rev-parse", "HEAD"], 0, "rev1")
        .on(&["rev-parse", "origin/main"], 0, "rev1")
        .with_default(0, "", "");
    let health = FakeHealthChecker::new();
    let clock = clock();
    let cfg = config(&repo);
    let svc = service("backend", &["dc.yml"], health_none(), &cfg.state_dir);

    let engine = SyncEngine::new(&runner, &health, &clock);
    let outcome = engine
        .sync_service(&cfg, &svc, SyncOptions { dry_run: false })
        .await;

    assert_eq!(outcome.status, SyncStatus::CommandFailed);
    assert_eq!(outcome.compose_reports[0].outcome, ComposeOutcome::NotFound);
}

#[tokio::test]
async fn git_fetch_failure_is_config_error() {
    let repo = tmpdir();
    std::fs::write(repo.join("dc.yml"), "x").unwrap();
    let runner = FakeRunner::new()
        .on_fail(&["fetch"], 1, "could not resolve host")
        .with_default(0, "", "");
    let health = FakeHealthChecker::new();
    let clock = clock();
    let cfg = config(&repo);
    let svc = service("backend", &["dc.yml"], health_none(), &cfg.state_dir);

    let engine = SyncEngine::new(&runner, &health, &clock);
    let outcome = engine
        .sync_service(&cfg, &svc, SyncOptions { dry_run: false })
        .await;

    assert_eq!(outcome.status, SyncStatus::ConfigError);
    assert!(outcome.compose_reports.is_empty());
}

#[tokio::test]
async fn dry_run_touches_nothing() {
    let repo = tmpdir();
    std::fs::write(repo.join("dc.yml"), "x").unwrap();
    let runner = FakeRunner::new()
        .on(&["rev-parse", "HEAD"], 0, "rev1")
        .on(&["rev-parse", "origin/main"], 0, "rev2")
        .on(&["config", "--format"], 0, COMPOSE_JSON)
        .on(&["{{.Id}}"], 0, "sha:new")
        .on(&["ps", "-q", "server"], 0, "c1")
        .on(&["{{.Image}}"], 0, "sha:old")
        .with_default(0, "", "");
    let health = FakeHealthChecker::new();
    let clock = clock();
    let cfg = config(&repo);
    let svc = service("backend", &["dc.yml"], health_none(), &cfg.state_dir);

    let engine = SyncEngine::new(&runner, &health, &clock);
    let outcome = engine
        .sync_service(&cfg, &svc, SyncOptions { dry_run: true })
        .await;

    assert_eq!(outcome.status, SyncStatus::Success);
    assert_eq!(outcome.compose_reports[0].outcome, ComposeOutcome::DryRun);
    // No mutating git/docker commands in dry-run.
    assert!(!runner.ran_matching(&["fetch"]));
    assert!(!runner.ran_matching(&["reset"]));
    assert!(!runner.ran_matching(&["pull"]));
    assert!(!runner.ran_matching(&["up", "-d"]));
}

#[tokio::test]
async fn env_change_not_persisted_on_failure() {
    let repo = tmpdir();
    std::fs::write(repo.join("dc.yml"), "x").unwrap();
    let env_file = repo.join(".env");
    std::fs::write(&env_file, "SECRET=new\n").unwrap();
    let state_dir = repo.join(".state");
    std::fs::create_dir_all(&state_dir).unwrap();
    std::fs::write(state_dir.join("backend.env.sha256"), "0000oldhash\n").unwrap();

    let runner = FakeRunner::new()
        .on(&["fetch"], 0, "")
        .on(&["rev-parse", "HEAD"], 0, "rev1")
        .on(&["rev-parse", "origin/main"], 0, "rev1")
        .on(&["config", "--format"], 0, COMPOSE_JSON)
        .on(&["pull"], 0, "")
        .on_fail(&["up", "-d"], 2, "boom")
        .with_default(0, "", "");
    let health = FakeHealthChecker::new();
    let clock = clock();
    let cfg = config(&repo);
    let mut svc = service("backend", &["dc.yml"], health_none(), &cfg.state_dir);
    svc.env_file = Some(env_file);

    let engine = SyncEngine::new(&runner, &health, &clock);
    let outcome = engine
        .sync_service(&cfg, &svc, SyncOptions { dry_run: false })
        .await;

    assert_eq!(outcome.status, SyncStatus::CommandFailed);
    assert!(outcome.env_changed);
    // Old hash MUST survive so the next tick retries the full-stack restart.
    let persisted = std::fs::read_to_string(cfg.state_dir.join("backend.env.sha256")).unwrap();
    assert_eq!(persisted.trim(), "0000oldhash");
}

#[tokio::test]
async fn multi_file_first_missing_second_still_runs() {
    let repo = tmpdir();
    // a.yml intentionally absent; dc.yml present and drifting.
    std::fs::write(repo.join("dc.yml"), "x").unwrap();
    let runner = FakeRunner::new()
        .on(&["fetch"], 0, "")
        .on(&["rev-parse", "HEAD"], 0, "rev1")
        .on(&["rev-parse", "origin/main"], 0, "rev1")
        .on(&["config", "--format"], 0, COMPOSE_JSON)
        .on(&["{{.Id}}"], 0, "sha:new")
        .on(&["ps", "-q", "server"], 0, "c1")
        .on(&["{{.Image}}"], 0, "sha:old")
        .on(&["pull"], 0, "")
        .on(&["up", "-d"], 0, "")
        .with_default(0, "", "");
    let health = FakeHealthChecker::new();
    let clock = clock();
    let cfg = config(&repo);
    let svc = service(
        "backend",
        &["a.yml", "dc.yml"],
        health_none(),
        &cfg.state_dir,
    );

    let engine = SyncEngine::new(&runner, &health, &clock);
    let outcome = engine
        .sync_service(&cfg, &svc, SyncOptions { dry_run: false })
        .await;

    // The failing first file must not abort processing of the second.
    assert_eq!(outcome.compose_reports.len(), 2);
    assert_eq!(outcome.compose_reports[0].outcome, ComposeOutcome::NotFound);
    assert_eq!(
        outcome.compose_reports[1].outcome,
        ComposeOutcome::Restarted
    );
    assert_eq!(outcome.status, SyncStatus::CommandFailed);
}

async fn run_order(files: &[&str]) -> SyncStatus {
    let repo = tmpdir();
    std::fs::write(repo.join("dc.yml"), "x").unwrap(); // "missing.yml" deliberately absent
    let runner = FakeRunner::new()
        .on(&["fetch"], 0, "")
        .on(&["rev-parse", "HEAD"], 0, "rev1")
        .on(&["rev-parse", "origin/main"], 0, "rev1")
        .on(&["config", "--format"], 0, COMPOSE_JSON)
        .on(&["{{.Id}}"], 0, "sha:new")
        .on(&["ps", "-q", "server"], 0, "c1")
        .on(&["{{.Image}}"], 0, "sha:old")
        .with_default(0, "", "");
    let health = FakeHealthChecker::new()
        .up("http://local/")
        .up("http://p1/")
        .down("http://p2/");
    let clock = clock();
    let cfg = config(&repo);
    let mut hc = health_none();
    hc.local_url = Some("http://local/".into());
    hc.peer_urls = vec!["http://p1/".into(), "http://p2/".into()];
    hc.peers_required = 2;
    let svc = service("backend", files, hc, &cfg.state_dir);
    let engine = SyncEngine::new(&runner, &health, &clock);
    engine
        .sync_service(&cfg, &svc, SyncOptions { dry_run: false })
        .await
        .status
}

#[tokio::test]
async fn multi_file_status_is_last_non_success() {
    // dc.yml -> RefusedQuorum, missing.yml -> CommandFailed; last write wins.
    assert_eq!(
        run_order(&["dc.yml", "missing.yml"]).await,
        SyncStatus::CommandFailed
    );
    assert_eq!(
        run_order(&["missing.yml", "dc.yml"]).await,
        SyncStatus::RefusedQuorum
    );
}

#[test]
fn worst_exit_code_is_max_across_services() {
    use crate::sync::{worst_exit_code, SyncOutcome};
    fn outcome(status: SyncStatus) -> SyncOutcome {
        SyncOutcome {
            service: "x".into(),
            dry_run: false,
            repo_advanced: false,
            from_rev: None,
            to_rev: None,
            env_changed: false,
            compose_reports: vec![],
            status,
            error: None,
        }
    }
    assert_eq!(worst_exit_code(&[]), 0);
    assert_eq!(
        worst_exit_code(&[outcome(SyncStatus::Success), outcome(SyncStatus::Success)]),
        0
    );
    assert_eq!(
        worst_exit_code(&[
            outcome(SyncStatus::Success),
            outcome(SyncStatus::CommandFailed),
            outcome(SyncStatus::RefusedQuorum),
        ]),
        3
    );
}
