//! Host-process services as transient systemd user units (VISION section 7): the cgroup, the journal and survival
//! of a rizr crash come from systemd. Unit `rizr-<project>-<instance>-<node>.service`; its environment carries
//! `RIZR_SPEC` and `RIZR_GEN`, so `observe` reads the running spec from the unit itself.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use rizr_core::event::{Event, RuntimeKind, Seen, Status};
use std::path::PathBuf;

use rizr_core::model::{Digest, Gen, InstanceId, NodeId, NodeName, Scope};

use crate::docker::Stack;
use crate::manifest::{Cmd, HostService};

/// What the supervisor needs of a stack; owned, so worker threads can hold it.
#[derive(Clone)]
pub struct Units {
    prefix: String,
    instance: InstanceId,
    root: PathBuf,
}

impl Units {
    pub fn of(stack: &Stack) -> Self {
        Self {
            prefix: format!("rizr-{}-{}-", stack.project.0, stack.instance.0),
            instance: stack.instance.clone(),
            root: stack.root.clone(),
        }
    }

    pub fn unit(&self, node: &str) -> String {
        format!("{}{node}.service", self.prefix)
    }
}

fn systemctl() -> Command {
    let mut c = Command::new("systemctl");
    c.args(["--user", "--no-pager"]);
    c
}

/// The start command; it replaces a failed unit of the same name, which systemd keeps loaded.
pub fn start(u: &Units, node: &str, svc: &HostService, spec: &Digest, generation: Gen) -> Command {
    let (units, u) = (u, u.unit(node));
    let _ = systemctl()
        .args(["stop", &u])
        .stderr(Stdio::null())
        .status();
    let _ = systemctl()
        .args(["reset-failed", &u])
        .stderr(Stdio::null())
        .status();
    let mut c = Command::new("systemd-run");
    c.args(["--user", "--quiet", "--unit", &u])
        .arg(format!(
            "--working-directory={}",
            units.root.join(svc.cwd.as_deref().unwrap_or(".")).display()
        ))
        .arg(format!("--setenv=RIZR_SPEC={}", spec.0))
        .arg(format!("--setenv=RIZR_GEN={}", generation.0))
        .arg(format!("--setenv=RIZR_NODE={node}"));
    for (k, v) in &svc.env {
        c.arg(format!("--setenv={k}={v}"));
    }
    match &svc.cmd {
        Cmd::Shell(s) => c.args(["sh", "-c", s]),
        Cmd::Argv(a) => c.args(a),
    };
    c
}

pub fn stop(u: &Units, node: &str) -> Command {
    let mut c = systemctl();
    c.args(["stop", &u.unit(node)]);
    c
}

/// The units of this instance, as the core sees containers.
pub fn observe(u: &Units) -> Result<Vec<Seen>, String> {
    let out = systemctl()
        .args(["show", &format!("{}*", u.prefix)])
        .args([
            "-p",
            "Id,ActiveState,ExecMainCode,ExecMainStatus,Environment",
        ])
        .output()
        .map_err(|e| format!("cannot run systemctl --user: {e}"))?;
    let text = String::from_utf8_lossy(&out.stdout);
    Ok(text
        .split("\n\n")
        .filter_map(|block| seen(u, block))
        .collect())
}

fn seen(u: &Units, block: &str) -> Option<Seen> {
    let props: BTreeMap<&str, &str> = block.lines().filter_map(|l| l.split_once('=')).collect();
    let node = props
        .get("Id")?
        .strip_prefix(&u.prefix)?
        .strip_suffix(".service")?;
    let env: BTreeMap<&str, &str> = props
        .get("Environment")?
        .split(' ')
        .filter_map(|kv| kv.split_once('='))
        .collect();
    let status = match *props.get("ActiveState")? {
        "active" | "reloading" => Status::Running { health: None },
        "activating" => Status::Created,
        _ => Status::Exited {
            code: exit_code(&props),
        },
    };
    Some(Seen {
        node: NodeId {
            scope: Scope::Instance(u.instance.clone()),
            name: NodeName(node.into()),
        },
        spec: Digest((*env.get("RIZR_SPEC")?).into()),
        generation: Gen(env.get("RIZR_GEN")?.parse().ok()?),
        status,
    })
}

/// The exit code as docker reports it: a process killed by signal N exits with 128 + N (systemd's
/// `ExecMainCode` 2 = killed, 3 = dumped core; `ExecMainStatus` then holds the signal).
fn exit_code(props: &BTreeMap<&str, &str>) -> i32 {
    let status = props
        .get("ExecMainStatus")
        .and_then(|c| c.parse::<i32>().ok())
        .unwrap_or(0);
    match props.get("ExecMainCode") {
        Some(&"2" | &"3") => 128 + status,
        _ => status,
    }
}

