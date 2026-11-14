//! Compose import (after trust): `docker compose config --format json` to the core model (SPEC-core section 7).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use rizr_core::model::{
    Dep, Digest, Gate, InstanceId, JobKind, Model, NodeId, NodeKind, NodeName, Probe, ProviderName,
    Scope,
};
use serde_json::Value;
use sha2::{Digest as _, Sha256};

const PRIMARY: [&str; 4] = [
    "compose.yaml",
    "compose.yml",
    "docker-compose.yaml",
    "docker-compose.yml",
];
const OVERRIDE: [&str; 4] = [
    "compose.override.yaml",
    "compose.override.yml",
    "docker-compose.override.yaml",
    "docker-compose.override.yml",
];

/// The files compose itself would pick: the first primary name, then the first override name.
pub fn files(root: &Path) -> Vec<PathBuf> {
    [&PRIMARY[..], &OVERRIDE[..]]
        .iter()
        .filter_map(|names| names.iter().map(|n| root.join(n)).find(|p| p.is_file()))
        .collect()
}

/// The files given with `-f`, relative to `root`; with none, the files compose itself would pick.
pub fn resolve(root: &Path, chosen: &[PathBuf]) -> Vec<PathBuf> {
    if chosen.is_empty() {
        files(root)
    } else {
        chosen.iter().map(|f| root.join(f)).collect()
    }
}

/// Other compose files in `root` (a top-level `services:`) that the resolved set leaves out, such as `pwd.yml`.
pub fn others(root: &Path, chosen: &[PathBuf]) -> Vec<String> {
    let used = resolve(root, chosen);
    let mut v: Vec<String> = std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| matches!(p.extension().and_then(|x| x.to_str()), Some("yml" | "yaml")))
        .filter(|p| !used.contains(p))
        .filter(|p| {
            std::fs::read_to_string(p).is_ok_and(|t| t.lines().any(|l| l.starts_with("services:")))
        })
        .filter_map(|p| Some(p.file_name()?.to_str()?.to_owned()))
        .collect();
    v.sort();
    v
}

/// The services that the compose file `other` in `root` defines and the resolved set does not, such as the
/// databases of a self-contained `pwd.yml` beside a `compose.yaml` that expects them elsewhere.
pub fn extra_services(root: &Path, chosen: &[PathBuf], other: &str) -> Vec<String> {
    let names = |f: &Path| -> BTreeSet<String> {
        std::fs::read(f)
            .ok()
            .and_then(|b| serde_norway::from_slice::<serde_norway::Value>(&b).ok())
            .and_then(|v| {
                Some(
                    v.get("services")?
                        .as_mapping()?
                        .keys()
                        .filter_map(|k| k.as_str().map(str::to_owned))
                        .collect(),
                )
            })
            .unwrap_or_default()
    };
    let used: BTreeSet<String> = resolve(root, chosen)
        .iter()
        .flat_map(|f| names(f))
        .collect();
    names(&root.join(other))
        .difference(&used)
        .cloned()
        .collect()
}

/// The project name plain `docker compose` picks: `COMPOSE_PROJECT_NAME`, else the last top-level `name:`,
/// else the directory name; normalized as compose does.
pub fn default_name(root: &Path, chosen: &[PathBuf]) -> String {
    let from_files = || {
        resolve(root, chosen).iter().rev().find_map(|f| {
            let v: serde_norway::Value = serde_norway::from_slice(&std::fs::read(f).ok()?).ok()?;
            v.get("name")?.as_str().map(str::to_owned)
        })
    };
    let raw = std::env::var("COMPOSE_PROJECT_NAME")
        .ok()
        .filter(|n| !n.is_empty())
        .or_else(from_files)
        .unwrap_or_else(|| {
            root.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
        });
    let name: String = raw
        .to_lowercase()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .collect();
    match name.trim_start_matches(['_', '-']) {
        "" => "root".into(),
        n => n.into(),
    }
}

