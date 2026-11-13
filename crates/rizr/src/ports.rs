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

/// Each published TCP port of compose project `project` with the result of one HTTP GET to it, so an agent
/// needs no own curl. A report for the agent, not a readiness probe (those never use host ports).
pub fn checks(project: &str) -> Vec<Value> {
    let out = Command::new("docker")
        .args(["ps", "--filter"])
        .arg(format!("label=com.docker.compose.project={project}"))
        .args([
            "--format",
            "{{.Label \"com.docker.compose.service\"}}\t{{.Ports}}",
        ])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    let mut found = std::collections::BTreeSet::new();
    for (svc, ports) in out.lines().filter_map(|l| l.split_once('\t')) {
        for p in ports.split(", ") {
            let Some((host, _)) = p.split_once("->").filter(|_| p.ends_with("/tcp")) else {
                continue;
            };
            if let Some(port) = host.rsplit(':').next().and_then(|n| n.parse::<u16>().ok()) {
                found.insert((svc.to_owned(), port));
            }
        }
    }
    std::thread::scope(|s| {
        let runs: Vec<_> = found
            .iter()
            .map(|(svc, port)| (svc, port, s.spawn(|| http_get(*port))))
            .collect();
        runs.into_iter()
            .map(|(svc, port, h)| {
                json!({"node": svc, "url": format!("http://localhost:{port}"), "check": h.join().unwrap_or_default()})
            })
            .collect()
    })
}

/// `GET /` with a 5 s limit: the status line, or why there is none.
fn http_get(port: u16) -> String {
    use std::io::{Read, Write};
    use std::time::Duration;
    let t = Duration::from_secs(5);
    let Ok(mut c) = std::net::TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), t) else {
        return "connection refused".into();
    };
    let _ = c.set_read_timeout(Some(t));
    let req = format!("GET / HTTP/1.0\r\nHost: localhost:{port}\r\n\r\n");
    let mut buf = [0u8; 128];
    let n = match c.write_all(req.as_bytes()).and_then(|()| c.read(&mut buf)) {
        Ok(n) => n,
        Err(e)
            if e.kind() == std::io::ErrorKind::WouldBlock
                || e.kind() == std::io::ErrorKind::TimedOut =>
        {
            return "port open, no HTTP reply in 5 s".into();
        }
        Err(e) => return format!("port open, {e}"),
    };
    let head = String::from_utf8_lossy(&buf[..n]);
    match head.lines().next() {
        Some(l) if l.starts_with("HTTP/") => {
            format!("HTTP {}", l.split_once(' ').map_or("", |x| x.1))
        }
        _ if n == 0 => "port open, closed without a reply".into(),
        _ => "port open, not HTTP".into(),
    }
}

/// A port is taken by another program when it cannot be bound and no container started from this repo
/// publishes it. A container of this repo (this stack, or a copy outside rizr) is not moved around: `strays`
/// reports a copy outside rizr with the command that stops it.
fn taken_by_other(ip: &str, port: u16, root: &Path) -> bool {
    if TcpListener::bind((ip, port)).is_ok() {
        return false;
    }
    let ours = Command::new("docker")
        .args(["ps", "-q", "--filter"])
        .arg(format!("publish={port}"))
        .arg("--filter")
        .arg(format!(
            "label=com.docker.compose.project.working_dir={}",
            root.display()
        ))
        .output()
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false);
    !ours
}

fn free_after(ip: &str, port: u16, root: &Path) -> Option<u16> {
    (port.checked_add(1)?..=u16::MAX).find(|p| !taken_by_other(ip, *p, root))
}

/// Compose projects started from this repo outside rizr (no `rizr.project` label), with their container count.
/// `own` is rizr's compose name for this repo: `up` adopts those containers, so they are no strays.
pub fn strays(root: &Path, own: &str) -> Vec<(String, usize)> {
    let out = Command::new("docker")
        .args(["ps", "-a", "--filter"])
        .arg(format!(
            "label=com.docker.compose.project.working_dir={}",
            root.display()
        ))
        .args([
            "--format",
            "{{.Label \"com.docker.compose.project\"}} {{.Label \"rizr.project\"}}",
        ])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    let mut n: BTreeMap<String, usize> = BTreeMap::new();
    for (p, rizr) in out.lines().filter_map(|l| l.split_once(' ')) {
        if !p.is_empty() && p != own && rizr.trim().is_empty() {
            *n.entry(p.into()).or_default() += 1;
        }
    }
    n.into_iter().collect()
}

/// One line per stray project, with the command that stops it; volumes stay.
pub fn stray_lines(root: &Path, own: &str) -> Vec<String> {
    strays(root, own)
        .into_iter()
        .map(|(p, n)| {
            format!("this repo also runs outside rizr as compose project `{p}` ({n} containers); it can hold ports and confuse checks; stop it, volumes stay: docker compose -p {p} down --remove-orphans")
        })
        .collect()
}

/// Writes `ports-<svc>.yml` when a published port of `svc` is remapped, and returns it for `-f`.
/// `ports` is the service's `ports` list from `docker compose config --format json`.
pub fn prepare(
    dir: &Path,
    root: &Path,
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
                None if taken_by_other(&ip, want, root) => {
                    let h = free_after(&ip, want, root);
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
        let file = prepare(dir.path(), Path::new("/rizr-test-none"), "app", &ports)
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
        prepare(dir.path(), Path::new("/rizr-test-none"), "app", &ports).unwrap();
        assert_eq!(
            load(dir.path())["app"][&want],
            host,
            "the port stays the same across runs"
        );
    }
}
