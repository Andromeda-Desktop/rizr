//! Trust gate (VISION section 7): before trust, rizr reads repo files only with its own parsers and starts no
//! repo tool. The store is outside the repo, keyed by the canonical repo path and the digest of the inputs.

use std::collections::BTreeMap;
use std::io::{BufRead, IsTerminal, Read, Write};
use std::path::{Component, Path, PathBuf};

use rizr_core::error::{Error, ErrorCode};
use rizr_core::model::Digest;
use serde_norway::Value;
use sha2::{Digest as _, Sha256};

/// Compose looks for these names; rizr hashes every one that exists.
const COMPOSE: [&str; 8] = [
    "compose.yaml",
    "compose.yml",
    "docker-compose.yaml",
    "docker-compose.yml",
    "compose.override.yaml",
    "compose.override.yml",
    "docker-compose.override.yaml",
    "docker-compose.override.yml",
];
/// rizr's own inputs, then the root files a detected task runs from (`rizr test`, `build`).
const PLAIN: [&str; 13] = [
    ".env",
    "rizr.toml",
    "rizr.lock",
    "package.json",
    "pyproject.toml",
    "Cargo.toml",
    "go.mod",
    "justfile",
    "Makefile",
    "Taskfile.yml",
    ".mise.toml",
    "mise.toml",
    ".tool-versions",
];
/// Larger inputs are refused: a trust input is a small text file, not a device or a data set.
const MAX_BYTES: u64 = 1 << 20;

pub struct Inputs {
    pub root: PathBuf,
    /// Present input files, relative to `root` where possible.
    pub files: Vec<PathBuf>,
    pub digest: Digest,
}

/// `chosen`: the compose files given with `-f`; they replace the default names.
pub fn inputs(root: &Path, chosen: &[PathBuf]) -> Result<Inputs, Error> {
    let root = root.canonicalize().map_err(|e| io_error(root, &e))?;
    let mut seen: BTreeMap<PathBuf, Option<Vec<u8>>> = BTreeMap::new();
    let compose: Vec<PathBuf> = if chosen.is_empty() {
        COMPOSE.iter().map(|n| root.join(n)).collect()
    } else {
        chosen.iter().map(|f| root.join(f)).collect()
    };
    let mut queue: Vec<(PathBuf, bool)> = compose
        .into_iter()
        .map(|p| (p, true))
        .chain(PLAIN.iter().map(|n| (root.join(n), false)))
        .collect();
    while let Some((path, compose)) = queue.pop() {
        let path = path.canonicalize().unwrap_or_else(|_| normalize(&path));
        if seen.contains_key(&path) {
            continue;
        }
        let bytes = read(&path)?;
        if compose && let Some(b) = &bytes {
            let dir = path.parent().unwrap_or(&root).to_path_buf();
            queue.extend(refs(&path, b)?.into_iter().map(|(r, c)| (dir.join(r), c)));
        }
        seen.insert(path, bytes);
    }
    let mut h = Sha256::new();
    for (path, bytes) in &seen {
        h.update(shown(&root, path).as_os_str().as_encoded_bytes());
        h.update([0]);
        match bytes {
            Some(b) => {
                h.update([1]);
                h.update((b.len() as u64).to_le_bytes());
                h.update(b);
            }
            None => h.update([0]),
        }
    }
    let digest = Digest(h.finalize().iter().map(|b| format!("{b:02x}")).collect());
    let files = seen
        .iter()
        .filter(|(_, b)| b.is_some())
        .map(|(p, _)| shown(&root, p))
        .collect();
    Ok(Inputs {
        root,
        files,
        digest,
    })
}

