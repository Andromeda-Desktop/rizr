//! Shared postgres (TASK-d1 step 5): one server container per project and major version; each instance gets
//! its own database on it.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
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
            // Over TCP: the image's init-time server answers on the socket only, then stops.
            .arg(format!(
                "--health-cmd=pg_isready -h 127.0.0.1 -U {}",
                self.user()
            ))
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
    /// The data mount the compose file gives the service, as (volume name or host path, target).
    pub old_data: Option<(String, String)>,
}

/// The database of one instance on a shared server; the compose service name resolves to the server.
#[derive(Clone, Debug)]
pub struct Db {
    pub shared: Shared,
    pub database: String,
    pub spec: Digest,
    /// Host ports the compose service published; host processes used them before the switch.
    pub published: Vec<u16>,
}

/// What `context`, `why` and `logs` show of the shared postgres resources; `import` writes it.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct Info {
    /// By compose service name.
    pub databases: BTreeMap<String, DbInfo>,
    /// Compose service env vars that rizr points at this instance's database.
    pub rewritten: Vec<Rewritten>,
    /// Postgres services that stay plain compose containers, with the reason.
    pub plain: BTreeMap<String, String>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct DbInfo {
    /// The server node, `pg<major>`.
    pub server: String,
    pub database: String,
    /// The database name in the compose file.
    pub base: String,
    pub user: String,
    pub password: String,
    pub published: Vec<u16>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Rewritten {
    pub service: String,
    pub var: String,
    pub value: String,
}

impl DbInfo {
    /// The URL of the database at `host`.
    pub fn url(&self, host: &str) -> String {
        let auth = if self.password.is_empty() {
            self.user.clone()
        } else {
            format!("{}:{}", self.user, self.password)
        };
        format!("postgresql://{auth}@{host}/{}", self.database)
    }
}

impl Info {
    const FILE: &str = "postgres.json";

    pub fn load(dir: &Path) -> Self {
        std::fs::read(dir.join(Self::FILE))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, dir: &Path) -> Result<(), String> {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        let json = serde_json::to_vec_pretty(self).map_err(|e| e.to_string())?;
        std::fs::write(dir.join(Self::FILE), json).map_err(|e| e.to_string())
    }
}

/// The loopback port of a running server container, for host processes.
pub fn host_port(container: &str) -> Option<u16> {
    crate::docker::inspect_one(container).ok()?["NetworkSettings"]["Ports"]["5432/tcp"][0]["HostPort"]
        .as_str()?
        .parse()
        .ok()
}

/// A postgres URL, split where rizr changes it.
struct Url<'a> {
    /// Scheme and user info.
    head: &'a str,
    host: &'a str,
    port: Option<u16>,
    /// `/db`, `/` or empty.
    path: &'a str,
    /// `?query`.
    tail: &'a str,
}

fn parse(v: &str) -> Option<Url<'_>> {
    let scheme = ["postgres://", "postgresql://"]
        .into_iter()
        .find(|s| v.starts_with(s))?
        .len();
    let rest = &v[scheme..];
    let (auth, after) = rest.split_at(rest.find(['/', '?']).unwrap_or(rest.len()));
    let at = auth.rfind('@').map_or(0, |i| i + 1);
    let (host, port) = match auth[at..].rsplit_once(':') {
        Some((h, p)) => (h, Some(p.parse().ok()?)),
        None => (&auth[at..], None),
    };
    let (path, tail) = after.split_at(after.find('?').unwrap_or(after.len()));
    Some(Url {
        head: &v[..scheme + at],
        host,
        port,
        path,
        tail,
    })
}

