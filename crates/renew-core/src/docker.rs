use crate::error::{Error, Result};
use crate::runner::CommandRunner;
use serde_json::Value;
use std::path::Path;

/// Runner-backed docker / docker-compose operations, bound to one repo dir.
pub struct Docker<'a> {
    pub runner: &'a dyn CommandRunner,
    pub bin: &'a str,
    pub cwd: &'a Path,
    pub env_file: Option<&'a Path>,
    pub project: Option<&'a str>,
}

impl<'a> Docker<'a> {
    fn compose_base(&self, file: &str) -> Vec<String> {
        let mut a = vec!["compose".to_string(), "-f".to_string(), file.to_string()];
        // Only pass --env-file when the file actually exists; otherwise fall back
        // to compose auto-discovery (a missing --env-file path fails every op).
        if let Some(ef) = self.env_file.filter(|p| p.exists()) {
            a.push("--env-file".to_string());
            a.push(ef.display().to_string());
        }
        a
    }

    fn env(&self) -> Vec<(String, String)> {
        match self.project {
            Some(p) => vec![("COMPOSE_PROJECT_NAME".to_string(), p.to_string())],
            None => Vec::new(),
        }
    }

    pub async fn compose_config(&self, file: &str) -> Result<Vec<ComposeService>> {
        let mut a = self.compose_base(file);
        a.push("config".to_string());
        a.push("--format".to_string());
        a.push("json".to_string());
        let out = self
            .runner
            .run(self.bin, &a, Some(self.cwd), &self.env())
            .await?;
        out.require_success(self.bin)
            .map_err(|e| Error::Docker(format!("compose config failed for {file}: {e}")))?;
        parse_compose_config(&out.stdout)
    }

    pub async fn image_id(&self, image: &str) -> Result<Option<String>> {
        let a = vec![
            "image".to_string(),
            "inspect".to_string(),
            "--format".to_string(),
            "{{.Id}}".to_string(),
            image.to_string(),
        ];
        let out = self
            .runner
            .run(self.bin, &a, Some(self.cwd), &self.env())
            .await?;
        Ok(non_empty(out.stdout_trimmed()).filter(|_| out.success()))
    }

    pub async fn container_id(&self, file: &str, service: &str) -> Result<Option<String>> {
        let mut a = self.compose_base(file);
        a.push("ps".to_string());
        a.push("-q".to_string());
        a.push(service.to_string());
        let out = self
            .runner
            .run(self.bin, &a, Some(self.cwd), &self.env())
            .await?;
        if !out.success() {
            return Ok(None);
        }
        Ok(out
            .stdout
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .map(|s| s.to_string()))
    }

    pub async fn running_image(&self, container_id: &str) -> Result<Option<String>> {
        let a = vec![
            "inspect".to_string(),
            "--format".to_string(),
            "{{.Image}}".to_string(),
            container_id.to_string(),
        ];
        let out = self
            .runner
            .run(self.bin, &a, Some(self.cwd), &self.env())
            .await?;
        Ok(non_empty(out.stdout_trimmed()).filter(|_| out.success()))
    }

    pub async fn pull(&self, file: &str, services: &[String]) -> Result<()> {
        let mut a = self.compose_base(file);
        a.push("pull".to_string());
        a.push("--quiet".to_string());
        a.extend(services.iter().cloned());
        let out = self
            .runner
            .run(self.bin, &a, Some(self.cwd), &self.env())
            .await?;
        out.require_success(self.bin)
            .map_err(|e| Error::Docker(format!("pull failed for {file}: {e}")))?;
        Ok(())
    }

    pub async fn up(&self, file: &str, services: &[String]) -> Result<()> {
        let mut a = self.compose_base(file);
        a.push("up".to_string());
        a.push("-d".to_string());
        a.push("--no-deps".to_string());
        a.extend(services.iter().cloned());
        let out = self
            .runner
            .run(self.bin, &a, Some(self.cwd), &self.env())
            .await?;
        out.require_success(self.bin)
            .map_err(|e| Error::Docker(format!("up -d failed for {file}: {e}")))?;
        Ok(())
    }
}

