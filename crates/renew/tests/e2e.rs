#![cfg(feature = "testkit")]

//! End-to-end test of the real `renew` binary against a real temporary
//! git repository and a mock `docker` (the `mockdocker` helper binary). No real
//! docker daemon is required; git is real. Exercises: config discovery, git
//! fetch + reset advancing the working tree, docker-compose drift detection, and
//! the pull + up reconcile — then a second pass proving convergence (no churn).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

static COUNTER: AtomicU64 = AtomicU64::new(0);

const DAEMON: &str = env!("CARGO_BIN_EXE_renew");
const MOCKDOCKER: &str = env!("CARGO_BIN_EXE_mockdocker");

fn tmpdir(tag: &str) -> PathBuf {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let c = COUNTER.fetch_add(1, Ordering::SeqCst);
    let mut p = std::env::temp_dir();
    p.push(format!("renew-e2e-{tag}-{n}-{c}"));
    std::fs::create_dir_all(&p).unwrap();
    p
}

struct Output {
    code: i32,
    stdout: String,
    stderr: String,
}

fn git(cwd: &Path, args: &[&str]) -> Output {
    run("git", args, Some(cwd), &[])
}

fn run(program: &str, args: &[&str], cwd: Option<&Path>, envs: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(program);
    cmd.args(args);
    if let Some(d) = cwd {
        cmd.current_dir(d);
    }
    // Deterministic identity so commits work without global git config.
    cmd.env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@e")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@e");
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let out = cmd
        .output()
        .unwrap_or_else(|e| panic!("spawn {program} failed: {e}"));
    Output {
        code: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

fn write(path: &Path, contents: &str) {
    std::fs::write(path, contents).unwrap();
}

const CONFIG: &str = r#"{
  "repo": { "remote": "origin", "branch": "main" },
  "services": [
    { "name": "backend", "compose_files": ["docker-compose1.yml"], "schedule": "*/3 * * * *" }
  ]
}"#;

const FIXTURE: &str = r#"{
  "compose": {
    "docker-compose1.yml": {
      "services": {
        "server": { "image": "img:server" },
        "builder": { "build": { "context": "." } }
      }
    }
  },
  "local_images": { "img:server": "sha:v2" },
  "containers": { "server": { "id": "c_server", "image": "sha:v1" } }
}"#;

#[test]
fn end_to_end_pull_reset_and_reconcile() {
    let base = tmpdir("repo");
    let remote = base.join("remote.git");
    let work = base.join("work");
    let work2 = base.join("work2");
    let fixture = base.join("fixture.json");
    let log = base.join("mock.log");

    // 1. bare remote + working clone
    assert_ok(run(
        "git",
        &["init", "--bare", "-b", "main", remote.to_str().unwrap()],
        None,
        &[],
    ));
    assert_ok(run(
        "git",
        &["clone", remote.to_str().unwrap(), work.to_str().unwrap()],
        None,
        &[],
    ));

    // 2. seed the repo with the config + a compose file and push
    write(&work.join("docker-compose1.yml"), "services: {}\n");
    write(&work.join("renew.json"), CONFIG);
    assert_ok(git(&work, &["add", "-A"]));
    assert_ok(git(&work, &["commit", "-m", "init"]));
    assert_ok(git(&work, &["push", "origin", "main"]));

    // 3. advance the remote one commit, leaving `work` behind
    assert_ok(run(
        "git",
        &["clone", remote.to_str().unwrap(), work2.to_str().unwrap()],
        None,
        &[],
    ));
    write(&work2.join("RELEASE.txt"), "v2\n");
    assert_ok(git(&work2, &["add", "-A"]));
    assert_ok(git(&work2, &["commit", "-m", "release v2"]));
    assert_ok(git(&work2, &["push", "origin", "main"]));
    let expected_head = git(&work2, &["rev-parse", "HEAD"])
        .stdout
        .trim()
        .to_string();

    // sanity: work is behind remote
    let before = git(&work, &["rev-parse", "HEAD"]).stdout.trim().to_string();
    assert_ne!(before, expected_head, "work should start behind remote");

    write(&fixture, FIXTURE);
    write(&log, "");

    // 4. first sync — drift present: expect git advance + pull + up
    let envs = [
        ("RENEW_DOCKER_BIN", MOCKDOCKER),
        ("MOCK_DOCKER_FIXTURE", fixture.to_str().unwrap()),
        ("MOCK_DOCKER_LOG", log.to_str().unwrap()),
        ("RUST_LOG", "warn"),
    ];
    let out = run(
        DAEMON,
        &["sync", "--service", "backend"],
        Some(&work),
        &envs,
    );
    assert_eq!(out.code, 0, "sync failed: {}\n{}", out.stdout, out.stderr);

    // git advanced to remote HEAD
    let after = git(&work, &["rev-parse", "HEAD"]).stdout.trim().to_string();
    assert_eq!(
        after, expected_head,
        "repo should be advanced to remote HEAD"
    );

    // docker drift reconciled
    let log1 = std::fs::read_to_string(&log).unwrap();
    assert!(
        log1.contains("config --format json"),
        "should read compose config:\n{log1}"
    );
    assert!(
        log1.contains("pull"),
        "should pull the drifted image:\n{log1}"
    );
    assert!(
        log1.lines()
            .any(|l| l.contains("up -d --no-deps") && l.contains("server")),
        "should bring up the drifted service:\n{log1}"
    );

    // 5. second sync — converged: no pull / up churn
    write(&log, "");
    let out2 = run(
        DAEMON,
        &["sync", "--service", "backend"],
        Some(&work),
        &envs,
    );
    assert_eq!(
        out2.code, 0,
        "second sync failed: {}\n{}",
        out2.stdout, out2.stderr
    );
    let log2 = std::fs::read_to_string(&log).unwrap();
    assert!(
        !log2.contains("pull"),
        "converged state must not pull again:\n{log2}"
    );
    assert!(
        !log2.lines().any(|l| l.contains("up -d")),
        "converged state must not restart again:\n{log2}"
    );
}

#[test]
fn validate_subcommand_reports_schedule() {
    let base = tmpdir("validate");
    write(&base.join("docker-compose1.yml"), "services: {}\n");
    write(&base.join("renew.json"), CONFIG);

    let out = run(DAEMON, &["validate"], Some(&base), &[]);
    assert_eq!(out.code, 0, "validate failed: {}", out.stderr);
    assert!(out.stdout.contains("backend"), "{}", out.stdout);
    assert!(out.stdout.contains("*/3 * * * *"), "{}", out.stdout);
    assert!(out.stdout.contains("next fires"), "{}", out.stdout);
}

fn assert_ok(out: Output) {
    assert_eq!(
        out.code, 0,
        "command failed: {}\n{}",
        out.stdout, out.stderr
    );
}
