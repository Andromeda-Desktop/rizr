//! Commands a repo's own docs give, read as text (no repo tool runs).

use std::path::{Path, PathBuf};

const MIGRATE: [&str; 9] = [
    "migrat",
    "prestart",
    "alembic upgrade",
    "db:setup",
    "db:prepare",
    "prisma db push",
    "flask db upgrade",
    "knex migrate",
    "sequelize db:migrate",
];

/// The markdown files to read: the repo root, `docs/`, and one level of sub-directories (`backend/README.md`).
fn files(root: &Path) -> Vec<PathBuf> {
    let md = |d: &Path| -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = std::fs::read_dir(d)
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("md")))
            .filter(|p| p.metadata().is_ok_and(|m| m.len() < 512 * 1024))
            .collect();
        v.sort();
        v
    };
    let mut out = md(root);
    let subdirs: Vec<PathBuf> = std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| {
            p.is_dir()
                && !p
                    .file_name()
                    .is_some_and(|n| n.to_string_lossy().starts_with('.'))
        })
        .collect();
    for d in subdirs {
        out.extend(md(&d));
    }
    out
}

/// The migration command the docs give as a `docker compose run|exec <service> <command>` line, as the
/// `rizr exec` call that runs it. A line with its own `-f` (a deploy file) ranks last.
pub fn migrate(root: &Path) -> Option<String> {
    let mut found: Vec<(bool, String)> = Vec::new();
    for f in files(root) {
        let Ok(text) = std::fs::read_to_string(&f) else {
            continue;
        };
        for line in text.lines() {
            let l = line.trim().trim_start_matches("$ ").trim();
            let lower = l.to_ascii_lowercase();
            if !MIGRATE.iter().any(|k| lower.contains(k)) {
                continue;
            }
            if let Some((own_file, cmd)) = as_exec(l) {
                found.push((own_file, cmd));
            }
        }
    }
    found.sort_by_key(|(own_file, _)| *own_file);
    found.into_iter().next().map(|(_, c)| c)
}

/// `docker compose [-f x] run|exec [options] <service> <command...>` as (has its own -f, `rizr exec ...`).
fn as_exec(line: &str) -> Option<(bool, String)> {
    let w: Vec<&str> = line.split_whitespace().collect();
    let rest = match w.as_slice() {
        ["docker", "compose", rest @ ..] | ["docker-compose", rest @ ..] => rest,
        _ => return None,
    };
    let verb = rest.iter().position(|t| *t == "run" || *t == "exec")?;
    let own_file = rest[..verb]
        .iter()
        .any(|t| *t == "-f" || t.starts_with("--file"));
    let mut i = verb + 1;
    while let Some(t) = rest.get(i) {
        if !t.starts_with('-') {
            break;
        }
        let takes_value = [
            "-e",
            "--env",
            "-u",
            "--user",
            "-w",
            "--workdir",
            "--entrypoint",
            "-v",
            "--volume",
            "--name",
            "-p",
            "--publish",
            "-l",
            "--label",
        ]
        .contains(t);
        i += if takes_value { 2 } else { 1 };
    }
    let svc = rest.get(i)?;
    let cmd = rest.get(i + 1..).filter(|c| !c.is_empty())?;
    let cmd: Vec<&str> = cmd
        .iter()
        .copied()
        .take_while(|t| !matches!(*t, "&&" | "||" | ";" | "|"))
        .collect();
    (!cmd.is_empty()).then(|| {
        (
            own_file,
            format!("rizr exec {svc} {}", cmd.join(" ").trim_end_matches('`')),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_documented_compose_run_becomes_rizr_exec() {
        assert_eq!(
            as_exec("docker compose run --rm backend bash scripts/prestart.sh"),
            Some((false, "rizr exec backend bash scripts/prestart.sh".into()))
        );
        assert_eq!(
            as_exec(
                "docker compose -f compose.yml -f compose.deploy.yml run --rm backend bash scripts/prestart.sh"
            ),
            Some((true, "rizr exec backend bash scripts/prestart.sh".into()))
        );
        assert_eq!(
            as_exec("docker-compose exec -T -u app api python manage.py migrate && echo ok"),
            Some((false, "rizr exec api python manage.py migrate".into()))
        );
        assert_eq!(as_exec("docker compose up -d"), None);
    }
}
