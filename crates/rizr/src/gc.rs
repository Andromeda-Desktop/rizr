//! `rizr footprint` lists what rizr made for this project; `rizr gc` releases the instances of removed worktrees.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use rizr_core::error::{Error, ErrorCode};

use crate::docker::Stack;

fn lines(c: &mut Command) -> Vec<String> {
    c.output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter(|l| !l.trim().is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn docker(args: &[&str]) -> Vec<String> {
    lines(Command::new("docker").args(args))
}

/// `main` and each linked worktree that still exists. Reads git's files only, as `main_worktree` does.
fn live(main_root: &Path) -> BTreeSet<String> {
    let mut v = BTreeSet::from(["main".to_owned()]);
    let dirs = std::fs::read_dir(main_root.join(".git/worktrees"))
        .into_iter()
        .flatten();
    for d in dirs.filter_map(Result::ok) {
        let Ok(gitfile) = std::fs::read_to_string(d.path().join("gitdir")) else {
            continue;
        };
        let gitfile = PathBuf::from(gitfile.trim());
        if gitfile.is_file()
            && let Some(root) = gitfile.parent()
        {
            v.insert(crate::docker::slug(root));
        }
    }
    v
}

/// The instances that have a data dir, by name.
fn instances(stack: &Stack) -> Vec<(String, PathBuf)> {
    let prefix = format!("{}-", stack.project.0);
    let data = stack.dir.parent().unwrap_or(Path::new("/"));
    let mut v: Vec<_> = std::fs::read_dir(data)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter(|e| e.path().is_dir())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            Some((name.strip_prefix(&prefix)?.to_owned(), e.path()))
        })
        .collect();
    v.sort();
    v
}

fn compose_name(stack: &Stack, instance: &str) -> String {
    if instance == "main" {
        stack.main_compose_name.clone()
    } else {
        format!("{}-{instance}", stack.main_compose_name)
    }
}

fn label(key: &str, value: &str) -> String {
    format!("label={key}={value}")
}

/// `rizr footprint`.
pub fn footprint(stack: &Stack) -> Result<(), Error> {
    let p = &stack.project.0;
    let alive = live(&stack.main_root);
    println!("project {p} (main worktree {})", stack.main_root.display());
    for (inst, dir) in instances(stack) {
        let gone = if alive.contains(&inst) {
            ""
        } else {
            "  (worktree removed: rizr gc releases it)"
        };
        let cp = compose_name(stack, &inst);
        println!("instance {inst}{gone}");
        println!("  data      {}", dir.display());
        let compose = label("com.docker.compose.project", &cp);
        for (what, args) in [
            (
                "container",
                vec![
                    "ps",
                    "-a",
                    "--filter",
                    &compose,
                    "--format",
                    "{{.Names}} ({{.State}})",
                ],
            ),
            (
                "network",
                vec![
                    "network",
                    "ls",
                    "--filter",
                    &compose,
                    "--format",
                    "{{.Name}}",
                ],
            ),
            (
                "volume",
                vec![
                    "volume",
                    "ls",
                    "--filter",
                    &compose,
                    "--format",
                    "{{.Name}}",
                ],
            ),
        ] {
            for l in docker(&args) {
                println!("  {what:<9} {l}");
            }
        }
        let units = format!("rizr-{p}-{inst}-*");
        for u in lines(Command::new("systemctl").args([
            "--user",
            "list-units",
            "--all",
            "--plain",
            "--no-legend",
            &units,
        ])) {
            println!(
                "  unit      {}",
                u.split_whitespace().next().unwrap_or_default()
            );
        }
        for d in crate::postgres::Info::load(&dir).databases.values() {
            println!("  database  {} on rizr-{p}-{}", d.database, d.server);
        }
    }
    let shared = label("rizr.scope", "project");
    let project = label("rizr.project", p);
    for s in docker(&[
        "ps",
        "-a",
        "--filter",
        &shared,
        "--filter",
        &project,
        "--format",
        "{{.Names}} ({{.State}})",
    ]) {
        println!("shared server {s}");
    }
    for v in docker(&[
        "volume",
        "ls",
        "--filter",
        &format!("name=^rizr-{p}-"),
        "--format",
        "{{.Name}}",
    ]) {
        println!("shared volume {v}");
    }
    let data = stack.dir.parent().unwrap_or(Path::new("/"));
    for f in [
        format!("{p}.ports.json"),
        format!("{p}.project.lock"),
        format!("{p}.ports.lock"),
        format!("{p}.subnets.json"),
        format!("{p}.subnets.lock"),
    ] {
        let f = data.join(f);
        if f.exists() {
            println!("file {}", f.display());
        }
    }
    Ok(())
}

