use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{CommandFactory, Parser, Subcommand};
use rizr_core::error::{Error, ErrorCode};
use rizr_core::model::{Gate, NodeKind};
use rizr_core::view::{self, Drift, InstanceStatus, Receipt};
use rizr_core::{Action, Event, State, plan, reduce};
use serde_json::json;

mod compose;
mod detect;
mod devenv;
mod docker;
mod docs;
mod doctor;
mod exec;
mod gc;
mod manifest;
mod ports;
mod postgres;
mod redis;
mod rewind;
mod runtime;
mod subnet;
mod supervisor;
mod task;
mod trust;

/// One control plane for any project: run a repo's stack with one command and see why it works.
#[derive(Parser)]
#[command(name = "rizr", version)]
struct Cli {
    /// Print machine-readable JSON.
    #[arg(long, global = true)]
    json: bool,
    /// A compose file to use instead of the default names; repeat it as with `docker compose -f`.
    #[arg(short = 'f', long = "file", global = true)]
    file: Vec<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    #[command(flatten)]
    Action(Action),
    /// Show the steps `up` would take now, with their reasons and real commands. Runs nothing.
    Plan,
    /// Show each node's state and each difference between desired and actual.
    Status,
    /// Explain one node: its state, readiness signal, dependencies and next step.
    Why { node: String },
    /// One call for agents: graph, live state, first failure and next commands (JSON).
    Context,
    /// Print the logs of the stack or of one node.
    Logs {
        node: Option<String>,
        /// Keep printing new lines (`-f` is `--file`, as in compose).
        #[arg(long)]
        follow: bool,
        #[arg(long, default_value = "200")]
        tail: String,
    },
    /// Write plain files that run this stack without rizr: env files and an `up.sh` in dependency order.
    Eject {
        #[arg(long, default_value = "rizr-eject")]
        out: PathBuf,
    },
    /// Run a one-off command, such as a migration, in a new container of a node with its env and network. Returns
    /// within 100 s; a longer command keeps running.
    Exec {
        node: String,
        /// The command; `--` before it is optional.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        cmd: Vec<String>,
    },
    /// Run a package's tests with the command the repo uses (CI, runner, scripts, docs or rizr.toml; `plan`
    /// lists them). Prints counts, failing tests and the last lines. Returns within 100 s; a longer run keeps
    /// running and the same command again waits for it.
    Test {
        /// The package directory; default: the root package.
        package: Option<String>,
    },
    /// Build a package with the command the repo uses, as `test` does.
    Build { package: Option<String> },
    /// Run a package's database migration with the command the repo uses (alembic, manage.py, prisma, ...), as
    /// `test` does: inside the service that runs the package, after `up`.
    Migrate { package: Option<String> },
    /// Wait up to 100 s more for the last `exec` (or the named job), then print its last output and exit code.
    Wait { job: Option<String> },
    /// Take a rewind point of each shared database now (or of the named one). rizr also takes one before each
    /// migrate or seed job and keeps the newest 3.
    Snapshot { db: Option<String> },
    /// Restore each shared database (or the named one) to its newest rewind point; `--back 2` takes the one
    /// before. The current data becomes a rewind point first.
    Rewind {
        db: Option<String>,
        #[arg(long, default_value_t = 1)]
        back: usize,
    },
    /// Empty each shared database (or the named one), so the next `up` reruns migrate and seed jobs. The current
    /// data becomes a rewind point first.
    Reset { db: Option<String> },
    /// Check the host for problems that break a stack: docker, network pools, disk, stale installs, database
    /// access. Each problem names its fix. Runs no repo tool, so it works before trust.
    Doctor,
    /// List everything rizr made for this project: instances, containers, networks, volumes, units, databases,
    /// files. Runs no repo tool.
    Footprint,
    /// Release the instances of removed worktrees: their containers, networks, volumes, units, databases, port
    /// claims and data.
    Gc,
    /// Print the JSON Schema of the API: actions, errors, receipt and drift.
    Schema,
}

fn main() -> ExitCode {
    // `rizr status | head` must end quietly, as other unix tools do, not panic on the closed pipe.
    // SAFETY: runs before any thread starts; SIG_DFL is a valid handler.
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    let cli = Cli::parse();
    let json = cli.json;
    let files = cli.file.clone();
    let engine = runtime::select().map_err(|m| {
        Error::new(
            ErrorCode::RuntimeUnavailable,
            m,
            "start podman's user socket, or set RIZR_RUNTIME=docker",
            "systemctl --user enable --now podman.socket",
        )
    });
    match engine.and_then(|_| run(cli)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            let e = known_cause(e, &files);
            if json {
                println!("{}", serde_json::to_string(&e).unwrap_or_default());
            } else {
                eprintln!("error: {}", e.message);
                if let Some(c) = e.cause.as_ref().filter(|c| !c.output.is_empty()) {
                    eprintln!("output:\n{}", indent(&c.output));
                }
                eprintln!("hint: {}\ntry: {}", e.hint, e.example);
            }
            ExitCode::from(e.code.class().exit_code())
        }
    }
}

