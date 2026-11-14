//! `rizr doctor`: host problems that break a stack before any service log shows them. It runs no repo tool, so
//! it works before trust.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

use rizr_core::error::{Error, ErrorCode};
use serde_json::Value;

use crate::docker::Stack;

#[derive(Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum Level {
    Ok,
    Warn,
    Fail,
}

#[derive(serde::Serialize)]
struct Check {
    check: &'static str,
    level: Level,
    detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    fix: Option<String>,
}

fn check(check: &'static str, level: Level, detail: String, fix: Option<String>) -> Check {
    Check {
        check,
        level,
        detail,
        fix,
    }
}

fn out(c: &mut Command) -> Option<String> {
    let o = c.output().ok().filter(|o| o.status.success())?;
    Some(String::from_utf8_lossy(&o.stdout).trim().to_owned())
}

pub fn run(stack: &Stack, json: bool) -> Result<(), Error> {
    let info: Option<Value> = out(Command::new("docker").args(["info", "--format", "{{json .}}"]))
        .and_then(|s| serde_json::from_str(&s).ok());
    let mut checks = vec![daemon(info.as_ref(), &stack.root)];
    if info.is_some() {
        checks.push(pools());
    }
    let docker_root = info
        .as_ref()
        .and_then(|i| i["DockerRootDir"].as_str())
        .map(PathBuf::from);
    checks.extend(disks(
        docker_root
            .iter()
            .map(|p| (p.as_path(), "docker images and volumes"))
            .chain([
                (stack.root.as_path(), "the repo"),
                (stack.dir.as_path(), "rizr data"),
            ]),
    ));
    checks.extend(toolchains(&stack.root));
    checks.extend(grants(stack));
    if json {
        println!("{:#}", serde_json::json!({ "checks": checks }));
    } else {
        for c in &checks {
            let level = match c.level {
                Level::Ok => "ok  ",
                Level::Warn => "warn",
                Level::Fail => "FAIL",
            };
            println!("{level}  {}: {}", c.check, c.detail);
            if let Some(f) = &c.fix {
                println!("      fix: {f}");
            }
        }
    }
    let failed = checks.iter().filter(|c| c.level == Level::Fail).count();
    if failed == 0 {
        Ok(())
    } else {
        Err(Error::new(
            ErrorCode::RuntimeUnavailable,
            format!("doctor found {failed} failing check(s)"),
            "each FAIL line names its fix",
            "rizr doctor",
        ))
    }
}

/// A daemon on another host (or another dockerd) cannot see this tree: compose bind mounts fail (sabai #10).
fn daemon(info: Option<&Value>, root: &Path) -> Check {
    let Some(i) = info else {
        return check(
            "docker",
            Level::Fail,
            "the docker daemon does not answer".into(),
            Some("systemctl start docker (or start Docker Desktop / podman machine)".into()),
        );
    };
    let host = std::env::var("DOCKER_HOST").unwrap_or_default();
    let engine = crate::runtime::current();
    let version = format!(
        "{} {}",
        engine.name(),
        i["ServerVersion"].as_str().unwrap_or("?")
    );
    if host.is_empty() || host.starts_with("unix://") {
        check(
            "docker",
            Level::Ok,
            format!(
                "{version}, local daemon{}: bind mounts of {} work",
                if engine == crate::runtime::Engine::Podman {
                    format!(" at {host} (docker API; RIZR_RUNTIME=docker to use docker)")
                } else {
                    String::new()
                },
                root.display()
            ),
            None,
        )
    } else {
        check(
            "docker",
            Level::Warn,
            format!(
                "{version} at DOCKER_HOST={host}: that daemon sees its own files, so bind mounts of {} work only \
                 where the same path exists there",
                root.display()
            ),
            Some("unset DOCKER_HOST to use the local daemon, or make the tree visible at the same path".into()),
        )
    }
}

/// Bridge networks the address pools allow, from daemon.json or docker's defaults.
fn pool_capacity() -> (u32, &'static str) {
    let home = std::env::var("HOME").unwrap_or_default();
    let configured = [
        "/etc/docker/daemon.json".to_owned(),
        format!("{home}/.config/docker/daemon.json"),
    ]
    .iter()
    .filter_map(|p| std::fs::read(p).ok())
    .filter_map(|b| serde_json::from_slice::<Value>(&b).ok())
    .find_map(|v| v["default-address-pools"].as_array().cloned());
    match configured {
        Some(pools) => {
            let n = pools
                .iter()
                .filter_map(|p| {
                    let prefix: u32 = p["base"].as_str()?.split_once('/')?.1.parse().ok()?;
                    let size = u32::try_from(p["size"].as_u64()?).ok()?;
                    size.checked_sub(prefix).map(|bits| 1u32 << bits.min(16))
                })
                .sum();
            (n, "daemon.json")
        }
        // 172.17-31.0.0/16 and 192.168.0.0/16 in /20 blocks.
        None => (15 + 16, "docker's default pools"),
    }
}

/// Each compose project takes one network or more; at the limit `up` fails with "all predefined address pools
/// have been fully subnetted".
fn pools() -> Check {
    let used =
        out(Command::new("docker").args(["network", "ls", "-q", "--filter", "driver=bridge"]))
            .map_or(0, |s| u32::try_from(s.lines().count()).unwrap_or(u32::MAX));
    let (cap, source) = pool_capacity();
    let left = cap.saturating_sub(used);
    let level = match left {
        0 => Level::Fail,
        1..=5 => Level::Warn,
        _ => Level::Ok,
    };
    check(
        "networks",
        level,
        format!("{used} of {cap} bridge networks in use ({source}); {left} left, an instance takes 1 or more"),
        (level != Level::Ok).then(|| {
            "add \"default-address-pools\": [{\"base\": \"10.200.0.0/16\", \"size\": 24}] to /etc/docker/daemon.json \
             (256 networks), then sudo systemctl restart docker; or remove unused ones: docker network prune"
                .into()
        }),
    )
}