fn non_empty(s: &str) -> Option<String> {
    let s = s.trim();
    if s.is_empty() {
        None
    } else {
        Some(s.to_string())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposeService {
    pub name: String,
    /// `None` for build-only services with no `image:` key.
    pub image: Option<String>,
}

/// Parse `docker compose config --format json` output into the declared services.
pub fn parse_compose_config(json: &str) -> Result<Vec<ComposeService>> {
    let v: Value = serde_json::from_str(json)
        .map_err(|e| Error::Docker(format!("invalid compose config json: {e}")))?;
    let services = v
        .get("services")
        .and_then(|s| s.as_object())
        .ok_or_else(|| Error::Docker("compose config has no services object".into()))?;

    let mut out = Vec::with_capacity(services.len());
    for (name, spec) in services {
        let image = spec
            .get("image")
            .and_then(|i| i.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());
        out.push(ComposeService {
            name: name.clone(),
            image,
        });
    }
    // Deterministic order for stable logs/tests.
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriftDecision {
    InSync,
    SkipBuildOnly,
    Restart(&'static str),
}

/// Decide whether a single service has drifted, mirroring sync.sh exactly:
///   - no declared image            -> build-only, skip
///   - no running container         -> restart
///   - declared image not pulled    -> restart
///   - running image != desired     -> restart
///   - otherwise                    -> in sync
pub fn decide(
    declared_image: Option<&str>,
    desired_id: Option<&str>,
    container_id: Option<&str>,
    running_id: Option<&str>,
) -> DriftDecision {
    if declared_image.is_none() {
        return DriftDecision::SkipBuildOnly;
    }
    let container_id = container_id.map(str::trim).filter(|s| !s.is_empty());
    if container_id.is_none() {
        return DriftDecision::Restart("no running container");
    }
    let desired_id = desired_id.map(str::trim).filter(|s| !s.is_empty());
    let running_id = running_id.map(str::trim).filter(|s| !s.is_empty());
    match desired_id {
        None => DriftDecision::Restart("declared image not pulled yet"),
        Some(desired) => match running_id {
            Some(running) if running == desired => DriftDecision::InSync,
            _ => DriftDecision::Restart("image drift"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_services_and_images() {
        let json = r#"{
            "services": {
                "server": { "image": "ghcr.io/app/server:abc" },
                "builder": { "build": { "context": "." } },
                "mongo": { "image": "mongo:7" }
            }
        }"#;
        let svc = parse_compose_config(json).unwrap();
        assert_eq!(svc.len(), 3);
        assert_eq!(svc[0].name, "builder");
        assert_eq!(svc[0].image, None);
        assert_eq!(svc[2].name, "server");
        assert_eq!(svc[2].image.as_deref(), Some("ghcr.io/app/server:abc"));
    }

    #[test]
    fn rejects_bad_json() {
        assert!(parse_compose_config("not json").is_err());
        assert!(parse_compose_config("{}").is_err());
    }

    #[test]
    fn drift_branches() {
        assert_eq!(decide(None, None, None, None), DriftDecision::SkipBuildOnly);
        assert_eq!(
            decide(Some("img"), Some("sha:desired"), None, None),
            DriftDecision::Restart("no running container")
        );
        assert_eq!(
            decide(Some("img"), None, Some("cid"), Some("sha:run")),
            DriftDecision::Restart("declared image not pulled yet")
        );
        assert_eq!(
            decide(
                Some("img"),
                Some("sha:desired"),
                Some("cid"),
                Some("sha:run")
            ),
            DriftDecision::Restart("image drift")
        );
        assert_eq!(
            decide(Some("img"), Some("sha:same"), Some("cid"), Some("sha:same")),
            DriftDecision::InSync
        );
    }
}