fn run(cli: Cli) -> Result<(), Error> {
    let json = cli.json;
    if let Command::Schema = cli.command {
        let schema = json!({
            "action": schemars::schema_for!(Action),
            "error": schemars::schema_for!(Error),
            "receipt": schemars::schema_for!(Receipt),
            "drift": schemars::schema_for!(Drift),
        });
        println!("{schema:#}");
        return Ok(());
    }
    if let Command::Action(Action::Trust { yes }) = cli.command {
        let i = if yes {
            trust::retrust(Path::new("."), &cli.file)?
        } else {
            trust::gate(Path::new("."), &cli.file, true)?
        };
        eprintln!("trusted {} ({} files)", i.root.display(), i.files.len());
        return Ok(());
    }
    let data = trust::store_path()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .ok_or_else(|| {
            Error::new(
                ErrorCode::Usage,
                "HOME is not set",
                "set HOME",
                "HOME=/home/me rizr up",
            )
        })?;
    if let Command::Doctor | Command::Footprint | Command::Gc = cli.command {
        let i = trust::inputs(Path::new("."), &cli.file)?;
        let stack = docker::Stack::new(i.root, cli.file.clone(), &data);
        return match cli.command {
            Command::Footprint => gc::footprint(&stack),
            Command::Gc => gc::gc(&stack),
            _ => doctor::run(&stack, json),
        };
    }
    let i = trust::gate(Path::new("."), &cli.file, !json)?;
    let stack = docker::Stack::new(i.root, cli.file.clone(), &data);
    match cli.command {
        Command::Schema
        | Command::Doctor
        | Command::Footprint
        | Command::Gc
        | Command::Action(Action::Trust { .. }) => Ok(()),
        Command::Action(action) => {
            let up = matches!(action, Action::Up { .. });
            let down = matches!(action, Action::Down);
            let _lock = stack.lock().map_err(|e| {
                Error::new(
                    ErrorCode::RuntimeUnavailable,
                    format!("cannot lock instance {}: {e}", stack.instance.0),
                    "rizr keeps a lock file in its data directory; check that it is writable",
                    format!("ls -l {}", stack.dir.display()),
                )
            })?;
            if up {
                gc::make_room(&stack);
            }
            let others = compose::others(&stack.root, &stack.files);
            if up && !others.is_empty() {
                eprintln!(
                    "note: rizr uses {}; other compose files here: {} (pick one with `rizr -f <file> up`)",
                    compose::resolve(&stack.root, &stack.files)
                        .iter()
                        .filter_map(|p| p.file_name()?.to_str())
                        .collect::<Vec<_>>()
                        .join(", "),
                    others.join(", ")
                );
            }
            let since = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
            let s = docker::run(&stack, i.digest, Some(action));
            for e in rewind::entries(&stack.dir) {
                if e.at >= since && !e.ok {
                    eprintln!("{}", rewind::warning(&e));
                }
            }
            // A failed up clears them too: an orphan can hold the port or the name that made it fail.
            if up {
                // After a good up every service runs with rizr's labels; after a failed one, keep the files' services.
                let services = if s.is_ok() {
                    Vec::new()
                } else {
                    stack
                        .compose()
                        .args(["config", "--services"])
                        .output()
                        .map(|o| {
                            String::from_utf8_lossy(&o.stdout)
                                .lines()
                                .map(str::to_owned)
                                .collect()
                        })
                        .unwrap_or_default()
                };
                for n in ports::remove_orphans(&stack.compose_name(), &services) {
                    eprintln!(
                        "removed container {n}: no compose file here runs it under rizr (as `docker compose up --remove-orphans`); its volumes stay"
                    );
                }
            }
            let s = s.map_err(|e| {
                let e = with_output(&stack, e, since);
                if up {
                    with_others(&stack, e, &others)
                } else {
                    e
                }
            })?;
            if down {
                exec::remove(&stack, true);
                // As `docker compose down`: containers a plain compose run left in this project go too.
                for n in ports::remove_orphans(&stack.compose_name(), &[]) {
                    eprintln!(
                        "removed container {n}: compose started it outside rizr in this project; its volumes stay"
                    );
                }
                // compose rm keeps networks; each holds an address pool until removed.
                let filter = format!("label=com.docker.compose.project={}", stack.compose_name());
                if let Ok(o) = std::process::Command::new("docker")
                    .args(["network", "ls", "-q", "--filter", &filter])
                    .output()
                {
                    let ids: Vec<String> = String::from_utf8_lossy(&o.stdout)
                        .split_whitespace()
                        .map(str::to_owned)
                        .collect();
                    if !ids.is_empty() {
                        let _ = std::process::Command::new("docker")
                            .args(["network", "rm"])
                            .args(&ids)
                            .output();
                    }
                }
            }
            let (urls, failed) = if up {
                url_checks(&stack, since)
            } else {
                (Vec::new(), None)
            };
            let notes = if up {
                empty_databases(&stack)
            } else {
                Vec::new()
            };
            if json && up {
                let mut r = serde_json::to_value(view::receipt(&s)).unwrap_or_default();
                r["urls"] = urls.into();
                if !notes.is_empty() {
                    r["notes"] = notes
                        .iter()
                        .map(|(n, _)| n.clone())
                        .collect::<Vec<_>>()
                        .into();
                }
                if let Some(e) = &failed {
                    r["error"] = serde_json::to_value(e).unwrap_or_default();
                    println!("{r}");
                    std::process::exit(e.code.class().exit_code().into());
                }
                println!("{r}");
            } else {
                print_status(&stack, &s, status_of(&s, failed.is_some()));
                print_urls(&urls);
                // Last, so a `| tail` keeps it.
                for (n, _) in &notes {
                    println!("note: {n}");
                }
            }
            failed.map_or(Ok(()), Err)
        }
        Command::Exec { node, cmd } => exec::start(&stack, &node, &cmd),
        Command::Wait { job } => exec::wait(&stack, job.as_deref()),
        Command::Snapshot { db } => rewind::snapshot(&stack, db.as_deref()),
        Command::Rewind { db, back } => rewind::rewind(&stack, db.as_deref(), back),
        Command::Reset { db } => rewind::reset(&stack, db.as_deref()),
        Command::Test { package } => task::run(
            &stack,
            rizr_core::code::TaskName::Test,
            package.as_deref(),
            json,
        ),
        Command::Migrate { package } => task::run(
            &stack,
            rizr_core::code::TaskName::Migrate,
            package.as_deref(),
            json,
        ),
        Command::Build { package } => task::run(
            &stack,
            rizr_core::code::TaskName::Build,
            package.as_deref(),
            json,
        ),
        Command::Plan => {
            let s = docker::run(&stack, i.digest, None)?;
            let s = reduce(
                s,
                Event::Action(Action::Up {
                    wait: Default::default(),
                }),
            );
            let steps: Vec<_> = plan(&s)
                .into_iter()
                .map(|e| json!({"reason": e.reason, "command": docker::render(&stack, &s, &e.op)}))
                .collect();
            let drift = view::drift(&s);
            if json {
                println!(
                    "{}",
                    json!({"steps": steps, "drift": drift, "findings": findings(&s)})
                );
                return Ok(());
            }
            print_findings(&s);
            println!("next steps of `rizr up` (later steps follow as nodes become ready):");
            for st in &steps {
                println!("  - {}", st["reason"].as_str().unwrap_or_default());
                if let Some(c) = st["command"].as_str() {
                    println!("    $ {c}");
                }
            }
            print_order(&s);
            print_tasks(&stack.root);
            Ok(())
        }
        Command::Status => {
            let s = docker::run(&stack, i.digest, None)?;
            let (urls, failed) = url_checks(&stack, 0);
            let status = status_of(&s, failed.is_some());
            if json {
                println!(
                    "{}",
                    json!({"status": status, "nodes": view::nodes(&s), "drift": view::drift(&s), "findings": findings(&s),
                           "urls": urls, "error": failed})
                );
            } else {
                print_status(&stack, &s, status);
                print_urls(&urls);
                if let Some(e) = &failed {
                    println!("error: {}\nhint: {}\ntry: {}", e.message, e.hint, e.example);
                }
            }
            Ok(())
        }
        Command::Why { node } => {
            let s = docker::run(&stack, i.digest, None)?;
            why(&stack, &s, &node, json)
        }
        Command::Context
            if compose::resolve(&stack.root, &stack.files).is_empty()
                && manifest::host_names(&stack.root).is_empty() =>
        {
            // No stack to run: the code graph alone, so an agent still learns how to build and test the repo.
            let packages = detect::detect(&detect::read(&stack.root));
            let next: Vec<&str> = [
                (rizr_core::code::TaskName::Test, "rizr test"),
                (rizr_core::code::TaskName::Build, "rizr build"),
            ]
            .into_iter()
            .filter(|(t, _)| packages.iter().any(|p| p.task(*t).is_some()))
            .map(|(_, c)| c)
            .collect();
            println!(
                "{}",
                json!({"root": stack.root, "status": "no_stack", "compose_files": [], "packages": packages, "next": next, "verbs": verbs()})
            );
            Ok(())
        }
        Command::Context => {
            let s = docker::run(&stack, i.digest, None)?;
            let (urls, url_error) = url_checks(&stack, 0);
            let status = status_of(&s, url_error.is_some());
            let mut failed: Vec<String> = view::nodes(&s)
                .into_iter()
                .filter(|n| n.state.starts_with("failed") || n.state == "unhealthy")
                .map(|n| n.node.name.0)
                .collect();
            let url_failed = urls
                .iter()
                .filter(|u| url_failed(u))
                .filter_map(|u| u["node"].as_str());
            for n in url_failed {
                if !failed.iter().any(|f| f == n) {
                    failed.push(n.to_owned());
                }
            }
            for f in findings(&s) {
                if !failed.contains(&f.node.name.0) {
                    failed.push(f.node.name.0.clone());
                }
            }
            let empty = empty_databases(&stack);
            let mut next: Vec<String> = empty.iter().filter_map(|(_, c)| c.clone()).collect();
            next.extend(url_error.iter().map(|e| e.example.clone()));
            next.extend(
                failed
                    .iter()
                    .flat_map(|n| [format!("rizr why {n}"), format!("rizr logs {n}")]),
            );
            let strays = ports::stray_lines(&stack.root, &stack.compose_name());
            next.extend(
                ports::strays(&stack.root, &stack.compose_name())
                    .into_iter()
                    .map(|(p, _)| format!("docker compose -p {p} down --remove-orphans")),
            );
            let unverified = view::unverified(&s);
            next.extend(unverified.iter().map(|n| format!("rizr logs {}", n.0)));
            let mut seen = std::collections::BTreeSet::new();
            next.retain(|n| seen.insert(n.clone()));
            next.push(match status {
                InstanceStatus::Up => "rizr status".into(),
                _ => "rizr up --wait ready".into(),
            });
            let graph: Vec<_> = s
                .loaded()
                .map(|m| {
                    m.topo()
                        .into_iter()
                        .filter_map(|id| {
                            let n = m.nodes.get(&id)?;
                            Some(json!({
                                "node": id.name.0,
                                "kind": if matches!(n.kind, NodeKind::Job { .. }) { "job" } else { "service" },
                                "signal": view::signal(Some(&n.kind)),
                                "deps": n.deps.iter().map(|d| json!({"node": d.node.name.0, "gate": gate(d.gate)})).collect::<Vec<_>>(),
                            }))
                        })
                        .collect()
                })
                .unwrap_or_default();
            let databases = databases(&stack);
            // One line: an agent's `| head -n 100` must not cut the graph or the next commands off.
            println!(
                "{}",
                json!({
                    "databases": databases,
                    "root": stack.root, "project": stack.compose_name(), "data_dir": stack.dir,
                    "compose_files": compose::resolve(&stack.root, &stack.files), "status": status,
                    "other_compose_files": compose::others(&stack.root, &stack.files),
                    "rewind": rewind::entries(&stack.dir).iter().rev().take(10).collect::<Vec<_>>(),
                    "findings": findings(&s), "unverified": unverified, "moved_ports": ports::lines(&stack.dir), "outside_rizr": strays,
                    "generation": s.intents.generation.0, "graph": graph, "nodes": view::nodes(&s),
                    "drift": view::drift(&s), "failed": failed, "urls": urls, "error": url_error, "next": next, "notes": empty.iter().map(|(n, _)| n.clone()).collect::<Vec<_>>(), "verbs": verbs(),
                    "packages": detect::detect(&detect::read(&stack.root)),
                })
            );
            Ok(())
        }
        Command::Logs {
            node: Some(node),
            follow,
            tail,
        } if manifest::host_names(&stack.root).contains(&node) => {
            let mut c = std::process::Command::new("journalctl");
            c.args(["--user", "-o", "cat", "--no-pager", "-n", &tail, "-u"])
                .arg(supervisor::Units::of(&stack).unit(&node));
            if follow {
                c.arg("--follow");
            }
            let st = c.status().map_err(|e| runtime_error(&e.to_string()))?;
            if st.success() {
                Ok(())
            } else {
                Err(runtime_error("journalctl --user failed"))
            }
        }
        Command::Logs { node, follow, tail } => {
            let shared = node
                .as_deref()
                .and_then(|n| docker::shared_container(&stack, n));
            let mut c = match &shared {
                Some(_) => std::process::Command::new("docker"),
                None => stack.compose(),
            };
            c.args(["logs", "--tail", &tail]);
            if shared.is_none() {
                c.arg("--no-color");
            }
            if follow {
                c.arg("--follow");
            }
            c.args(shared.or(node.clone()));
            let st = c.status().map_err(|e| runtime_error(&e.to_string()))?;
            if st.success() {
                return Ok(());
            }
            let services: Vec<String> = stack
                .compose()
                .args(["config", "--services"])
                .output()
                .map(|o| {
                    String::from_utf8_lossy(&o.stdout)
                        .lines()
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default();
            match node.filter(|n| !services.is_empty() && !services.contains(n)) {
                Some(n) => Err(Error::new(
                    ErrorCode::Usage,
                    format!("no service {n} in the compose files rizr uses"),
                    format!("its services: {}", services.join(", ")),
                    "rizr status",
                )),
                None => Err(runtime_error("docker compose logs failed")),
            }
        }
        Command::Eject { out } => {
            let s = docker::run(&stack, i.digest, None)?;
            eject(&stack, &s, &out)
        }
    }
}

/// Where a node runs: a compose service, or a systemd user unit for a host process from `rizr.toml`.
/// The shared databases of this instance: URLs, the psql command, and every env var rizr rewrote.
fn databases(stack: &docker::Stack) -> serde_json::Value {
    let info = postgres::Info::load(&stack.dir);
    let host = manifest::host_services(&stack.root);
    let mut rewritten: Vec<_> = info
        .rewritten
        .iter()
        .map(|r| json!({"service": r.service, "var": r.var, "value": r.value}))
        .collect();
    let dbs: Vec<_> = info
        .databases
        .iter()
        .map(|(svc, db)| {
            let container = format!("rizr-{}-{}", stack.project.0, db.server);
            let port = postgres::host_port(&container);
            if let Some(p) = port {
                let one = std::collections::BTreeMap::from([(svc.clone(), db.clone())]);
                for (h, s) in &host {
                    for (k, v) in postgres::rewrite(&s.env, &one, Some(p)) {
                        rewritten.push(json!({"service": h, "var": k, "value": v}));
                    }
                }
            }
            json!({
                "service": svc, "database": db.database, "server": container,
                "url": db.url(&format!("{svc}:5432")),
                "host_url": port.map(|p| db.url(&format!("127.0.0.1:{p}"))),
                "psql": format!("docker exec -it {container} psql -U {} -d {}", db.user, db.database),
            })
        })
        .collect();
    json!({"shared": dbs, "rewritten_env": rewritten, "plain": info.plain, "redis": redis::context(&stack.compose_name())})
}

fn runtime_line(stack: &docker::Stack, name: &str) -> String {
    let info = postgres::Info::load(&stack.dir);
    if let Some(db) = info.databases.get(name) {
        return format!(
            "database `{}` on the shared server rizr-{}-{}, which answers to `{name}` on this instance's networks",
            db.database, stack.project.0, db.server
        );
    }
    if let Some(reason) = info.plain.get(name) {
        return format!(
            "compose service `{name}` in project {}; not on a shared server: {reason}",
            stack.compose_name()
        );
    }
    if manifest::host_names(&stack.root).contains(name) {
        format!(
            "systemd user unit `{}` (host process from rizr.toml)",
            supervisor::Units::of(stack).unit(name)
        )
    } else {
        format!(
            "compose service `{name}` in project {}",
            stack.compose_name()
        )
    }
}

/// A node that answers HTTP 5xx; a missing table in its log means the schema was never migrated.
fn server_error(u: &serde_json::Value) -> Error {
    let node = u["node"].as_str().unwrap_or_default();
    let message = format!(
        "{node} {} -> {}",
        u["url"].as_str().unwrap_or_default(),
        u["check"].as_str().unwrap_or_default()
    );
    let log = u["log_tail"]
        .as_str()
        .unwrap_or_default()
        .to_ascii_lowercase();
    let unmigrated = [
        "does not exist",
        "no such table",
        "doesn't exist",
        "undefinedtable",
        "unapplied migration",
        "pending migration",
    ]
    .iter()
    .any(|p| log.contains(p));
    if !unmigrated {
        return Error::new(
            ErrorCode::Unhealthy,
            message,
            format!("{node} runs but answers with a server error; its log shows why"),
            format!("rizr logs {node}"),
        );
    }
    let migrate = if log.contains("django") {
        "python3 manage.py migrate"
    } else if log.contains("activerecord") {
        "bin/rails db:migrate"
    } else if log.contains("sqlalchemy") || log.contains("alembic") {
        "alembic upgrade head"
    } else {
        "<the migrate command from the README>"
    };
    Error::new(
        ErrorCode::Unhealthy,
        message,
        format!(
            "{node} runs, but its database has no tables: run the app's migration once, then `rizr up` again"
        ),
        format!("rizr exec {node} -- {migrate}"),
    )
}

/// Databases of this instance with no tables yet, as a note line and the migrate command the repo's docs give.
fn empty_databases(stack: &docker::Stack) -> Vec<(String, Option<String>)> {
    let empty = postgres::empty(&stack.dir, &stack.project.0);
    if empty.is_empty() {
        return Vec::new();
    }
    let cmd = docs::migrate(&stack.root).or_else(|| {
        detect::detect(&detect::read(&stack.root))
            .iter()
            .any(|p| p.task(rizr_core::code::TaskName::Migrate).is_some())
            .then(|| "rizr migrate".to_owned())
    });
    empty
        .into_iter()
        .map(|(svc, db)| {
            let line = match &cmd {
                Some(c) => format!(
                    "database {db} ({svc}) has no tables: the app's migration has not run; run it before anything that reads the database: `{c}`"
                ),
                None => format!(
                    "database {db} ({svc}) has no tables: the app's migration has not run; run it (see the repo's README) before anything that reads the database, with `rizr exec <service> <command>`"
                ),
            };
            (line, cmd.clone())
        })
        .collect()
}

/// A failed `up` beside other compose files may have run the wrong one: the hint names them, since a `| tail`
/// cuts the note printed before up's output. When one of them defines services the used files lack (a
/// self-contained `pwd.yml` with its own database), `rizr -f <file> up` becomes the command to try first.
fn with_others(stack: &docker::Stack, mut e: Error, others: &[String]) -> Error {
    if others.is_empty()
        || !matches!(
            e.code,
            ErrorCode::StartFailed
                | ErrorCode::StillRunning
                | ErrorCode::StartTimeout
                | ErrorCode::Unhealthy
                | ErrorCode::JobFailed
        )
    {
        return e;
    }
    let fuller = others.iter().find_map(|o| {
        let extra = compose::extra_services(&stack.root, &stack.files, o);
        (!extra.is_empty()).then_some((o, extra))
    });
    match fuller {
        Some((file, extra)) => {
            e.hint = format!(
                "{}; or this file is not the stack to run: {file} beside it also defines {}, which it lacks; else `{}`",
                e.hint,
                extra.join(", "),
                e.example
            );
            e.example = format!("rizr -f {file} up");
        }
        None => {
            e.hint = format!(
                "{}; other compose files here: {} (`rizr -f <file> up` runs one)",
                e.hint,
                others.join(", ")
            );
        }
    }
    e
}

/// Adds runtime evidence to a node failure: the last healthcheck output for an unhealthy node, else its log lines.
fn with_output(stack: &docker::Stack, mut e: Error, since: u64) -> Error {
    let Some(cause) = e.cause.as_deref_mut() else {
        return e;
    };
    if !matches!(
        e.code,
        ErrorCode::StartFailed
            | ErrorCode::StartTimeout
            | ErrorCode::Unhealthy
            | ErrorCode::JobFailed
    ) {
        return e;
    }
    if e.code == ErrorCode::Unhealthy {
        cause.output = health_output(stack, &cause.node);
    }
    if cause.output.trim().is_empty() {
        cause.output = log_lines(stack, &cause.node, since);
    }
    e
}

/// The output of the last healthcheck run of a compose node, at most 15 lines.
fn health_output(stack: &docker::Stack, node: &str) -> String {
    let out = |mut c: std::process::Command| {
        c.output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
            .unwrap_or_default()
    };
    let mut ps = stack.compose();
    ps.args(["ps", "-aq", node]);
    let Some(id) = out(ps).lines().next().map(str::to_owned) else {
        return String::new();
    };
    let mut c = std::process::Command::new("docker");
    c.args(["inspect", "--format", "{{json .State.Health.Log}}", &id]);
    let log: serde_json::Value = serde_json::from_str(&out(c)).unwrap_or_default();
    let last = log.as_array().and_then(|a| a.last());
    let mut text = last
        .and_then(|l| l["Output"].as_str())
        .unwrap_or_default()
        .to_owned();
    // Podman keeps no output for a check whose program is missing: run the check once more to see why it fails.
    if text.trim().is_empty() && last.is_some_and(|l| l["ExitCode"] != 0) {
        let mut t = std::process::Command::new("docker");
        t.args([
            "inspect",
            "--format",
            "{{json .Config.Healthcheck.Test}}",
            &id,
        ]);
        let test: Vec<String> = serde_json::from_str(&out(t)).unwrap_or_default();
        let argv: Vec<String> = match test.first().map(String::as_str) {
            Some("CMD") => test[1..].to_vec(),
            Some("CMD-SHELL") => ["sh", "-c"]
                .into_iter()
                .map(str::to_owned)
                .chain(test[1..].iter().cloned())
                .collect(),
            _ => Vec::new(),
        };
        if !argv.is_empty() {
            text = std::process::Command::new("timeout")
                .args(["10", "docker", "exec", &id])
                .args(&argv)
                .output()
                .map(|o| {
                    let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
                    s.push_str(&String::from_utf8_lossy(&o.stderr));
                    s
                })
                .unwrap_or_default();
        }
    }
    let lines: Vec<&str> = text.trim_end().lines().collect();
    let code = last.map_or(String::new(), |l| {
        format!("healthcheck exit {}\n", l["ExitCode"])
    });
    format!(
        "{code}{}",
        lines[lines.len().saturating_sub(15)..].join("\n")
    )
    .trim_end()
    .to_owned()
}

/// The log lines of `node` since `since` (unix seconds), else its last lines; at most 15, colour codes and repeated
/// lines removed.
/// A host node reads the journal of its unit; a shared node its container's logs; a compose node compose logs.
fn log_lines(stack: &docker::Stack, node: &str, since: u64) -> String {
    let host = manifest::host_names(&stack.root).contains(node);
    let logs = |since: Option<u64>| {
        let mut c = if host {
            let mut c = std::process::Command::new("journalctl");
            c.args(["--user", "-o", "cat", "--no-pager", "-n", "15", "-u"])
                .arg(supervisor::Units::of(stack).unit(node));
            if let Some(t) = since {
                c.arg(format!("--since=@{t}"));
            }
            c
        } else if let Some(id) = docker::shared_container(stack, node) {
            let mut c = std::process::Command::new("docker");
            c.args(["logs", "--tail", "15"]);
            if let Some(t) = since {
                c.args(["--since", &t.to_string()]);
            }
            c.arg(id);
            c
        } else {
            let mut c = stack.compose();
            c.args(["logs", "--no-color", "--no-log-prefix", "--tail", "15"]);
            if let Some(t) = since {
                c.args(["--since", &t.to_string()]);
            }
            c.arg(node);
            c
        };
        c.output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default()
    };
    let mut text = logs(Some(since));
    if text.trim().is_empty() {
        text = logs(None);
    }
    let mut out = String::new();
    let mut esc = false;
    for ch in text.chars() {
        match (esc, ch) {
            (false, '\u{1b}') => esc = true,
            (true, c) if c.is_ascii_alphabetic() => esc = false,
            (true, _) => {}
            (false, c) => out.push(c),
        }
    }
    let mut seen = std::collections::HashSet::new();
    let lines: Vec<&str> = out.lines().filter(|l| seen.insert(*l)).collect();
    let repeated = out.lines().count() - lines.len();
    let mut text = lines.join("\n").trim_end().to_owned();
    if repeated > 0 {
        text.push_str(&format!(
            "\n({repeated} repeated lines left out; a restart loop repeats them)"
        ));
    }
    text
}

fn indent(text: &str) -> String {
    text.lines()
        .map(|l| format!("      {l}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Each verb with its one-line help, so an agent needs no `--help` call.
fn verbs() -> serde_json::Map<String, serde_json::Value> {
    Cli::command()
        .get_subcommands()
        .map(|c| {
            let about = c.get_about().map(|a| a.to_string()).unwrap_or_default();
            let args = c
                .get_arguments()
                .filter(|a| !a.is_global_set() && a.get_id() != "help");
            let usage: String = args
                .map(|a| {
                    let pv: Vec<String> = a
                        .get_possible_values()
                        .iter()
                        .map(|v| v.get_name().to_owned())
                        .collect();
                    let val = if pv.is_empty() {
                        a.get_value_names()
                            .map_or_else(|| a.get_id().to_string().to_uppercase(), |v| v.join(" "))
                    } else {
                        pv.join("|")
                    };
                    let one = match a.get_long() {
                        None => format!("<{val}>"),
                        Some(l) if a.get_action().takes_values() => format!("--{l} <{val}>"),
                        Some(l) => format!("--{l}"),
                    };
                    if a.is_required_set() {
                        format!(" {one}")
                    } else {
                        format!(" [{one}]")
                    }
                })
                .collect();
            (format!("rizr {}{usage}", c.get_name()), about.into())
        })
        .collect()
}

/// Defects of the files that no start fixes (`Model::findings`).
fn findings(s: &State) -> &[rizr_core::model::Finding] {
    s.loaded().map_or(&[], |m| &m.findings)
}

fn print_findings(s: &State) {
    for f in findings(s) {
        println!(
            "problem: {}
  fix: {}",
            f.error.message, f.error.hint
        );
    }
}

const OUTDATED: &str = "its compose definition changed since it started; `rizr up` recreates it";

/// `Up` from the containers alone, `Degraded` when a published URL answers 5xx.
fn status_of(s: &State, url_failed: bool) -> InstanceStatus {
    match view::instance_status(s) {
        InstanceStatus::Up if url_failed => InstanceStatus::Degraded,
        st => st,
    }
}

fn url_failed(u: &serde_json::Value) -> bool {
    u["check"].as_str().is_some_and(|c| c.starts_with("HTTP 5"))
}

/// One GET per published port; a failing URL carries its node's log tail since `since` (unix seconds, 0 for all).
fn url_checks(stack: &docker::Stack, since: u64) -> (Vec<serde_json::Value>, Option<Error>) {
    let mut urls = ports::checks(&stack.compose_name());
    for u in &mut urls {
        let check = u["check"].as_str().unwrap_or_default();
        if check.starts_with("HTTP 5")
            || check.contains("no HTTP reply")
            || check.contains("refused")
        {
            let node = u["node"].as_str().unwrap_or_default().to_owned();
            u["log_tail"] = indent(&log_lines(stack, &node, since)).into();
        }
    }
    let failed = urls.iter().find(|u| url_failed(u)).map(server_error);
    (urls, failed)
}

fn print_urls(urls: &[serde_json::Value]) {
    for u in urls {
        println!(
            "  {} {} -> {}",
            u["node"].as_str().unwrap_or_default(),
            u["url"].as_str().unwrap_or_default(),
            u["check"].as_str().unwrap_or_default()
        );
        if let Some(t) = u["log_tail"].as_str() {
            println!("    last log lines:\n{t}");
        }
    }
}

fn print_status(stack: &docker::Stack, s: &State, status: InstanceStatus) {
    print_findings(s);
    println!(
        "{}: {:?} (generation {})",
        stack.compose_name(),
        status,
        s.intents.generation.0
    );
    let drift = view::drift(s);
    for n in view::nodes(s) {
        let next = drift
            .iter()
            .find(|d| d.node == n.node)
            .and_then(|d| d.next.clone())
            .or_else(|| (n.state == "outdated").then(|| OUTDATED.into()))
            .map(|x| format!("  next: {x}"))
            .unwrap_or_default();
        println!("  {:<20} {:<16} {}{next}", n.node.name.0, n.state, n.signal);
    }
    for l in ports::lines(&stack.dir)
        .into_iter()
        .chain(ports::stray_lines(&stack.root, &stack.compose_name()))
    {
        println!("{l}");
    }
    let unverified: Vec<String> = view::unverified(s).into_iter().map(|n| n.0).collect();
    if !unverified.is_empty() {
        println!(
            "unverified: {} started, but no healthcheck proves they serve requests; read each URL check below, or `rizr logs <node>`",
            unverified.join(", ")
        );
    }
}

/// The task each package runs, with its source, for `plan`.
fn print_tasks(root: &Path) {
    let packages = detect::detect(&detect::read(root));
    if packages.is_empty() {
        return;
    }
    println!("tasks:");
    for p in &packages {
        for (name, list) in &p.tasks {
            let Some(t) = list.first() else { continue };
            let name = serde_json::to_value(name).unwrap_or_default();
            let name = name.as_str().unwrap_or_default();
            println!(
                "  {} {name}: {} ({:?} from {})",
                p.dir, t.cmd, t.source, t.from
            );
        }
    }
}

fn print_order(s: &State) {
    let Some(m) = s.loaded() else { return };
    println!("order:");
    for id in m.topo() {
        let Some(n) = m.nodes.get(&id) else { continue };
        let deps: Vec<String> = n
            .deps
            .iter()
            .map(|d| format!("{} {}", d.node.name.0, gate(d.gate)))
            .collect();
        let kind = if matches!(n.kind, NodeKind::Job { .. }) {
            "job"
        } else {
            "service"
        };
        let after = if deps.is_empty() {
            String::new()
        } else {
            format!(" after {}", deps.join(", "))
        };
        println!(
            "  {} ({kind}, {}){after}",
            id.name.0,
            view::signal(Some(&n.kind))
        );
    }
}

fn gate(g: Gate) -> &'static str {
    match g {
        Gate::Started => "started",
        Gate::Ready => "ready",
        Gate::Completed => "completed",
    }
}

fn why(stack: &docker::Stack, s: &State, name: &str, json: bool) -> Result<(), Error> {
    let nodes = view::nodes(s);
    let Some(v) = nodes.iter().find(|n| n.node.name.0 == name) else {
        let names: Vec<_> = nodes.iter().map(|n| n.node.name.0.as_str()).collect();
        return Err(Error::new(
            ErrorCode::Usage,
            format!("no node `{name}`; nodes: {}", names.join(", ")),
            "name a service or job of the compose file",
            format!("rizr why {}", names.first().unwrap_or(&"<node>")),
        ));
    };
    let state_of = |n: &str| {
        nodes
            .iter()
            .find(|x| x.node.name.0 == n)
            .map_or("unknown".into(), |x| x.state.clone())
    };
    let spec = s.loaded().and_then(|m| m.nodes.get(&v.node));
    let deps: Vec<_> = spec
        .map(|sp| {
            sp.deps
                .iter()
                .map(|d| json!({"node": d.node.name.0, "gate": gate(d.gate), "state": state_of(&d.node.name.0)}))
                .collect()
        })
        .unwrap_or_default();
    let needed_by: Vec<String> = s
        .loaded()
        .map(|m| {
            m.nodes
                .iter()
                .filter(|(_, n)| n.deps.iter().any(|d| d.node == v.node))
                .map(|(id, _)| id.name.0.clone())
                .collect()
        })
        .unwrap_or_default();
    let next = view::drift(s)
        .into_iter()
        .find(|d| d.node == v.node)
        .and_then(|d| d.next);
    let kind = match spec.map(|sp| &sp.kind) {
        Some(NodeKind::Job { .. }) => "job: a service that others wait on to exit 0",
        Some(NodeKind::Service { .. }) => "service",
        None => "removed from the compose file",
    };
    let problems: Vec<_> = findings(s)
        .iter()
        .filter(|f| f.node == v.node)
        .map(|f| &f.error)
        .collect();
    let (urls, url_error) = url_checks(stack, 0);
    let urls: Vec<_> = urls.into_iter().filter(|u| u["node"] == name).collect();
    let url_error = url_error.filter(|_| urls.iter().any(url_failed));
    // An unexplained node: its recent log is the evidence, never "nothing".
    let next = next
        .or_else(|| url_error.as_ref().map(|e| e.example.clone()))
        .or_else(|| (v.state == "outdated").then(|| OUTDATED.into()))
        .or_else(|| (!v.state.starts_with("ready")).then(|| format!("rizr logs {name}")));
    let recent = if (v.state.starts_with("ready") && url_error.is_none())
        || urls.iter().any(|u| u["log_tail"].is_string())
    {
        String::new()
    } else {
        log_lines(stack, name, 0)
    };
    let generated = std::fs::read_to_string(stack.dir.join("dev.env")).unwrap_or_default();
    let generated: Vec<&str> = generated
        .lines()
        .filter_map(|l| l.split('=').next())
        .collect();
    if json {
        println!(
            "{}",
            json!({"node": name, "state": v.state, "kind": kind, "signal": v.signal, "deps": deps,
                   "needed_by": needed_by, "next": next, "runtime": runtime_line(stack, name),
                   "generated_env": generated, "problems": problems, "databases": databases(stack),
                   "urls": urls, "error": url_error, "recent_log": recent})
        );
        return Ok(());
    }
    println!("{name}: {}", v.state);
    for e in problems.iter().copied().chain(url_error.as_ref()) {
        println!("  problem: {}\n    fix: {}", e.message, e.hint);
    }
    print_urls(&urls);
    println!("  kind: {kind}");
    println!("  ready when: {}", v.signal);
    for d in &deps {
        println!(
            "  waits for: {} to be {} (now {})",
            d["node"].as_str().unwrap_or_default(),
            d["gate"].as_str().unwrap_or_default(),
            d["state"].as_str().unwrap_or_default()
        );
    }
    if !needed_by.is_empty() {
        println!("  needed by: {}", needed_by.join(", "));
    }
    println!("  runtime: {}", runtime_line(stack, name));
    let dbs = databases(stack);
    if let Some(db) = dbs["shared"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|d| d["service"] == name)
    {
        let s = |k: &str| db[k].as_str().unwrap_or("(server not running)").to_owned();
        println!("  from compose services: {}", s("url"));
        println!("  from host processes: {}", s("host_url"));
        println!("  psql: {}", s("psql"));
        for r in dbs["rewritten_env"].as_array().into_iter().flatten() {
            println!(
                "  env set by rizr: {}: {}={}",
                r["service"].as_str().unwrap_or_default(),
                r["var"].as_str().unwrap_or_default(),
                r["value"].as_str().unwrap_or_default()
            );
        }
    }
    if !generated.is_empty() {
        println!(
            "  env generated by rizr (all services): {} in {}",
            generated.join(", "),
            stack.dir.join("dev.env").display()
        );
    }
    if !recent.is_empty() {
        println!("  recent log:\n{}", indent(&recent));
    }
    println!("  next: {}", next.as_deref().unwrap_or("nothing"));
    Ok(())
}

fn eject(stack: &docker::Stack, s: &State, out: &Path) -> Result<(), Error> {
    let io = |e: std::io::Error| {
        Error::new(
            ErrorCode::Usage,
            format!("cannot write {}: {e}", out.display()),
            "choose another directory",
            "rizr eject --out /tmp/stack",
        )
    };
    let m = s
        .loaded()
        .ok_or_else(|| runtime_error("the compose model did not load"))?;
    std::fs::create_dir_all(out).map_err(io)?;
    let out = out.canonicalize().map_err(io)?;
    let mut args: Vec<String> = Vec::new();
    for f in compose::resolve(&stack.root, &stack.files) {
        args.push(format!("-f '{}'", f.display()));
    }
    let a = devenv::args(&stack.root, &stack.dir);
    for pair in a.chunks(2) {
        let [flag, file] = pair else { continue };
        let src = PathBuf::from(file);
        let dst = out.join(src.file_name().unwrap_or_default());
        if src != dst && src.starts_with(&stack.dir) {
            std::fs::copy(&src, &dst).map_err(io)?;
            args.push(format!(
                "{} \"$here/{}\"",
                flag.to_string_lossy(),
                dst.file_name().unwrap_or_default().to_string_lossy()
            ));
        } else {
            args.push(format!("{} '{}'", flag.to_string_lossy(), src.display()));
        }
    }
    // Moved host ports (ports.rs) stay moved without rizr.
    for svc in ports::load(&stack.dir).keys() {
        let name = format!("ports-{svc}.yml");
        std::fs::copy(stack.dir.join(&name), out.join(&name)).map_err(io)?;
        args.push(format!("-f \"$here/{name}\""));
    }
    let mut sh = format!(
        "#!/bin/sh\n# Written by `rizr eject`: starts the stack in dependency order without rizr.\nset -eu\nhere=$(cd \"$(dirname \"$0\")\" && pwd)\nc() {{ docker compose -p {} --project-directory '{}' {} \"$@\"; }}\n",
        stack.compose_name(),
        stack.root.display(),
        args.join(" ")
    );
    for id in m.topo() {
        let Some(n) = m.nodes.get(&id) else { continue };
        let svc = &id.name.0;
        sh.push_str(&match n.kind {
            NodeKind::Job { .. } => format!("c up -d --no-deps {svc}\ntest \"$(docker wait \"$(c ps -aq {svc})\")\" = 0 || {{ echo 'job {svc} failed' >&2; exit 1; }}\n"),
            NodeKind::Service { .. } => format!("c up -d --no-deps --wait {svc}\n"),
        });
    }
    let script = out.join("up.sh");
    std::fs::write(&script, sh).map_err(io)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).map_err(io)?;
    }
    println!("wrote {}", script.display());
    Ok(())
}

fn runtime_error(msg: &str) -> Error {
    Error::new(
        ErrorCode::RuntimeUnavailable,
        msg,
        "check that docker runs",
        "docker info",
    )
}

/// The failure catalog (VISION section 9): a known cause gets its own hint and next command.
fn known_cause(mut e: Error, files: &[PathBuf]) -> Error {
    if e.code == ErrorCode::RuntimeUnavailable
        && e.example == "systemctl start docker"
        && !e.message.to_ascii_lowercase().contains("permission denied")
        && runtime::current() == runtime::Engine::Podman
    {
        e.hint = "start podman's API socket; rizr drives podman through it".into();
        e.example = "systemctl --user enable --now podman.socket".into();
        return e;
    }
    let m = e.message.to_ascii_lowercase();
    if m.contains("port is already allocated") || m.contains("address already in use") {
        let port = e.message.rsplit(':').find_map(|p| {
            p.split(|c: char| !c.is_ascii_digit())
                .next()?
                .parse::<u16>()
                .ok()
        });
        // rizr moves a port that another program holds (ports.rs); a copy of this repo outside rizr it does not.
        let strays = Path::new(".")
            .canonicalize()
            .map(|r| ports::strays(&r, &compose::default_name(&r, files)))
            .unwrap_or_default();
        if let Some((p, n)) = strays.first() {
            e.hint = format!(
                "this repo also runs outside rizr as compose project `{p}` ({n} containers) and holds the port; stop it, volumes stay"
            );
            e.example = format!("docker compose -p {p} down --remove-orphans");
        } else {
            e.hint =
                "another process or stack holds the port; stop it, or change the published port"
                    .into();
            e.example = port.map_or("ss -ltnp".into(), |p| format!("ss -ltnp 'sport = :{p}'"));
        }
    } else if [
        "pull access denied",
        "manifest unknown",
        "failed to resolve reference",
        "not found: manifest",
        "requested access to the resource is denied",
        "name unknown",
    ]
    .iter()
    .any(|k| m.contains(k))
    {
        e.hint = "the image does not exist or needs a login; check its name and tag".into();
        e.example = "docker login".into();
    } else if m.contains("fully subnetted") || m.contains("non-overlapping ipv4 address pool") {
        e.hint = "docker has no free address pool for another network: each compose project takes one or more; remove unused networks, or give docker more pools (\"default-address-pools\" in /etc/docker/daemon.json, see `rizr doctor`)".into();
        e.example = "docker network prune".into();
    } else if let Some((a, b)) = e
        .message
        .split_once("service \"")
        .and_then(|(_, r)| r.split_once("\" depends on undefined service \""))
        .and_then(|(a, r)| Some((a.to_owned(), r.split_once('"')?.0.to_owned())))
    {
        e.hint = format!(
            "{a} depends on {b}, which no compose file in use defines; add {b}, remove it from {a}.depends_on, or pick the compose file that has it with `rizr -f`"
        );
        e.example = "docker compose config --services".into();
    } else if let Some(cmd) = e
        .message
        .split("exec: \"")
        .nth(1)
        .and_then(|r| r.split('"').next())
        .filter(|_| m.contains("executable file not found") && e.code != ErrorCode::Unhealthy)
        .map(str::to_owned)
    {
        let node = e.cause.as_ref().map_or("", |c| c.node.as_str()).to_owned();
        e.hint = format!(
            "{node} starts with {cmd}, which its image does not have; fix `command`/`entrypoint` in the compose file, or the image tag"
        );
        e.example = format!(
            "docker compose run --rm --entrypoint sh {node} -c 'command -v {cmd}; echo $PATH'"
        );
    } else if m.contains("failed to solve") {
        let node = e.cause.as_ref().map_or("", |c| c.node.as_str()).to_owned();
        e.hint = format!(
            "the image build of {node} failed at the Dockerfile step above; fix that step (or the files it copies), then run up again"
        );
        e.example = format!("docker compose build {node}");
    } else if let Some(file) = e
        .message
        .split("env file ")
        .nth(1)
        .and_then(|r| r.split(" not found").next())
        .filter(|_| m.contains(" not found"))
        .map(str::to_owned)
    {
        e.hint = format!(
            "the compose file needs {file}, and the repo has no example of it to copy; create it with the variables the service reads (see the README)"
        );
        e.example = format!("touch {file}");
    } else if m.contains("permission denied") && m.contains("docker") && m.contains("sock") {
        if runtime::current() == runtime::Engine::Podman {
            e.hint = "this user may not use the socket DOCKER_HOST names; rootless podman serves each user their own: unset DOCKER_HOST and rizr finds it".into();
            e.example = "unset DOCKER_HOST".into();
        } else {
            e.hint = "docker runs, but this user may not use its socket; add the user to the docker group, then log in again".into();
            e.example = "sudo usermod -aG docker $USER".into();
        }
    } else if let Some(name) = e
        .message
        // docker: `name "/x" is already in use by container "id"`; podman: `name "x" is already in use by id`.
        .split("is already in use by")
        .next()
        .filter(|_| m.contains("is already in use by") && m.contains("container name"))
        .and_then(|h| h.rsplit('"').nth(1))
    {
        let name = name.trim_start_matches('/').to_owned();
        e.hint = format!(
            "a container outside this stack holds the name `{name}` (container_name in the compose file); remove it, its volumes stay"
        );
        e.example = format!("docker rm -f {name}");
    } else if let Some(c) = e.cause.as_ref().filter(|c| {
        let o = c.output.to_ascii_lowercase();
        [
            "access denied for user",
            "password authentication failed",
            "authentication failed",
        ]
        .iter()
        .any(|k| o.contains(k))
    }) {
        let p = Path::new(".")
            .canonicalize()
            .map(|r| compose::default_name(&r, files))
            .unwrap_or_default();
        e.hint = format!(
            "{} cannot log in to its database: a database volume keeps the user and password it was first made with, and a changed password in the compose or env files does not reach it; put the old password back, or delete the volumes (their data goes) and start again",
            c.node
        );
        e.example = format!(
            "rizr down && docker volume rm $(docker volume ls -q --filter label=com.docker.compose.project={p}) && rizr up"
        );
    } else if let Some(c) = e.cause.as_ref().filter(|c| {
        let o = c.output.to_ascii_lowercase();
        o.contains("executable file not found") || o.contains(": not found")
    }) && e.code == ErrorCode::Unhealthy
    {
        let tool = c
            .output
            .split("exec: \"")
            .nth(1)
            .and_then(|r| r.split('"').next())
            .unwrap_or("its command")
            .to_owned();
        e.hint = format!(
            "the healthcheck of {} runs {tool}, which the image does not have; change the healthcheck to a tool the image has (wget, nc, the app's own binary) or install {tool} in the image",
            c.node
        );
        e.example = format!(
            "docker compose exec {} sh -c 'command -v curl wget nc'",
            c.node
        );
    } else if let Some(c) = e.cause.as_ref().filter(|c| {
        c.output
            .to_ascii_lowercase()
            .contains("no space left on device")
    }) {
        e.hint = format!(
            "{} ran out of disk space: the docker disk, a volume or a tmpfs mount is full; free space or raise the tmpfs size",
            c.node
        );
        e.example = "docker system df".into();
    } else if let Some(c) = e.cause.as_ref().filter(|_| m.ends_with("exit 137")) {
        let p = Path::new(".")
            .canonicalize()
            .map(|r| compose::default_name(&r, files))
            .unwrap_or_default();
        let ctr = format!("{p}-{}-1", c.node);
        let oom = std::process::Command::new("docker")
            .args(["inspect", "-f", "{{.State.OOMKilled}}", &ctr])
            .output()
            .is_ok_and(|o| o.stdout.starts_with(b"true"));
        e.hint = if oom {
            format!(
                "{} ran out of memory and the kernel killed it; raise its mem_limit (or deploy.resources.limits.memory), or make it use less",
                c.node
            )
        } else {
            format!(
                "{} got SIGKILL (exit 137) from outside: kill -9, docker kill, or the host ran out of memory",
                c.node
            )
        };
        e.example = format!("docker inspect -f '{{{{.State.OOMKilled}}}}' {ctr}");
    }
    e
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Exhausting docker's address pools on a dev machine breaks its other stacks, so this cause has no live case.
    #[test]
    fn no_free_address_pool_names_the_network_fix() {
        let e = Error::new(
            ErrorCode::StartFailed,
            "a failed: start error: Error response from daemon: all predefined address pools have been fully subnetted",
            "read its logs",
            "rizr logs a",
        );
        let e = known_cause(e, &[]);
        assert!(e.hint.contains("address pool"), "{}", e.hint);
        assert_eq!(e.example, "docker network prune");
    }
}