/// Files a compose file pulls in: `include` and `extends.file` (compose, followed) and `env_file` (plain).
/// A path with `$` needs interpolation, which runs only after trust; the compose text that holds it is hashed.
fn refs(path: &Path, bytes: &[u8]) -> Result<Vec<(String, bool)>, Error> {
    // `apply_merge` expands `<<: *anchor`: compose files often set `env_file` through one.
    let doc: Value = serde_norway::from_slice(bytes)
        .and_then(|mut v: Value| v.apply_merge().map(|()| v))
        .map_err(|e| {
            Error::new(
                ErrorCode::ImportFailed,
                format!("{} is not valid YAML: {e}", path.display()),
                "fix the compose file",
                "rizr trust",
            )
        })?;
    let mut out = Vec::new();
    for v in seq(doc.get("include")) {
        let p = v.get("path").unwrap_or(v);
        out.extend(seq(Some(p)).filter_map(Value::as_str).map(|s| (s, true)));
        out.extend(env_files(v.get("env_file")).map(|s| (s, false)));
    }
    if let Some(services) = doc.get("services").and_then(Value::as_mapping) {
        for svc in services.values() {
            out.extend(env_files(svc.get("env_file")).map(|s| (s, false)));
            if let Some(f) = svc
                .get("extends")
                .and_then(|e| e.get("file"))
                .and_then(Value::as_str)
            {
                out.push((f, true));
            }
        }
    }
    Ok(out
        .into_iter()
        .filter(|(s, _)| !s.contains('$'))
        .map(|(s, c)| (s.to_owned(), c))
        .collect())
}

fn seq(v: Option<&Value>) -> impl Iterator<Item = &Value> {
    match v {
        Some(Value::Sequence(s)) => s.iter().collect::<Vec<_>>(),
        Some(v @ (Value::String(_) | Value::Mapping(_))) => vec![v],
        _ => vec![],
    }
    .into_iter()
}

fn env_files(v: Option<&Value>) -> impl Iterator<Item = &str> {
    seq(v).filter_map(|e| e.as_str().or_else(|| e.get("path").and_then(Value::as_str)))
}

fn read(path: &Path) -> Result<Option<Vec<u8>>, Error> {
    let Ok(meta) = std::fs::metadata(path) else {
        return Ok(None);
    };
    if !meta.is_file() || meta.len() > MAX_BYTES {
        return Err(Error::new(
            ErrorCode::Untrusted,
            format!("{} is not a regular file under 1 MiB", path.display()),
            "a trust input must be a small text file",
            "rizr trust",
        ));
    }
    let mut buf = Vec::new();
    std::fs::File::open(path)
        .and_then(|f| f.take(MAX_BYTES + 1).read_to_end(&mut buf))
        .map_err(|e| io_error(path, &e))?;
    Ok(Some(buf))
}

fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            c => out.push(c),
        }
    }
    out
}

fn shown(root: &Path, path: &Path) -> PathBuf {
    path.strip_prefix(root).unwrap_or(path).to_path_buf()
}

fn io_error(path: &Path, e: &std::io::Error) -> Error {
    Error::new(
        ErrorCode::Usage,
        format!("cannot read {}: {e}", path.display()),
        "run rizr in the repo root",
        "cd <repo> && rizr up",
    )
}

