//! One-off commands in a running node (`exec`, `wait`). A job outlives the rizr call that starts it,
//! so no call runs past an agent shell's timeout; `wait` picks the job up again.

use std::fs;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use rizr_core::error::{Error, ErrorCode};

use crate::docker::Stack;

/// Below the 120 s after which Claude Code moves a foreground call to the background.
const BUDGET: Duration = Duration::from_secs(100);
const TAIL_LINES: usize = 30;

#[derive(Debug, PartialEq)]
enum Job {
    Running,
    Exited(i32),
    /// The process is gone and wrote no exit code (killed, or the machine restarted).
    Lost,
}

fn state(dir: &Path) -> Job {
    let exited = || {
        fs::read_to_string(dir.join("exit"))
            .ok()
            .map(|s| Job::Exited(s.trim().parse().unwrap_or(-1)))
    };
    if let Some(j) = exited() {
        return j;
    }
    let alive = fs::read_to_string(dir.join("pid"))
        .ok()
        .and_then(|p| p.trim().parse::<u32>().ok())
        .is_some_and(|p| Path::new(&format!("/proc/{p}")).exists());
    // The job writes its exit code just before it ends: read it again once the process is gone.
    if alive {
        Job::Running
    } else {
        exited().unwrap_or(Job::Lost)
    }
}

fn jobs(stack: &Stack) -> PathBuf {
    stack.dir.join("jobs")
}

fn io(e: std::io::Error) -> Error {
    Error::new(
        ErrorCode::RuntimeUnavailable,
        format!("cannot start the job: {e}"),
        "rizr keeps job output in its data directory; check that it is writable",
        "rizr status",
    )
}

pub fn start(stack: &Stack, node: &str, cmd: &[String]) -> Result<(), Error> {
    let id = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis())
        .to_string();
    let dir = jobs(stack).join(&id);
    fs::create_dir_all(&dir).map_err(io)?;
    let compose = stack.compose();
    let mut sh = Command::new("sh");
    sh.arg("-c")
        .arg(r#"d=$1; shift; "$@" >"$d/log" 2>&1 </dev/null; echo $? >"$d/exit.tmp"; mv "$d/exit.tmp" "$d/exit""#)
        .arg("sh")
        .arg(&dir)
        .arg(compose.get_program())
        .args(compose.get_args())
        .args(["exec", "-T", node])
        .args(cmd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        // Its own process group: a signal to the agent's shell does not stop the job.
        .process_group(0);
    for (k, v) in compose.get_envs() {
        match v {
            Some(v) => sh.env(k, v),
            None => sh.env_remove(k),
        };
    }
    if let Some(d) = compose.get_current_dir() {
        sh.current_dir(d);
    }
    let child = sh.spawn().map_err(io)?;
    fs::write(dir.join("pid"), child.id().to_string()).map_err(io)?;
    fs::write(jobs(stack).join("last"), &id).map_err(io)?;
    println!("job {id}: {node} $ {}", cmd.join(" "));
    follow(&dir, &id)
}

pub fn wait(stack: &Stack, job: Option<&str>) -> Result<(), Error> {
    let id = match job {
        Some(j) => j.to_owned(),
        None => fs::read_to_string(jobs(stack).join("last"))
            .map(|s| s.trim().to_owned())
            .map_err(|_| {
                Error::new(
                    ErrorCode::Usage,
                    "no job to wait for",
                    "start a one-off command with `rizr exec` first",
                    "rizr exec <node> -- <command>",
                )
            })?,
    };
    let dir = jobs(stack).join(&id);
    if !dir.is_dir() {
        return Err(Error::new(
            ErrorCode::Usage,
            format!("no job {id}"),
            "omit the job id to wait for the last `rizr exec`",
            "rizr wait",
        ));
    }
    follow(&dir, &id)
}

fn follow(dir: &Path, id: &str) -> Result<(), Error> {
    let start = Instant::now();
    let job = loop {
        match state(dir) {
            Job::Running if start.elapsed() < BUDGET => {
                std::thread::sleep(Duration::from_millis(200))
            }
            j => break j,
        }
    };
    let log = dir.join("log");
    let text = fs::read_to_string(&log).unwrap_or_default();
    let lines: Vec<_> = text.lines().collect();
    let skip = lines.len().saturating_sub(TAIL_LINES);
    if skip > 0 {
        println!("({skip} earlier lines in {})", log.display());
    }
    for l in &lines[skip..] {
        println!("  {l}");
    }
    match job {
        Job::Exited(0) => {
            println!("job {id}: exit 0");
            Ok(())
        }
        Job::Exited(c) => Err(Error::new(
            ErrorCode::JobFailed,
            format!("job {id} exited with code {c}"),
            "the output above shows why",
            format!("cat {}", log.display()),
        )),
        Job::Lost => Err(Error::new(
            ErrorCode::JobFailed,
            format!("job {id} ended without an exit code"),
            "the job was killed; start it again",
            "rizr exec <node> -- <command>",
        )),
        Job::Running => Err(Error::new(
            ErrorCode::StillRunning,
            format!("job {id} still runs after {} s", BUDGET.as_secs()),
            "it keeps running in the background; wait again, each call waits up to 100 s",
            format!("rizr wait {id}"),
        )),
    }
}
