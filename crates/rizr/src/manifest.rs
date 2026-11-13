//! `rizr.toml`: what compose cannot say. Today: host-process services that run under the supervisor
//! (systemd user units) in the same graph as the compose nodes (VISION sections 7 and 8).

use std::collections::BTreeMap;
use std::path::Path;

use rizr_core::model::{
    Dep, Digest, Gate, InstanceId, Model, NodeId, NodeKind, NodeName, NodeSpec, Probe,
    ProviderName, Scope,
};
use serde::Deserialize;
use sha2::{Digest as _, Sha256};

pub const PROVIDER: &str = "systemd";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    version: u32,
    #[serde(default)]
    service: BTreeMap<String, HostService>,
    /// Readiness of compose services: `[ready] web = { log = "Listening on" }`.
    #[serde(default)]
    ready: BTreeMap<String, ComposeReady>,
    /// Shared resources: `[resource.db] shared = false` keeps a compose postgres service its own container.
    #[serde(default)]
    resource: BTreeMap<String, Resource>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Resource {
    shared: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ComposeReady {
    log: String,
}

#[derive(Clone, Debug, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct HostService {
    pub cmd: Cmd,
    /// Repo-relative working directory.
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub ready: Option<Ready>,
}

#[derive(Clone, Debug, Deserialize, serde::Serialize)]
#[serde(untagged)]
pub enum Cmd {
    /// Runs through `sh -c`.
    Shell(String),
    Argv(Vec<String>),
}

#[derive(Clone, Debug, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct Ready {
    pub tcp: Option<u16>,
    /// `http://localhost:<port>/<path>`; ready on a 2xx or 3xx reply.
    pub http: Option<String>,
    /// Ready when a line of the service's output contains this text.
    pub log: Option<String>,
}

/// Adds the host services of `<root>/rizr.toml` to `m`. Returns them by name for the supervisor.
pub fn merge(
    root: &Path,
    instance: &InstanceId,
    m: &mut Model,
) -> Result<BTreeMap<String, HostService>, String> {
    let path = root.join("rizr.toml");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(BTreeMap::new());
    };
    let man: Manifest = toml::from_str(&text).map_err(|e| format!("rizr.toml: {e}"))?;
    if man.version != 1 {
        return Err(format!(
            "rizr.toml: version {} is newer than this rizr understands (1); update rizr",
            man.version
        ));
    }
    let id = |n: &str| NodeId {
        scope: Scope::Instance(instance.clone()),
        name: NodeName(n.into()),
    };
    for (name, svc) in &man.service {
        if m.nodes.contains_key(&id(name)) {
            return Err(format!(
                "rizr.toml: service `{name}` is also a compose service; give it another name"
            ));
        }
        for d in &svc.depends_on {
            if !m.nodes.contains_key(&id(d)) && !man.service.contains_key(d) {
                return Err(format!(
                    "rizr.toml: `{name}` depends on `{d}`, which is neither a compose nor a host service"
                ));
            }
        }
    }
    for (name, r) in &man.ready {
        match m.nodes.get_mut(&id(name)) {
            Some(NodeSpec {
                kind: NodeKind::Service { probe },
                ..
            }) => {
                *probe = Probe::LogLine {
                    pattern: r.log.clone(),
                }
            }
            _ => {
                return Err(format!(
                    "rizr.toml: [ready] `{name}` is not a compose service; a host service sets `ready` in its own table"
                ));
            }
        }
    }
    if let Some(name) = man.resource.keys().find(|n| !m.nodes.contains_key(&id(n))) {
        return Err(format!(
            "rizr.toml: [resource] `{name}` is not a compose service"
        ));
    }
    for (name, svc) in &man.service {
        let probe = match &svc.ready {
            Some(Ready { tcp: Some(p), .. }) => Probe::Tcp { ports: vec![*p] },
            Some(Ready { http: Some(u), .. }) => {
                let rest = u
                    .strip_prefix("http://localhost:")
                    .or_else(|| u.strip_prefix("http://127.0.0.1:"))
                    .ok_or_else(|| {
                        format!(
                            "rizr.toml: `{name}` ready.http must be http://localhost:<port>/..."
                        )
                    })?;
                let (port, path) = rest.split_once('/').unwrap_or((rest, ""));
                Probe::Http {
                    port: port
                        .parse()
                        .map_err(|_| format!("rizr.toml: `{name}` ready.http has no port"))?,
                    path: format!("/{path}"),
                }
            }
            Some(Ready { log: Some(l), .. }) => Probe::LogLine { pattern: l.clone() },
            _ => Probe::AtStart,
        };
        let spec = serde_json::to_string(svc).map_err(|e| e.to_string())?;
        let spec = Sha256::digest(spec);
        m.nodes.insert(
            id(name),
            NodeSpec {
                kind: NodeKind::Service { probe },
                deps: svc
                    .depends_on
                    .iter()
                    .map(|d| Dep {
                        node: id(d),
                        gate: Gate::Ready,
                    })
                    .collect(),
                spec: Digest(spec.iter().map(|b| format!("{b:02x}")).collect()),
                provider: ProviderName(PROVIDER.into()),
            },
        );
    }
    Ok(man.service)
}

/// Compose services that `rizr.toml` keeps out of the shared servers; empty when it is absent or does not parse.
pub fn unshared(root: &Path) -> std::collections::BTreeSet<String> {
    std::fs::read_to_string(root.join("rizr.toml"))
        .ok()
        .and_then(|t| toml::from_str::<Manifest>(&t).ok())
        .map(|m| {
            m.resource
                .into_iter()
                .filter(|(_, r)| !r.shared)
                .map(|(n, _)| n)
                .collect()
        })
        .unwrap_or_default()
}

/// Names of the host services in `<root>/rizr.toml`; empty when it is absent or does not parse.
pub fn host_names(root: &Path) -> std::collections::BTreeSet<String> {
    std::fs::read_to_string(root.join("rizr.toml"))
        .ok()
        .and_then(|t| toml::from_str::<Manifest>(&t).ok())
        .map(|m| m.service.into_keys().collect())
        .unwrap_or_default()
}