/// The command prefix every compose call of this stack uses.
/// `dir` is rizr's data directory for the instance: the dev env files live there.
pub fn command(root: &Path, chosen: &[PathBuf], project: &str, dir: &Path) -> Command {
    let mut c = files_only(root, chosen, project, dir);
    let nets = crate::subnet::file(dir);
    if nets.is_file() {
        c.arg("-f").arg(nets);
    }
    c
}

/// As `command`, without the instance's network override: the compose files' own config.
fn files_only(root: &Path, chosen: &[PathBuf], project: &str, dir: &Path) -> Command {
    let mut c = Command::new("docker");
    c.arg("compose")
        .arg("-p")
        .arg(project)
        .arg("--project-directory")
        .arg(root);
    for f in resolve(root, chosen) {
        c.arg("-f").arg(f);
    }
    c.args(crate::devenv::args(root, dir));
    c
}

/// The model and, per service, whether its compose file sets `restart:`.
pub fn import(
    root: &Path,
    chosen: &[PathBuf],
    project: &str,
    dir: &Path,
    instance: &InstanceId,
    inputs: Digest,
) -> Result<(Model, BTreeMap<String, Side>), String> {
    let compose_files = resolve(root, chosen);
    if compose_files.is_empty() {
        return Err(format!("no compose file in {}", root.display()));
    }
    crate::devenv::prepare(root, dir, &compose_files)?;
    // Compose names one unset required variable per run; each round fills one.
    let out = loop {
        let out = files_only(root, chosen, project, dir)
            .args(["config", "--format", "json"])
            .output()
            .map_err(|e| format!("cannot run docker compose: {e}"))?;
        if out.status.success() {
            break out;
        }
        if !crate::devenv::fill(dir, &String::from_utf8_lossy(&out.stderr))? {
            return Err(tail(&out.stderr));
        }
    };
    let config: Value =
        serde_json::from_slice(&out.stdout).map_err(|e| format!("compose config: {e}"))?;
    // Shared by the project's instances: `<data>/<project>-<instance>` -> `<data>/<project>.subnets.json`.
    let claims = dir.with_file_name(format!(
        "{}.subnets.json",
        dir.file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_suffix(&format!("-{}", instance.0)))
            .unwrap_or(project)
    ));
    crate::subnet::prepare(&config, dir, &claims, &instance.0, project)?;
    let side = config["services"]
        .as_object()
        .into_iter()
        .flatten()
        .map(|(n, s)| {
            let side = Side {
                restart: !s["restart"].is_null() || !s["deploy"]["restart_policy"].is_null(),
                container_name: s["container_name"].as_str().map(str::to_owned),
                ports: s["ports"].as_array().cloned().unwrap_or_default(),
                pg: crate::postgres::shared(s, &config),
                pg_plain: crate::postgres::plain(s),
                env: s["environment"]
                    .as_object()
                    .into_iter()
                    .flatten()
                    .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_owned())))
                    .collect(),
            };
            (n.clone(), side)
        })
        .collect();
    Ok((model(&config, instance, inputs)?, side))
}

/// What the provider needs per service beside the model.
#[derive(Clone, Debug, Default)]
pub struct Side {
    /// The compose file sets `restart:` or `deploy.restart_policy`; otherwise the override sets `on-failure:5`.
    pub restart: bool,
    /// The resolved `ports` list, for taken-port remaps.
    pub ports: Vec<Value>,
    /// The shared server this service can move to, before `rizr.toml` opts it out.
    pub pg: Option<crate::postgres::Shared>,
    /// Why a postgres service stays a plain compose container.
    pub pg_plain: Option<String>,
    /// The resolved `environment`.
    pub env: BTreeMap<String, String>,
    /// A fixed `container_name`; other instances get `<name>-<instance>` so they do not collide.
    pub container_name: Option<String>,
}

