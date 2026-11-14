//! Per-instance subnets. A compose file that pins a network subnet (`ipam.config[].subnet`) would make every
//! instance of the project ask docker for the same address space ("Pool overlaps"). The main instance keeps the
//! file's subnet; every other instance gets a free subnet of the same size, claimed per instance under a lock as
//! ports are, and `gateway`, `ip_range`, `aux_addresses` and the services' `ipv4_address` move with it.

use std::collections::BTreeMap;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::{Value, json};

/// The override in the instance's data directory; every compose call of the instance reads it when it exists.
pub fn file(dir: &Path) -> PathBuf {
    dir.join("networks.yml")
}

/// Per instance: network key -> claimed subnet (CIDR).
type Claims = BTreeMap<String, BTreeMap<String, String>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Net {
    base: u32,
    prefix: u8,
}

impl Net {
    fn parse(s: &str) -> Option<Self> {
        let (a, p) = s.split_once('/')?;
        let prefix: u8 = p.parse().ok().filter(|p| *p <= 32)?;
        let base = u32::from(a.parse::<Ipv4Addr>().ok()?) & mask(prefix);
        Some(Self { base, prefix })
    }
    fn size(self) -> u64 {
        1u64 << (32 - self.prefix)
    }
    fn end(self) -> u64 {
        u64::from(self.base) + self.size()
    }
    fn overlaps(self, o: Self) -> bool {
        u64::from(self.base) < o.end() && u64::from(o.base) < self.end()
    }
    fn contains(self, a: u32) -> bool {
        a & mask(self.prefix) == self.base
    }
}

impl std::fmt::Display for Net {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/{}", Ipv4Addr::from(self.base), self.prefix)
    }
}

fn mask(prefix: u8) -> u32 {
    if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix)
    }
}

/// The private range a subnet lies in; a moved subnet stays inside it.
fn range(n: Net) -> Net {
    [
        "10.0.0.0/8",
        "172.16.0.0/12",
        "192.168.0.0/16",
        "100.64.0.0/10",
    ]
    .into_iter()
    .filter_map(Net::parse)
    .find(|r| r.prefix <= n.prefix && r.contains(n.base))
    .unwrap_or(n)
}

/// The IPv4 subnets the file pins, per network key (external networks are not compose's to create).
fn pinned(config: &Value) -> BTreeMap<String, Net> {
    let mut out = BTreeMap::new();
    for (key, net) in config["networks"].as_object().into_iter().flatten() {
        if net["external"].as_bool() == Some(true) {
            continue;
        }
        let first = net["ipam"]["config"]
            .as_array()
            .into_iter()
            .flatten()
            .find_map(|c| Net::parse(c["subnet"].as_str()?));
        if let Some(n) = first {
            out.insert(key.clone(), n);
        }
    }
    out
}

