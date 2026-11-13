//! Shared postgres (TASK-d1 step 5): one server container per project and major version; each instance gets
//! its own database on it.

use std::collections::{BTreeMap, BTreeSet};
use std::process::Command;

use rizr_core::model::{
    Dep, Digest, Gate, InstanceId, Model, NodeId, NodeKind, NodeName, NodeSpec, Probe,
    ProviderName, Scope,
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

/// A compose service that moves to a shared server.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Shared {
    pub server: Server,
    /// The database the compose file names: `POSTGRES_DB`, else `POSTGRES_USER`, else `postgres`.
    pub db: String,
    /// The compose project name and the service's networks as (compose key, docker name).
    pub project: String,
    pub networks: Vec<(String, String)>,
}

/// The database of one instance on a shared server; the compose service name resolves to the server.
#[derive(Clone, Debug)]
pub struct Db {
    pub shared: Shared,
    pub database: String,
    pub spec: Digest,
}

impl Db {
    /// Creates the database when it is missing, then gives the server the service name on each network of the
    /// service. Creates a missing network with compose's labels, so compose adopts it.
    pub fn attach(&self, project: &str, alias: &str) -> Result<(), String> {
        let server = self.shared.server.container(project);
        for (key, net) in &self.shared.networks {
            if !ok(Command::new("docker").args(["network", "inspect", net])) {
                run(Command::new("docker")
                    .args(["network", "create"])
                    .arg(format!(
                        "--label=com.docker.compose.project={}",
                        self.shared.project
                    ))
                    .arg(format!("--label=com.docker.compose.network={key}"))
                    .arg(net))?;
            }
        }
        let user = self.shared.server.user();
        let sql = format!(
            "select 1 from pg_database where datname = '{}'",
            self.database
        );
        let exists = Command::new("docker")
            .args([
                "exec", &server, "psql", "-U", user, "-d", "postgres", "-tAc", &sql,
            ])
            .output()
            .map_err(|e| e.to_string())?;
        if !exists.status.success() {
            return Err(crate::compose::tail(&exists.stderr));
        }
        if String::from_utf8_lossy(&exists.stdout).trim() != "1" {
            run(Command::new("docker").args([
                "exec",
                &server,
                "createdb",
                "-U",
                user,
                &self.database,
            ]))?;
        }
        let state = crate::docker::inspect_one(&server)?;
        for (_, net) in &self.shared.networks {
            match aliases(&state, net) {
                Some(a) if a.iter().any(|x| x == alias) => continue,
                Some(_) => {
                    run(Command::new("docker").args(["network", "disconnect", net, &server]))?
                }
                None => {}
            }
            run(
                Command::new("docker").args(["network", "connect", "--alias", alias, net, &server])
            )?;
        }
        Ok(())
    }

    /// Takes the service name off the server; the database stays.
    pub fn detach(&self, project: &str) -> Result<(), String> {
        let server = self.shared.server.container(project);
        let Ok(state) = crate::docker::inspect_one(&server) else {
            return Ok(());
        };
        for (_, net) in &self.shared.networks {
            if aliases(&state, net).is_some() {
                run(Command::new("docker").args(["network", "disconnect", net, &server]))?;
            }
        }
        Ok(())
    }

    /// The running server answers to `alias` on every network of the service.
    pub fn attached(&self, state: &Value, alias: &str) -> bool {
        state["State"]["Running"] == true
            && self
                .shared
                .networks
                .iter()
                .all(|(_, n)| aliases(state, n).is_some_and(|a| a.iter().any(|x| x == alias)))
    }
}

/// The aliases of a container on `net`; None when it is not connected.
fn aliases(state: &Value, net: &str) -> Option<Vec<String>> {
    let n = state["NetworkSettings"]["Networks"].get(net)?;
    Some(
        n["Aliases"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|a| a.as_str().map(str::to_owned))
            .collect(),
    )
}

fn ok(c: &mut Command) -> bool {
    c.output().is_ok_and(|o| o.status.success())
}

fn run(c: &mut Command) -> Result<(), String> {
    let o = c.output().map_err(|e| format!("cannot run docker: {e}"))?;
    if o.status.success() {
        Ok(())
    } else {
        Err(crate::compose::tail(&o.stderr))
    }
}

