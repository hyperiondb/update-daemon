use crate::error::{Error, Result};
use crate::schedule::Schedule;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub const CONFIG_FILE_NAME: &str = "renew.json";
const DEFAULT_SCHEDULE: &str = "*/3 * * * *";
const DEFAULT_HEALTH_TIMEOUT_SECS: u64 = 5;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawConfig {
    #[serde(default)]
    pub repo: RepoCfg,
    pub state_dir: Option<String>,
    pub git_bin: Option<String>,
    pub docker_bin: Option<String>,
    #[serde(default)]
    pub run_on_start: bool,
    pub log_format: Option<String>,
    #[serde(default)]
    pub defaults: ServiceDefaults,
    pub services: Vec<RawService>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoCfg {
    pub dir: Option<String>,
    #[serde(default = "default_remote")]
    pub remote: String,
    #[serde(default = "default_branch")]
    pub branch: String,
}

impl Default for RepoCfg {
    fn default() -> Self {
        Self {
            dir: None,
            remote: default_remote(),
            branch: default_branch(),
        }
    }
}

fn default_remote() -> String {
    "origin".to_string()
}
fn default_branch() -> String {
    "main".to_string()
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceDefaults {
    pub schedule: Option<String>,
    pub env_file: Option<String>,
    pub clean_script: Option<String>,
    pub compose_project_name: Option<String>,
    #[serde(default)]
    pub health: HealthCfg,
    pub self_hosts: Option<Vec<String>>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HealthCfg {
    pub local_url: Option<String>,
    #[serde(default)]
    pub peer_urls: Vec<String>,
    pub peers_required: Option<usize>,
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawService {
    pub name: String,
    pub compose_files: Vec<String>,
    pub schedule: Option<String>,
    pub env_file: Option<String>,
    pub clean_script: Option<String>,
    pub compose_project_name: Option<String>,
    pub health: Option<HealthCfg>,
    pub self_hosts: Option<Vec<String>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogFormat {
    Text,
    Json,
}

#[derive(Debug, Clone)]
pub struct ResolvedHealth {
    pub local_url: Option<String>,
    pub peer_urls: Vec<String>,
    pub peers_required: usize,
    pub timeout: Duration,
}

#[derive(Debug, Clone)]
pub struct ResolvedService {
    pub name: String,
    pub compose_files: Vec<String>,
    pub env_file: Option<PathBuf>,
    pub clean_script: Option<PathBuf>,
    pub compose_project_name: Option<String>,
    pub schedule_expr: String,
    pub schedule: Schedule,
    pub health: ResolvedHealth,
    pub self_hosts: Vec<String>,
    pub env_state_file: PathBuf,
}

#[derive(Debug, Clone)]
pub struct ResolvedConfig {
    pub config_path: PathBuf,
    pub repo_dir: PathBuf,
    pub remote: String,
    pub branch: String,
    pub state_dir: PathBuf,
    pub git_bin: String,
    pub docker_bin: String,
    pub run_on_start: bool,
    pub log_format: LogFormat,
    pub services: Vec<ResolvedService>,
}

impl ResolvedConfig {
    /// Find the nearest `renew.json` walking up from `start`.
    pub fn discover(start: &Path) -> Option<PathBuf> {
        let mut dir = Some(start);
        while let Some(d) = dir {
            let candidate = d.join(CONFIG_FILE_NAME);
            if candidate.is_file() {
                return Some(candidate);
            }
            dir = d.parent();
        }
        None
    }

    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| Error::Config(format!("cannot read {}: {e}", path.display())))?;
        let raw: RawConfig = serde_json::from_str(&text)
            .map_err(|e| Error::Config(format!("cannot parse {}: {e}", path.display())))?;
        Self::resolve(raw, path)
    }

    pub fn resolve(raw: RawConfig, config_path: &Path) -> Result<Self> {
        let config_dir = config_path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));

        let repo_dir = match &raw.repo.dir {
            Some(d) => resolve_path(&config_dir, d),
            None => config_dir.clone(),
        };

        let state_dir = match &raw.state_dir {
            Some(d) => resolve_path(&config_dir, d),
            None => repo_dir.join(".renew"),
        };

        let git_bin = raw.git_bin.clone().unwrap_or_else(|| "git".to_string());
        let docker_bin = raw
            .docker_bin
            .clone()
            .unwrap_or_else(|| "docker".to_string());

        let log_format = match raw.log_format.as_deref() {
            None | Some("text") => LogFormat::Text,
            Some("json") => LogFormat::Json,
            Some(other) => {
                return Err(Error::Config(format!(
                    "log_format must be `text` or `json`, got `{other}`"
                )))
            }
        };

        if raw.services.is_empty() {
            return Err(Error::Config("at least one service is required".into()));
        }

        let mut seen = std::collections::HashSet::new();
        let mut services = Vec::with_capacity(raw.services.len());
        for svc in &raw.services {
            if svc.name.trim().is_empty() {
                return Err(Error::Config("service name must not be empty".into()));
            }
            if !is_safe_name(&svc.name) {
                return Err(Error::Config(format!(
                    "service name `{}` may only contain [A-Za-z0-9._-]",
                    svc.name
                )));
            }
            if is_windows_reserved(&svc.name) {
                return Err(Error::Config(format!(
                    "service name `{}` is a reserved Windows device name",
                    svc.name
                )));
            }
            // Compare case-insensitively: state files collide on case-insensitive
            // filesystems (NTFS, APFS), so `Backend` and `backend` are duplicates.
            if !seen.insert(svc.name.to_ascii_lowercase()) {
                return Err(Error::Config(format!(
                    "duplicate service name `{}` (names are compared case-insensitively)",
                    svc.name
                )));
            }
            if svc.compose_files.is_empty() {
                return Err(Error::Config(format!(
                    "service `{}` must declare at least one compose file",
                    svc.name
                )));
            }

            let schedule_expr = svc
                .schedule
                .clone()
                .or_else(|| raw.defaults.schedule.clone())
                .unwrap_or_else(|| DEFAULT_SCHEDULE.to_string());
            let schedule = Schedule::parse(&schedule_expr).map_err(|e| {
                Error::Config(format!("service `{}`: invalid schedule: {e}", svc.name))
            })?;

            let env_file = svc
                .env_file
                .clone()
                .or_else(|| raw.defaults.env_file.clone())
                .map(|p| resolve_path(&repo_dir, &p));

            let clean_script = svc
                .clean_script
                .clone()
                .or_else(|| raw.defaults.clean_script.clone())
                .map(|p| resolve_path(&repo_dir, &p));

            let compose_project_name = svc
                .compose_project_name
                .clone()
                .or_else(|| raw.defaults.compose_project_name.clone())
                .filter(|s| !s.is_empty());

            let raw_health = svc
                .health
                .clone()
                .unwrap_or_else(|| raw.defaults.health.clone());
            let peers_required = match raw_health.peers_required {
                Some(n) => n,
                None if !raw_health.peer_urls.is_empty() => 1,
                None => 0,
            };
            if peers_required > raw_health.peer_urls.len() {
                return Err(Error::Config(format!(
                    "service `{}`: peers_required ({}) exceeds the {} configured peer_urls; quorum could never be met",
                    svc.name,
                    peers_required,
                    raw_health.peer_urls.len()
                )));
            }
            if raw_health.timeout_secs == Some(0) {
                return Err(Error::Config(format!(
                    "service `{}`: health.timeout_secs must be greater than 0",
                    svc.name
                )));
            }
            let health = ResolvedHealth {
                local_url: raw_health.local_url.filter(|s| !s.is_empty()),
                peer_urls: raw_health.peer_urls,
                peers_required,
                timeout: Duration::from_secs(
                    raw_health
                        .timeout_secs
                        .unwrap_or(DEFAULT_HEALTH_TIMEOUT_SECS),
                ),
            };

            let mut self_hosts = raw.defaults.self_hosts.clone().unwrap_or_default();
            if let Some(extra) = &svc.self_hosts {
                self_hosts.extend(extra.iter().cloned());
            }

            let env_state_file = state_dir.join(format!("{}.env.sha256", svc.name));

            services.push(ResolvedService {
                name: svc.name.clone(),
                compose_files: svc.compose_files.clone(),
                env_file,
                clean_script,
                compose_project_name,
                schedule_expr,
                schedule,
                health,
                self_hosts,
                env_state_file,
            });
        }

        Ok(ResolvedConfig {
            config_path: config_path.to_path_buf(),
            repo_dir,
            remote: raw.repo.remote,
            branch: raw.repo.branch,
            state_dir,
            git_bin,
            docker_bin,
            run_on_start: raw.run_on_start,
            log_format,
            services,
        })
    }
}

fn resolve_path(base: &Path, p: &str) -> PathBuf {
    let candidate = Path::new(p);
    if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        base.join(candidate)
    }
}