/// The env vars that point at a shared database, with their values for this instance. `host_port`: the server's
/// loopback port, for a host process (which used a published port); None for a compose service (which used the
/// service name). Other vars change only when the value is exactly the compose database name, and never a
/// user or password var.
pub fn rewrite(
    env: &BTreeMap<String, String>,
    dbs: &BTreeMap<String, DbInfo>,
    host_port: Option<u16>,
) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for (k, v) in env {
        for (svc, db) in dbs {
            let new = if let Some(u) = parse(v) {
                let ours = match host_port {
                    None => u.host == svc,
                    Some(_) => {
                        matches!(u.host, "localhost" | "127.0.0.1")
                            && u.port.is_some_and(|p| db.published.contains(&p))
                    }
                };
                if !ours {
                    continue;
                }
                let hostport = match (host_port, u.port) {
                    (Some(p), _) => format!("127.0.0.1:{p}"),
                    (None, Some(p)) => format!("{}:{p}", u.host),
                    (None, None) => u.host.to_owned(),
                };
                let path = if u.path.strip_prefix('/') == Some(db.base.as_str()) {
                    format!("/{}", db.database)
                } else {
                    u.path.to_owned()
                };
                format!("{}{hostport}{path}{}", u.head, u.tail)
            } else if *v == db.base
                && !k.to_uppercase().contains("USER")
                && !k.to_uppercase().contains("PASS")
            {
                db.database.clone()
            } else {
                continue;
            };
            if new != *v {
                out.insert(k.clone(), new);
            }
            break;
        }
    }
    out
}

impl Db {
    pub fn info(&self) -> DbInfo {
        DbInfo {
            server: self.shared.server.node(),
            database: self.database.clone(),
            base: self.shared.db.clone(),
            user: self.shared.server.user().to_owned(),
            password: self
                .shared
                .server
                .env
                .get("POSTGRES_PASSWORD")
                .cloned()
                .unwrap_or_default(),
            published: self.published.clone(),
        }
    }

    /// Creates the database when it is missing, then gives the server the service name on each network of the
    /// service. Creates a missing network with compose's labels, so compose adopts it.
    pub fn rewind_target(&self, project: &str) -> crate::rewind::Target {
        crate::rewind::Target {
            server: self.shared.server.container(project),
            user: self.shared.server.user().to_owned(),
            database: self.database.clone(),
        }
    }