/// The shared server and database of a compose service. None: not a shareable postgres service, or it has a
/// `network_mode` and so no network to give the service name on.
pub fn shared(svc: &Value, config: &Value) -> Option<Shared> {
    let server = detect(svc)?;
    if !svc["network_mode"].is_null() {
        return None;
    }
    let keys: Vec<String> = match svc["networks"].as_object() {
        Some(n) => n.keys().cloned().collect(),
        None => vec!["default".into()],
    };
    let networks = keys
        .into_iter()
        .map(|k| {
            let name = config["networks"][&k]["name"].as_str()?.to_owned();
            Some((k, name))
        })
        .collect::<Option<_>>()?;
    let env = |k: &str| svc["environment"][k].as_str().filter(|v| !v.is_empty());
    Some(Shared {
        db: env("POSTGRES_DB")
            .or_else(|| env("POSTGRES_USER"))
            .unwrap_or("postgres")
            .to_owned(),
        server,
        project: config["name"].as_str()?.to_owned(),
        networks,
    })
}

/// The database of `instance`: the compose name for the main worktree, `<db>_<instance>` for the others.
fn database(db: &str, instance: &InstanceId) -> String {
    if instance.0 == "main" {
        return db.to_owned();
    }
    let suffix: String = instance
        .0
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    format!("{db}_{suffix}")
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

/// The servers by node name and the databases by compose service name.
pub type Resources = (BTreeMap<String, Server>, BTreeMap<String, Db>);

/// Adds one project node per server the shared services need, and makes each shared service an instance node
/// of this provider that waits for its server.
pub fn add(
    m: &mut Model,
    side: &BTreeMap<String, crate::compose::Side>,
    unshared: &BTreeSet<String>,
    instance: &InstanceId,
) -> Result<Resources, String> {
    let mut servers: BTreeMap<String, (String, Server)> = BTreeMap::new();
    let mut dbs = BTreeMap::new();
    for (svc, s) in side {
        let Some(shared) = s.pg.as_ref().filter(|_| !unshared.contains(svc)) else {
            continue;
        };
        let pg = &shared.server;
        let id = NodeId {
            scope: Scope::Instance(instance.clone()),
            name: NodeName(svc.clone()),
        };
        let Some(node) = m.nodes.get_mut(&id) else {
            continue;
        };
        let database = database(&shared.db, instance);
        if !database
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            return Err(format!(
                "compose service `{svc}`: database name `{database}` has characters other than letters, digits, _ \
                 and -; set `[resource.{svc}] shared = false` in rizr.toml"
            ));
        }
        let spec = digest(&(shared, &database))?;
        node.kind = NodeKind::Service {
            probe: Probe::AtStart,
        };
        node.provider = ProviderName(PROVIDER.into());
        node.spec = spec.clone();
        node.deps.push(Dep {
            node: NodeId {
                scope: Scope::Project,
                name: NodeName(pg.node()),
            },
            gate: Gate::Ready,
        });
        dbs.insert(
            svc.clone(),
            Db {
                shared: shared.clone(),
                database,
                spec,
            },
        );
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
        let spec = digest(pg)?;
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
                spec,
                provider: ProviderName(PROVIDER.into()),
            },
        );
    }
    Ok((servers.into_iter().map(|(n, (_, s))| (n, s)).collect(), dbs))
}

fn digest(v: &impl serde::Serialize) -> Result<Digest, String> {
    let s = serde_json::to_string(v).map_err(|e| e.to_string())?;
    Ok(Digest(
        Sha256::digest(s)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect(),
    ))
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
    fn shared_services_wait_for_their_server_and_get_an_instance_database() {
        let config = json!({"name": "p", "networks": {"default": {"name": "p_default"}}, "services": {
            "a": {"image": "postgres:17", "environment": {"POSTGRES_DB": "app"}},
            "b": {"image": "pgvector/pgvector:pg17"}}});
        let wt: InstanceId = "wt-2".into();
        let side = |svc: &str| crate::compose::Side {
            pg: shared(&config["services"][svc], &config),
            ..Default::default()
        };
        let side = BTreeMap::from([("a".to_string(), side("a")), ("b".to_string(), side("b"))]);
        let model = || crate::compose::model(&config, &wt, Digest(String::new())).unwrap();
        assert!(add(&mut model(), &side, &BTreeSet::new(), &wt).is_err());
        let mut m = model();
        let (servers, dbs) = add(&mut m, &side, &BTreeSet::from(["b".to_string()]), &wt).unwrap();
        assert_eq!(servers["pg17"].image, "postgres:17");
        assert_eq!(dbs["a"].database, "app_wt_2");
        assert!(!dbs.contains_key("b"));
        let a = &m.nodes[&NodeId {
            scope: Scope::Instance(wt.clone()),
            name: NodeName("a".into()),
        }];
        assert_eq!(a.provider.0, PROVIDER);
        assert!(
            a.deps
                .iter()
                .any(|d| d.node.scope == Scope::Project && d.node.name.0 == "pg17")
        );
        assert_eq!(m.nodes.len(), 3);
    }
}
