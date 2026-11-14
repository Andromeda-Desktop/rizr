//! Taken host ports (VISION section 6): when another program holds a published port, the service gets the next
//! free one. The choice is stored in the instance data directory, so the port stays the same across runs.

use std::collections::{BTreeMap, BTreeSet};
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
                let by = docker_lines(&["ps", "--filter", &format!("publish={want}"), "--format", "{{.Names}}"])
                    .first()
                    .map_or("another program".into(), |n| format!("container {n}"));
                format!("{svc}: host port {want} is taken by {by}; using {host} (http://localhost:{host})")
            })
        })
        .collect()
}

/// Each published TCP port of compose project `project` with the result of one HTTP GET to it (a redis: PING), so
/// an agent needs no own curl. A report for the agent, not a readiness probe (those never use host ports).
pub fn checks(project: &str) -> Vec<Value> {
    let out = Command::new("docker")
        .args(["ps", "--filter"])
        .arg(format!("label=com.docker.compose.project={project}"))
        .args([
            "--format",
            "{{.Label \"com.docker.compose.service\"}}\t{{.Image}}\t{{.Ports}}",
        ])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    let mut found = std::collections::BTreeSet::new();
    for (svc, image, ports) in out.lines().filter_map(|l| {
        let mut f = l.splitn(3, '\t');
        Some((f.next()?, f.next()?, f.next()?))
    }) {
        let redis = crate::redis::image(image);
        for p in ports.split(", ") {
            let Some((host, _)) = p.split_once("->").filter(|_| p.ends_with("/tcp")) else {
                continue;
            };
            if let Some(port) = host.rsplit(':').next().and_then(|n| n.parse::<u16>().ok()) {
                found.insert((svc.to_owned(), port, redis));
            }
        }
    }
    std::thread::scope(|s| {
        let runs: Vec<_> = found
            .iter()
            .map(|(svc, port, redis)| {
                let (port, redis) = (*port, *redis);
                let h = s.spawn(move || if redis { ping(port) } else { http_get(port) });
                (svc, port, redis, h)
            })
            .collect();
        runs.into_iter()
            .map(|(svc, port, redis, h)| {
                let url = if redis {
                    format!("redis://localhost:{port}")
                } else {
                    format!("http://localhost:{port}/")
                };
                json!({"node": svc, "url": url, "check": h.join().unwrap_or_default()})
            })
            .collect()
    })
}

/// A redis `PING`: `PONG`, or what the server says instead (a password it wants, data it still loads).
fn ping(port: u16) -> String {
    use std::io::{Read, Write};
    use std::time::Duration;
    let Ok(mut c) = std::net::TcpStream::connect_timeout(
        &([127, 0, 0, 1], port).into(),
        Duration::from_secs(5),
    ) else {
        return "connection refused".into();
    };
    let _ = c.set_read_timeout(Some(Duration::from_secs(5)));
    let mut buf = [0u8; 128];
    match c.write_all(b"PING\r\n").and_then(|()| c.read(&mut buf)) {
        Ok(n) => {
            let r = String::from_utf8_lossy(&buf[..n]);
            let r = r.trim();
            r.strip_prefix('+')
                .or_else(|| r.strip_prefix('-'))
                .unwrap_or(r)
                .to_owned()
        }
        Err(e) => format!("port open, {e}"),
    }
}