/// Write (or remove) the instance's network override from the compose config without it.
/// `claims` is shared by every instance of the project; `project` is this instance's compose project.
pub fn prepare(
    config: &Value,
    dir: &Path,
    claims: &Path,
    instance: &str,
    project: &str,
) -> Result<(), String> {
    let pinned = pinned(config);
    let path = file(dir);
    if pinned.is_empty() && !path.exists() && !claims.exists() {
        return Ok(());
    }
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let lock = std::fs::File::create(claims.with_extension("lock")).map_err(|e| e.to_string())?;
    lock.lock().map_err(|e| e.to_string())?;
    let mut held: Claims = std::fs::read(claims)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    let others: Vec<Net> = held
        .iter()
        .filter(|(i, _)| *i != instance)
        .flat_map(|(_, m)| m.values().filter_map(|s| Net::parse(s)))
        .collect();
    let mut mine = BTreeMap::new();
    let mut moved = BTreeMap::new();
    if instance == "main" {
        mine = pinned
            .iter()
            .map(|(k, n)| (k.clone(), n.to_string()))
            .collect();
    } else if !pinned.is_empty() {
        let docker = in_use(project).into_iter().chain(routes());
        let old = held.get(instance).cloned().unwrap_or_default();
        let mut taken: Vec<Net> = others.iter().copied().chain(docker).collect();
        // Main may not have run yet: the file's own subnets are its.
        taken.extend(pinned.values().copied());
        for (key, n) in &pinned {
            let keep = old
                .get(key)
                .and_then(|s| Net::parse(s))
                .filter(|c| c.prefix == n.prefix && !taken.iter().any(|t| t.overlaps(*c)));
            let new = keep.or_else(|| free(*n, &taken)).ok_or_else(|| {
                format!(
                    "no free private subnet of size /{} for network `{key}` of instance {instance}: other instances \
                     and docker networks hold them all; remove unused ones (rizr gc, docker network prune)",
                    n.prefix
                )
            })?;
            taken.push(new);
            mine.insert(key.clone(), new.to_string());
            moved.insert(key.clone(), (*n, new));
        }
    }
    if mine.is_empty() {
        held.remove(instance);
    } else {
        held.insert(instance.to_owned(), mine);
    }
    std::fs::write(
        claims,
        serde_json::to_vec(&held).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    if moved.is_empty() {
        if path.exists() {
            std::fs::remove_file(&path).map_err(|e| e.to_string())?;
        }
        return Ok(());
    }
    std::fs::write(&path, render(config, &moved)).map_err(|e| e.to_string())
}

/// The first block of `n`'s size that overlaps nothing taken: in `n`'s own private range, from the block after
/// `n` round to it; then in the other ranges (docker's default pools often fill 172.16.0.0/12 with /16s).
fn free(n: Net, taken: &[Net]) -> Option<Net> {
    let own = range(n);
    let others = ["10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16"]
        .into_iter()
        .filter_map(Net::parse)
        .filter(|r| *r != own && r.prefix <= n.prefix);
    for (r, start) in std::iter::once((own, n.end())).chain(others.map(|r| (r, u64::from(r.base))))
    {
        let count = r.size() / n.size();
        let first = (start - u64::from(r.base)) / n.size();
        for k in 0..count {
            let base = u64::from(r.base) + (first + k) % count * n.size();
            let c = Net {
                base: u32::try_from(base).ok()?,
                prefix: n.prefix,
            };
            if !taken.iter().any(|t| t.overlaps(c)) {
                return Some(c);
            }
        }
    }
    None
}

/// Networks the host routes (LAN, VPN, other bridges), from `/proc/net/route`; the default route is left out.
fn routes() -> Vec<Net> {
    let text = std::fs::read_to_string("/proc/net/route").unwrap_or_default();
    text.lines()
        .skip(1)
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            let dest = u32::from_str_radix(f.get(1)?, 16).ok()?.swap_bytes();
            let mask = u32::from_str_radix(f.get(7)?, 16).ok()?.swap_bytes();
            let prefix = u8::try_from(mask.count_ones()).ok()?;
            (prefix > 0).then_some(Net { base: dest, prefix })
        })
        .collect()
}