fn is_safe_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

fn is_windows_reserved(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name);
    let stem = stem.trim_end_matches([' ', '.']).to_ascii_uppercase();
    matches!(
        stem.as_str(),
        "CON"
            | "PRN"
            | "AUX"
            | "NUL"
            | "COM1"
            | "COM2"
            | "COM3"
            | "COM4"
            | "COM5"
            | "COM6"
            | "COM7"
            | "COM8"
            | "COM9"
            | "LPT1"
            | "LPT2"
            | "LPT3"
            | "LPT4"
            | "LPT5"
            | "LPT6"
            | "LPT7"
            | "LPT8"
            | "LPT9"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(json: &str) -> Result<ResolvedConfig> {
        let raw: RawConfig = serde_json::from_str(json)?;
        ResolvedConfig::resolve(raw, Path::new("/project/renew.json"))
    }

    #[test]
    fn minimal_config_with_defaults() {
        let cfg = parse(
            r#"{ "services": [ { "name": "backend", "compose_files": ["docker-compose1.yml"] } ] }"#,
        )
        .unwrap();
        assert_eq!(cfg.remote, "origin");
        assert_eq!(cfg.branch, "main");
        assert_eq!(cfg.repo_dir, PathBuf::from("/project"));
        assert_eq!(cfg.git_bin, "git");
        assert_eq!(cfg.services.len(), 1);
        let s = &cfg.services[0];
        assert_eq!(s.schedule_expr, "*/3 * * * *");
        assert_eq!(s.health.peers_required, 0);
        assert!(cfg.state_dir.ends_with(".renew"));
        assert!(s.env_state_file.ends_with("backend.env.sha256"));
    }

    #[test]
    fn defaults_merge_into_services() {
        let cfg = parse(
            r#"{
                "defaults": { "schedule": "*/5 * * * *", "env_file": ".env" },
                "services": [
                    { "name": "a", "compose_files": ["a.yml"] },
                    { "name": "b", "compose_files": ["b.yml"], "schedule": "1-59/3 * * * *" }
                ]
            }"#,
        )
        .unwrap();
        assert_eq!(cfg.services[0].schedule_expr, "*/5 * * * *");
        assert_eq!(cfg.services[1].schedule_expr, "1-59/3 * * * *");
        assert!(cfg.services[0].env_file.as_ref().unwrap().ends_with(".env"));
    }

    #[test]
    fn peers_required_defaults_to_one_when_peers_present() {
        let cfg = parse(
            r#"{ "services": [ {
                "name": "x", "compose_files": ["x.yml"],
                "health": { "local_url": "http://127.0.0.1:3010/", "peer_urls": ["http://10.0.0.2:3010/"] }
            } ] }"#,
        )
        .unwrap();
        assert_eq!(cfg.services[0].health.peers_required, 1);
    }

    #[test]
    fn rejects_empty_services() {
        assert!(parse(r#"{ "services": [] }"#).is_err());
    }

    #[test]
    fn rejects_duplicate_names() {
        let r = parse(
            r#"{ "services": [
                { "name": "a", "compose_files": ["a.yml"] },
                { "name": "a", "compose_files": ["b.yml"] }
            ] }"#,
        );
        assert!(r.is_err());
    }

    #[test]
    fn rejects_unsafe_name() {
        assert!(
            parse(r#"{ "services": [ { "name": "../evil", "compose_files": ["a.yml"] } ] }"#)
                .is_err()
        );
    }

    #[test]
    fn rejects_unknown_fields() {
        assert!(parse(
            r#"{ "services": [ { "name": "a", "compose_files": ["a.yml"], "bogus": 1 } ] }"#
        )
        .is_err());
    }

    #[test]
    fn rejects_missing_compose_files() {
        assert!(parse(r#"{ "services": [ { "name": "a", "compose_files": [] } ] }"#).is_err());
    }

    #[test]
    fn rejects_windows_reserved_names() {
        assert!(
            parse(r#"{ "services": [ { "name": "nul", "compose_files": ["a.yml"] } ] }"#).is_err()
        );
        assert!(
            parse(r#"{ "services": [ { "name": "COM1", "compose_files": ["a.yml"] } ] }"#).is_err()
        );
        assert!(
            parse(r#"{ "services": [ { "name": "console", "compose_files": ["a.yml"] } ] }"#)
                .is_ok()
        );
    }

    #[test]
    fn rejects_case_insensitive_duplicates() {
        let r = parse(
            r#"{ "services": [
                { "name": "Backend", "compose_files": ["a.yml"] },
                { "name": "backend", "compose_files": ["b.yml"] }
            ] }"#,
        );
        assert!(r.is_err());
    }

    #[test]
    fn rejects_peers_required_over_peer_count() {
        assert!(parse(
            r#"{ "services": [ {
                "name": "x", "compose_files": ["x.yml"],
                "health": { "peer_urls": ["http://a/"], "peers_required": 2 }
            } ] }"#
        )
        .is_err());
    }

    #[test]
    fn rejects_zero_timeout() {
        assert!(parse(
            r#"{ "services": [ {
                "name": "x", "compose_files": ["x.yml"],
                "health": { "local_url": "http://a/", "timeout_secs": 0 }
            } ] }"#
        )
        .is_err());
    }
}