/// A dependency that is not a finished job must share a network with its dependent, or its name never resolves.
/// A service with `network_mode` (host, `service:x`) is skipped: it has no networks of its own.
fn unreachable(
    services: &serde_json::Map<String, Value>,
    nodes: &BTreeMap<NodeId, rizr_core::model::NodeSpec>,
    id: &dyn Fn(&str) -> NodeId,
) -> Vec<rizr_core::model::Finding> {
    let nets = |name: &str| -> Option<BTreeSet<String>> {
        let svc = &services[name];
        if !svc["network_mode"].is_null() {
            return None;
        }
        Some(match svc["networks"].as_object() {
            Some(n) => n.keys().cloned().collect(),
            None => BTreeSet::from(["default".to_string()]),
        })
    };
    let mut out = Vec::new();
    for (node, spec) in nodes {
        let a = &node.name.0;
        for d in spec.deps.iter().filter(|d| d.gate != Gate::Completed) {
            let b = &d.node.name.0;
            let (Some(na), Some(nb)) = (nets(a), nets(b)) else {
                continue;
            };
            if na.is_disjoint(&nb) {
                let list = |n: &BTreeSet<String>| n.iter().cloned().collect::<Vec<_>>().join(", ");
                out.push(rizr_core::model::Finding {
                    node: id(a),
                    error: rizr_core::error::Error::new(
                        rizr_core::error::ErrorCode::Unreachable,
                        format!("{a} cannot reach {b}: {a} is on {}, {b} on {}", list(&na), list(&nb)),
                        format!("add a network of {b} to `{a}.networks` in the compose file, then run up again"),
                        "rizr up --wait ready",
                    ),
                });
            }
        }
    }
    out
}

/// The images a compose `build` starts FROM, read from its Dockerfile; empty when the file does not read.
fn base_images(build: &Value) -> Vec<String> {
    let text = match build["dockerfile_inline"].as_str() {
        Some(t) => t.to_owned(),
        None => {
            let context = Path::new(build["context"].as_str().unwrap_or("."));
            let file = build["dockerfile"].as_str().unwrap_or("Dockerfile");
            std::fs::read_to_string(context.join(file)).unwrap_or_default()
        }
    };
    text.lines()
        .filter_map(|l| {
            let mut words = l.split_whitespace();
            if !words.next()?.eq_ignore_ascii_case("FROM") {
                return None;
            }
            words.find(|w| !w.starts_with("--")).map(str::to_owned)
        })
        .collect()
}