/// The trust store: one JSON map from canonical repo path to the trusted digest, in the user's data directory.
/// Only the user's environment picks the directory; no repo file can.
pub fn store_path() -> Option<PathBuf> {
    let data = std::env::var_os("XDG_DATA_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))?;
    Some(data.join("rizr/trust.json"))
}

/// A trusted repo: the digest a person (or `trust --yes`) accepted, and the git commit checked out then.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
enum Entry {
    Pinned { digest: Digest, head: String },
    Digest(Digest),
}

impl Entry {
    fn digest(&self) -> &Digest {
        match self {
            Entry::Pinned { digest, .. } | Entry::Digest(digest) => digest,
        }
    }
}

/// The commit the worktree has checked out, read from `.git` files: running git before trust could start a
/// program the repo's git config names (fsmonitor, hooks).
fn git_head(root: &Path) -> Option<String> {
    let dot = root.join(".git");
    let gitdir = if dot.is_file() {
        let t = std::fs::read_to_string(&dot).ok()?;
        root.join(t.strip_prefix("gitdir:")?.trim())
    } else {
        dot
    };
    let head = std::fs::read_to_string(gitdir.join("HEAD")).ok()?;
    let Some(name) = head.trim().strip_prefix("ref:").map(str::trim) else {
        return Some(head.trim().to_owned());
    };
    let common = std::fs::read_to_string(gitdir.join("commondir"))
        .map_or_else(|_| gitdir.clone(), |c| gitdir.join(c.trim()));
    for d in [&gitdir, &common] {
        if let Ok(sha) = std::fs::read_to_string(d.join(name)) {
            return Some(sha.trim().to_owned());
        }
    }
    std::fs::read_to_string(common.join("packed-refs"))
        .ok()?
        .lines()
        .find_map(|l| {
            l.split_once(' ')
                .filter(|(_, r)| *r == name)
                .map(|(sha, _)| sha.to_owned())
        })
}

fn load(store: &Path) -> BTreeMap<String, Entry> {
    std::fs::read(store)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

pub fn is_trusted(store: &Path, i: &Inputs) -> bool {
    load(store)
        .get(&*i.root.to_string_lossy())
        .is_some_and(|e| *e.digest() == i.digest)
}

/// The files differ from the trusted ones, but the commit is the one a person trusted: the change was made in
/// this worktree (by the user or their agent), not pulled or checked out from elsewhere (user, 2026-11-14).
fn local_edit(store: &Path, i: &Inputs) -> bool {
    matches!(load(store).get(&*i.root.to_string_lossy()), Some(Entry::Pinned { head, .. }) if git_head(&i.root).as_ref() == Some(head))
}

pub fn grant(store: &Path, i: &Inputs) -> std::io::Result<()> {
    if let Some(dir) = store.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // Two repos trusted at once must not drop each other's entry: read, change and write under one lock.
    let lock = std::fs::File::create(store.with_extension("lock"))?;
    lock.lock()?;
    let mut map = load(store);
    let entry = match git_head(&i.root) {
        Some(head) => Entry::Pinned {
            digest: i.digest.clone(),
            head,
        },
        None => Entry::Digest(i.digest.clone()),
    };
    map.insert(i.root.to_string_lossy().into_owned(), entry);
    let tmp = store.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(&map)?)?;
    std::fs::rename(tmp, store)
}

/// A person trusted this repo once; its files may have changed since.
fn known(store: &Path, i: &Inputs) -> bool {
    load(store).contains_key(&*i.root.to_string_lossy())
}

/// `rizr trust --yes`: trusts the present files without a terminal, but only in a repo a person trusted before.
/// The first trust of a repo always needs a person (user, 2026-11-13).
pub fn retrust(root: &Path, chosen: &[PathBuf]) -> Result<Inputs, Error> {
    let i = inputs(root, chosen)?;
    let store = store_path().ok_or_else(|| {
        Error::new(
            ErrorCode::Usage,
            "neither XDG_DATA_HOME nor HOME is set",
            "rizr keeps the trust store in the user's data directory",
            "HOME=/home/me rizr up",
        )
    })?;
    if !known(&store, &i) {
        return Err(untrusted(&i, chosen, false));
    }
    grant(&store, &i).map_err(|e| io_error(&store, &e))?;
    Ok(i)
}

/// The gate before any verb that reads or runs repo content. Asks on a terminal; refuses without one.
pub fn gate(root: &Path, chosen: &[PathBuf], ask: bool) -> Result<Inputs, Error> {
    let i = inputs(root, chosen)?;
    let store = store_path().ok_or_else(|| {
        Error::new(
            ErrorCode::Usage,
            "neither XDG_DATA_HOME nor HOME is set",
            "rizr keeps the trust store in the user's data directory",
            "HOME=/home/me rizr up",
        )
    })?;
    if is_trusted(&store, &i) {
        return Ok(i);
    }
    if local_edit(&store, &i) {
        grant(&store, &i).map_err(|e| io_error(&store, &e))?;
        eprintln!(
            "note: running local edits to the trusted files ({}); the git commit is the one trusted",
            i.files
                .iter()
                .map(|f| f.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
        return Ok(i);
    }
    let known = known(&store, &i);
    let terminal = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
    if !ask || !terminal {
        return Err(untrusted(&i, chosen, known));
    }
    eprintln!(
        "rizr has not run {} with these files yet:",
        i.root.display()
    );
    for f in &i.files {
        eprintln!("  {}", f.display());
    }
    eprintln!(
        "Trusting lets rizr run compose with them: build steps and containers they define.\n\
         The same as running `docker compose up` by hand. Any change to these files asks again."
    );
    eprint!("Trust and continue? [y/N] ");
    let _ = std::io::stderr().flush();
    let mut line = String::new();
    let _ = std::io::stdin().lock().read_line(&mut line);
    if !matches!(line.trim(), "y" | "Y" | "yes") {
        return Err(untrusted(&i, chosen, known));
    }
    grant(&store, &i).map_err(|e| io_error(&store, &e))?;
    Ok(i)
}

fn untrusted(i: &Inputs, chosen: &[PathBuf], known: bool) -> Error {
    let trust: String = chosen
        .iter()
        .map(|f| format!("-f {} ", f.display()))
        .chain([if known { "trust --yes" } else { "trust" }.into()])
        .collect();
    let files: Vec<_> = i.files.iter().map(|f| f.display().to_string()).collect();
    let again = format!(
        "rizr {trust} && {}",
        std::iter::once("rizr".to_owned())
            .chain(std::env::args().skip(1))
            .collect::<Vec<_>>()
            .join(" ")
    );
    Error::new(
        ErrorCode::Untrusted,
        format!(
            "{} is not trusted; rizr ran nothing (inputs: {})",
            i.root.display(),
            if files.is_empty() {
                "none".into()
            } else {
                files.join(", ")
            }
        ),
        if known {
            format!(
                "these files are new or changed since this repo was trusted. If you or your task chose or changed them, run this one command: `{again}`. Read `git diff` first only if you did not"
            )
        } else {
            format!("a human reviews the files, then runs `rizr {trust}` in a terminal")
        },
        if known {
            again
        } else {
            format!("rizr {trust}")
        },
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "a test panics on a broken precondition")]
mod tests {
    use super::*;

    #[test]
    fn a_change_to_any_input_asks_again() {
        let repo = tempfile::tempdir().unwrap();
        let store = repo.path().join("store/trust.json");
        let w = |p: &str, s: &str| std::fs::write(repo.path().join(p), s).unwrap();
        std::fs::create_dir(repo.path().join("sub")).unwrap();
        w(
            "compose.yml",
            "include: [sub/inc.yml]\nx-base: &base\n  env_file: [a.env]\nservices:\n  a:\n    <<: *base\n    image: x\n",
        );
        w(
            "sub/inc.yml",
            "services:\n  b:\n    image: y\n    env_file: b.env\n",
        );
        w("a.env", "A=1\n");
        w("sub/b.env", "B=1\n");
        grant(&store, &inputs(repo.path(), &[]).unwrap()).unwrap();
        assert!(is_trusted(&store, &inputs(repo.path(), &[]).unwrap()));

        for (file, text) in [
            ("a.env", "A=2\n"),
            ("sub/b.env", "B=2\n"),
            ("sub/inc.yml", "services: {}\n"),
            (".env", "COMPOSE_FILE=evil.yml\n"),
        ] {
            w(file, text);
            assert!(
                !is_trusted(&store, &inputs(repo.path(), &[]).unwrap()),
                "{file}"
            );
            grant(&store, &inputs(repo.path(), &[]).unwrap()).unwrap();
        }
    }
}
