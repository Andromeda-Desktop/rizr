//! The confirm channel: under a grant, an action that needs the human waits for `rizr confirm` on the host.
//! A pending ask is `<id>.json` plus `<id>.sock` in `$XDG_RUNTIME_DIR/rizr/confirm/`; a grant sandbox cannot reach it.

use std::io::{BufRead, BufReader, IsTerminal, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use rizr_core::error::{Error, ErrorCode};
use serde::{Deserialize, Serialize};

pub const TIMEOUT: Duration = Duration::from_secs(600);

#[derive(Serialize, Deserialize, Debug)]
pub struct Ask {
    pub grant: String,
    pub verb: String,
    pub target: String,
    pub summary: String,
    #[serde(default)]
    pub files: Vec<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Answer {
    Allow,
    Deny,
    Timeout,
}

fn dir() -> Result<PathBuf, String> {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|v| !v.is_empty())
        .ok_or("XDG_RUNTIME_DIR is not set")?;
    let d = PathBuf::from(base).join("rizr").join("confirm");
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&d)
        .map_err(|e| format!("cannot make {}: {e}", d.display()))?;
    Ok(d)
}

/// Posts the ask and blocks until the human answers or `timeout` passes. Any setup failure counts as Deny.
pub fn request(ask: &Ask, timeout: Duration) -> Answer {
    match post(ask) {
        Ok((listener, base)) => {
            eprintln!(
                "waiting for the human: `rizr confirm` on the host ({})",
                ask.summary
            );
            let answer = wait(&listener, timeout);
            let _ = std::fs::remove_file(base.with_extension("json"));
            let _ = std::fs::remove_file(base.with_extension("sock"));
            answer
        }
        Err(e) => {
            eprintln!("cannot ask the human: {e}");
            Answer::Deny
        }
    }
}

fn post(ask: &Ask) -> Result<(UnixListener, PathBuf), String> {
    let base = dir()?.join(format!(
        "{}-{}",
        ask.grant,
        crate::devenv::random_hex()?.get(..8).unwrap_or("0")
    ));
    let sock = base.with_extension("sock");
    let listener =
        UnixListener::bind(&sock).map_err(|e| format!("cannot bind {}: {e}", sock.display()))?;
    std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| e.to_string())?;
    let json = base.with_extension("json");
    let tmp = base.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(ask).unwrap_or_default())
        .and_then(|()| std::fs::rename(&tmp, &json))
        .map_err(|e| format!("cannot write {}: {e}", json.display()))?;
    Ok((listener, base))
}

fn wait(listener: &UnixListener, timeout: Duration) -> Answer {
    let mut pfd = libc::pollfd {
        fd: listener.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let ms = libc::c_int::try_from(timeout.as_millis()).unwrap_or(libc::c_int::MAX);
    // SAFETY: one valid pollfd that lives for the call.
    if unsafe { libc::poll(&mut pfd, 1, ms) } <= 0 {
        return Answer::Timeout;
    }
    let Ok((conn, _)) = listener.accept() else {
        return Answer::Deny;
    };
    let _ = conn.set_read_timeout(Some(Duration::from_secs(5)));
    let mut line = String::new();
    let _ = BufReader::new((&conn).take(16)).read_line(&mut line);
    if line.trim() == "allow" {
        Answer::Allow
    } else {
        Answer::Deny
    }
}

/// `rizr confirm`: the human answers each pending ask on the terminal.
pub fn run() -> Result<(), Error> {
    if !(std::io::stdin().is_terminal() && std::io::stderr().is_terminal()) {
        return Err(Error::new(
            ErrorCode::Usage,
            "rizr confirm needs a terminal",
            "run it in your own terminal on the host",
            "rizr confirm",
        ));
    }
    let d = dir().map_err(|e| {
        Error::new(
            ErrorCode::Usage,
            e,
            "run it in your login session",
            "rizr confirm",
        )
    })?;
    let mut asks: Vec<PathBuf> = std::fs::read_dir(&d)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    asks.sort();
    if asks.is_empty() {
        eprintln!("no pending asks");
    }
    for json in asks {
        let Some(ask) = std::fs::read(&json)
            .ok()
            .and_then(|b| serde_json::from_slice::<Ask>(&b).ok())
        else {
            continue;
        };
        eprintln!(
            "grant {} asks: {} {}\n  {}",
            ask.grant, ask.verb, ask.target, ask.summary
        );
        for f in &ask.files {
            eprintln!("  {f}");
        }
        eprint!("Allow? [y/N] ");
        let _ = std::io::stderr().flush();
        let mut line = String::new();
        let _ = std::io::stdin().lock().read_line(&mut line);
        let allow = matches!(line.trim(), "y" | "Y" | "yes");
        answer(&json.with_extension("sock"), allow);
    }
    Ok(())
}

fn answer(sock: &Path, allow: bool) {
    match UnixStream::connect(sock) {
        Ok(mut c) => {
            let _ = writeln!(c, "{}", if allow { "allow" } else { "deny" });
        }
        Err(_) => eprintln!("  the ask ended before the answer (timed out or the verb stopped)"),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "a test panics on a broken precondition")]
mod tests {
    use super::*;

    fn ask() -> Ask {
        Ask {
            grant: "t".into(),
            verb: "trust".into(),
            target: "/r".into(),
            summary: "s".into(),
            files: vec![],
        }
    }

    #[test]
    fn the_human_answer_reaches_the_waiting_verb() {
        let run = tempfile::tempdir().unwrap();
        // SAFETY: the tests in this module are the only ones that read XDG_RUNTIME_DIR.
        unsafe { std::env::set_var("XDG_RUNTIME_DIR", run.path()) };
        for (reply, want) in [(true, Answer::Allow), (false, Answer::Deny)] {
            let d = run.path().join("rizr/confirm");
            let human = std::thread::spawn(move || {
                loop {
                    let sock = std::fs::read_dir(&d)
                        .into_iter()
                        .flatten()
                        .flatten()
                        .map(|e| e.path())
                        .find(|p| p.extension().is_some_and(|x| x == "json"));
                    if let Some(p) = sock {
                        answer(&p.with_extension("sock"), reply);
                        return;
                    }
                    std::thread::yield_now();
                }
            });
            assert_eq!(request(&ask(), Duration::from_secs(10)), want);
            human.join().unwrap();
        }
        assert_eq!(request(&ask(), Duration::from_millis(50)), Answer::Timeout);
        assert_eq!(
            std::fs::read_dir(run.path().join("rizr/confirm"))
                .unwrap()
                .count(),
            0
        );
    }
}
