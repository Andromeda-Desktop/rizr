//! `rizr test` and `rizr build`: run a detected task of a package and report its result in one call.

use std::fs::{File, OpenOptions, TryLockError};
use std::io::Seek;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{RecvTimeoutError, channel};
use std::time::{Duration, Instant};

use rizr_core::code::{Kind, Manager, Package, PyTool, TaskName};
use rizr_core::error::{Error, ErrorCode};
use serde::Serialize;

use serde_json::Value;

use crate::{detect, docker, postgres};

const BUDGET: Duration = Duration::from_secs(100);
const EXIT_MARK: &str = "rizr-task-exit:";

#[derive(Debug, Default, PartialEq, Eq, Serialize)]
pub struct Summary {
    pub passed: u64,
    pub failed: u64,
    pub skipped: u64,
    /// Names of failing tests, as the runner prints them (at most 20).
    pub failures: Vec<String>,
    /// True when a known runner printed its counts.
    pub counted: bool,
}

#[derive(Serialize)]
struct Receipt<'a> {
    task: TaskName,
    package: &'a str,
    place: String,
    cmd: &'a str,
    source: rizr_core::code::Source,
    from: &'a str,
    exit: i32,
    seconds: u64,
    #[serde(flatten)]
    summary: &'a Summary,
    tail: Vec<&'a str>,
    log: &'a Path,
}

fn number_before(text: &str, word: &str) -> u64 {
    let words: Vec<&str> = text
        .split(|c: char| c.is_whitespace() || c == ',' || c == ';' || c == '|' || c == '(')
        .filter(|w| !w.is_empty())
        .collect();
    words
        .windows(2)
        .filter(|w| w[1].trim_end_matches(['.', ')']) == word)
        .filter_map(|w| w[0].parse::<u64>().ok())
        .sum()
}

/// Counts and failing test names from the output of cargo, pytest, jest, vitest or go test.
pub fn summarize(out: &str) -> Summary {
    let mut s = Summary::default();
    for line in out.lines() {
        let l = line.trim();
        if let Some(rest) = l.strip_prefix("test result: ") {
            s.passed += number_before(rest, "passed");
            s.failed += number_before(rest, "failed");
            s.skipped += number_before(rest, "ignored");
            s.counted = true;
        } else if l.starts_with('=')
            && l.ends_with('=')
            && (l.contains(" passed") || l.contains(" failed"))
        {
            s.passed = number_before(l, "passed");
            s.failed =
                number_before(l, "failed") + number_before(l, "error") + number_before(l, "errors");
            s.skipped = number_before(l, "skipped");
            s.counted = true;
        } else if let Some(rest) = l
            .strip_prefix("Tests:")
            .or_else(|| l.strip_prefix("Tests "))
        {
            s.passed = number_before(rest, "passed");
            s.failed = number_before(rest, "failed");
            s.skipped = number_before(rest, "skipped") + number_before(rest, "todo");
            s.counted = true;
        } else if l.starts_with("--- PASS:") {
            s.passed += 1;
            s.counted = true;
        } else if l.starts_with("--- SKIP:") {
            s.skipped += 1;
        }
        let name = if let Some(n) = l.strip_prefix("--- FAIL: ") {
            s.failed += 1;
            s.counted = true;
            n.split_whitespace().next()
        } else if let Some(n) = l.strip_prefix("FAILED ") {
            n.split(" - ").next()
        } else if l.starts_with("test ") && l.ends_with(" ... FAILED") {
            l.strip_prefix("test ")
                .and_then(|n| n.strip_suffix(" ... FAILED"))
        } else {
            l.strip_prefix("● ")
        };
        if let Some(n) = name
            && s.failures.len() < 20
            && !s.failures.iter().any(|f| f == n)
        {
            s.failures.push(n.to_owned());
        }
    }
    s
}

fn pick<'a>(
    packages: &'a [Package],
    name: TaskName,
    dir: Option<&str>,
) -> Result<&'a Package, Error> {
    let list = || {
        packages
            .iter()
            .filter(|p| p.task(name).is_some())
            .map(|p| p.dir.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    let verb = verb(name);
    let found = match dir {
        Some(d) => packages
            .iter()
            .find(|p| p.dir == d.trim_end_matches('/').trim_start_matches("./")),
        None => packages
            .iter()
            .find(|p| p.dir == ".")
            .filter(|p| p.task(name).is_some())
            .or_else(|| packages.iter().find(|p| p.task(name).is_some())),
    };
    let none = |what: String| {
        Error::new(
            ErrorCode::NoTask,
            what,
            format!(
                "packages with a {verb} task: {}; or add `[tasks] {verb} = \"<command>\"` to rizr.toml",
                if list().is_empty() {
                    "none".into()
                } else {
                    list()
                }
            ),
            format!("rizr {verb} <package dir>"),
        )
    };
    let p = found.ok_or_else(|| none(format!("no package {}", dir.unwrap_or("in this repo"))))?;
    if p.task(name).is_none() {
        return Err(none(format!("package {} has no {verb} command", p.dir)));
    }
    Ok(p)
}

fn verb(name: TaskName) -> &'static str {
    match name {
        TaskName::Build => "build",
        TaskName::Test => "test",
        TaskName::Lint => "lint",
        TaskName::Migrate => "migrate",
    }
}

