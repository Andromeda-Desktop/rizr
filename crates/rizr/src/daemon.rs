//! The grant daemon: a sandboxed agent runs rizr verbs in one worktree through a socket, with no host access.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs::Permissions;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{ExitCode, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use clap::Parser;
use rizr_core::Action;
use rizr_core::error::{Error, ErrorCode};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{Cli, Command};

const LINE_CAP: u64 = 64 * 1024;
/// A client that sends no full request line in this time loses its turn, so it cannot hold the grant.
const READ_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(clap::Subcommand)]
pub enum AgentOp {
    /// Open a socket that runs rizr in this worktree; prints the bwrap flags for the agent sandbox.
    Grant { path: Option<PathBuf> },
    /// Close a grant's socket.
    Revoke { id: String },
    /// List the open grants.
    List,
}

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
enum Control {
    Grant { worktree: PathBuf },
    Revoke { id: String },
    List,
}

#[derive(Deserialize)]
struct Request {
    argv: Vec<String>,
}

struct Daemon {
    dir: PathBuf,
    store: PathBuf,
    /// Grant id -> canonical worktree.
    grants: Mutex<BTreeMap<String, PathBuf>>,
}

fn run_dir() -> Result<PathBuf, Error> {
    std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|v| !v.is_empty())
        .map(|v| PathBuf::from(v).join("rizr"))
        .ok_or_else(|| {
            Error::new(
                ErrorCode::Usage,
                "XDG_RUNTIME_DIR is not set",
                "run the daemon and `rizr agent` in your login session",
                "rizr daemon",
            )
        })
}

fn grant_socket(dir: &Path, id: &str) -> PathBuf {
    dir.join("grants").join(format!("{id}.sock"))
}

fn env_error(message: String) -> Error {
    Error::new(
        ErrorCode::RuntimeUnavailable,
        message,
        "check that XDG_RUNTIME_DIR and the rizr data directory are writable",
        "rizr daemon",
    )
}

fn bind(path: &Path) -> Result<UnixListener, String> {
    let _ = std::fs::remove_file(path);
    let l = UnixListener::bind(path).map_err(|e| format!("cannot bind {}: {e}", path.display()))?;
    std::fs::set_permissions(path, Permissions::from_mode(0o600))
        .map_err(|e| format!("cannot chmod {}: {e}", path.display()))?;
    Ok(l)
}

/// `rizr daemon`: serve the control socket and every stored grant, in the foreground.
pub fn serve(data: &Path) -> Result<(), Error> {
    let dir = run_dir()?;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir.join("grants"))
        .and_then(|()| std::fs::set_permissions(&dir, Permissions::from_mode(0o700)))
        .map_err(|e| env_error(format!("cannot make {}: {e}", dir.display())))?;
    let control = dir.join("daemon.sock");
    if UnixStream::connect(&control).is_ok() {
        return Err(Error::new(
            ErrorCode::Usage,
            format!("a rizr daemon already serves {}", control.display()),
            "use the running daemon",
            "rizr agent list",
        ));
    }
    let listener = bind(&control).map_err(env_error)?;
    let store = data.join("grants.json");
    let grants: BTreeMap<String, PathBuf> = match std::fs::read(&store) {
        Ok(b) => serde_json::from_slice(&b)
            .map_err(|e| env_error(format!("{} is not valid: {e}", store.display())))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => BTreeMap::new(),
        Err(e) => return Err(env_error(format!("cannot read {}: {e}", store.display()))),
    };
    let ids: Vec<String> = grants.keys().cloned().collect();
    let d = Arc::new(Daemon {
        dir,
        store,
        grants: Mutex::new(grants),
    });
    for id in ids {
        listen(&d, id).map_err(env_error)?;
    }
    eprintln!("rizr daemon: {}", control.display());
    for conn in listener.incoming().flatten() {
        let d = d.clone();
        std::thread::spawn(move || {
            let reply = read_line(&conn)
                .and_then(|l| serde_json::from_str::<Control>(&l).map_err(|e| e.to_string()))
                .and_then(|op| control_op(&d, op))
                .unwrap_or_else(|e| json!({ "error": e }));
            let _ = writeln!(&conn, "{reply}");
        });
    }
    Ok(())
}

impl Daemon {
    fn save(&self, grants: &BTreeMap<String, PathBuf>) -> Result<(), String> {
        let tmp = self.store.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(grants).unwrap_or_default())
            .and_then(|()| std::fs::rename(&tmp, &self.store))
            .map_err(|e| format!("cannot write {}: {e}", self.store.display()))
    }
}

