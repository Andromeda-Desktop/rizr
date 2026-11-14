//! Rewind points: copies of an instance's postgres databases on their shared server, taken before each data job
//! and on demand (VISION: rewind). A copy that takes longer than the budget is skipped with a warning.

use std::path::Path;
use std::process::Command;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use rizr_core::error::{Error, ErrorCode};

use crate::docker::Stack;
use crate::postgres::{admin, clone, close};

const FILE: &str = "rewind.json";
/// Rewind points kept per database; a rewind or reset adds one more until the next job.
const KEEP: usize = 3;

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Entry {
    /// Unix seconds.
    pub at: u64,
    pub db: String,
    pub job: String,
    pub snapshot: String,
    pub ms: u64,
    pub ok: bool,
    pub note: String,
}

/// One database on a shared server.
pub struct Target {
    pub server: String,
    pub user: String,
    pub database: String,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// The copy budget in ms: `RIZR_REWIND_BUDGET_MS`, default 5000.
fn budget() -> u64 {
    std::env::var("RIZR_REWIND_BUDGET_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5000)
}

pub fn entries(dir: &Path) -> Vec<Entry> {
    std::fs::read(dir.join(FILE))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn record(dir: &Path, e: &Entry) {
    let mut all = entries(dir);
    all.push(e.clone());
    let skip = all.len().saturating_sub(50);
    let _ = std::fs::create_dir_all(dir);
    if let Ok(b) = serde_json::to_vec_pretty(&all[skip..]) {
        let _ = std::fs::write(dir.join(FILE), b);
    }
}

/// The rewind points of `t`, newest first.
fn points(t: &Target) -> Result<Vec<String>, String> {
    let prefix = format!("{}__rw_", t.database);
    let mut v: Vec<String> = admin(&t.server, &t.user, "select datname from pg_database", None)?
        .lines()
        .filter(|d| d.starts_with(&prefix))
        .map(str::to_owned)
        .collect();
    v.sort_by(|a, b| b.cmp(a));
    Ok(v)
}

/// Take a rewind point of `t` before `job`; with `prune`, keep only the newest `KEEP`.
pub fn take(t: &Target, dir: &Path, job: &str, prune: bool) -> Entry {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis());
    let snapshot = format!("{}__rw_{millis}", t.database);
    let start = Instant::now();
    let r = clone(&t.server, &t.user, &t.database, &snapshot, Some(budget()));
    let ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
    let note = match &r {
        Ok(()) => String::new(),
        Err(e) if e.contains("statement timeout") => {
            format!("the copy took over the {} ms budget", budget())
        }
        Err(e) => e.lines().next().unwrap_or_default().to_owned(),
    };
    if r.is_err() {
        let _ = admin(
            &t.server,
            &t.user,
            &format!("DROP DATABASE IF EXISTS \"{snapshot}\""),
            None,
        );
    } else if prune {
        for old in points(t).unwrap_or_default().iter().skip(KEEP) {
            let _ = admin(
                &t.server,
                &t.user,
                &format!("DROP DATABASE \"{old}\""),
                None,
            );
        }
    }
    let e = Entry {
        at: now(),
        db: t.database.clone(),
        job: job.to_owned(),
        snapshot,
        ms,
        ok: r.is_ok(),
        note,
    };
    record(dir, &e);
    e
}

/// The warning for a skipped rewind point.
pub fn warning(e: &Entry) -> String {
    format!(
        "warning: no rewind point for {} before {}: {}",
        e.db, e.job, e.note
    )
}

fn fail(message: String, hint: &str, example: &str) -> Error {
    Error::new(ErrorCode::Usage, message, hint, example)
}

/// This instance's shared databases, all or the one named by compose service or database name.
fn targets(stack: &Stack, db: Option<&str>) -> Result<Vec<Target>, Error> {
    let info = crate::postgres::Info::load(&stack.dir);
    let v: Vec<Target> = info
        .databases
        .iter()
        .filter(|(svc, d)| db.is_none_or(|n| n == svc.as_str() || n == d.database))
        .map(|(_, d)| Target {
            server: format!("rizr-{}-{}", stack.project.0, d.server),
            user: d.user.clone(),
            database: d.database.clone(),
        })
        .collect();
    if v.is_empty() {
        return Err(fail(
            format!(
                "no shared postgres database{} in this instance",
                db.map(|n| format!(" named {n}")).unwrap_or_default()
            ),
            "rizr up moves a compose postgres service to a shared server; context lists the databases",
            "rizr context",
        ));
    }
    Ok(v)
}

fn locked(stack: &Stack) -> Result<std::fs::File, Error> {
    stack.lock().map_err(|e| {
        Error::new(
            ErrorCode::RuntimeUnavailable,
            format!("cannot lock instance {}: {e}", stack.instance.0),
            "rizr keeps a lock file in its data directory; check that it is writable",
            format!("ls -l {}", stack.dir.display()),
        )
    })
}

/// `rizr snapshot`: a rewind point of each named database now.
pub fn snapshot(stack: &Stack, db: Option<&str>) -> Result<(), Error> {
    let _lock = locked(stack)?;
    let mut failed = None;
    for t in targets(stack, db)? {
        let e = take(&t, &stack.dir, "snapshot", true);
        if e.ok {
            println!("rewind point {} ({} ms)", e.snapshot, e.ms);
        } else {
            failed = Some(fail(
                format!("no rewind point for {}: {}", e.db, e.note),
                "the copy needs the database alone for a moment; RIZR_REWIND_BUDGET_MS raises the budget",
                "RIZR_REWIND_BUDGET_MS=30000 rizr snapshot",
            ));
        }
    }
    failed.map_or(Ok(()), Err)
}

/// Replace `t` with a copy of `from`: no session may stay on `t` while it goes.
fn replace(t: &Target, from: Option<&str>) -> Result<(), String> {
    close(&t.server, &t.user, &t.database)?;
    admin(
        &t.server,
        &t.user,
        &format!("DROP DATABASE \"{}\"", t.database),
        None,
    )?;
    let create = match from {
        Some(p) => format!("CREATE DATABASE \"{}\" TEMPLATE \"{p}\"", t.database),
        None => format!("CREATE DATABASE \"{}\"", t.database),
    };
    admin(&t.server, &t.user, &create, None).map(|_| ())
}

/// The current data becomes a rewind point first: no restore or reset goes over data without one.
fn keep_current(t: &Target, dir: &Path, why: &str) -> Result<Entry, Error> {
    let e = take(t, dir, why, false);
    if e.ok {
        Ok(e)
    } else {
        Err(fail(
            format!(
                "{why} stopped: cannot save the current data of {}: {}",
                e.db, e.note
            ),
            "RIZR_REWIND_BUDGET_MS raises the copy budget",
            "RIZR_REWIND_BUDGET_MS=30000 rizr rewind",
        ))
    }
}

/// `rizr rewind`: restore each named database to its `back`-th newest rewind point.
pub fn rewind(stack: &Stack, db: Option<&str>, back: usize) -> Result<(), Error> {
    let _lock = locked(stack)?;
    for t in targets(stack, db)? {
        let pts = points(&t).map_err(|e| fail(e, "the shared server must run", "rizr up"))?;
        let Some(p) = pts.get(back.saturating_sub(1)) else {
            return Err(fail(
                format!(
                    "{} has {} rewind point(s), so --back {back} has none",
                    t.database,
                    pts.len()
                ),
                "rizr takes one before each migrate or seed job, and `rizr snapshot` takes one now",
                "rizr snapshot",
            ));
        };
        let saved = keep_current(&t, &stack.dir, "rewind")?;
        replace(&t, Some(p)).map_err(|e| fail(e, "the shared server must run", "rizr up"))?;
        println!(
            "restored {} to {p}; the data before is rewind point {}",
            t.database, saved.snapshot
        );
    }
    remove_jobs(stack);
    println!("fix the migration, then rizr up (it reruns migrate and seed jobs)");
    Ok(())
}

/// `rizr reset`: empty each named database and redis, and remove the instance's finished job containers, so the
/// next `up` reruns migrate and seed jobs.
pub fn reset(stack: &Stack, db: Option<&str>) -> Result<(), Error> {
    let _lock = locked(stack)?;
    let redis: Vec<_> = crate::redis::running(&stack.compose_name())
        .into_iter()
        .filter(|r| db.is_none_or(|n| n == r.service))
        .collect();
    let pg = match targets(stack, db) {
        Err(_) if !redis.is_empty() => Vec::new(),
        r => r?,
    };
    for r in &redis {
        crate::redis::flush(r).map_err(|e| {
            fail(
                format!("cannot empty redis {}: {e}", r.service),
                "redis must run; a password set only in its command needs REDISCLI_AUTH in its environment",
                &format!("docker exec -it {} redis-cli", r.container),
            )
        })?;
        println!("emptied redis {} (FLUSHALL)", r.service);
    }
    for t in pg {
        let saved = keep_current(&t, &stack.dir, "reset")?;
        replace(&t, None).map_err(|e| fail(e, "the shared server must run", "rizr up"))?;
        println!(
            "emptied {}; the data before is rewind point {}",
            t.database, saved.snapshot
        );
    }
    remove_jobs(stack);
    println!("next: rizr up (it reruns migrate and seed jobs)");
    Ok(())
}

/// Remove the instance's exited containers: a job without its container counts as not run, so `up` reruns it.
fn remove_jobs(stack: &Stack) {
    let ids = Command::new("docker")
        .args(["ps", "-aq", "--filter=status=exited"])
        .arg(format!(
            "--filter=label=com.docker.compose.project={}",
            stack.compose_name()
        ))
        .output()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .split_whitespace()
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if !ids.is_empty() {
        let _ = Command::new("docker").args(["rm"]).args(&ids).output();
    }
}