fn log_path(data: &Path, name: TaskName, dir: &str) -> PathBuf {
    let slug: String = dir
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    data.join(format!("task-{}-{slug}.log", verb(name)))
}

/// Where a task runs: the host, or the compose service whose build context is the package dir.
enum Place {
    Host,
    /// The service, and a bind mount `dir:workdir` when the service does not mount its build context.
    Service(String, Option<String>),
}

impl std::fmt::Display for Place {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Place::Host => Ok(()),
            Place::Service(svc, _) => write!(f, " (service {svc})"),
        }
    }
}

fn place(stack: &docker::Stack, pkg: &Package) -> Place {
    let Ok(dir) = stack.root.join(&pkg.dir).canonicalize() else {
        return Place::Host;
    };
    let Ok(out) = stack
        .compose()
        .args(["config", "--format", "json"])
        .stderr(Stdio::null())
        .output()
    else {
        return Place::Host;
    };
    let config: Value = serde_json::from_slice(&out.stdout).unwrap_or_default();
    let same = |p: &str| Path::new(p).canonicalize().is_ok_and(|p| p == dir);
    for (name, svc) in config["services"].as_object().into_iter().flatten() {
        let Some(ctx) = svc["build"]["context"].as_str().filter(|c| same(c)) else {
            continue;
        };
        let mounted = svc["volumes"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|v| v["type"] == "bind" && v["source"].as_str().is_some_and(same));
        if mounted {
            return Place::Service(name.clone(), None);
        }
        let synced = svc["develop"]["watch"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|w| {
                w["path"].as_str().is_some_and(same)
                    && w["action"].as_str().is_some_and(|a| a.starts_with("sync"))
            })
            .and_then(|w| w["target"].as_str().map(str::to_owned));
        let dockerfile =
            Path::new(ctx).join(svc["build"]["dockerfile"].as_str().unwrap_or("Dockerfile"));
        let target = synced.or_else(|| {
            std::fs::read_to_string(dockerfile)
                .ok()
                .and_then(|t| workdir(&t))
        });
        if let Some(t) = target {
            return Place::Service(name.clone(), Some(format!("{}:{t}", dir.display())));
        }
    }
    Place::Host
}

/// The last absolute `WORKDIR` of a Dockerfile: the final stage's working directory.
fn workdir(dockerfile: &str) -> Option<String> {
    dockerfile
        .lines()
        .filter_map(|l| {
            let mut w = l.split_whitespace();
            w.next().filter(|k| k.eq_ignore_ascii_case("WORKDIR"))?;
            w.next().filter(|d| d.starts_with('/')).map(str::to_owned)
        })
        .next_back()
}

/// Brings the stack up (`rizr up --wait ready`) for a task that runs in one of its services.
fn up(stack: &docker::Stack) -> Result<(), Error> {
    let exe = std::env::current_exe().unwrap_or_else(|_| "rizr".into());
    let mut c = Command::new(exe);
    for f in &stack.files {
        c.arg("-f").arg(f);
    }
    // up's report goes to stderr: stdout holds only the task's result.
    let status = c
        .args(["up", "--wait", "ready"])
        .stdin(Stdio::null())
        .stdout(std::io::stderr())
        .status();
    match status.ok().and_then(|s| s.code()) {
        Some(0) => return Ok(()),
        // Exit 5 (not ready): up keeps going in the background and the next call waits for it.
        Some(5) => {
            return Err(Error::new(
                ErrorCode::StillRunning,
                "the stack is still starting, so the task has not run yet",
                "the task runs inside a compose service; the same command again waits up to 100 s more",
                "rizr test",
            ));
        }
        _ => {}
    }
    Err(Error::new(
        ErrorCode::DependencyFailed,
        "the stack did not come up, so the task cannot run in its service",
        "the task runs inside a compose service and needs the stack; fix the error above",
        "rizr up",
    ))
}

