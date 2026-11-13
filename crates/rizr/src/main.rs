use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use rizr_core::error::{Error, ErrorCode};
use rizr_core::model::{Gate, NodeKind};
use rizr_core::view::{self, Drift, InstanceStatus, Receipt};
use rizr_core::{Action, Event, State, plan, reduce};
use serde_json::json;

mod compose;
mod devenv;
mod docker;
mod ports;
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
        #[arg(long, short)]
        follow: bool,
        #[arg(long, default_value = "200")]
        tail: String,
    },
    /// Write plain files that run this stack without rizr: env files and an `up.sh` in dependency order.
    Eject {
        #[arg(long, default_value = "rizr-eject")]
        out: PathBuf,
    },
    /// Print the JSON Schema of the API: actions, errors, receipt and drift.
    Schema,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let json = cli.json;
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            let e = known_cause(e);
            if json {
                println!("{}", serde_json::to_string(&e).unwrap_or_default());
            } else {
                eprintln!("error: {}\nhint: {}\ntry: {}", e.message, e.hint, e.example);
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
    let i = trust::gate(Path::new("."), &cli.file, !json)?;
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
    let stack = docker::Stack::new(i.root, cli.file.clone(), &data);
    match cli.command {
        Command::Schema | Command::Action(Action::Trust { .. }) => Ok(()),
        Command::Action(action) => {
            let up = matches!(action, Action::Up { .. });
            let s = docker::run(&stack, i.digest, Some(action))?;
            if json && up {
                println!(
                    "{}",
                    serde_json::to_string(&view::receipt(&s)).unwrap_or_default()
                );
            } else {
                print_status(&stack, &s);
            }
            Ok(())
        }
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
                .map(|e| json!({"reason": e.reason, "command": docker::render(&stack, &e.op)}))
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
            let strays = ports::stray_lines(&stack.root);
            next.extend(
                ports::strays(&stack.root)
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
            println!(
                "{:#}",
                json!({
                    "root": stack.root, "project": stack.compose_name(), "data_dir": stack.dir,
                    "compose_files": compose::resolve(&stack.root, &stack.files), "status": status,
                    "findings": findings(&s), "unverified": unverified, "moved_ports": ports::lines(&stack.dir), "outside_rizr": strays,
                    "generation": s.intents.generation.0, "graph": graph, "nodes": view::nodes(&s),
                    "drift": view::drift(&s), "failed": failed, "next": next,
                })
            );
            Ok(())
        }
        Command::Logs { node, follow, tail } => {
            let mut c = stack.compose();
            c.args(["logs", "--no-color", "--tail", &tail]);
            if follow {
                c.arg("--follow");
            }
            c.args(node);
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
        .chain(ports::stray_lines(&stack.root))
    {
        println!("{l}");
    }
    let unverified: Vec<String> = view::unverified(s).into_iter().map(|n| n.0).collect();
    if !unverified.is_empty() {
        println!(
            "unverified: {} started, but no healthcheck proves they serve requests; check each one (its URL, or `rizr logs <node>`)",
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
                   "needed_by": needed_by, "next": next, "runtime": format!("compose service `{name}` in project {}", stack.compose_name()),
                   "generated_env": generated, "problems": problems})
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
    println!(
        "  runtime: compose service `{name}` in project {}",
        stack.compose_name()
    );
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
fn known_cause(mut e: Error) -> Error {
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
            .map(|r| ports::strays(&r))
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
