//! Taken host ports (VISION section 6): when another program holds a published port, the service gets the next
//! free one. The choice is stored in the instance data directory, so the port stays the same across runs.

use std::collections::BTreeMap;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{Value, json};

/// Per service: the published port the compose file asks for -> the host port rizr uses.
pub type Remaps = BTreeMap<String, BTreeMap<u16, u16>>;

fn store(dir: &Path) -> PathBuf {
    dir.join("ports.json")
}

pub fn load(dir: &Path) -> Remaps {
    std::fs::read(store(dir))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

/// `http://localhost:<port>` lines for each remapped port, for status and context.
pub fn lines(dir: &Path) -> Vec<String> {
    load(dir)
        .iter()
        .flat_map(|(svc, m)| {
            m.iter().map(move |(want, host)| {
                format!("{svc}: host port {want} is taken by another program; using {host} (http://localhost:{host})")
            })
        })
        .collect()
}

/// A port is taken when it cannot be bound and no container of this compose project publishes it.
fn taken_by_other(ip: &str, port: u16, project: &str) -> bool {
    if TcpListener::bind((ip, port)).is_ok() {
        return false;
    }
    let ours = Command::new("docker")
        .args(["ps", "-q", "--filter"])
        .arg(format!("publish={port}"))
        .arg("--filter")
        .arg(format!("label=com.docker.compose.project={project}"))
        .output()
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false);
    !ours
}

fn free_after(ip: &str, port: u16, project: &str) -> Option<u16> {
    (port.checked_add(1)?..=u16::MAX).find(|p| !taken_by_other(ip, *p, project))
}

/// Writes `ports-<svc>.yml` when a published port of `svc` is remapped, and returns it for `-f`.
/// `ports` is the service's `ports` list from `docker compose config --format json`.
pub fn prepare(
    dir: &Path,
    project: &str,
    svc: &str,
    ports: &[Value],
) -> Result<Option<PathBuf>, String> {
    let mut all = load(dir);
    let mut changed = false;
    let mut out = Vec::new();
    for p in ports {
        let mut p = p.clone();
        let want = p["published"].as_str().and_then(|s| s.parse::<u16>().ok());
        if let Some(want) = want {
            let ip = p["host_ip"].as_str().unwrap_or("0.0.0.0").to_string();
            let known = all.get(svc).and_then(|m| m.get(&want)).copied();
            let host = match known {
                Some(h) => Some(h),
                None if taken_by_other(&ip, want, project) => {
                    let h = free_after(&ip, want, project);
                    if let Some(h) = h {
                        all.entry(svc.into()).or_default().insert(want, h);
                        changed = true;
                    }
                    h
                }
                None => None,
            };
            if let Some(h) = host {
                p["published"] = json!(h.to_string());
            }
        }
        out.push(p);
    }
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    if changed {
        std::fs::write(
            store(dir),
            serde_json::to_vec_pretty(&all).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
    }
    let path = dir.join(format!("ports-{svc}.yml"));
    if !all.contains_key(svc) {
        let _ = std::fs::remove_file(&path);
        return Ok(None);
    }
    // `!override` replaces the list; a plain list would be appended to the repo's ports. Flow JSON is YAML.
    let body = format!(
        "services:\n  {}:\n    ports: !override {}\n",
        serde_json::to_string(svc).map_err(|e| e.to_string())?,
        Value::Array(out)
    );
    std::fs::write(&path, body).map_err(|e| e.to_string())?;
    Ok(Some(path))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "a test panics on a broken precondition")]
mod tests {
    use super::*;

    #[test]
    fn a_port_held_by_another_program_moves_and_stays_moved() {
        let dir = tempfile::tempdir().unwrap();
        let other = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let want = other.local_addr().unwrap().port();
        let ports = [
            json!({"target": 80, "published": want.to_string(), "host_ip": "127.0.0.1", "protocol": "tcp"}),
        ];
        let file = prepare(dir.path(), "rizr-test-none", "app", &ports)
            .unwrap()
            .unwrap();
        let host = load(dir.path())["app"][&want];
        assert_ne!(host, want);
        assert!(
            std::fs::read_to_string(&file)
                .unwrap()
                .contains(&format!("\"published\":\"{host}\""))
        );
        drop(other);
        prepare(dir.path(), "rizr-test-none", "app", &ports).unwrap();
        assert_eq!(
            load(dir.path())["app"][&want],
            host,
            "the port stays the same across runs"
        );
    }
}
