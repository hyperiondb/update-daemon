use crate::error::Result;
use sha2::{Digest, Sha256};
use std::path::Path;

pub fn to_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0f) as usize] as char);
    }
    s
}

pub fn sha256_file(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path)?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    Ok(to_hex(&hasher.finalize()))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvHashStatus {
    /// Current hash of the env file, if it exists.
    pub current: Option<String>,
    /// Whether the hash differs from the persisted one. First-ever run is treated
    /// as *unchanged* (matching sync.sh) so a fresh host does not force a restart.
    pub changed: bool,
}

/// Evaluate the env file against the stored hash without mutating state.
pub fn evaluate(env_file: Option<&Path>, state_file: &Path) -> Result<EnvHashStatus> {
    let path = match env_file {
        Some(p) if p.exists() => p,
        _ => {
            return Ok(EnvHashStatus {
                current: None,
                changed: false,
            })
        }
    };

    let current = sha256_file(path)?;
    let stored = read_state(state_file);
    let changed = match stored {
        Some(prev) if !prev.is_empty() => prev != current,
        _ => false,
    };
    Ok(EnvHashStatus {
        current: Some(current),
        changed,
    })
}

pub fn read_state(state_file: &Path) -> Option<String> {
    std::fs::read_to_string(state_file)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

pub fn write_state(state_file: &Path, hash: &str) -> Result<()> {
    if let Some(dir) = state_file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(state_file, format!("{hash}\n"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        // Unique-enough within a single test process without extra deps.
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        p.push(format!("renew-envhash-{n}"));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn hex_encoding() {
        assert_eq!(to_hex(&[0x00, 0x0f, 0xff, 0xa5]), "000fffa5");
    }

    #[test]
    fn first_run_is_unchanged() {
        let dir = tmp();
        let env = dir.join(".env");
        std::fs::write(&env, "A=1\n").unwrap();
        let state = dir.join("env.sha256");
        let status = evaluate(Some(&env), &state).unwrap();
        assert!(status.current.is_some());
        assert!(!status.changed, "first run must be unchanged");
    }

    #[test]
    fn detects_change() {
        let dir = tmp();
        let env = dir.join(".env");
        std::fs::write(&env, "A=1\n").unwrap();
        let state = dir.join("env.sha256");

        let s1 = evaluate(Some(&env), &state).unwrap();
        write_state(&state, s1.current.as_ref().unwrap()).unwrap();

        // Unchanged after persisting.
        let s2 = evaluate(Some(&env), &state).unwrap();
        assert!(!s2.changed);

        // Now mutate the env file.
        std::fs::write(&env, "A=2\n").unwrap();
        let s3 = evaluate(Some(&env), &state).unwrap();
        assert!(s3.changed, "modified env must be detected");
    }

    #[test]
    fn missing_env_is_unchanged() {
        let dir = tmp();
        let state = dir.join("env.sha256");
        let status = evaluate(Some(&dir.join("nope.env")), &state).unwrap();
        assert!(status.current.is_none());
        assert!(!status.changed);
    }
}