/// The command that installs a package's missing dependencies before a host task, and why it runs.
fn install(root: &Path, pkg: &Package) -> Option<(&'static str, &'static str)> {
    let dir = root.join(&pkg.dir);
    let lock = |name: &str| dir.join(name).is_file() || root.join(name).is_file();
    match &pkg.kind {
        Kind::Node { manager } if !dir.join("node_modules").exists() => {
            let cmd = match manager {
                Manager::Npm if lock("package-lock.json") => "npm ci",
                Manager::Npm => "npm install",
                Manager::Pnpm if lock("pnpm-lock.yaml") => "pnpm install --frozen-lockfile",
                Manager::Pnpm => "pnpm install",
                Manager::Yarn if lock("yarn.lock") => "yarn install --frozen-lockfile",
                Manager::Yarn => "yarn install",
                Manager::Bun if lock("bun.lock") || lock("bun.lockb") => {
                    "bun install --frozen-lockfile"
                }
                Manager::Bun => "bun install",
            };
            Some((cmd, "node_modules missing"))
        }
        Kind::Python { tool: PyTool::Uv }
            if lock("uv.lock") && !dir.join(".venv").exists() && !root.join(".venv").exists() =>
        {
            Some(("uv sync --frozen", ".venv missing"))
        }
        _ => None,
    }
}