fn control_op(d: &Arc<Daemon>, op: Control) -> Result<Value, String> {
    match op {
        Control::Grant { worktree } => {
            let w = worktree
                .canonicalize()
                .map_err(|e| format!("{}: {e}", worktree.display()))?;
            if !w.is_dir() {
                return Err(format!("{} is not a directory", w.display()));
            }
            let id = crate::devenv::random_hex()?[..16].to_string();
            {
                let mut g = d.grants.lock().unwrap_or_else(|p| p.into_inner());
                g.insert(id.clone(), w.clone());
                if let Err(e) = d.save(&g) {
                    g.remove(&id);
                    return Err(e);
                }
            }
            let socket = listen(d, id.clone())?;
            Ok(json!({ "id": id, "worktree": w, "socket": socket }))
        }
        Control::Revoke { id } => {
            {
                let mut g = d.grants.lock().unwrap_or_else(|p| p.into_inner());
                if g.remove(&id).is_none() {
                    return Err(format!("no grant {id}"));
                }
                d.save(&g)?;
            }
            let path = grant_socket(&d.dir, &id);
            // The grant's accept loop wakes on this connection, finds the grant gone and ends.
            let _ = UnixStream::connect(&path);
            let _ = std::fs::remove_file(&path);
            Ok(json!({ "id": id }))
        }
        Control::List => {
            let g = d.grants.lock().unwrap_or_else(|p| p.into_inner());
            let list: Vec<Value> = g
                .iter()
                .map(|(id, w)| json!({ "id": id, "worktree": w, "socket": grant_socket(&d.dir, id) }))
                .collect();
            Ok(json!({ "grants": list }))
        }
    }
}

/// Serves one grant socket. One thread per grant answers its requests in turn, so one verb runs at a time.
fn listen(d: &Arc<Daemon>, id: String) -> Result<PathBuf, String> {
    let path = grant_socket(&d.dir, &id);
    let l = bind(&path)?;
    let d = d.clone();
    std::thread::spawn(move || {
        for conn in l.incoming().flatten() {
            let worktree = d
                .grants
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .get(&id)
                .cloned();
            let Some(worktree) = worktree else { break };
            let reply = match read_line(&conn)
                .and_then(|l| serde_json::from_str::<Request>(&l).map_err(|e| e.to_string()))
            {
                Err(e) => reply_error(
                    &Error::new(
                        ErrorCode::Usage,
                        format!("bad request: {e}"),
                        "send one JSON line with the rizr arguments",
                        r#"{"argv":["status"]}"#,
                    ),
                    false,
                ),
                Ok(r) => match check(&r.argv) {
                    Err(e) => reply_error(&e, r.argv.iter().any(|a| a == "--json")),
                    Ok(()) => run_verb(&id, &worktree, &r.argv),
                },
            };
            let _ = writeln!(&conn, "{reply}");
        }
    });
    Ok(path)
}

fn read_line(conn: &UnixStream) -> Result<String, String> {
    conn.set_read_timeout(Some(READ_TIMEOUT))
        .map_err(|e| e.to_string())?;
    let mut line = String::new();
    BufReader::new(conn.take(LINE_CAP))
        .read_line(&mut line)
        .map_err(|e| e.to_string())?;
    if !line.ends_with('\n') {
        return Err(format!(
            "the request must be one line of at most {LINE_CAP} bytes"
        ));
    }
    Ok(line)
}

/// Verbs that change the host or trust stay with the human; `-f` would reach compose files outside the grant.
fn check(argv: &[String]) -> Result<(), Error> {
    let cli =
        match Cli::try_parse_from(std::iter::once("rizr").chain(argv.iter().map(String::as_str))) {
            Ok(c) => c,
            Err(e)
                if matches!(
                    e.kind(),
                    clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
                ) =>
            {
                return Ok(());
            }
            Err(e) => {
                let msg = e.to_string();
                let first = msg
                    .lines()
                    .next()
                    .unwrap_or("bad arguments")
                    .trim_start_matches("error: ");
                return Err(Error::new(
                    ErrorCode::Usage,
                    first,
                    "run `rizr --help` for the verbs",
                    "rizr status",
                ));
            }
        };
    if !cli.file.is_empty() {
        return Err(Error::new(
            ErrorCode::Usage,
            "-f is not allowed through a grant: it runs the worktree's own compose files",
            "drop -f",
            "rizr up",
        ));
    }
    let verb = match cli.command {
        Command::Action(Action::Trust { .. }) => "trust",
        Command::Eject { .. } => "eject",
        Command::Doctor => "doctor",
        Command::Footprint => "footprint",
        Command::Gc => "gc",
        Command::Daemon => "daemon",
        Command::Agent { .. } => "agent",
        Command::Confirm => "confirm",
        _ => return Ok(()),
    };
    Err(Error::new(
        ErrorCode::Refused,
        format!("`rizr {verb}` is not open to an agent grant"),
        "ask the human to run it on the host",
        format!("rizr {verb}"),
    ))
}

fn reply_error(e: &Error, json: bool) -> Value {
    let (stdout, stderr) = render(e, json);
    json!({ "exit": e.code.class().exit_code(), "stdout": stdout, "stderr": stderr })
}