/// `rizr gc`: everything of an instance whose worktree is gone. The worktree was deleted, so its data goes too.
pub fn gc(stack: &Stack) -> Result<(), Error> {
    let p = &stack.project.0;
    let alive = live(&stack.main_root);
    let stale: Vec<_> = instances(stack)
        .into_iter()
        .filter(|(i, _)| !alive.contains(i))
        .collect();
    if stale.is_empty() {
        println!("nothing to release: every instance of {p} has its worktree");
        return Ok(());
    }
    for (inst, dir) in &stale {
        let compose = label("com.docker.compose.project", &compose_name(stack, inst));
        let ids = docker(&["ps", "-aq", "--filter", &compose]);
        if !ids.is_empty() {
            let _ = Command::new("docker")
                .args(["rm", "-f"])
                .args(&ids)
                .output();
        }
        let _ = Command::new("systemctl")
            .args(["--user", "stop", &format!("rizr-{p}-{inst}-*")])
            .output();
        let nets = docker(&["network", "ls", "-q", "--filter", &compose]);
        // The shared server joins each instance network; it must leave before the network can go.
        for n in &nets {
            let attached = docker(&[
                "network",
                "inspect",
                n,
                "--format",
                "{{range .Containers}}{{.Name}}\n{{end}}",
            ]);
            for c in attached {
                let _ = Command::new("docker")
                    .args(["network", "disconnect", "-f", n, &c])
                    .output();
            }
        }
        if !nets.is_empty() {
            let _ = Command::new("docker")
                .args(["network", "rm"])
                .args(&nets)
                .output();
        }
        let vols = docker(&["volume", "ls", "-q", "--filter", &compose]);
        if !vols.is_empty() {
            let _ = Command::new("docker")
                .args(["volume", "rm"])
                .args(&vols)
                .output();
        }
        let mut dbs = Vec::new();
        for d in crate::postgres::Info::load(dir).databases.values() {
            let server = format!("rizr-{p}-{}", d.server);
            let names =
                crate::postgres::admin(&server, &d.user, "select datname from pg_database", None)
                    .unwrap_or_default();
            let rw = format!("{}__rw_", d.database);
            for name in names
                .lines()
                .filter(|n| *n == d.database || n.starts_with(&rw))
            {
                let dropped = crate::postgres::close(&server, &d.user, name).and_then(|()| {
                    crate::postgres::admin(
                        &server,
                        &d.user,
                        &format!("DROP DATABASE \"{name}\""),
                        None,
                    )
                });
                if dropped.is_ok() {
                    dbs.push(name.to_owned());
                }
            }
        }
        for kind in ["ports", "subnets"] {
            release(stack, inst, kind);
        }
        std::fs::remove_dir_all(dir).map_err(|e| {
            Error::new(
                ErrorCode::RuntimeUnavailable,
                format!("cannot remove {}: {e}", dir.display()),
                "rizr keeps instance data there; check its permissions",
                format!("ls -ld {}", dir.display()),
            )
        })?;
        println!(
            "released {inst}: {} container(s), {} network(s), {} volume(s), databases [{}], {}",
            ids.len(),
            nets.len(),
            vols.len(),
            dbs.join(", "),
            dir.display()
        );
    }
    Ok(())
}

