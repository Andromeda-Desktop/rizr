//! Dev env input for a repo that does not run on a fresh clone (scratch/d0-repos.md, finding). Everything goes
//! in rizr's data directory for the instance, never in the repo:
//! - `dotenv`: a copy of `.env.example` (or a sibling name) when the repo has no `.env`;
//! - `env-override.yml`: marks a missing `env_file` that has an example optional and adds the example copy;
//! - `dev.env`: generated values for unset required variables. Values stay once written: a database keeps its
//!   password across runs.

use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};

use serde_json::json;

const EXAMPLES: [&str; 4] = [".example", ".sample", ".dist", ".template"];

/// Global compose arguments: env files for interpolation, then the env override file.
pub fn args(root: &Path, dir: &Path) -> Vec<OsString> {
    let mut out = Vec::new();
    let repo_env = root.join(".env");
    let dotenv = if repo_env.is_file() {
        repo_env
    } else {
        dir.join("dotenv")
    };
    for f in [dotenv, dir.join("dev.env")] {
        if f.is_file() {
            out.extend(["--env-file".into(), f.into_os_string()]);
        }
    }
    let ov = dir.join("env-override.yml");
    if ov.is_file() {
        out.extend(["-f".into(), ov.into_os_string()]);
    }
    out
}

/// Writes `dotenv` and `env-override.yml` from the repo files. Runs after trust, before `compose config`.
pub fn prepare(root: &Path, dir: &Path, compose_files: &[PathBuf]) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let dotenv = dir.join("dotenv");
    if !root.join(".env").is_file()
        && !dotenv.is_file()
        && let Some(ex) = example(&root.join(".env"))
    {
        std::fs::copy(&ex, &dotenv).map_err(|e| e.to_string())?;
    }
    let mut services = serde_json::Map::new();
    for file in compose_files {
        let Ok(text) = std::fs::read(file) else {
            continue;
        };
        let Ok(doc) = serde_norway::from_slice::<serde_norway::Value>(&text)
            .and_then(|mut v| v.apply_merge().map(|()| v))
        else {
            continue;
        };
        let Some(svcs) = doc.get("services").and_then(|s| s.as_mapping()) else {
            continue;
        };
        for (name, svc) in svcs {
            let Some(name) = name.as_str() else { continue };
            let mut entries = Vec::new();
            for raw in env_files(svc.get("env_file")) {
                let path = root.join(&raw);
                if raw.contains('$') || path.is_file() {
                    continue;
                }
                let copy = if path.file_name().is_some_and(|n| n == ".env") && dotenv.is_file() {
                    Some(dotenv.clone())
                } else if let Some(ex) = example(&path) {
                    let copy = dir.join(format!("env-{name}-{}", sanitize(&raw)));
                    if !copy.is_file() {
                        std::fs::copy(&ex, &copy).map_err(|e| e.to_string())?;
                    }
                    Some(copy)
                } else {
                    None
                };
                // Without an example rizr cannot know the values: compose stops on the missing file (catalog).
                if let Some(c) = copy {
                    entries.push(json!({"path": raw, "required": false}));
                    entries.push(json!({"path": c}));
                }
            }
            if !entries.is_empty() {
                services.insert(name.into(), json!({"env_file": entries}));
            }
        }
    }
    let ov = dir.join("env-override.yml");
    if services.is_empty() {
        let _ = std::fs::remove_file(&ov);
        Ok(())
    } else {
        // JSON is YAML: compose reads it.
        std::fs::write(&ov, json!({"services": services}).to_string()).map_err(|e| e.to_string())
    }
}

/// Fills one unset required variable named in a `compose config` error. `Ok(false)`: the error is another one.
pub fn fill(dir: &Path, stderr: &str) -> Result<bool, String> {
    let Some(name) = stderr
        .split("required variable ")
        .nth(1)
        .and_then(|r| r.split(' ').next())
        .filter(|n| !n.is_empty())
    else {
        return Ok(false);
    };
    let dev = dir.join("dev.env");
    let upper = name.to_ascii_uppercase();
    let value = if ["SECRET", "KEY", "TOKEN", "PASSWORD", "SALT"]
        .iter()
        .any(|w| upper.contains(w))
    {
        random_hex()?
    } else if upper.ends_with("_URL") {
        "http://localhost".to_owned()
    } else {
        return Err(format!(
            "the compose file needs `{name}`, and rizr cannot choose a value; add `{name}=<value>` to {}",
            dev.display()
        ));
    };
    let mut text = std::fs::read_to_string(&dev).unwrap_or_default();
    if text.lines().any(|l| l.split('=').next() == Some(name)) {
        return Err(format!(
            "`{name}` is in {} but compose still reports it unset",
            dev.display()
        ));
    }
    text.push_str(&format!("{name}={value}\n"));
    std::fs::write(&dev, text).map_err(|e| e.to_string())?;
    Ok(true)
}

fn example(path: &Path) -> Option<PathBuf> {
    let name = path.file_name()?.to_string_lossy().into_owned();
    EXAMPLES
        .iter()
        .map(|s| path.with_file_name(format!("{name}{s}")))
        .chain(std::iter::once(
            path.with_file_name(format!("example{name}")),
        ))
        .chain(std::iter::once(path.with_file_name(format!(
            "example.{}",
            name.trim_start_matches('.')
        ))))
        .find(|p| p.is_file())
}

fn env_files(v: Option<&serde_norway::Value>) -> Vec<String> {
    let one = |e: &serde_norway::Value| {
        e.as_str()
            .or_else(|| e.get("path").and_then(|p| p.as_str()))
            .map(str::to_owned)
    };
    match v {
        Some(serde_norway::Value::Sequence(s)) => s.iter().filter_map(one).collect(),
        Some(e) => one(e).into_iter().collect(),
        None => vec![],
    }
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

pub(crate) fn random_hex() -> Result<String, String> {
    let mut b = [0u8; 24];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut b))
        .map_err(|e| format!("no random source: {e}"))?;
    Ok(crate::compose::hex(&b))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "a test panics on a broken precondition")]
mod tests {
    use super::*;

    #[test]
    fn secrets_are_generated_once_and_others_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let err = |v: &str| {
            format!("error while interpolating x: required variable {v} is missing a value: x")
        };
        assert!(fill(dir.path(), &err("APP_SECRET")).unwrap());
        let first = std::fs::read_to_string(dir.path().join("dev.env")).unwrap();
        assert!(first.starts_with("APP_SECRET=") && first.trim().len() > "APP_SECRET=".len() + 32);
        assert!(
            fill(dir.path(), &err("APP_SECRET")).is_err(),
            "a second fill would loop"
        );
        assert!(
            fill(dir.path(), &err("ADMIN_EMAIL"))
                .unwrap_err()
                .contains("ADMIN_EMAIL")
        );
        assert!(!fill(dir.path(), "service \"x\" has no image").unwrap());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("dev.env")).unwrap(),
            first
        );
    }
}