fn render(e: &Error, json: bool) -> (String, String) {
    if json {
        (
            format!("{}\n", serde_json::to_string(e).unwrap_or_default()),
            String::new(),
        )
    } else {
        (
            String::new(),
            format!(
                "error: {}\nhint: {}\ntry: {}\n",
                e.message, e.hint, e.example
            ),
        )
    }
}

fn run_verb(id: &str, worktree: &Path, argv: &[String]) -> Value {
    let out = std::env::current_exe().and_then(|exe| {
        std::process::Command::new(exe)
            .args(argv)
            .current_dir(worktree)
            .env("RIZR_GRANT", id)
            .env_remove("RIZR_SOCKET")
            .stdin(Stdio::null())
            .output()
    });
    match out {
        Ok(o) => json!({
            "exit": o.status.code().unwrap_or(1),
            "stdout": String::from_utf8_lossy(&o.stdout),
            "stderr": String::from_utf8_lossy(&o.stderr),
        }),
        Err(e) => {
            json!({ "exit": 4, "stdout": "", "stderr": format!("error: the rizr daemon cannot start rizr: {e}\n") })
        }
    }
}

fn call(path: &Path, req: &Value) -> std::io::Result<Value> {
    let mut conn = UnixStream::connect(path)?;
    writeln!(conn, "{req}")?;
    let mut s = String::new();
    conn.read_to_string(&mut s)?;
    serde_json::from_str(&s).map_err(std::io::Error::other)
}

/// True for the verbs that run in this process even under RIZR_SOCKET.
pub fn local(argv: &[String]) -> bool {
    Cli::try_parse_from(std::iter::once("rizr").chain(argv.iter().map(String::as_str))).is_ok_and(
        |c| {
            matches!(
                c.command,
                Command::Daemon | Command::Agent { .. } | Command::Confirm
            )
        },
    )
}

/// The agent side: send the arguments to the grant socket and replay the answer.
pub fn forward(socket: &OsStr, argv: Vec<String>) -> ExitCode {
    let json = argv.iter().any(|a| a == "--json");
    let reply = call(Path::new(socket), &json!({ "argv": argv })).unwrap_or_else(|e| {
        reply_error(
            &Error::new(
                ErrorCode::RuntimeUnavailable,
                format!(
                    "cannot reach the rizr daemon at {}: {e}",
                    Path::new(socket).display()
                ),
                "the grant may be closed; ask the human to run `rizr agent grant` again",
                "rizr agent list",
            ),
            json,
        )
    });
    print!("{}", reply["stdout"].as_str().unwrap_or_default());
    eprint!("{}", reply["stderr"].as_str().unwrap_or_default());
    let code = reply["exit"]
        .as_u64()
        .and_then(|c| u8::try_from(c).ok())
        .unwrap_or(1);
    ExitCode::from(code)
}

/// `rizr agent grant|revoke|list`: the human side, through the control socket.
pub fn agent(op: &AgentOp, json: bool) -> Result<(), Error> {
    let req = match op {
        AgentOp::Grant { path } => {
            let p = path.clone().unwrap_or_else(|| PathBuf::from("."));
            let p = p.canonicalize().map_err(|e| {
                Error::new(
                    ErrorCode::Usage,
                    format!("{}: {e}", p.display()),
                    "name a worktree directory",
                    "rizr agent grant .",
                )
            })?;
            json!({ "op": "grant", "worktree": p })
        }
        AgentOp::Revoke { id } => json!({ "op": "revoke", "id": id }),
        AgentOp::List => json!({ "op": "list" }),
    };
    let control = run_dir()?.join("daemon.sock");
    let reply = call(&control, &req).map_err(|e| {
        Error::new(
            ErrorCode::RuntimeUnavailable,
            format!("no rizr daemon at {}: {e}", control.display()),
            "start the daemon in your session",
            "rizr daemon",
        )
    })?;
    if let Some(e) = reply["error"].as_str() {
        return Err(Error::new(
            ErrorCode::Usage,
            e,
            "list the grants",
            "rizr agent list",
        ));
    }
    if json {
        println!("{reply}");
        return Ok(());
    }
    match op {
        AgentOp::Grant { .. } => {
            let s = reply["socket"].as_str().unwrap_or_default();
            println!("grant {}", reply["id"].as_str().unwrap_or_default());
            println!("bwrap --bind {s} {s} --setenv RIZR_SOCKET {s}");
        }
        AgentOp::Revoke { id } => println!("revoked {id}"),
        AgentOp::List => {
            for g in reply["grants"].as_array().into_iter().flatten() {
                println!(
                    "{}  {}",
                    g["id"].as_str().unwrap_or_default(),
                    g["worktree"].as_str().unwrap_or_default()
                );
            }
        }
    }
    Ok(())
}