pub fn model(config: &Value, instance: &InstanceId, inputs: Digest) -> Result<Model, String> {
    let services = config["services"]
        .as_object()
        .ok_or("compose config has no services")?;
    let id = |name: &str| NodeId {
        scope: Scope::Instance(instance.clone()),
        name: NodeName(name.into()),
    };
    // A service that another service waits on to exit 0 is a job (VISION section 3).
    let jobs: BTreeSet<&str> = services
        .values()
        .filter_map(|s| s["depends_on"].as_object())
        .flat_map(|d| d.iter())
        .filter(|(_, c)| c["condition"] == "service_completed_successfully")
        .map(|(n, _)| n.as_str())
        .collect();
    // A service that compose must never restart runs once: a setup step that nothing waits on (frappe
    // create-site). It runs once per data set, not on every `up`.
    let once = |svc: &Value| {
        svc["deploy"]["restart_policy"]["condition"] == "none" || svc["restart"] == "no"
    };
    // `docker compose up --no-deps` pulls an image that another service builds instead of building it.
    let mut builders: BTreeMap<&str, &str> = BTreeMap::new();
    for (n, s) in services.iter().filter(|(_, s)| !s["build"].is_null()) {
        if let Some(i) = s["image"].as_str() {
            builders.entry(i).or_insert(n);
        }
    }
    let mut nodes = BTreeMap::new();
    for (name, svc) in services {
        let mut deps: Vec<Dep> = svc["depends_on"]
            .as_object()
            .into_iter()
            .flat_map(|d| d.iter())
            .map(|(n, c)| Dep {
                node: id(n),
                // Plain `depends_on` (`service_started`) gets the readiness gate (DECISIONS 2026-11-13).
                gate: if c["condition"] == "service_completed_successfully" {
                    Gate::Completed
                } else {
                    Gate::Ready
                },
            })
            .collect();
        // A service with a build needs the images its Dockerfile starts FROM; one without needs its `image`.
        let needs = if svc["build"].is_null() {
            svc["image"]
                .as_str()
                .map(str::to_owned)
                .into_iter()
                .collect()
        } else {
            base_images(&svc["build"])
        };
        for b in needs
            .iter()
            .filter_map(|i| builders.get(i.as_str()).copied())
        {
            if b != name
                && deps.iter().all(|d| d.node.name.0 != b)
                && services[b]["depends_on"].get(name).is_none()
            {
                let job = jobs.contains(b) || once(&services[b]);
                deps.push(Dep {
                    node: id(b),
                    gate: if job { Gate::Completed } else { Gate::Ready },
                });
            }
        }
        let kind = if jobs.contains(name.as_str()) {
            NodeKind::Job {
                kind: JobKind::Migrate,
            }
        } else if once(svc) {
            NodeKind::Job {
                kind: JobKind::Seed,
            }
        } else {
            NodeKind::Service { probe: probe(svc) }
        };
        let context = svc["build"]["context"].as_str().map(Path::new);
        // A service that mounts its build context runs the edited files already.
        let mounted = svc["volumes"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|v| v["type"] == "bind" && v["source"].as_str().map(Path::new) == context);
        let mut spec = svc.to_string().into_bytes();
        if let Some(ctx) = context.filter(|_| !mounted) {
            spec.extend(sources(ctx));
        }
        let spec = Digest(hex(&Sha256::digest(spec)));
        nodes.insert(
            id(name),
            rizr_core::model::NodeSpec {
                kind,
                deps,
                spec,
                provider: ProviderName("docker".into()),
            },
        );
    }
    let findings = unreachable(services, &nodes, &id);
    let m = Model {
        inputs,
        nodes,
        findings,
    };
    if let Some(n) = cycle(&m) {
        return Err(format!("dependency cycle through `{}`", n.name.0));
    }
    Ok(m)
}

/// Readiness signal: healthcheck event, else a redis server's ready line, else a tcp probe on each container
/// port, else ready at start.
/// The sources of a build context as git sees them: the committed tree, the uncommitted diff and the untracked
/// files. An edit changes it, so the service's spec changes and `up` builds the image again. Empty outside git.
fn sources(ctx: &Path) -> Vec<u8> {
    let git = |args: &[&str]| {
        Command::new("git")
            .current_dir(ctx)
            .args(args)
            .stderr(std::process::Stdio::null())
            .output()
            .map(|o| o.stdout)
            .unwrap_or_default()
    };
    let mut s = git(&["rev-parse", "HEAD:./"]);
    s.extend(git(&["diff", "HEAD", "--", "."]));
    let untracked = git(&["ls-files", "-o", "--exclude-standard", "-z", "--", "."]);
    for f in String::from_utf8_lossy(&untracked)
        .split('\0')
        .filter(|f| !f.is_empty())
    {
        if let Ok(m) = ctx.join(f).metadata() {
            s.extend(format!("{f} {} {:?}\n", m.len(), m.modified().ok()).bytes());
        }
    }
    s
}

fn probe(svc: &Value) -> Probe {
    let hc = &svc["healthcheck"];
    if hc.is_object() && hc["disable"] != true && hc["test"] != serde_json::json!(["NONE"]) {
        return Probe::HealthEvent;
    }
    if redis_server(svc) {
        // Redis opens its port before it loads its data; until this line it answers LOADING.
        return Probe::LogLine {
            pattern: "Ready to accept connections".into(),
        };
    }
    let mut ports: Vec<u16> = svc["ports"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|p| p["protocol"].as_str().unwrap_or("tcp") == "tcp")
        .filter_map(|p| p["target"].as_u64())
        .chain(
            svc["expose"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|e| e.as_str().and_then(|s| s.split('/').next()?.parse().ok())),
        )
        .filter_map(|p| u16::try_from(p).ok())
        .collect();
    ports.sort_unstable();
    ports.dedup();
    if ports.is_empty() {
        Probe::AtStart
    } else {
        Probe::Tcp { ports }
    }
}

