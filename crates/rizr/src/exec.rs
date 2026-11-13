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
    follow(&id)
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
            format!("job {id} still runs after {} s", BUDGET.as_secs()),
            "it keeps running in the background; wait again, each call waits up to 100 s",
            format!("rizr wait {id}"),
        )),
    }
}
