//! One-off commands as compose job containers (`exec`, `wait`). Docker holds each job: its name, exit code and
//! logs. A job outlives the rizr call that starts it, so no call runs past an agent shell's timeout.

use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rizr_core::error::{Error, ErrorCode};

use crate::docker::Stack;

/// Below the 120 s after which Claude Code moves a foreground call to the background.
const BUDGET: Duration = Duration::from_secs(100);
const LABEL: &str = "rizr.job";

/// Remove this stack's job containers: the finished ones, or with `running` also those that still run (`down`).
pub fn remove(stack: &Stack, running: bool) {
    let ids = Command::new("docker")
        .args(["ps", "-aq", "--filter"])
        .arg(format!("label={LABEL}"))
        .arg("--filter")
        .arg(format!(
            "label=com.docker.compose.project={}",
            stack.compose_name()
        ))
        .args(if running {
            &[][..]
        } else {
            &["--filter", "status=exited"][..]
        })
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    if !ids.trim().is_empty() {
        let _ = Command::new("docker")
            .args(["rm", "--force"])
            .args(ids.split_whitespace())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

pub fn start(stack: &Stack, node: &str, cmd: &[String]) -> Result<(), Error> {
    // Earlier jobs stay until the next `exec`, so `docker logs` keeps their full output.
    remove(stack, false);
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    let id = format!("{}-job-{node}-{millis}", stack.compose_name());
    let out = stack
        .compose()
        .args(["run", "-d", "--no-deps", "--name", &id, "--label"])
        .arg(format!("{LABEL}={node}"))
        .arg(node)
        .args(cmd)
        .output()
        .map_err(|e| runtime(&e.to_string()))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(Error::new(
            ErrorCode::JobFailed,
            format!(
                "cannot start a job in {node}: {}",
                err.trim()
                    .lines()
                    .last()
                    .unwrap_or("docker compose run failed")
            ),
            "the node must be a service of this stack; `rizr up` builds its image and starts its dependencies",
            "rizr status",
        ));
    }
    println!("job {id}: {node} $ {}", cmd.join(" "));
    follow(&id).map_err(|e| unsynced(stack, node, cmd, &id, e))
}

/// A job starts from the node's image. A path it did not find that the repo has under a `develop.watch` sync entry
/// of the node reaches a container only under `docker compose watch`: say so, and give one command that copies the
/// tree into the running container and runs the command there.
fn unsynced(stack: &Stack, node: &str, cmd: &[String], id: &str, mut e: Error) -> Error {
    if e.code != ErrorCode::JobFailed {
        return e;
    }
    let text = |c: &mut Command| {
        c.output()
            .map(|o| {
                let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
                s.push_str(&String::from_utf8_lossy(&o.stderr));
                s
            })
            .unwrap_or_default()
    };
    let logs = text(Command::new("docker").args(["logs", "--tail", "30", id]));
    let missing = missing_paths(&logs);
    if missing.is_empty() {
        return e;
    }
    let config: serde_json::Value =
        serde_json::from_str(&text(stack.compose().args(["config", "--format", "json"])))
            .unwrap_or_default();
    let syncs = config["services"][node]["develop"]["watch"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|w| w["action"].as_str().is_some_and(|a| a.starts_with("sync")));
    for w in syncs {
        let (Some(host), Some(target)) = (w["path"].as_str(), w["target"].as_str()) else {
            continue;
        };
        let host = stack.root.join(host);
        let Some(path) = missing
            .iter()
            .find(|m| covers(&host, target, m).is_some_and(|p| p.exists()))
        else {
            continue;
        };
        let rel = host.strip_prefix(&stack.root).unwrap_or(&host);
        let rel = if rel.as_os_str().is_empty() {
            ".".into()
        } else {
            format!("./{}", rel.display())
        };
        let ignore: std::collections::BTreeSet<String> = w["ignore"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|i| {
                let name = std::path::Path::new(i.as_str()?).file_name()?.to_str()?;
                Some(format!(" --exclude={}", quote(name)))
            })
            .collect();
        let base = compose_base(stack);
        let copy = if ignore.is_empty() {
            format!("{base} cp {rel}/. {node}:{target}")
        } else {
            format!(
                "tar -C {rel}{} -cf - . | {base} exec -T {node} tar -C {target} -xf -",
                ignore.into_iter().collect::<String>()
            )
        };
        let run: Vec<String> = cmd.iter().map(|a| quote(a)).collect();
        let tree = if rel == "." { "the repo" } else { &rel };
        e.hint = format!(
            "{node}'s image lacks {path}, which the repo has: compose copies {tree} into {target} only under \
             `docker compose watch` (develop.watch sync), and a job starts from the image; copy the tree into the \
             running {node} container and run the command there"
        );
        e.example = format!("{copy} && {base} exec -T {node} {}", run.join(" "));
        return e;
    }
    e
}

/// Paths named on the not-found lines of a job's output, as written (`tests/`, `/app/x.py`).
fn missing_paths(logs: &str) -> Vec<String> {
    const NOT_FOUND: [&str; 6] = [
        "no such file",
        "not found",
        "cannot find",
        "can't open file",
        "does not exist",
        "could not find",
    ];
    let mut v: Vec<String> = Vec::new();
    for line in logs.lines() {
        let l = line.to_ascii_lowercase();
        if !NOT_FOUND.iter().any(|k| l.contains(k)) {
            continue;
        }
        for t in line.split(|c: char| c.is_whitespace() || "'\"`,;()[]".contains(c)) {
            let t = t.trim_end_matches([':', '.']).trim_start_matches("./");
            if t.contains('/') && !t.contains("://") && !v.iter().any(|x| x == t) {
                v.push(t.to_owned());
            }
        }
    }
    v
}

/// The host path a container path maps to under a sync entry from `host` to `target`; a relative path is taken
/// from `target`, the usual working directory.
fn covers(host: &std::path::Path, target: &str, path: &str) -> Option<std::path::PathBuf> {
    let p = std::path::Path::new(path);
    let rel = if p.is_absolute() {
        p.strip_prefix(target).ok()?
    } else {
        p
    };
    Some(host.join(rel)).filter(|h| h != host)
}

/// The plain `docker compose` call that reaches this stack's containers.
fn compose_base(stack: &Stack) -> String {
    let mut s = String::from("docker compose");
    let name = stack.compose_name();
    if name != crate::compose::default_name(&stack.root, &stack.files) {
        s.push_str(&format!(" -p {}", quote(&name)));
    }
    for f in &stack.files {
        s.push_str(&format!(" -f {}", quote(&f.to_string_lossy())));
    }
    s
}

fn quote(a: &str) -> String {
    if !a.is_empty()
        && a.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./=:@+%".contains(c))
    {
        a.to_owned()
    } else {
        format!("'{}'", a.replace('\'', r"'\''"))
    }
}

pub fn wait(stack: &Stack, job: Option<&str>) -> Result<(), Error> {
    let id = match job {
        Some(j) => j.to_owned(),
        None => Command::new("docker")
            .args(["ps", "-a", "--format", "{{.Names}}", "--filter"])
            .arg(format!("label={LABEL}"))
            .arg("--filter")
            .arg(format!(
                "label=com.docker.compose.project={}",
                stack.compose_name()
            ))
            .output()
            .ok()
            .and_then(|o| {
                // `docker ps` lists the newest container first.
                String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .next()
                    .map(str::to_owned)
            })
            .ok_or_else(|| {
                Error::new(
                    ErrorCode::Usage,
                    "no job to wait for",
                    "start a one-off command with `rizr exec` first",
                    "rizr exec <node> -- <command>",
                )
            })?,
    };
    follow(&id)
}

fn runtime(e: &str) -> Error {
    Error::new(
        ErrorCode::RuntimeUnavailable,
        format!("cannot run docker: {e}"),
        "start the docker daemon",
        "docker info",
    )
}

/// Block on `docker wait` until the job ends or the budget passes, then print its last log lines.
fn follow(id: &str) -> Result<(), Error> {
    let mut child = Command::new("docker")
        .args(["wait", id])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| runtime(&e.to_string()))?;
    let mut stdout = child.stdout.take();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut s = String::new();
        if let Some(o) = stdout.as_mut() {
            let _ = o.read_to_string(&mut s);
        }
        let _ = tx.send(s);
    });
    let code = match rx.recv_timeout(BUDGET) {
        Ok(s) => {
            let _ = child.wait();
            match s.trim().parse::<i32>() {
                Ok(c) => Some(c),
                Err(_) => {
                    return Err(Error::new(
                        ErrorCode::Usage,
                        format!("no job {id}"),
                        "omit the job name to wait for the last `rizr exec`",
                        "rizr wait",
                    ));
                }
            }
        }
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            None
        }
    };
    let _ = Command::new("docker")
        .args(["logs", "--tail", "30", id])
        .status();
    match code {
        Some(0) => {
            println!("job {id}: exit 0");
            Ok(())
        }
        Some(c) => Err(Error::new(
            ErrorCode::JobFailed,
            format!("job {id} exited with code {c}"),
            "the output above shows why",
            format!("docker logs {id}"),
        )),
        None => Err(Error::new(
            ErrorCode::StillRunning,
            format!(
                "job {id} has not failed: it still runs after {} s (exit 5 means not done yet; its output so far is above)",
                BUDGET.as_secs()
            ),
            "it keeps running in the background; `rizr wait` waits up to 100 s more and prints its exit code",
            format!("rizr wait {id}"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_paths_reads_not_found_lines() {
        let logs = "collected 0 items\nERROR: file or directory not found: tests/\n\
                    python: can't open file '/app/backend/x.py': [Errno 2] No such file or directory\n\
                    GET http://db:5432/ not found\n";
        assert_eq!(missing_paths(logs), ["tests/", "/app/backend/x.py"]);
    }

    #[test]
    fn covers_maps_into_the_sync_source() {
        let host = std::path::Path::new("/r/backend");
        assert_eq!(
            covers(host, "/app/backend", "/app/backend/tests"),
            Some("/r/backend/tests".into())
        );
        assert_eq!(
            covers(host, "/app/backend", "tests/"),
            Some("/r/backend/tests/".into())
        );
        assert_eq!(covers(host, "/app/backend", "/etc/x"), None);
    }
}