/// Runs the task, or waits for the same task that a former call left running. Returns within 100 s.
pub fn run(
    stack: &docker::Stack,
    name: TaskName,
    dir: Option<&str>,
    json: bool,
) -> Result<(), Error> {
    let packages = detect::detect(&detect::read(&stack.root));
    let pkg = pick(&packages, name, dir)?;
    let Some(task) = pkg.task(name) else {
        return Ok(());
    };
    let (root, data) = (stack.root.as_path(), stack.dir.as_path());
    let place = place(stack, pkg);
    // Steps that run first, in the same script: (verb, command, reason).
    let mut before = Vec::new();
    match &place {
        Place::Host => {
            before.extend(install(root, pkg).map(|(c, why)| ("install", c.to_owned(), why)))
        }
        Place::Service(..) => {
            up(stack)?;
            if name == TaskName::Test
                && let Some(m) = pkg.task(TaskName::Migrate)
                && !postgres::empty(data, &stack.project.0).is_empty()
            {
                before.push(("migrate", m.cmd.clone(), "empty database"));
            }
        }
    }
    let io = |e: std::io::Error| {
        Error::new(
            ErrorCode::RuntimeUnavailable,
            format!("cannot run the task: {e}"),
            "rizr keeps task logs in its data directory; check that it is writable",
            format!("ls -l {}", data.display()),
        )
    };
    std::fs::create_dir_all(data).map_err(io)?;
    let log = log_path(data, name, &pkg.dir);
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .read(true)
        .open(&log)
        .map_err(io)?;
    let started = Instant::now();
    let (tx, rx) = channel();
    // The task's stdout holds the lock: a later call that finds it taken waits for that run instead of a second one.
    let attached = match file.try_lock() {
        Ok(()) => {
            file.set_len(0).map_err(io)?;
            file.rewind().map_err(io)?;
            eprintln!(
                "rizr: {} in {}{}: {} (from {})",
                verb(name),
                pkg.dir,
                place,
                task.cmd,
                task.from
            );
            let mut cmd = format!("({})", task.cmd);
            for (v, c, why) in before.iter().rev() {
                eprintln!("rizr: {v} in {}{place}: {c} ({why})", pkg.dir);
                cmd = format!("{c} && {cmd}");
            }
            let script = format!("{cmd}\nprintf '\\n{EXIT_MARK}%s\\n' \"$?\"");
            let out = file.try_clone().map_err(io)?;
            let err = file.try_clone().map_err(io)?;
            let mut c = match &place {
                Place::Host => {
                    let mut c = Command::new("sh");
                    c.current_dir(root.join(&pkg.dir));
                    c
                }
                Place::Service(svc, mount) => {
                    let mut c = stack.compose();
                    c.args(["run", "--rm", "--no-deps", "-T"]);
                    if let Some(m) = mount {
                        c.args(["-v", m]);
                    }
                    c.args([svc.as_str(), "sh"]);
                    c
                }
            };
            let mut child = c
                .args(["-c", &script])
                .stdin(Stdio::null())
                .stdout(out)
                .stderr(err)
                .spawn()
                .map_err(io)?;
            drop(file);
            std::thread::spawn(move || {
                let _ = child.wait();
                let _ = tx.send(());
            });
            false
        }
        Err(TryLockError::WouldBlock) => {
            eprintln!(
                "rizr: a former `rizr {}` still runs; waiting for it",
                verb(name)
            );
            std::thread::spawn(move || {
                let _ = File::lock(&file);
                let _ = tx.send(());
            });
            true
        }
        Err(TryLockError::Error(e)) => return Err(io(e)),
    };
    match rx.recv_timeout(BUDGET) {
        Ok(()) | Err(RecvTimeoutError::Disconnected) => {}
        Err(RecvTimeoutError::Timeout) => {
            return Err(Error::new(
                ErrorCode::StillRunning,
                format!(
                    "{} in {} still runs after {} s; its output goes to {}",
                    verb(name),
                    pkg.dir,
                    BUDGET.as_secs(),
                    log.display()
                ),
                "it keeps running; the same command again waits up to 100 s more and starts no second run",
                format!(
                    "rizr {}",
                    std::env::args().skip(1).collect::<Vec<_>>().join(" ")
                ),
            ));
        }
    }
    let out = String::from_utf8_lossy(&std::fs::read(&log).map_err(io)?).into_owned();
    let exit = out
        .lines()
        .rev()
        .find_map(|l| l.strip_prefix(EXIT_MARK))
        .and_then(|c| c.trim().parse::<i32>().ok())
        .unwrap_or(-1);
    let body: Vec<&str> = out.lines().filter(|l| !l.starts_with(EXIT_MARK)).collect();
    let summary = summarize(&out);
    let tail: Vec<&str> = body.iter().rev().take(40).rev().copied().collect();
    let seconds = started.elapsed().as_secs();
    if json {
        let r = Receipt {
            task: name,
            package: &pkg.dir,
            place: match &place {
                Place::Host => "host".to_owned(),
                Place::Service(svc, _) => format!("service {svc}"),
            },
            cmd: &task.cmd,
            source: task.source,
            from: &task.from,
            exit,
            seconds,
            summary: &summary,
            tail: tail.clone(),
            log: &log,
        };
        println!("{}", serde_json::to_string(&r).unwrap_or_default());
    }
    let counts = if summary.counted {
        format!(
            "pass {}, fail {}, skip {}",
            summary.passed, summary.failed, summary.skipped
        )
    } else {
        format!("exit {exit}")
    };
    let when = if attached {
        ", a former call's run"
    } else {
        ""
    };
    let line = format!(
        "{} {}: {counts} ({}, {seconds} s{when})",
        verb(name),
        pkg.dir,
        task.cmd
    );
    if exit == 0 {
        if !json {
            println!("{line}");
        }
        return Ok(());
    }
    if !json {
        println!("{line}");
        for f in &summary.failures {
            println!("  failed: {f}");
        }
        println!("--- last lines (full log: {})", log.display());
        for l in &tail {
            println!("{l}");
        }
    }
    let (message, hint) = if exit == 127 || out.contains(": not found") && body.len() < 5 {
        (
            format!(
                "{} in {}: the command was not found: {}",
                verb(name),
                pkg.dir,
                task.cmd
            ),
            "install the tool the command names, or run `rizr doctor`".to_owned(),
        )
    } else if summary.failed > 0 {
        (
            format!("{} in {}: {} failed", verb(name), pkg.dir, summary.failed),
            "fix the failing tests named above; the same command reruns them".to_owned(),
        )
    } else {
        (
            format!("{} in {} exited {exit}", verb(name), pkg.dir),
            "read the last lines above".to_owned(),
        )
    };
    Err(Error::new(
        ErrorCode::TaskFailed,
        message,
        hint,
        format!(
            "rizr {}",
            std::env::args().skip(1).collect::<Vec<_>>().join(" ")
        ),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_and_failures_of_each_runner() {
        let cargo = "running 3 tests\ntest a::b ... ok\ntest a::c ... FAILED\ntest result: FAILED. 2 passed; 1 failed; 1 ignored; 0 measured\n\ntest result: ok. 4 passed; 0 failed; 0 ignored\n";
        let s = summarize(cargo);
        assert_eq!((s.passed, s.failed, s.skipped), (6, 1, 1));
        assert_eq!(s.failures, ["a::c"]);
        let pytest = "FAILED tests/test_x.py::test_y - assert 1 == 2\n==== 1 failed, 10 passed, 2 skipped in 0.52s ====\n";
        let s = summarize(pytest);
        assert_eq!((s.passed, s.failed, s.skipped), (10, 1, 2));
        assert_eq!(s.failures, ["tests/test_x.py::test_y"]);
        let jest = "● math › adds\nTests:       1 failed, 5 passed, 6 total\n";
        let s = summarize(jest);
        assert_eq!((s.passed, s.failed), (5, 1));
        assert_eq!(s.failures, ["math › adds"]);
        let vitest = " Test Files  1 passed (1)\n      Tests  7 passed | 1 skipped (8)\n";
        let s = summarize(vitest);
        assert_eq!((s.passed, s.failed, s.skipped), (7, 0, 1));
        let go = "--- FAIL: TestParse (0.00s)\n--- PASS: TestOk (0.00s)\nFAIL\nFAIL\texample.com/x\t0.01s\n";
        let s = summarize(go);
        assert_eq!((s.passed, s.failed), (1, 1));
        assert_eq!(s.failures, ["TestParse"]);
        assert!(!summarize("compiling...\nerror: x\n").counted);
    }
}