/// Space on each file system that holds `paths`, once per file system (sabai #9).
fn disks<'a>(paths: impl Iterator<Item = (&'a Path, &'static str)>) -> Vec<Check> {
    let mut seen = Vec::new();
    let mut v = Vec::new();
    for (path, what) in paths {
        let probe = path.ancestors().find(|p| p.exists()).unwrap_or(path);
        let Some(line) = out(Command::new("df").args(["-P", "-B1"]).arg(probe))
            .and_then(|s| s.lines().nth(1).map(str::to_owned))
        else {
            continue;
        };
        let f: Vec<&str> = line.split_whitespace().collect();
        let (Some(avail), Some(pct), Some(mount)) = (
            f.get(3).and_then(|a| a.parse::<u64>().ok()),
            f.get(4)
                .and_then(|p| p.trim_end_matches('%').parse::<u32>().ok()),
            f.get(5),
        ) else {
            continue;
        };
        if seen.iter().any(|s: &String| s == mount) {
            continue;
        }
        seen.push((*mount).to_owned());
        let gib = avail / (1 << 30);
        let level = if pct >= 98 || gib < 1 {
            Level::Fail
        } else if pct >= 90 || gib < 5 {
            Level::Warn
        } else {
            Level::Ok
        };
        let fix = (level != Level::Ok).then(|| {
            if what.starts_with("docker") {
                "docker system df, then docker builder prune and docker image prune".to_owned()
            } else {
                format!("free space on {mount}")
            }
        });
        v.push(check(
            "disk",
            level,
            format!("{mount} ({what}): {pct}% used, {gib} GiB free"),
            fix,
        ));
    }
    v
}

fn mtime(p: &Path) -> Option<SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

/// An install older than its lock file: a host process then runs old dependencies (sabai #5).
fn toolchains(root: &Path) -> Vec<Check> {
    const PAIRS: [(&str, &str, &[&str], &str); 6] = [
        ("uv.lock", ".venv", &["lib"], "uv sync"),
        ("poetry.lock", ".venv", &["lib"], "poetry install"),
        (
            "package-lock.json",
            "node_modules",
            &[".package-lock.json"],
            "npm ci",
        ),
        (
            "pnpm-lock.yaml",
            "node_modules",
            &[".modules.yaml"],
            "pnpm install",
        ),
        (
            "yarn.lock",
            "node_modules",
            &[".yarn-state.yml", ".yarn-integrity"],
            "yarn install",
        ),
        ("bun.lock", "node_modules", &[], "bun install"),
    ];
    let dirs = std::iter::once(root.to_path_buf()).chain(
        std::fs::read_dir(root)
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| {
                p.is_dir()
                    && !p
                        .file_name()
                        .is_some_and(|n| n.to_string_lossy().starts_with('.'))
            }),
    );
    let mut v = Vec::new();
    for dir in dirs {
        for (lock, install, markers, fix) in PAIRS {
            let (lock_p, install_p) = (dir.join(lock), dir.join(install));
            let (Some(locked), Some(_)) = (mtime(&lock_p), mtime(&install_p)) else {
                continue;
            };
            // The install's own record of the last sync, else the directory itself.
            let installed = std::iter::once(install_p.clone())
                .chain(markers.iter().map(|m| install_p.join(m)))
                .filter_map(|p| mtime(&p))
                .max();
            let rel = dir.strip_prefix(root).unwrap_or(&dir);
            let at = if rel.as_os_str().is_empty() {
                String::new()
            } else {
                format!("cd {} && ", rel.display())
            };
            if installed.is_some_and(|i| i < locked) {
                v.push(check(
                    "toolchain",
                    Level::Warn,
                    format!(
                        "{} is older than {}",
                        rel.join(install).display(),
                        rel.join(lock).display()
                    ),
                    Some(format!("{at}{fix}")),
                ));
            }
        }
    }
    v
}

/// Each shared database accepts its app user (sabai #7: a role without a grant fails only at runtime).
fn grants(stack: &Stack) -> Vec<Check> {
    crate::postgres::Info::load(&stack.dir)
        .databases
        .values()
        .map(|d| {
            let server = format!("rizr-{}-{}", stack.project.0, d.server);
            let r = Command::new("docker")
                .args([
                    "exec",
                    &server,
                    "psql",
                    "-U",
                    &d.user,
                    "-d",
                    &d.database,
                    "-tAc",
                ])
                .arg("select has_schema_privilege('public', 'CREATE')")
                .output();
            match r {
                Ok(o) if o.status.success() && String::from_utf8_lossy(&o.stdout).trim() == "t" => {
                    check(
                        "database",
                        Level::Ok,
                        format!(
                            "{} on {server}: {} connects and may create in schema public",
                            d.database, d.user
                        ),
                        None,
                    )
                }
                Ok(o) if o.status.success() => check(
                    "database",
                    Level::Warn,
                    format!(
                        "{} on {server}: {} connects but may not create in schema public",
                        d.database, d.user
                    ),
                    Some(format!(
                        "docker exec {server} psql -d {} -c 'GRANT ALL ON SCHEMA public TO {}'",
                        d.database, d.user
                    )),
                ),
                Ok(o) => check(
                    "database",
                    Level::Fail,
                    format!(
                        "{} on {server}: {} cannot connect: {}",
                        d.database,
                        d.user,
                        crate::compose::tail(&o.stderr)
                    ),
                    Some("rizr up starts the server; rizr why <db> shows its state".into()),
                ),
                Err(e) => check("database", Level::Fail, e.to_string(), None),
            }
        })
        .collect()
}