/// A redis or valkey image that runs its server: the image's own command, or one that starts the server.
fn redis_server(svc: &Value) -> bool {
    let command = match &svc["command"] {
        Value::Array(a) => a
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(" "),
        Value::String(s) => s.clone(),
        _ => String::new(),
    };
    crate::redis::image(svc["image"].as_str().unwrap_or_default())
        && svc["entrypoint"].is_null()
        && (command.is_empty()
            || command.contains("redis-server")
            || command.contains("valkey-server"))
}

pub fn cycle(m: &Model) -> Option<NodeId> {
    fn visit(m: &Model, id: &NodeId, path: &mut Vec<NodeId>, done: &mut BTreeSet<NodeId>) -> bool {
        if path.contains(id) {
            return true;
        }
        if done.contains(id) {
            return false;
        }
        path.push(id.clone());
        let found = m
            .nodes
            .get(id)
            .is_some_and(|s| s.deps.iter().any(|d| visit(m, &d.node, path, done)));
        path.pop();
        done.insert(id.clone());
        found
    }
    let mut done = BTreeSet::new();
    m.nodes
        .keys()
        .find(|id| visit(m, id, &mut Vec::new(), &mut done))
        .cloned()
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn tail(stderr: &[u8]) -> String {
    let s = String::from_utf8_lossy(stderr);
    let lines: Vec<&str> = s.lines().filter(|l| !l.trim().is_empty()).collect();
    lines[lines.len().saturating_sub(5)..].join("\n")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "a test panics on a broken precondition")]
mod tests {
    use super::*;

    #[test]
    fn a_dependency_on_no_shared_network_is_a_finding() {
        let config = serde_json::json!({"services": {
            "redis": {"networks": {"back": null}},
            "migrate": {"networks": {"back": null}},
            "host": {"network_mode": "host"},
            "worker": {"networks": {"front": null}, "depends_on": {
                "redis": {"condition": "service_healthy"},
                "migrate": {"condition": "service_completed_successfully"},
                "host": {"condition": "service_started"}}},
            "vote": {"networks": {"front": null, "back": null}, "depends_on": {"redis": {"condition": "service_started"}}},
        }});
        let m = model(&config, &"main".into(), "d".into()).unwrap();
        let found: Vec<_> = m
            .findings
            .iter()
            .map(|f| (f.node.name.0.as_str(), f.error.message.as_str()))
            .collect();
        assert_eq!(
            found,
            [(
                "worker",
                "worker cannot reach redis: worker is on front, redis on back"
            )]
        );
    }

