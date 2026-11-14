//! Compose options that hand a service the host: in gateway mode rizr refuses them unless `[allow]` in
//! rizr.toml names the service and the option (SPEC-d3a R5). A human trusted the repo, so a human run keeps them.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde_json::Value;

/// A request of a sandboxed agent: the daemon sets `RIZR_GRANT` on the run it starts for a grant socket.
pub fn gateway() -> bool {
    std::env::var_os("RIZR_GRANT").is_some()
}

/// Each refused (service, option, detail) of the resolved compose config (`docker compose config --format json`).
pub fn violations(config: &Value, root: &Path) -> Vec<(String, &'static str, String)> {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let mut out = Vec::new();
    for (svc, s) in config["services"].as_object().into_iter().flatten() {
        let mut add = |key: &'static str, detail: String| out.push((svc.clone(), key, detail));
        if s["privileged"] == true {
            add("privileged", "privileged: true".into());
        }
        if s["build"]["privileged"] == true {
            add("privileged", "build.privileged: true".into());
        }
        for (key, field) in [("cap_add", "cap_add"), ("devices", "devices")] {
            if s[field].as_array().is_some_and(|a| !a.is_empty()) {
                add(key, format!("{field}: {}", s[field]));
            }
        }
        if let Some(m) = s["network_mode"].as_str()
            && (m == "host" || m.starts_with("container:"))
        {
            add("network_mode", format!("network_mode: {m}"));
        }
        if s["build"]["network"] == "host" {
            add("network_mode", "build.network: host".into());
        }
        for field in ["pid", "ipc", "userns_mode"] {
            if s[field] == "host" {
                add("host_namespace", format!("{field}: host"));
            }
        }
        for o in s["security_opt"].as_array().into_iter().flatten() {
            let o = o.as_str().unwrap_or_default();
            if o.contains("unconfined") || o.replace(':', "=") == "label=disable" {
                add("security_opt", format!("security_opt: {o}"));
            }
        }
        if let Some(c) = s["cgroup_parent"].as_str() {
            add("cgroup_parent", format!("cgroup_parent: {c}"));
        }
        for v in s["volumes"].as_array().into_iter().flatten() {
            let source = v["source"].as_str().unwrap_or_default();
            if v["type"] == "bind" && !Path::new(source).starts_with(&root) {
                add("bind", format!("bind mount {source}"));
            }
        }
    }
    out
}

/// Refuses the config when a violation is not in `allow` (service -> options).
pub fn check(
    config: &Value,
    root: &Path,
    allow: &BTreeMap<String, BTreeSet<String>>,
) -> Result<(), String> {
    let refused: Vec<String> = violations(config, root)
        .into_iter()
        .filter(|(svc, key, _)| !allow.get(svc).is_some_and(|a| a.contains(*key)))
        .map(|(svc, key, detail)| format!("{svc}: {detail} (allow key `{key}`)"))
        .collect();
    if refused.is_empty() {
        return Ok(());
    }
    Err(format!(
        "rizr refuses compose options that give a service the host: {}",
        refused.join("; ")
    ))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "a test panics on a broken precondition")]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn refuses_host_access_and_keeps_repo_binds() {
        let root = Path::new("/repo");
        let config = json!({"services": {
            "web": {"volumes": [{"type": "bind", "source": "/repo/src", "target": "/app"},
                                {"type": "volume", "source": "data", "target": "/data"}]},
            "proxy": {"volumes": [{"type": "bind", "source": "/var/run/docker.sock", "target": "/var/run/docker.sock"}]},
            "root": {"privileged": true, "network_mode": "host", "pid": "host"},
        }});
        let keys: Vec<_> = violations(&config, root)
            .into_iter()
            .map(|(s, k, _)| (s, k))
            .collect();
        assert_eq!(
            keys,
            [
                ("proxy".to_owned(), "bind"),
                ("root".to_owned(), "privileged"),
                ("root".to_owned(), "network_mode"),
                ("root".to_owned(), "host_namespace"),
            ]
        );
        let allow = BTreeMap::from([("proxy".to_owned(), BTreeSet::from(["bind".to_owned()]))]);
        let e = check(&config, root, &allow).unwrap_err();
        assert!(e.contains("root: privileged: true") && !e.contains("proxy"));
    }
}