/// Unit state changes from the user manager's bus signals: no polling. Each change is checked with
/// `systemctl show`, which gives the spec and the exit code.
pub fn subscribe(
    u: &Units,
    send: impl Fn(Event) -> bool + Send + 'static,
) -> Result<Child, String> {
    let mut child = Command::new("busctl")
        .args(["--user", "monitor", "--json=short", "--match"])
        .arg("type='signal',interface='org.freedesktop.DBus.Properties',member='PropertiesChanged',path_namespace='/org/freedesktop/systemd1/unit'")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("cannot run busctl --user: {e}"))?;
    let out = child.stdout.take().ok_or("busctl has no stdout")?;
    let (pre, instance) = (u.prefix.clone(), u.instance.clone());
    let escaped = pre.replace('-', "_2d");
    std::thread::spawn(move || {
        let mut last: BTreeMap<String, String> = BTreeMap::new();
        for line in BufReader::new(out).lines().map_while(Result::ok) {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            let Some(name) = v["path"]
                .as_str()
                .and_then(|p| p.strip_prefix("/org/freedesktop/systemd1/unit/"))
                .and_then(|p| p.strip_prefix(&escaped))
                .and_then(|p| p.strip_suffix("_2eservice"))
                .map(|p| p.replace("_2d", "-").replace("_5f", "_"))
            else {
                continue;
            };
            let data = &v["payload"]["data"];
            if data[0] != "org.freedesktop.systemd1.Unit" {
                continue;
            }
            let Some(state) = data[1]["ActiveState"]["data"].as_str() else {
                continue;
            };
            let before = last.insert(name.clone(), state.into());
            if before.as_deref() == Some(state) {
                continue;
            }
            // A new unit reports `inactive` before it starts: only a stop after a start is an exit.
            if matches!(state, "inactive" | "failed")
                && !matches!(
                    before.as_deref(),
                    Some("active" | "activating" | "deactivating")
                )
            {
                continue;
            }
            let props = Command::new("systemctl")
                .args(["--user", "--no-pager", "show"])
                .arg(format!("{pre}{name}.service"))
                .args(["-p", "ExecMainCode,ExecMainStatus,Environment"])
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
                .unwrap_or_default();
            let props: BTreeMap<&str, &str> =
                props.lines().filter_map(|l| l.split_once('=')).collect();
            let spec = props
                .get("Environment")
                .and_then(|e| e.split(' ').find_map(|kv| kv.strip_prefix("RIZR_SPEC=")))
                .unwrap_or_default()
                .to_owned();
            let kind = match state {
                "active" => RuntimeKind::Start,
                "inactive" | "failed" => RuntimeKind::Die {
                    code: exit_code(&props),
                },
                _ => continue,
            };
            let at = v["timestamp-realtime"].as_u64().unwrap_or(0) * 1000;
            let ev = Event::Runtime {
                node: NodeId {
                    scope: Scope::Instance(instance.clone()),
                    name: NodeName(name),
                },
                spec: Digest(spec),
                kind,
                at,
            };
            if !send(ev) {
                break;
            }
        }
    });
    Ok(child)
}

/// Readiness of a host process: every TCP port accepts, or the HTTP path answers 2xx or 3xx.
pub fn probe(ports: &[u16], http: Option<(u16, &str)>) -> bool {
    let connect = |p: u16| {
        TcpStream::connect_timeout(&([127, 0, 0, 1], p).into(), Duration::from_millis(400))
    };
    if let Some((port, path)) = http {
        let Ok(mut s) = connect(port) else {
            return false;
        };
        let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
        if write!(s, "GET {path} HTTP/1.0\r\nHost: localhost\r\n\r\n").is_err() {
            return false;
        }
        let mut head = [0u8; 12];
        return s.read_exact(&mut head).is_ok() && matches!(head.get(9), Some(b'2' | b'3'));
    }
    !ports.is_empty() && ports.iter().all(|p| connect(*p).is_ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_killed_host_process_exits_like_a_killed_container() {
        let killed = BTreeMap::from([("ExecMainCode", "2"), ("ExecMainStatus", "9")]);
        let exited = BTreeMap::from([("ExecMainCode", "1"), ("ExecMainStatus", "3")]);
        // The core restarts a service that exits 137 (kill -9) on the next `up`; it reports exit 3 as a failure.
        assert_eq!(exit_code(&killed), 137);
        assert_eq!(exit_code(&exited), 3);
    }
}
