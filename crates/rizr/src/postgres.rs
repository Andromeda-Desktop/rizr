//! Shared postgres (TASK-d1 step 5): one server container per project and major version; each instance gets
//! its own database on it.

use std::collections::{BTreeMap, BTreeSet};
use std::process::Command;

use rizr_core::model::{
    Digest, Model, NodeId, NodeKind, NodeName, NodeSpec, Probe, ProviderName, Scope,
};
use serde_json::Value;
use sha2::{Digest as _, Sha256};

pub const PROVIDER: &str = "postgres";

/// The server a compose postgres service moves to.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Server {
    pub major: u32,
    pub image: String,
    /// The service's `POSTGRES_*` variables except `POSTGRES_DB`: the database is per instance.
    pub env: BTreeMap<String, String>,
}

impl Server {
    pub fn node(&self) -> String {
        format!("pg{}", self.major)
    }

    pub fn user(&self) -> &str {
        self.env
            .get("POSTGRES_USER")
            .map_or("postgres", String::as_str)
    }

    pub fn container(&self, project: &str) -> String {
        format!("rizr-{project}-{}", self.node())
    }

    /// The `docker run` of the server: a named volume, one loopback port for host services, and a health
    /// check that probes fast only while it starts (no idle wakeups).
    pub fn run(&self, project: &str, spec: &Digest, generation: u64) -> Command {
        let name = self.container(project);
        // Images from 18 on keep PGDATA under /var/lib/postgresql/<major>.
        let data = if self.major >= 18 {
            "/var/lib/postgresql"
        } else {
            "/var/lib/postgresql/data"
        };
        let mut c = Command::new("docker");
        c.args(["run", "-d", "--name", &name, "--restart", "on-failure:5"])
            .arg(format!("--label=rizr.project={project}"))
            .arg("--label=rizr.scope=project")
            .arg(format!("--label=rizr.node={}", self.node()))
            .arg(format!("--label=rizr.spec={}", spec.0))
            .arg(format!("--label=rizr.gen={generation}"))
            .arg(format!("--health-cmd=pg_isready -U {}", self.user()))
            .args([
                "--health-interval=30s",
                "--health-start-period=60s",
                "--health-start-interval=500ms",
                "--health-timeout=5s",
            ])
            .arg(format!("--volume={name}:{data}"))
            .arg("--publish=127.0.0.1::5432");
        for (k, v) in &self.env {
            c.arg(format!("--env={k}={v}"));
        }
        c.arg(&self.image);
        c
    }
}

/// The server a compose service can move to. None: not a versioned postgres image, or the service has init
/// scripts or a custom command that a shared server would skip.
pub fn detect(svc: &Value) -> Option<Server> {
    let major = major(svc["image"].as_str()?)?;
    let init = svc["volumes"].as_array().into_iter().flatten().any(|v| {
        v["target"]
            .as_str()
            .is_some_and(|t| t.starts_with("/docker-entrypoint-initdb.d"))
    });
    if init || !svc["command"].is_null() {
        return None;
    }
    let env = svc["environment"]
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(k, _)| k.starts_with("POSTGRES_") && *k != "POSTGRES_DB")
        .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_owned())))
        .collect();
    Some(Server {
        major,
        image: svc["image"].as_str()?.to_owned(),
        env,
    })
}

/// The major version of a postgres, pgvector or postgis image; None for an untagged or `latest` image.
fn major(image: &str) -> Option<u32> {
    let image = image.split('@').next()?;
    let (repo, tag) = image.rsplit_once(':')?;
    if tag.contains('/') {
        return None;
    }
    let digits = |s: &str| {
        s.chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>()
            .parse()
            .ok()
    };
    match repo.rsplit('/').next()? {
        "postgres" | "postgis" => digits(tag),
        "pgvector" => digits(tag.strip_prefix("pg")?),
        _ => None,
    }
}

/// Adds one project node per server the shared services need. Returns the servers by node name.
pub fn add(
    m: &mut Model,
    side: &BTreeMap<String, crate::compose::Side>,
    unshared: &BTreeSet<String>,
) -> Result<BTreeMap<String, Server>, String> {
    let mut servers: BTreeMap<String, (String, Server)> = BTreeMap::new();
    for (svc, s) in side {
        let Some(pg) = s.pg.as_ref().filter(|_| !unshared.contains(svc)) else {
            continue;
        };
        match servers.get(&pg.node()) {
            Some((other, p)) if p != pg => {
                return Err(format!(
                    "compose services `{other}` and `{svc}` need one shared {} server but differ in image or \
                     POSTGRES_* variables; set `[resource.{svc}] shared = false` in rizr.toml",
                    pg.node()
                ));
            }
            Some(_) => {}
            None => {
                servers.insert(pg.node(), (svc.clone(), pg.clone()));
            }
        }
    }
    for (name, (_, pg)) in &servers {
        let spec = serde_json::to_string(pg).map_err(|e| e.to_string())?;
        m.nodes.insert(
            NodeId {
                scope: Scope::Project,
                name: NodeName(name.clone()),
            },
            NodeSpec {
                kind: NodeKind::Service {
                    probe: Probe::HealthEvent,
                },
                deps: vec![],
                spec: Digest(
                    Sha256::digest(spec)
                        .iter()
                        .map(|b| format!("{b:02x}"))
                        .collect(),
                ),
                provider: ProviderName(PROVIDER.into()),
            },
        );
    }
    Ok(servers.into_iter().map(|(n, (_, s))| (n, s)).collect())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "a test panics on a broken precondition")]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn detects_versioned_images_and_skips_custom_services() {
        let pg = |svc: Value| detect(&svc).map(|s| s.major);
        assert_eq!(pg(json!({"image": "postgres:17-alpine"})), Some(17));
        assert_eq!(pg(json!({"image": "pgvector/pgvector:pg16"})), Some(16));
        assert_eq!(pg(json!({"image": "postgis/postgis:15-3.4"})), Some(15));
        assert_eq!(
            pg(json!({"image": "registry:5000/postgres:18@sha256:ab"})),
            Some(18)
        );
        assert_eq!(pg(json!({"image": "postgres:latest"})), None);
        assert_eq!(pg(json!({"image": "postgres"})), None);
        assert_eq!(pg(json!({"image": "registry:5000/postgres"})), None);
        assert_eq!(pg(json!({"image": "mysql:8"})), None);
        assert_eq!(
            pg(json!({"image": "postgres:17", "command": ["postgres", "-c", "wal_level=logical"]})),
            None
        );
        assert_eq!(
            pg(json!({"image": "postgres:17", "volumes": [
                {"type": "bind", "source": "./init", "target": "/docker-entrypoint-initdb.d"}]})),
            None
        );
    }

    #[test]
    fn two_services_with_different_servers_of_one_major_fail_unless_one_is_unshared() {
        let side = |image: &str| crate::compose::Side {
            pg: detect(&json!({"image": image})),
            ..Default::default()
        };
        let side = BTreeMap::from([
            ("a".to_string(), side("postgres:17")),
            ("b".to_string(), side("pgvector/pgvector:pg17")),
        ]);
        let mut m = Model {
            inputs: Digest(String::new()),
            nodes: BTreeMap::new(),
            findings: vec![],
        };
        assert!(add(&mut m, &side, &BTreeSet::new()).is_err());
        let servers = add(&mut m, &side, &BTreeSet::from(["b".to_string()])).unwrap();
        assert_eq!(servers["pg17"].image, "postgres:17");
        assert_eq!(m.nodes.len(), 1);
    }
}
