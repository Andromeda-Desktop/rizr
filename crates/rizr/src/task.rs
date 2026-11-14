//! `rizr test` and `rizr build`: run a detected task of a package and report its result in one call.

use std::fs::{File, OpenOptions, TryLockError};
use std::io::Seek;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{RecvTimeoutError, channel};
use std::time::{Duration, Instant};

use rizr_core::code::{Package, TaskName};
use rizr_core::error::{Error, ErrorCode};
use serde::Serialize;

use crate::detect;

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

/// Runs the task, or waits for the same task that a former call left running. Returns within 100 s.
pub fn run(
    root: &Path,
    data: &Path,
    name: TaskName,
    dir: Option<&str>,
    json: bool,
) -> Result<(), Error> {
    let packages = detect::detect(&detect::read(root));
    let pkg = pick(&packages, name, dir)?;
    let Some(task) = pkg.task(name) else {
        return Ok(());
    };
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
                "rizr: {} in {}: {} (from {})",
                verb(name),
                pkg.dir,
                task.cmd,
                task.from
            );
            let script = format!("{}\nprintf '\\n{EXIT_MARK}%s\\n' \"$?\"", task.cmd);
            let out = file.try_clone().map_err(io)?;
            let err = file.try_clone().map_err(io)?;
            let mut child = Command::new("sh")
                .args(["-c", &script])
                .current_dir(root.join(&pkg.dir))
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