    /// Returns true when it created the database.
    pub fn attach(&self, project: &str, alias: &str) -> Result<bool, String> {
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
        let created = String::from_utf8_lossy(&exists.stdout).trim() != "1";
        if created {
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
        Ok(created)
    }

    /// Copies the old compose data of the service into its database once: roles first, then the database. It
    /// runs only while the database has no tables and no import mark. The old data is read, never changed.
    /// On a failure it drops the database, so the next start tries again. Returns true when it copied data.
    /// Fill a new instance database from main's data: main's database on this server, else main's running
    /// compose container, else main's compose volume. With none of them it stays empty.
    pub fn seed(
        &self,
        project: &str,
        svc: &str,
        main_compose: &str,
        compose: &str,
    ) -> Result<(), String> {
        let server = self.shared.server.container(project);
        let user = self.shared.server.user();
        let main = &self.shared.db;
        let on_server = Command::new("docker")
            .args(["exec", &server, "psql", "-U", user, "-d", main, "-tAc"])
            .arg("select count(*) from pg_tables where schemaname not in ('pg_catalog', 'information_schema')")
            .output()
            .map_err(|e| e.to_string())?;
        // The image creates an empty POSTGRES_DB at init: only a database with tables holds main's data.
        let tables: u64 = String::from_utf8_lossy(&on_server.stdout)
            .trim()
            .parse()
            .unwrap_or(0);
        if tables > 0 {
            run(Command::new("docker").args([
                "exec",
                &server,
                "dropdb",
                "-U",
                user,
                "--maintenance-db=template1",
                &self.database,
            ]))?;
            return clone(&server, user, main, &self.database, None);
        }
        let running = Command::new("docker")
            .args(["ps", "-q"])
            .arg(format!(
                "--filter=label=com.docker.compose.project={main_compose}"
            ))
            .arg(format!("--filter=label=com.docker.compose.service={svc}"))
            .output()
            .map_err(|e| e.to_string())?;
        if let Some(id) = String::from_utf8_lossy(&running.stdout).lines().next() {
            let mut dump = Command::new("docker")
                .args([
                    "exec",
                    id,
                    "pg_dump",
                    "-U",
                    user,
                    "--no-owner",
                    "--no-acl",
                    main,
                ])
                .stdout(std::process::Stdio::piped())
                .spawn()
                .map_err(|e| e.to_string())?;
            let out = dump.stdout.take().ok_or("pg_dump gave no output")?;
            let load = Command::new("docker")
                .args([
                    "exec",
                    "-i",
                    &server,
                    "psql",
                    "-q",
                    "-v",
                    "ON_ERROR_STOP=1",
                    "-U",
                    user,
                    "-d",
                ])
                .arg(&self.database)
                .stdin(out)
                .stdout(std::process::Stdio::null())
                .output()
                .map_err(|e| e.to_string())?;
            let dumped = dump.wait().map_err(|e| e.to_string())?;
            if !dumped.success() || !load.status.success() {
                return Err(format!(
                    "copy of {main} from {main_compose} failed: {}",
                    crate::compose::tail(&load.stderr)
                ));
            }
            return Ok(());
        }
        // A named volume of this instance is `<compose>_<name>`; main's is `<main_compose>_<name>`.
        let main_volume = self.shared.old_data.as_ref().and_then(|(source, target)| {
            let name = source.strip_prefix(&format!("{compose}_"))?;
            Some((format!("{main_compose}_{name}"), target.clone()))
        });
        match main_volume {
            Some(old) => self.import(project, svc, &old).map(|_| ()),
            None => Ok(()),
        }
    }

    pub fn import(
        &self,
        project: &str,
        instance: &str,
        old: &(String, String),
    ) -> Result<bool, String> {
        let server = self.shared.server.container(project);
        let user = self.shared.server.user();
        let q = |db: &str, sql: &str| -> Result<String, String> {
            let o = Command::new("docker")
                .args(["exec", &server, "psql", "-U", user, "-d", db, "-tAc", sql])
                .output()
                .map_err(|e| e.to_string())?;
            if o.status.success() {
                Ok(String::from_utf8_lossy(&o.stdout).trim().to_owned())
            } else {
                Err(crate::compose::tail(&o.stderr))
            }
        };
        let d = &self.database;
        let mark = format!("COMMENT ON DATABASE \"{d}\" IS '{IMPORTED}'");
        let comment = q(
            "postgres",
            &format!(
                "select coalesce(shobj_description(oid, 'pg_database'), '') from pg_database where datname = '{d}'"
            ),
        )?;
        let tables = q(
            d,
            "select count(*) from pg_class c join pg_namespace n on n.oid = c.relnamespace where c.relkind in \
             ('r', 'p') and n.nspname not in ('pg_catalog', 'information_schema') and n.nspname not like 'pg_toast%'",
        )?;
        if comment == IMPORTED || tables != "0" {
            return q("postgres", &mark).map(|_| false);
        }
        // A volume that compose never made, or an empty bind, holds no old data; `docker run` would create it.
        let present = if Path::new(&old.0).is_absolute() {
            std::fs::read_dir(&old.0).is_ok_and(|mut d| d.next().is_some())
        } else {
            ok(Command::new("docker").args(["volume", "inspect", &old.0]))
        };
        if !present {
            return q("postgres", &mark).map(|_| false);
        }
        let tmp = format!("rizr-{project}-import-{instance}");
        let _ = Command::new("docker")
            .args(["rm", "--force", &tmp])
            .output();
        let r = self.copy(&server, &tmp, old);
        let _ = Command::new("docker")
            .args(["rm", "--force", &tmp])
            .output();
        match r {
            Ok(copied) => q("postgres", &mark).map(|_| copied),
            Err(e) => {
                let _ = Command::new("docker")
                    .args([
                        "exec",
                        &server,
                        "dropdb",
                        "-U",
                        user,
                        "--maintenance-db=template1",
                        d,
                    ])
                    .output();
                Err(format!("import of the old data from {}: {e}", old.0))
            }
        }
    }

    fn copy(&self, server: &str, tmp: &str, old: &(String, String)) -> Result<bool, String> {
        let user = self.shared.server.user();
        // Health events end the wait; the old data needs no init, so the server starts as soon as it recovers.
        let mut events = Command::new("timeout")
            .args(["120", "docker", "events", "--format", "{{.Action}}"])
            .arg(format!("--filter=container={tmp}"))
            .arg("--filter=event=health_status")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| e.to_string())?;
        let started = run(Command::new("docker")
            .args(["run", "-d", "--name", tmp, "--network", "none"])
            .arg(format!("--volume={}:{}", old.0, old.1))
            .arg(format!("--health-cmd=pg_isready -h 127.0.0.1 -U {user}"))
            .args([
                "--health-interval=30s",
                "--health-start-period=120s",
                "--health-start-interval=300ms",
            ])
            .args(
                self.shared
                    .server
                    .env
                    .iter()
                    .map(|(k, v)| format!("--env={k}={v}")),
            )
            .arg(&self.shared.server.image));
        let healthy = started.is_ok()
            && events.stdout.take().is_some_and(|out| {
                std::io::BufRead::lines(std::io::BufReader::new(out))
                    .map_while(Result::ok)
                    .any(|l| l.ends_with(": healthy"))
            });
        let _ = events.kill();
        let _ = events.wait();
        started?;
        if !healthy {
            return Err("the old data did not start within 120 s".into());
        }
        let exists = Command::new("docker")
            .args(["exec", tmp, "psql", "-U", user, "-d", "postgres", "-tAc"])
            .arg(format!(
                "select 1 from pg_database where datname = '{}'",
                self.shared.db
            ))
            .output()
            .map_err(|e| e.to_string())?;
        if String::from_utf8_lossy(&exists.stdout).trim() != "1" {
            return Ok(false);
        }
        // Roles that already exist on the server fail alone; psql goes on with the rest.
        pipe(
            Command::new("docker").args(["exec", tmp, "pg_dumpall", "-U", user, "--globals-only"]),
            Command::new("docker").args([
                "exec", "-i", server, "psql", "-U", user, "-d", "postgres", "-q",
            ]),
        )?;
        pipe(
            Command::new("docker").args([
                "exec",
                tmp,
                "pg_dump",
                "-U",
                user,
                "-Fc",
                &self.shared.db,
            ]),
            Command::new("docker").args([
                "exec",
                "-i",
                server,
                "pg_restore",
                "-U",
                user,
                "-d",
                &self.database,
                "--exit-on-error",
            ]),
        )?;
        Ok(true)
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

/// The comment that marks a database as imported, so a later start never imports into it again.
const IMPORTED: &str = "rizr: old compose data imported";

/// Runs `from | to`; fails when either side fails.
fn pipe(from: &mut Command, to: &mut Command) -> Result<(), String> {
    let mut a = from
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    let out = a.stdout.take().ok_or("no stdout")?;
    let b = to
        .stdin(out)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .output()
        .map_err(|e| e.to_string())?;
    let a = a.wait_with_output().map_err(|e| e.to_string())?;
    if !a.status.success() {
        return Err(crate::compose::tail(&a.stderr));
    }
    if !b.status.success() {
        return Err(crate::compose::tail(&b.stderr));
    }
    Ok(())
}

/// Run one admin statement on `template1`; with a budget, the server cancels it after that many ms.
pub fn admin(
    server: &str,
    user: &str,
    sql: &str,
    budget_ms: Option<u64>,
) -> Result<String, String> {
    let mut c = Command::new("docker");
    c.arg("exec");
    if let Some(ms) = budget_ms {
        c.args(["-e", &format!("PGOPTIONS=-c statement_timeout={ms}")]);
    }
    let o = c
        .args([server, "psql", "-U", user, "-d", "template1", "-qtAc", sql])
        .output()
        .map_err(|e| e.to_string())?;
    if o.status.success() {
        Ok(String::from_utf8_lossy(&o.stdout).trim().to_owned())
    } else {
        Err(crate::compose::tail(&o.stderr))
    }
}

/// The services whose database on a shared server has no tables yet, as (service, database): the app's
/// migration has not run. A server that does not answer counts as not empty.
pub fn empty(dir: &Path, project: &str) -> Vec<(String, String)> {
    Info::load(dir)
        .databases
        .into_iter()
        .filter(|(_, d)| {
            Command::new("docker")
                .args([
                    "exec",
                    &format!("rizr-{project}-{}", d.server),
                    "psql",
                    "-U",
                    &d.user,
                    "-d",
                ])
                .arg(&d.database)
                .args([
                    "-qtAc",
                    "select count(*) from information_schema.tables \
                     where table_schema not in ('pg_catalog', 'information_schema')",
                ])
                .output()
                .is_ok_and(|o| {
                    o.status.success() && String::from_utf8_lossy(&o.stdout).trim() == "0"
                })
        })
        .map(|(svc, d)| (svc, d.database))
        .collect()
}

/// Close `db` to new sessions and end its open ones: a template, a drop or a rename needs it alone.
pub fn close(server: &str, user: &str, db: &str) -> Result<(), String> {
    admin(
        server,
        user,
        &format!("ALTER DATABASE \"{db}\" ALLOW_CONNECTIONS false"),
        None,
    )?;
    admin(
        server,
        user,
        &format!("select pg_terminate_backend(pid) from pg_stat_activity where datname = '{db}' and pid <> pg_backend_pid()"),
        None,
    )
    .map(|_| ())
}

/// Copy database `from` to `to` on one server. `from` loses its sessions for the copy and takes connections
/// again afterwards, also on error.
pub fn clone(
    server: &str,
    user: &str,
    from: &str,
    to: &str,
    budget_ms: Option<u64>,
) -> Result<(), String> {
    let r = close(server, user, from).and_then(|()| {
        admin(
            server,
            user,
            &format!("CREATE DATABASE \"{to}\" TEMPLATE \"{from}\""),
            budget_ms,
        )
        .map(|_| ())
    });
    let allowed = admin(
        server,
        user,
        &format!("ALTER DATABASE \"{from}\" ALLOW_CONNECTIONS true"),
        None,
    );
    r.and(allowed.map(|_| ()))
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
    let old_data = svc["volumes"]
        .as_array()
        .into_iter()
        .flatten()
        .find_map(|v| {
            let target = v["target"].as_str()?;
            let source = v["source"].as_str()?;
            if !target.starts_with("/var/lib/postgresql") {
                return None;
            }
            let source = match v["type"].as_str()? {
                "volume" => config["volumes"][source]["name"].as_str().unwrap_or(source),
                "bind" => source,
                _ => return None,
            };
            Some((source.to_owned(), target.to_owned()))
        });
    Some(Shared {
        old_data,
        db: env("POSTGRES_DB")
            .or_else(|| env("POSTGRES_USER"))
            .unwrap_or("postgres")
            .to_owned(),
        server,
        project: config["name"].as_str()?.to_owned(),
        networks,
    })
}

/// Why a postgres service stays a plain compose container; None when it is not postgres or can be shared.
pub fn plain(svc: &Value) -> Option<String> {
    let image = svc["image"].as_str()?;
    let name = image
        .split('@')
        .next()?
        .split(':')
        .next()?
        .rsplit('/')
        .next()?;
    if !matches!(name, "postgres" | "pgvector" | "postgis") {
        return None;
    }
    let init = svc["volumes"].as_array().into_iter().flatten().any(|v| {
        v["target"]
            .as_str()
            .is_some_and(|t| t.starts_with("/docker-entrypoint-initdb.d"))
    });
    Some(
        if major(image).is_none() {
            "its image has no major version tag, so rizr cannot pick a server"
        } else if init {
            "it mounts /docker-entrypoint-initdb.d, and a shared server would skip those scripts"
        } else if !svc["command"].is_null() {
            "it sets `command`, and a shared server would not get those flags"
        } else if !svc["network_mode"].is_null() {
            "it sets `network_mode`, so it has no network for the service name"
        } else {
            return None;
        }
        .to_owned(),
    )
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
    let mut env: BTreeMap<String, String> = svc["environment"]
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(k, _)| k.starts_with("POSTGRES_") && *k != "POSTGRES_DB")
        .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_owned())))
        .collect();
    // The image refuses to start without a password; the repo asks for none, and the server listens only on
    // loopback and the stack's networks.
    if env.get("POSTGRES_PASSWORD").is_none_or(String::is_empty) {
        env.entry("POSTGRES_HOST_AUTH_METHOD".into())
            .or_insert_with(|| "trust".into());
    }
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

