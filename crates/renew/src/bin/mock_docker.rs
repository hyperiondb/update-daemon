//! A stand-in for the `docker` CLI used by the e2e tests. It emulates exactly the
//! subset of `docker` / `docker compose` calls the sync engine makes, reading the
//! "world" from a JSON fixture (MOCK_DOCKER_FIXTURE) and appending every
//! invocation to a log (MOCK_DOCKER_LOG) so tests can assert what ran.

use serde_json::{json, Value};
use std::io::Write;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if let Ok(log) = std::env::var("MOCK_DOCKER_LOG") {
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log)
        {
            let _ = writeln!(f, "{}", args.join(" "));
        }
    }

    let fixture_path = std::env::var("MOCK_DOCKER_FIXTURE").ok();
    let fixture: Value = fixture_path
        .as_ref()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| json!({}));

    let code = dispatch(&args, &fixture, fixture_path.as_deref());
    std::process::exit(code);
}

fn dispatch(args: &[String], fixture: &Value, fixture_path: Option<&str>) -> i32 {
    match args.first().map(String::as_str) {
        Some("compose") => compose(args, fixture, fixture_path),
        Some("image") if args.get(1).map(String::as_str) == Some("inspect") => {
            let image = match args.last() {
                Some(i) => i,
                None => return 1,
            };
            match fixture["local_images"].get(image).and_then(Value::as_str) {
                Some(id) => {
                    println!("{id}");
                    0
                }
                None => 1, // image not pulled locally
            }
        }
        Some("inspect") => {
            let cid = match args.last() {
                Some(c) => c,
                None => return 1,
            };
            if let Some(map) = fixture["containers"].as_object() {
                for (_svc, c) in map {
                    if c["id"].as_str() == Some(cid.as_str()) {
                        if let Some(img) = c["image"].as_str() {
                            println!("{img}");
                            return 0;
                        }
                    }
                }
            }
            1
        }
        _ => 0,
    }
}

fn compose(args: &[String], fixture: &Value, fixture_path: Option<&str>) -> i32 {
    let file = arg_after(args, "-f").unwrap_or_default();
    let sub = args
        .iter()
        .find(|a| matches!(a.as_str(), "config" | "ps" | "pull" | "up"))
        .map(String::as_str);

    match sub {
        Some("config") => {
            let services = fixture["compose"]
                .get(&file)
                .cloned()
                .unwrap_or_else(|| json!({ "services": {} }));
            println!("{services}");
            0
        }
        Some("ps") => {
            // `compose -f F ps -q <service>`
            if let Some(service) = args.last() {
                if let Some(c) = fixture["containers"].get(service.as_str()) {
                    if let Some(id) = c["id"].as_str() {
                        println!("{id}");
                    }
                }
            }
            0
        }
        Some("pull") => 0,
        Some("up") => {
            let services = trailing_services(args, "up");
            converge(fixture_path, &file, &services);
            0
        }
        _ => 0,
    }
}

fn arg_after(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn trailing_services(args: &[String], after: &str) -> Vec<String> {
    let start = match args.iter().position(|a| a == after) {
        Some(i) => i + 1,
        None => return Vec::new(),
    };
    args[start..]
        .iter()
        .filter(|a| !a.starts_with('-'))
        .cloned()
        .collect()
}

/// Simulate the effect of `up -d`: each restarted service's running container now
/// matches the locally-pulled image id, so a subsequent reconcile sees no drift.
fn converge(fixture_path: Option<&str>, file: &str, services: &[String]) {
    let path = match fixture_path {
        Some(p) => p,
        None => return,
    };
    let mut fixture: Value = match std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
    {
        Some(v) => v,
        None => return,
    };

    if !fixture["containers"].is_object() {
        fixture["containers"] = json!({});
    }

    for svc in services {
        let declared = fixture["compose"][file]["services"][svc]["image"]
            .as_str()
            .map(|s| s.to_string());
        if let Some(image) = declared {
            if let Some(new_id) = fixture["local_images"]
                .get(&image)
                .and_then(Value::as_str)
                .map(|s| s.to_string())
            {
                let entry = fixture["containers"]
                    .as_object_mut()
                    .map(|m| m.entry(svc.clone()).or_insert_with(|| json!({})));
                if let Some(c) = entry {
                    c["id"] = json!(format!("c_{svc}"));
                    c["image"] = json!(new_id);
                }
            }
        }
    }

    let _ = std::fs::write(path, fixture.to_string());
}