/// IPv4 subnets of docker networks other than this project's own.
fn in_use(project: &str) -> Vec<Net> {
    let ids = Command::new("docker")
        .args(["network", "ls", "-q"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    if ids.trim().is_empty() {
        return Vec::new();
    }
    let out = Command::new("docker")
        .args(["network", "inspect", "--format"])
        .arg(r#"{{index .Labels "com.docker.compose.project"}} {{range .IPAM.Config}}{{.Subnet}} {{end}}"#)
        .args(ids.split_whitespace())
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    out.lines()
        .filter_map(|l| {
            let mut w = l.split_whitespace();
            // A network without the label prints an empty first field: its first word is then a subnet.
            let first = w.next()?;
            if first == project {
                return None;
            }
            Some(
                std::iter::once(first)
                    .chain(w)
                    .filter_map(Net::parse)
                    .collect::<Vec<_>>(),
            )
        })
        .flatten()
        .collect()
}

fn shift(addr: &str, from: Net, to: Net) -> Option<String> {
    let (a, suffix) = match addr.split_once('/') {
        Some((a, p)) => (a, format!("/{p}")),
        None => (addr, String::new()),
    };
    let a = u32::from(a.parse::<Ipv4Addr>().ok()?);
    from.contains(a)
        .then(|| format!("{}{suffix}", Ipv4Addr::from(a - from.base + to.base)))
}

/// The override: each moved network's whole `ipam.config` (`!override`, as compose would append to a list) and
/// the services' fixed addresses on it. JSON flow values are YAML; only the tag needs text.
fn render(config: &Value, moved: &BTreeMap<String, (Net, Net)>) -> String {
    let mut out = String::from("networks:\n");
    for (key, (from, to)) in moved {
        let entries: Vec<Value> = config["networks"][key]["ipam"]["config"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|c| {
                let mut c = c.clone();
                if let Some(o) = c.as_object_mut() {
                    o.retain(|_, v| !v.is_null());
                }
                for f in ["subnet", "gateway", "ip_range"] {
                    if let Some(s) = c[f].as_str().and_then(|s| shift(s, *from, *to)) {
                        c[f] = s.into();
                    }
                }
                if let Some(aux) = c.get_mut("aux_addresses").and_then(Value::as_object_mut) {
                    for v in aux.values_mut() {
                        if let Some(s) = v.as_str().and_then(|s| shift(s, *from, *to)) {
                            *v = s.into();
                        }
                    }
                }
                c
            })
            .collect();
        out.push_str(&format!(
            "  {}:\n    ipam:\n      config: !override {}\n",
            json!(key),
            Value::from(entries)
        ));
    }
    let mut services = serde_json::Map::new();
    for (name, svc) in config["services"].as_object().into_iter().flatten() {
        for (key, (from, to)) in moved {
            let Some(a) = svc["networks"][key]["ipv4_address"].as_str() else {
                continue;
            };
            if let Some(new) = shift(a, *from, *to) {
                services
                    .entry(name.clone())
                    .or_insert_with(|| json!({"networks": {}}))["networks"][key] =
                    json!({"ipv4_address": new});
            }
        }
    }
    if !services.is_empty() {
        out.push_str(&format!("services: {}\n", Value::from(services)));
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "a test panics on a broken precondition")]
mod tests {
    use super::*;

    fn config() -> Value {
        json!({
            "networks": {
                "back": {"ipam": {"config": [{"subnet": "172.28.0.0/16", "gateway": "172.28.0.1",
                                              "ip_range": "172.28.5.0/24", "aux_addresses": {"h": "172.28.1.5"}},
                                             {"subnet": "fd00::/64", "gateway": null}]}},
                "ext": {"external": true, "ipam": {"config": [{"subnet": "10.9.0.0/16"}]}},
                "plain": {}
            },
            "services": {
                "db": {"networks": {"back": {"ipv4_address": "172.28.0.10"}, "plain": null}},
                "web": {"networks": {"plain": null}}
            }
        })
    }

    #[test]
    fn a_second_instance_moves_the_subnet_and_its_fixed_addresses() {
        let to = Net::parse("172.29.0.0/16").unwrap();
        let from = Net::parse("172.28.0.0/16").unwrap();
        let moved = BTreeMap::from([("back".to_owned(), (from, to))]);
        let y = render(&config(), &moved);
        assert!(y.contains(r#"config: !override [{"aux_addresses":{"h":"172.29.1.5"},"gateway":"172.29.0.1","ip_range":"172.29.5.0/24","subnet":"172.29.0.0/16"},{"subnet":"fd00::/64"}]"#), "{y}");
        assert!(
            y.contains(r#"services: {"db":{"networks":{"back":{"ipv4_address":"172.29.0.10"}}}}"#),
            "{y}"
        );
        assert!(!y.contains("ext"), "{y}");
    }

    #[test]
    fn free_skips_taken_blocks_then_tries_the_other_private_ranges() {
        let n = Net::parse("172.28.0.0/16").unwrap();
        let taken: Vec<Net> = ["172.16.0.0/14", "172.20.0.0/16", "172.28.0.0/16"]
            .into_iter()
            .filter_map(Net::parse)
            .collect();
        assert_eq!(free(n, &taken).unwrap().to_string(), "172.29.0.0/16");
        let wrap: Vec<Net> = ["172.28.0.0/14"]
            .into_iter()
            .filter_map(Net::parse)
            .chain(taken.iter().copied())
            .collect();
        assert_eq!(free(n, &wrap).unwrap().to_string(), "172.21.0.0/16");
        let full: Vec<Net> = ["172.16.0.0/12", "10.0.0.0/9"]
            .into_iter()
            .filter_map(Net::parse)
            .collect();
        assert_eq!(free(n, &full).unwrap().to_string(), "10.128.0.0/16");
        let all: Vec<Net> = ["172.16.0.0/12", "10.0.0.0/8", "192.168.0.0/16"]
            .into_iter()
            .filter_map(Net::parse)
            .collect();
        assert_eq!(free(n, &all), None);
    }

    #[test]
    fn two_instances_claim_different_subnets_and_keep_them() {
        let d = std::env::temp_dir().join(format!("rizr-subnet-{}", std::process::id()));
        let claims = d.join("p.subnets.json");
        for i in ["main", "wt-a", "wt-b"] {
            prepare(&config(), &d.join(i), &claims, i, &format!("p-{i}-none")).unwrap();
        }
        let held: Claims = serde_json::from_slice(&std::fs::read(&claims).unwrap()).unwrap();
        let a = &held["wt-a"]["back"];
        let b = &held["wt-b"]["back"];
        assert_eq!(held["main"]["back"], "172.28.0.0/16");
        assert!(
            a != b && a != "172.28.0.0/16" && b != "172.28.0.0/16",
            "{held:?}"
        );
        assert!(!file(&d.join("main")).exists());
        prepare(&config(), &d.join("wt-a"), &claims, "wt-a", "p-wt-a-none").unwrap();
        let again: Claims = serde_json::from_slice(&std::fs::read(&claims).unwrap()).unwrap();
        assert_eq!(&again["wt-a"]["back"], a);
        let _ = std::fs::remove_dir_all(&d);
    }
}