#[derive(Debug, Default)]
pub struct Resources {
    /// By node name.
    pub servers: BTreeMap<String, Server>,
    /// By compose service name.
    pub dbs: BTreeMap<String, Db>,
    /// Per compose service, the env vars that rizr sets in its override.
    pub env: BTreeMap<String, BTreeMap<String, String>>,
    pub info: Info,
}

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
    let mut plain = BTreeMap::new();
    for (svc, s) in side {
        if let Some(reason) = &s.pg_plain {
            plain.insert(svc.clone(), reason.clone());
        }
        if s.pg.is_some() && unshared.contains(svc) {
            plain.insert(
                svc.clone(),
                format!("rizr.toml sets `[resource.{svc}] shared = false`"),
            );
        }
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
            probe: Probe::HealthEvent,
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
                published: s
                    .ports
                    .iter()
                    .filter_map(|p| p["published"].as_str()?.parse().ok())
                    .collect(),
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
    let infos: BTreeMap<String, DbInfo> = dbs.iter().map(|(n, d)| (n.clone(), d.info())).collect();
    let mut env = BTreeMap::new();
    let mut rewritten = Vec::new();
    for (svc, s) in side.iter().filter(|(n, _)| !dbs.contains_key(*n)) {
        let vars = rewrite(&s.env, &infos, None);
        if vars.is_empty() {
            continue;
        }
        let id = NodeId {
            scope: Scope::Instance(instance.clone()),
            name: NodeName(svc.clone()),
        };
        // The override is part of what the container runs: a changed value recreates it.
        if let Some(n) = m.nodes.get_mut(&id) {
            n.spec = digest(&(&n.spec.0, &vars))?;
        }
        rewritten.extend(vars.iter().map(|(k, v)| Rewritten {
            service: svc.clone(),
            var: k.clone(),
            value: v.clone(),
        }));
        env.insert(svc.clone(), vars);
    }
    Ok(Resources {
        servers: servers.into_iter().map(|(n, (_, s))| (n, s)).collect(),
        dbs,
        env,
        info: Info {
            databases: infos,
            rewritten,
            plain,
        },
    })
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
        let r = add(&mut m, &side, &BTreeSet::from(["b".to_string()]), &wt).unwrap();
        assert_eq!(r.servers["pg17"].image, "postgres:17");
        assert_eq!(r.dbs["a"].database, "app_wt_2");
        assert!(!r.dbs.contains_key("b"));
        assert!(r.info.plain["b"].contains("shared = false"));
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

    #[test]
    fn rewrites_urls_and_exact_names_but_not_users_or_other_hosts() {
        let db = DbInfo {
            server: "pg17".into(),
            database: "app_wt".into(),
            base: "app".into(),
            user: "app".into(),
            password: "dev".into(),
            published: vec![15498],
        };
        let dbs = BTreeMap::from([("db".to_string(), db)]);
        let env = |pairs: &[(&str, &str)]| {
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect::<BTreeMap<_, _>>()
        };
        let compose = env(&[
            (
                "DATABASE_URL",
                "postgresql://app:dev@db:5432/app?sslmode=disable",
            ),
            ("DB_NAME", "app"),
            ("POSTGRES_USER", "app"),
            ("OTHER_URL", "postgres://app:dev@cache:5432/app"),
        ]);
        assert_eq!(
            rewrite(&compose, &dbs, None),
            env(&[
                (
                    "DATABASE_URL",
                    "postgresql://app:dev@db:5432/app_wt?sslmode=disable"
                ),
                ("DB_NAME", "app_wt"),
            ])
        );
        let host = env(&[
            ("DATABASE_URL", "postgres://app:dev@localhost:15498/app"),
            ("REMOTE", "postgres://app:dev@localhost:5999/app"),
        ]);
        assert_eq!(
            rewrite(&host, &dbs, Some(32768)),
            env(&[("DATABASE_URL", "postgres://app:dev@127.0.0.1:32768/app_wt")])
        );
    }
}
