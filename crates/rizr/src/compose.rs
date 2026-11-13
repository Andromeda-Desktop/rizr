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

/// The command prefix every compose call of this stack uses.
/// `dir` is rizr's data directory for the instance: the dev env files live there.
pub fn command(root: &Path, chosen: &[PathBuf], project: &str, dir: &Path) -> Command {
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
        let out = command(root, chosen, project, dir)
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
    let side = config["services"]
        .as_object()
        .into_iter()
        .flatten()
        .map(|(n, s)| {
            let side = Side {
                restart: !s["restart"].is_null(),
                ports: s["ports"].as_array().cloned().unwrap_or_default(),
            };
            (n.clone(), side)
        })
        .collect();
    Ok((model(&config, instance, inputs)?, side))
}

/// What the provider needs per service beside the model.
#[derive(Clone, Debug, Default)]
pub struct Side {
    /// The compose file sets `restart:`; otherwise the override sets `on-failure:5`.
    pub restart: bool,
    /// The resolved `ports` list, for taken-port remaps.
    pub ports: Vec<Value>,
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
    let mut nodes = BTreeMap::new();
    for (name, svc) in services {
        let deps = svc["depends_on"]
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
        let kind = if jobs.contains(name.as_str()) {
            NodeKind::Job {
                kind: JobKind::Migrate,
            }
        } else {
            NodeKind::Service { probe: probe(svc) }
        };
        let spec = Digest(hex(&Sha256::digest(svc.to_string())));
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

/// Readiness signal: healthcheck event, else a tcp probe on each container port, else ready at start.
fn probe(svc: &Value) -> Probe {
    let hc = &svc["healthcheck"];
    if hc.is_object() && hc["disable"] != true && hc["test"] != serde_json::json!(["NONE"]) {
        return Probe::HealthEvent;
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

fn cycle(m: &Model) -> Option<NodeId> {
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
    fn jobs_gates_and_readiness_signals() {
        let config = serde_json::json!({"services": {
            "db": {"healthcheck": {"test": ["CMD", "true"]}, "ports": [{"target": 5432, "protocol": "tcp"}]},
            "cache": {"expose": ["6379/tcp"]},
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
            get("migrate").kind,
            NodeKind::Job {
                kind: JobKind::Migrate
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
