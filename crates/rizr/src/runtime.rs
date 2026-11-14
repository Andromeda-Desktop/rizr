//! The container engine. rizr drives docker and podman with the same docker CLI and compose plugin: podman
//! serves the docker API on its own socket, so every call, event and format stays the same.
//! Choice: `RIZR_RUNTIME=docker|podman`; else an explicit `DOCKER_HOST`; else the docker socket; else podman's.

use std::path::PathBuf;
use std::sync::OnceLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Engine {
    Docker,
    Podman,
}

impl Engine {
    pub fn name(self) -> &'static str {
        match self {
            Self::Docker => "docker",
            Self::Podman => "podman",
        }
    }
}

static ENGINE: OnceLock<Engine> = OnceLock::new();

/// The engine `select` chose (docker before it ran).
pub fn current() -> Engine {
    ENGINE.get().copied().unwrap_or(Engine::Docker)
}

/// Podman's API socket: the user's (rootless) first, then the system one.
pub fn podman_socket() -> Option<PathBuf> {
    let user =
        std::env::var_os("XDG_RUNTIME_DIR").map(|d| PathBuf::from(d).join("podman/podman.sock"));
    user.into_iter()
        .chain([PathBuf::from("/run/podman/podman.sock")])
        .find(|p| p.exists())
}

/// Choose the engine and point the docker CLI at it. Must run before any thread starts: it may set `DOCKER_HOST`.
/// Returns the reason when `RIZR_RUNTIME=podman` names a podman that does not serve its socket.
pub fn select() -> Result<Engine, String> {
    let asked = std::env::var("RIZR_RUNTIME").unwrap_or_default();
    let host = std::env::var("DOCKER_HOST").unwrap_or_default();
    let engine = match asked.as_str() {
        "docker" => Engine::Docker,
        "podman" => Engine::Podman,
        "" if !host.is_empty() => {
            if host.contains("podman") {
                Engine::Podman
            } else {
                Engine::Docker
            }
        }
        "" if std::path::Path::new("/var/run/docker.sock").exists()
            || podman_socket().is_none() =>
        {
            Engine::Docker
        }
        "" => Engine::Podman,
        other => {
            return Err(format!(
                "RIZR_RUNTIME={other} is not an engine rizr knows; use docker or podman"
            ));
        }
    };
    // An explicit DOCKER_HOST wins: it may name podman's socket at another path, or a daemon on purpose.
    if engine == Engine::Podman && host.is_empty() {
        let Some(sock) = podman_socket() else {
            return Err("podman's API socket is not running; rizr drives podman through it".into());
        };
        // SAFETY: called first in main, before any thread exists; nothing reads the environment concurrently.
        unsafe { std::env::set_var("DOCKER_HOST", format!("unix://{}", sock.display())) };
    }
    let _ = ENGINE.set(engine);
    Ok(engine)
}
