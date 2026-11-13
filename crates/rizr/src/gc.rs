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
        release_ports(stack, inst);
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

/// Drop the instance's host-port claims, so other instances may take those ports.
fn release_ports(stack: &Stack, instance: &str) {
    let data = stack.dir.parent().unwrap_or(Path::new("/"));
    let claims = data.join(format!("{}.ports.json", stack.project.0));
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
