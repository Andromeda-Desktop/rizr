use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{CommandFactory, Parser, Subcommand};
use rizr_core::error::{Error, ErrorCode};
use rizr_core::model::{Gate, NodeKind};
use rizr_core::view::{self, Drift, InstanceStatus, Receipt};
use rizr_core::{Action, Event, State, plan, reduce};
use serde_json::json;

mod compose;
mod devenv;
mod docker;
mod doctor;
mod exec;
mod gc;
mod manifest;
mod ports;
mod postgres;
mod rewind;
mod supervisor;
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
        #[arg(last = true, required = true)]
        cmd: Vec<String>,
    },
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
    let cli = Cli::parse();
    let json = cli.json;
    let files = cli.file.clone();
    match run(cli) {
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
            let s = s.map_err(|e| with_output(&stack, e, since))?;
            if up {
                for n in ports::remove_orphans(&stack.compose_name()) {
                    eprintln!(
                        "removed container {n}: no compose file here runs it under rizr (as `docker compose up --remove-orphans`); its volumes stay"
                    );
                }
            }
            if down {
                exec::remove(&stack, true);
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
            let mut urls = if up {
                ports::checks(&stack.compose_name())
            } else {
                Vec::new()
            };
            for u in &mut urls {
                let check = u["check"].as_str().unwrap_or_default();
                if check.starts_with("HTTP 5")
                    || check.contains("no HTTP reply")
                    || check.contains("refused")
                {
                    let node = u["node"].as_str().unwrap_or_default().to_owned();
                    u["log_tail"] = indent(&log_lines(&stack, &node, since)).into();
                }
            }
            let failed = urls
                .iter()
                .find(|u| u["check"].as_str().is_some_and(|c| c.starts_with("HTTP 5")))
                .map(server_error);
            if json && up {
                let mut r = serde_json::to_value(view::receipt(&s)).unwrap_or_default();
                r["urls"] = urls.into();
                if let Some(e) = &failed {
                    r["error"] = serde_json::to_value(e).unwrap_or_default();
                    println!("{r}");
                    std::process::exit(e.code.class().exit_code().into());
                }
                println!("{r}");
            } else {
                print_status(&stack, &s);
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
            failed.map_or(Ok(()), Err)
        }
        Command::Exec { node, cmd } => exec::start(&stack, &node, &cmd),
        Command::Wait { job } => exec::wait(&stack, job.as_deref()),
        Command::Snapshot { db } => rewind::snapshot(&stack, db.as_deref()),
        Command::Rewind { db, back } => rewind::rewind(&stack, db.as_deref(), back),
        Command::Reset { db } => rewind::reset(&stack, db.as_deref()),
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
            Ok(())
        }
        Command::Status => {
            let s = docker::run(&stack, i.digest, None)?;
            if json {
                println!(
                    "{}",
                    json!({"status": view::instance_status(&s), "nodes": view::nodes(&s), "drift": view::drift(&s), "findings": findings(&s)})
                );
            } else {
                print_status(&stack, &s);
            }
            Ok(())
        }
        Command::Why { node } => {
            let s = docker::run(&stack, i.digest, None)?;
            why(&stack, &s, &node, json)
        }
        Command::Context => {
            let s = docker::run(&stack, i.digest, None)?;
            let status = view::instance_status(&s);
            let mut failed: Vec<String> = view::nodes(&s)
                .into_iter()
                .filter(|n| n.state.starts_with("failed") || n.state == "unhealthy")
                .map(|n| n.node.name.0)
                .collect();
            for f in findings(&s) {
                if !failed.contains(&f.node.name.0) {
                    failed.push(f.node.name.0.clone());
                }
            }
            let mut next: Vec<String> = failed
                .iter()
                .flat_map(|n| [format!("rizr why {n}"), format!("rizr logs {n}")])
                .collect();
            let strays = ports::stray_lines(&stack.root, &stack.compose_name());
            next.extend(
                ports::strays(&stack.root, &stack.compose_name())
                    .into_iter()
                    .map(|(p, _)| format!("docker compose -p {p} down --remove-orphans")),
            );
            let unverified = view::unverified(&s);
            next.extend(unverified.iter().map(|n| format!("rizr logs {}", n.0)));
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
            println!(
                "{:#}",
                json!({
                    "databases": databases,
                    "root": stack.root, "project": stack.compose_name(), "data_dir": stack.dir,
                    "compose_files": compose::resolve(&stack.root, &stack.files), "status": status,
                    "other_compose_files": compose::others(&stack.root, &stack.files),
                    "rewind": rewind::entries(&stack.dir).iter().rev().take(10).collect::<Vec<_>>(),
                    "findings": findings(&s), "unverified": unverified, "moved_ports": ports::lines(&stack.dir), "outside_rizr": strays,
                    "generation": s.intents.generation.0, "graph": graph, "nodes": view::nodes(&s),
                    "drift": view::drift(&s), "failed": failed, "next": next, "verbs": verbs(),
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
            c.args(shared.or(node));
            let st = c.status().map_err(|e| runtime_error(&e.to_string()))?;
            if st.success() {
                Ok(())
            } else {
                Err(runtime_error("docker compose logs failed"))
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
    json!({"shared": dbs, "rewritten_env": rewritten, "plain": info.plain})
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
    let text = last.and_then(|l| l["Output"].as_str()).unwrap_or_default();
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

fn print_status(stack: &docker::Stack, s: &State) {
    print_findings(s);
    println!(
        "{}: {:?} (generation {})",
        stack.compose_name(),
        view::instance_status(s),
        s.intents.generation.0
    );
    let drift = view::drift(s);
    for n in view::nodes(s) {
        let next = drift
            .iter()
            .find(|d| d.node == n.node)
            .and_then(|d| d.next.clone())
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
                   "generated_env": generated, "problems": problems, "databases": databases(stack)})
        );
        return Ok(());
    }
    println!("{name}: {}", v.state);
    for e in &problems {
        println!("  problem: {}\n    fix: {}", e.message, e.hint);
    }
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
    ]
    .iter()
    .any(|k| m.contains(k))
    {
        e.hint = "the image does not exist or needs a login; check its name and tag".into();
        e.example = "docker login".into();
    }
    e
}