/// Drop the instance's claims of one kind (host ports, subnets), so other instances may take them.
fn release(stack: &Stack, instance: &str, kind: &str) {
    let data = stack.dir.parent().unwrap_or(Path::new("/"));
    let claims = data.join(format!("{}.{kind}.json", stack.project.0));
    let Ok(lock) = std::fs::File::create(claims.with_extension("lock")) else {
        return;
    };
    if lock.lock().is_err() {
        return;
    }
    let Some(mut held) = std::fs::read(&claims).ok().and_then(|b| {
        serde_json::from_slice::<serde_json::Map<String, serde_json::Value>>(&b).ok()
    }) else {
        return;
    };
    if held.remove(instance).is_some() {
        let _ = serde_json::to_vec(&held).map(|b| std::fs::write(&claims, b));
    }
}

/// Before `up`: with `max_instances` set and reached, stop the other instance that rizr used least recently.
/// Its containers and units stop; its data stays, and `rizr up` there starts it again.
/// An agent grant stops another instance only when `[agents] other_instances` and the human allow it.
fn grant_may_stop(stack: &Stack, stop: &[&String], max: usize) -> bool {
    use crate::confirm::{self, Answer, Ask};
    let names = stop
        .iter()
        .map(|s| s.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let answer = if crate::manifest::agents(&stack.main_root).other_instances
        == crate::manifest::Policy::Deny
    {
        Answer::Deny
    } else {
        confirm::request(
            &Ask {
                grant: std::env::var("RIZR_GRANT").unwrap_or_default(),
                verb: "stop".into(),
                target: names.clone(),
                summary: format!(
                    "stop instance {names} to stay within max_instances = {max}; its data stays"
                ),
                files: vec![],
            },
            confirm::TIMEOUT,
        )
    };
    if answer != Answer::Allow {
        eprintln!(
            "kept instance {names} running ({answer:?}): more than max_instances = {max} run now"
        );
    }
    answer == Answer::Allow
}

pub fn make_room(stack: &Stack) {
    let Some(max) = crate::manifest::max_instances(&stack.main_root) else {
        return;
    };
    let p = &stack.project.0;
    let project = label("rizr.project", p);
    let running: BTreeSet<String> = docker(&[
        "ps",
        "--filter",
        &project,
        "--format",
        "{{.Label \"rizr.instance\"}}",
    ])
    .into_iter()
    .filter(|i| !i.is_empty() && *i != stack.instance.0)
    .collect();
    if running.len() < max {
        return;
    }
    // Every rizr call rewrites the instance's generation file: its age is the time since last use.
    let used = |dir: &Path| {
        std::fs::metadata(dir.join("generation"))
            .and_then(|m| m.modified())
            .ok()
    };
    let mut by_age: Vec<_> = instances(stack)
        .into_iter()
        .filter(|(i, _)| running.contains(i))
        .map(|(i, d)| (used(&d), i))
        .collect();
    by_age.sort();
    let stop: Vec<&String> = by_age
        .iter()
        .take(running.len() + 1 - max)
        .map(|(_, i)| i)
        .collect();
    if crate::refuse::gateway() && !grant_may_stop(stack, &stop, max) {
        return;
    }
    for inst in stop {
        let ids = docker(&[
            "ps",
            "-q",
            "--filter",
            &project,
            "--filter",
            &label("rizr.instance", inst),
        ]);
        if !ids.is_empty() {
            let _ = Command::new("docker").arg("stop").args(&ids).output();
        }
        let _ = Command::new("systemctl")
            .args(["--user", "stop", &format!("rizr-{p}-{inst}-*")])
            .output();
        eprintln!(
            "stopped instance {inst} (least recently used) to stay within max_instances = {max}; its data stays, \
             `rizr up` there starts it again"
        );
    }
}
