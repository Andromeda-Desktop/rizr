//! Redis in an instance. Each instance keeps its own redis (DECISIONS 2026-11-14): a shared server would leak
//! pub/sub and FLUSHALL across worktrees. rizr finds the containers, names their URLs and empties one on reset.

use std::process::Command;

use serde_json::{Value, json};

pub struct Redis {
    pub service: String,
    pub container: String,
    pub host_port: Option<u16>,
}

/// A redis or valkey image name, any registry and tag.
pub fn image(image: &str) -> bool {
    let repo = image.split(['@', ':']).next().unwrap_or_default();
    matches!(
        repo.rsplit('/').next().unwrap_or_default(),
        "redis" | "valkey" | "redis-stack-server"
    )
}

/// The running redis containers of a compose project.
pub fn running(project: &str) -> Vec<Redis> {
    let out = Command::new("docker")
        .args(["ps", "--filter"])
        .arg(format!("label=com.docker.compose.project={project}"))
        .args([
            "--format",
            "{{.Label \"com.docker.compose.service\"}}\t{{.Names}}\t{{.Image}}\t{{.Ports}}",
        ])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    out.lines()
        .filter_map(|l| {
            let mut f = l.split('\t');
            let (service, container, img, ports) = (f.next()?, f.next()?, f.next()?, f.next()?);
            image(img).then(|| Redis {
                service: service.to_owned(),
                container: container.to_owned(),
                host_port: host_port(ports),
            })
        })
        .collect()
}

/// The host port published for 6379, from `docker ps` ports: `0.0.0.0:32768->6379/tcp, [::]:32768->6379/tcp`.
fn host_port(ports: &str) -> Option<u16> {
    ports.split(", ").find_map(|p| {
        p.strip_suffix("->6379/tcp")?
            .rsplit(':')
            .next()?
            .parse()
            .ok()
    })
}

/// The redis entries of `rizr context`.
pub fn context(project: &str) -> Value {
    running(project)
        .iter()
        .map(|r| {
            json!({
                "service": r.service, "container": r.container,
                "url": format!("redis://{}:6379", r.service),
                "host_url": r.host_port.map(|p| format!("redis://127.0.0.1:{p}")),
                "cli": format!("docker exec -it {} redis-cli", r.container),
            })
        })
        .collect()
}

/// Empties every database of one redis. A password set by env (`REDIS_PASSWORD`, bitnami) is used.
pub fn flush(r: &Redis) -> Result<(), String> {
    let o = Command::new("docker")
        .args(["exec", &r.container, "sh", "-c"])
        .arg(
            "export REDISCLI_AUTH=${REDISCLI_AUTH:-$REDIS_PASSWORD}; \
             if command -v redis-cli >/dev/null; then exec redis-cli FLUSHALL; else exec valkey-cli FLUSHALL; fi",
        )
        .output()
        .map_err(|e| e.to_string())?;
    let out = String::from_utf8_lossy(&o.stdout);
    if o.status.success() && out.trim() == "OK" {
        Ok(())
    } else {
        Err(format!(
            "{}{}",
            out.trim(),
            String::from_utf8_lossy(&o.stderr).trim()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_host_port_of_6379() {
        assert_eq!(
            host_port("0.0.0.0:32768->6379/tcp, [::]:32768->6379/tcp"),
            Some(32768)
        );
        assert_eq!(host_port("6379/tcp"), None);
    }
}