/// `GET /`: 5 s to connect, 15 s to reply (a hung request often logs its cause late): the status line, or why there is none.
fn http_get(port: u16) -> String {
    use std::io::{Read, Write};
    use std::time::Duration;
    let t = Duration::from_secs(5);
    let reply = Duration::from_secs(15);
    let Ok(mut c) = std::net::TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), t) else {
        return "connection refused".into();
    };
    let _ = c.set_read_timeout(Some(reply));
    let req = format!("GET / HTTP/1.0\r\nHost: localhost:{port}\r\n\r\n");
    let mut buf = [0u8; 128];
    let n = match c.write_all(req.as_bytes()).and_then(|()| c.read(&mut buf)) {
        Ok(n) => n,
        Err(e)
            if e.kind() == std::io::ErrorKind::WouldBlock
                || e.kind() == std::io::ErrorKind::TimedOut =>
        {
            return "port open, no HTTP reply in 15 s".into();
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

fn free_after(ip: &str, port: u16, root: &Path, others: &BTreeSet<u16>) -> Option<u16> {
    (port.checked_add(1)?..=u16::MAX).find(|p| !others.contains(p) && !taken_by_other(ip, *p, root))
}

/// Host ports each instance of a project holds, in `<data>/<project>.ports.json`. A port is claimed before its
/// container binds it, so two worktrees that start at once never pick the same one.
type Claims = BTreeMap<String, BTreeSet<u16>>;

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

/// Containers of rizr's own compose project that `up` did not start: a service gone from the compose files, or one
/// rizr serves elsewhere (a shared database). `up` recreates every service it runs with its labels, so after `up`
/// an unlabelled one is an orphan, as for `docker compose up --remove-orphans`. One-off `exec` jobs are kept.
/// `services` are the compose files' own services: a failed `up` may not have recreated those yet, so they stay.
pub fn remove_orphans(own: &str, services: &[String]) -> Vec<String> {
    let out = Command::new("docker")
        .args(["ps", "-a", "--filter"])
        .arg(format!("label=com.docker.compose.project={own}"))
        .args([
            "--format",
            "{{.Names}} {{.Label \"com.docker.compose.oneoff\"}} {{.Label \"com.docker.compose.service\"}} {{.Label \"rizr.project\"}}",
        ])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    let names: Vec<String> = out
        .lines()
        .filter_map(|l| {
            let mut f = l.split(' ');
            let (name, oneoff, svc) = (f.next()?, f.next()?, f.next()?);
            let rizr = f.next().unwrap_or("");
            (oneoff != "True" && rizr.trim().is_empty() && !services.iter().any(|s| s == svc))
                .then(|| name.to_owned())
        })
        .collect();
    if names.is_empty()
        || !Command::new("docker")
            .args(["rm", "-f"])
            .args(&names)
            .output()
            .is_ok_and(|o| o.status.success())
    {
        return Vec::new();
    }
    names
}

/// One line per stray project, with the command that stops it; volumes stay. Running containers of rizr's own
/// project that rizr did not start (a plain `docker compose up`) get one line too: their nodes show as absent.
pub fn stray_lines(root: &Path, own: &str) -> Vec<String> {
    let unadopted: Vec<String> = docker_lines(&[
        "ps",
        "--filter",
        &format!("label=com.docker.compose.project={own}"),
        "--format",
        "{{.Names}} {{.Label \"rizr.project\"}}",
    ])
    .into_iter()
    .filter(|l| !l.contains(' ') || l.ends_with(' '))
    .map(|l| l.trim().to_owned())
    .collect();
    let own_line = (!unadopted.is_empty()).then(|| {
        format!("running outside rizr in compose project `{own}`: {}; rizr did not start them, so their nodes show as absent; `rizr up` in this checkout recreates them under rizr; `rizr up` in a worktree leaves them running", unadopted.join(", "))
    });
    strays(root, own)
        .into_iter()
        .map(|(p, n)| {
            format!("this repo also runs outside rizr as compose project `{p}` ({n} containers); it can hold ports and confuse checks; stop it, volumes stay: docker compose -p {p} down --remove-orphans")
        })
        .chain(own_line)
        .collect()
}

fn docker_lines(args: &[&str]) -> Vec<String> {
    Command::new("docker")
        .args(args)
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

/// Writes `ports-<svc>.yml` when a published port of `svc` is remapped, and returns it for `-f`.
/// `ports` is the service's `ports` list from `docker compose config --format json`.
/// `claims` is the project's claim file; its lock (`<claims>.lock`) is held while ports are chosen and recorded.
pub fn prepare(
    dir: &Path,
    root: &Path,
    svc: &str,
    ports: &[Value],
    claims: &Path,
    instance: &str,
) -> Result<Option<PathBuf>, String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let lock = std::fs::File::create(claims.with_extension("lock")).map_err(|e| e.to_string())?;
    lock.lock().map_err(|e| e.to_string())?;
    let mut held: Claims = std::fs::read(claims)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    let others: BTreeSet<u16> = held
        .iter()
        .filter(|(i, _)| *i != instance)
        .flat_map(|(_, p)| p.iter().copied())
        .collect();
    let mut mine = BTreeSet::new();
    let mut all = load(dir);
    // A moved port also avoids this instance's claims, the moves already made and this service's other ports:
    // two ports of one service moved to the same host port make the container fail to create.
    let mut avoid: BTreeSet<u16> = others
        .iter()
        .chain(held.get(instance).into_iter().flatten())
        .chain(all.values().flat_map(|m| m.values()))
        .copied()
        .collect();
    avoid.extend(
        ports
            .iter()
            .filter_map(|p| p["published"].as_str().and_then(|s| s.parse::<u16>().ok())),
    );
    let mut changed = false;
    let mut out = Vec::new();
    for p in ports {
        let mut p = p.clone();
        let want = p["published"].as_str().and_then(|s| s.parse::<u16>().ok());
        if let Some(want) = want {
            let ip = p["host_ip"].as_str().unwrap_or("0.0.0.0").to_string();
            let free = |port| TcpListener::bind((ip.as_str(), port)).is_ok();
            // A move of the main worktree ends when the stack is down and the compose port is free again.
            let known = all
                .get(svc)
                .and_then(|m| m.get(&want))
                .copied()
                .filter(|h| {
                    instance != "main" || others.contains(&want) || !(free(want) && free(*h))
                });
            if known.is_none() && all.get_mut(svc).is_some_and(|m| m.remove(&want).is_some()) {
                all.retain(|_, m| !m.is_empty());
                changed = true;
            }
            let host = match known {
                Some(h) => Some(h),
                // The compose file's own ports belong to the main worktree, also when it is not running.
                None if instance != "main"
                    || others.contains(&want)
                    || taken_by_other(&ip, want, root) =>
                {
                    let h = free_after(&ip, want, root, &avoid);
                    if let Some(h) = h {
                        avoid.insert(h);
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
            mine.insert(host.unwrap_or(want));
        }
        out.push(p);
    }
    let own = held.entry(instance.into()).or_default();
    if !mine.is_subset(own) {
        own.extend(mine);
        std::fs::write(
            claims,
            serde_json::to_vec_pretty(&held).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
    }
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
    fn a_port_held_by_another_program_moves_until_it_is_free_and_the_stack_is_down() {
        let dir = tempfile::tempdir().unwrap();
        let other = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let want = other.local_addr().unwrap().port();
        let ports = [
            json!({"target": 80, "published": want.to_string(), "host_ip": "127.0.0.1", "protocol": "tcp"}),
        ];
        let claims = dir.path().join("p.ports.json");
        let file = prepare(
            dir.path(),
            Path::new("/rizr-test-none"),
            "app",
            &ports,
            &claims,
            "main",
        )
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
        let running = TcpListener::bind(("127.0.0.1", host)).unwrap();
        let again = || {
            prepare(
                dir.path(),
                Path::new("/rizr-test-none"),
                "app",
                &ports,
                &claims,
                "main",
            )
            .unwrap()
        };
        again();
        assert_eq!(
            load(dir.path())["app"][&want],
            host,
            "the port stays while the stack runs on it"
        );
        drop(running);
        assert_eq!(
            again(),
            None,
            "a stopped stack returns to the free compose port"
        );
        assert!(!load(dir.path()).contains_key("app"));
    }

    #[test]
    fn two_taken_ports_of_one_service_move_to_different_ports() {
        let dir = tempfile::tempdir().unwrap();
        // Two neighbouring ports held by another program: the first free port after each is the same one.
        let (a, _held) = loop {
            let first = TcpListener::bind(("127.0.0.1", 0)).unwrap();
            let a = first.local_addr().unwrap().port();
            if let Ok(second) = TcpListener::bind(("127.0.0.1", a + 1)) {
                break (a, (first, second));
            }
        };
        let ports = [a, a + 1].map(|p| {
            json!({"target": p, "published": p.to_string(), "host_ip": "127.0.0.1", "protocol": "tcp"})
        });
        let claims = dir.path().join("p.ports.json");
        prepare(
            dir.path(),
            Path::new("/rizr-test-none"),
            "app",
            &ports,
            &claims,
            "main",
        )
        .unwrap();
        let moved = &load(dir.path())["app"];
        assert_ne!(moved[&a], moved[&(a + 1)]);
    }

    #[test]
    fn two_worktrees_that_start_at_once_get_different_ports() {
        let data = tempfile::tempdir().unwrap();
        let free = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let want = free.local_addr().unwrap().port();
        drop(free);
        let ports = [
            json!({"target": 80, "published": want.to_string(), "host_ip": "127.0.0.1", "protocol": "tcp"}),
        ];
        let claims = data.path().join("p.ports.json");
        let host = |inst: &str| {
            let dir = data.path().join(format!("p-{inst}"));
            prepare(
                &dir,
                Path::new("/rizr-test-none"),
                "app",
                &ports,
                &claims,
                inst,
            )
            .unwrap();
            load(&dir)["app"][&want]
        };
        // Neither container binds its port yet: only the claim tells the second worktree that the first holds it.
        assert_ne!(host("wt1"), host("wt2"));
    }
}
