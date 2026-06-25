mod health;

use clap::{Parser, Subcommand};
use health::HttpHealthChecker;
use renew_core::config::LogFormat;
use renew_core::sync::worst_exit_code;
use renew_core::{
    clock::format_utc, resolve_config_path, Clock, Daemon, ResolvedConfig, SystemClock,
    SystemRunner,
};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use tokio::sync::Notify;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(name = "renew", version, about = "Self-healing deployment daemon")]
struct Cli {
    /// Path to renew.json (default: discovered by walking up from cwd).
    #[arg(long, short, global = true, env = "RENEW_CONFIG")]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the scheduler loop until interrupted (default).
    Run,
    /// Run one reconcile pass over the selected services and exit.
    Sync {
        /// Limit to these service names (repeatable). Default: all.
        #[arg(long)]
        service: Vec<String>,
        /// Log intended actions without touching git or docker.
        #[arg(long)]
        dry_run: bool,
    },
    /// Validate the config and print the resolved schedule for each service.
    Validate,
}

fn main() -> ExitCode {
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("failed to start runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    rt.block_on(async_main())
}

async fn async_main() -> ExitCode {
    let cli = Cli::parse();
    let config_path = match resolve_config_path(cli.config.as_deref()) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("config error: {e}");
            return ExitCode::FAILURE;
        }
    };

    let mut cfg = match ResolvedConfig::load(&config_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    apply_env_overrides(&mut cfg);
    init_logging(cfg.log_format);

    match cli.command.unwrap_or(Command::Run) {
        Command::Run => run_daemon(config_path).await,
        Command::Sync { service, dry_run } => run_sync(&cfg, service, dry_run).await,
        Command::Validate => validate(&cfg),
    }
}

async fn run_daemon(config_path: PathBuf) -> ExitCode {
    let daemon = Daemon::new(
        Arc::new(build_runner()),
        Arc::new(HttpHealthChecker::new()),
        Arc::new(SystemClock),
    );
    let shutdown = Arc::new(Notify::new());
    spawn_signal_listener(shutdown.clone());

    match daemon.run(config_path, shutdown).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("daemon error: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run_sync(cfg: &ResolvedConfig, service: Vec<String>, dry_run: bool) -> ExitCode {
    let daemon = Daemon::new(
        Arc::new(build_runner()),
        Arc::new(HttpHealthChecker::new()),
        Arc::new(SystemClock),
    );
    let filter = if service.is_empty() {
        None
    } else {
        Some(service.as_slice())
    };
    let outcomes = daemon.run_once(cfg, filter, dry_run).await;
    let code = worst_exit_code(&outcomes);
    ExitCode::from(code as u8)
}

fn validate(cfg: &ResolvedConfig) -> ExitCode {
    let clock = SystemClock;
    let now = clock.now_utc();
    println!("config:        {}", cfg.config_path.display());
    println!("repo dir:      {}", cfg.repo_dir.display());
    println!("remote/branch: {}/{}", cfg.remote, cfg.branch);
    println!("state dir:     {}", cfg.state_dir.display());
    println!("run on start:  {}", cfg.run_on_start);
    println!("services:      {}", cfg.services.len());
    for svc in &cfg.services {
        println!();
        println!("  [{}]", svc.name);
        println!("    schedule:      {}", svc.schedule_expr);
        println!("    compose files: {}", svc.compose_files.join(", "));
        if let Some(env) = &svc.env_file {
            println!("    env file:      {}", env.display());
        }
        if svc.health.local_url.is_some() || !svc.health.peer_urls.is_empty() {
            println!(
                "    health:        local={} peers={} required={}",
                svc.health.local_url.as_deref().unwrap_or("(none)"),
                svc.health.peer_urls.len(),
                svc.health.peers_required
            );
        }
        let mut t = now;
        print!("    next fires:    ");
        let mut printed = Vec::new();
        for _ in 0..3 {
            match svc.schedule.next_after(t) {
                Some(next) => {
                    printed.push(format_utc(next));
                    t = next;
                }
                None => break,
            }
        }
        println!("{}", printed.join("  "));
    }
    ExitCode::SUCCESS
}

fn apply_env_overrides(cfg: &mut ResolvedConfig) {
    if let Ok(git) = std::env::var("RENEW_GIT_BIN") {
        if !git.is_empty() {
            cfg.git_bin = git;
        }
    }
    if let Ok(docker) = std::env::var("RENEW_DOCKER_BIN") {
        if !docker.is_empty() {
            cfg.docker_bin = docker;
        }
    }
}

fn build_runner() -> SystemRunner {
    match std::env::var("RENEW_COMMAND_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|&n| n > 0)
    {
        Some(secs) => SystemRunner::with_timeout(std::time::Duration::from_secs(secs)),
        None => SystemRunner::new(),
    }
}

fn init_logging(format: LogFormat) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    match format {
        LogFormat::Json => builder.json().init(),
        LogFormat::Text => builder.init(),
    }
}

/// Listen for shutdown signals. The first triggers a graceful drain; a second
/// forces an immediate exit so an operator is never stuck behind a slow drain.
fn spawn_signal_listener(shutdown: Arc<Notify>) {
    tokio::spawn(async move {
        wait_for_signal().await;
        shutdown.notify_one();
        wait_for_signal().await;
        eprintln!("second signal received — forcing immediate exit");
        std::process::exit(130);
    });
}

async fn wait_for_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