    #[test]
    fn an_edit_in_a_build_context_changes_the_spec_of_its_service_only() {
        let dir = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            assert!(
                Command::new("git")
                    .current_dir(dir.path())
                    .args([
                        "-c",
                        "user.name=t",
                        "-c",
                        "user.email=t@t",
                        "-c",
                        "commit.gpgsign=false"
                    ])
                    .args(args)
                    .output()
                    .unwrap()
                    .status
                    .success()
            );
        };
        std::fs::create_dir(dir.path().join("worker")).unwrap();
        std::fs::write(dir.path().join("worker/main.c"), "int main;").unwrap();
        git(&["init", "-q"]);
        git(&["add", "-A"]);
        git(&["commit", "-qm", "init"]);
        let config = serde_json::json!({"services": {
            "worker": {"build": {"context": dir.path().join("worker")}},
            "db": {"image": "postgres:15"},
        }});
        let spec = |n: &str| {
            let m = model(&config, &"main".into(), "d".into()).unwrap();
            m.nodes
                .iter()
                .find(|(k, _)| k.name.0 == n)
                .unwrap()
                .1
                .spec
                .clone()
        };
        let (worker, db) = (spec("worker"), spec("db"));
        std::fs::write(dir.path().join("worker/main.c"), "int main = 1;").unwrap();
        assert_ne!(spec("worker"), worker);
        assert_eq!(spec("db"), db);
    }

    #[test]
    fn a_service_waits_for_the_service_that_builds_its_image() {
        let config = serde_json::json!({"services": {
            "rails": {"build": {"context": "/nonexistent"}, "image": "app:dev"},
            "web": {"image": "app:dev"},
            "worker": {"image": "app:dev", "depends_on": {"rails": {"condition": "service_healthy"}}},
            "vite": {"build": {"dockerfile_inline": "FROM --platform=linux/amd64 app:dev\nRUN true\n"}, "image": "vite:dev"},
            "sidekiq": {"build": {"context": "/nonexistent"}, "image": "app:dev"},
        }});
        let m = model(&config, &"main".into(), "d".into()).unwrap();
        let deps = |n: &str| {
            let spec = m.nodes.iter().find(|(k, _)| k.name.0 == n).unwrap().1;
            spec.deps
                .iter()
                .map(|d| d.node.name.0.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(deps("web"), ["rails"]);
        assert_eq!(deps("worker"), ["rails"]);
        assert_eq!(deps("vite"), ["rails"]);
        assert!(deps("rails").is_empty());
        assert!(deps("sidekiq").is_empty());
    }

    #[test]
    fn jobs_gates_and_readiness_signals() {
        let config = serde_json::json!({"services": {
            "db": {"healthcheck": {"test": ["CMD", "true"]}, "ports": [{"target": 5432, "protocol": "tcp"}]},
            "cache": {"expose": ["6379/tcp"]},
            "redis": {"image": "redis:7-alpine", "command": ["redis-server", "--appendonly", "yes"]},
            "redis-cli": {"image": "redis:7", "command": ["redis-cli", "-h", "redis", "monitor"], "expose": ["6379/tcp"]},
            "create-site": {"deploy": {"restart_policy": {"condition": "none"}}},
            "migrate": {"depends_on": {"db": {"condition": "service_healthy"}}},
            "api": {"depends_on": {
                "migrate": {"condition": "service_completed_successfully"},
                "cache": {"condition": "service_started"}}},
        }});
        let m = model(&config, &"main".into(), "d".into()).unwrap();
        let get = |n: &str| m.nodes.iter().find(|(k, _)| k.name.0 == n).unwrap().1;
        assert_eq!(
            get("db").kind,
            NodeKind::Service {
                probe: Probe::HealthEvent
            }
        );
        assert_eq!(
            get("cache").kind,
            NodeKind::Service {
                probe: Probe::Tcp { ports: vec![6379] }
            }
        );
        assert_eq!(
            get("redis").kind,
            NodeKind::Service {
                probe: Probe::LogLine {
                    pattern: "Ready to accept connections".into()
                }
            }
        );
        assert_eq!(
            get("redis-cli").kind,
            NodeKind::Service {
                probe: Probe::Tcp { ports: vec![6379] }
            }
        );
        assert_eq!(
            get("migrate").kind,
            NodeKind::Job {
                kind: JobKind::Migrate
            }
        );
        assert_eq!(
            get("create-site").kind,
            NodeKind::Job {
                kind: JobKind::Seed
            }
        );
        let gates: Vec<_> = get("api")
            .deps
            .iter()
            .map(|d| (d.node.name.0.as_str(), d.gate))
            .collect();
        assert_eq!(
            gates,
            [("cache", Gate::Ready), ("migrate", Gate::Completed)]
        );

        let loop_ = serde_json::json!({"services": {
            "a": {"depends_on": {"b": {}}}, "b": {"depends_on": {"a": {}}}}});
        assert!(model(&loop_, &"main".into(), "d".into()).is_err());
    }
}
