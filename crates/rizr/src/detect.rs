//! Packages and tasks read from a repo's own files with rizr's parsers: safe before trust (no repo tool runs).

use std::collections::BTreeMap;
use std::path::Path;

use rizr_core::code::{Kind, Manager, Package, PyTool, Source, Task, TaskName};
use serde_json::Value;

/// Repo-relative path -> file text. A directory that matters (`tests/`) is a key ending in `/` with no text.
pub type Tree = BTreeMap<String, String>;

const MANIFESTS: [&str; 6] = [
    "Cargo.toml",
    "package.json",
    "pyproject.toml",
    "setup.py",
    "setup.cfg",
    "go.mod",
];
const OTHERS: [&str; 22] = [
    "Cargo.lock",
    "pnpm-lock.yaml",
    "yarn.lock",
    "bun.lock",
    "bun.lockb",
    "package-lock.json",
    "uv.lock",
    "poetry.lock",
    "pytest.ini",
    "tox.ini",
    "noxfile.py",
    "alembic.ini",
    "manage.py",
    "justfile",
    "Justfile",
    ".justfile",
    "Makefile",
    "GNUmakefile",
    "Taskfile.yml",
    ".mise.toml",
    "mise.toml",
    "rizr.toml",
];
const SKIP: [&str; 9] = [
    "node_modules",
    "target",
    "vendor",
    "venv",
    ".venv",
    "dist",
    "build",
    "__pycache__",
    "testdata",
];
const MAX_BYTES: u64 = 1 << 20;
const DEPTH: usize = 3;

/// Reads the files detection needs: manifests and runners up to 3 levels deep, CI workflows, README files.
pub fn read(root: &Path) -> Tree {
    let mut tree = Tree::new();
    walk(root, root, 0, &mut tree);
    for dir in [".github/workflows"] {
        for e in std::fs::read_dir(root.join(dir))
            .into_iter()
            .flatten()
            .flatten()
        {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.ends_with(".yml") || name.ends_with(".yaml") {
                add(root, &e.path(), &mut tree);
            }
        }
    }
    for f in ["bin/rails", "prisma/schema.prisma", ".cargo/config.toml"] {
        add(root, &root.join(f), &mut tree);
    }
    tree
}

fn add(root: &Path, path: &Path, tree: &mut Tree) {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return;
    };
    if !meta.is_file() || meta.len() > MAX_BYTES {
        return;
    }
    if let (Ok(rel), Ok(text)) = (path.strip_prefix(root), std::fs::read_to_string(path)) {
        tree.insert(rel.to_string_lossy().into_owned(), text);
    }
}

fn walk(root: &Path, dir: &Path, depth: usize, tree: &mut Tree) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        let Ok(kind) = e.file_type() else {
            continue;
        };
        if kind.is_dir() {
            if name.starts_with('.') || SKIP.contains(&name.as_str()) {
                continue;
            }
            if ["tests", "test"].contains(&name.as_str())
                && let Ok(rel) = e.path().strip_prefix(root)
            {
                tree.insert(format!("{}/", rel.to_string_lossy()), String::new());
            }
            if depth < DEPTH {
                walk(root, &e.path(), depth + 1, tree);
            }
        } else if MANIFESTS.contains(&name.as_str())
            || OTHERS.contains(&name.as_str())
            || (depth == 0 && name.to_ascii_lowercase().starts_with("readme"))
        {
            add(root, &e.path(), tree);
        }
    }
}

fn join(dir: &str, file: &str) -> String {
    if dir == "." {
        file.to_owned()
    } else {
        format!("{dir}/{file}")
    }
}

fn parent(path: &str) -> String {
    match path.rsplit_once('/') {
        Some((d, _)) => d.to_owned(),
        None => ".".to_owned(),
    }
}

/// The packages of a repo, root first, with every task candidate and its source.
pub fn detect(tree: &Tree) -> Vec<Package> {
    let mut dirs: Vec<String> = tree
        .keys()
        .filter(|p| MANIFESTS.iter().any(|m| p.rsplit('/').next() == Some(m)))
        .map(|p| parent(p))
        .collect();
    dirs.sort_by_key(|d| (d != ".", d.matches('/').count(), d.clone()));
    dirs.dedup();
    let mut out: Vec<Package> = Vec::new();
    for dir in dirs {
        let Some(kind) = kind(tree, &dir) else {
            continue;
        };
        // A member of a cargo or node workspace is tested from the workspace root.
        if out.iter().any(|p| p.kind == kind && member(tree, p, &dir)) {
            continue;
        }
        let mut pkg = Package {
            dir: dir.clone(),
            kind,
            tasks: BTreeMap::new(),
        };
        defaults(tree, &mut pkg);
        scripts(tree, &mut pkg);
        runners(tree, &mut pkg);
        if dir == "." {
            ci(tree, &mut pkg);
            docs(tree, &mut pkg);
        }
        manifest(tree, &mut pkg);
        out.push(pkg);
    }
    out
}

fn text<'a>(tree: &'a Tree, dir: &str, file: &str) -> Option<&'a str> {
    tree.get(&join(dir, file)).map(String::as_str)
}

fn has(tree: &Tree, dir: &str, file: &str) -> bool {
    tree.contains_key(&join(dir, file))
}

fn kind(tree: &Tree, dir: &str) -> Option<Kind> {
    if has(tree, dir, "Cargo.toml") {
        return Some(Kind::Cargo);
    }
    if has(tree, dir, "go.mod") {
        return Some(Kind::Go);
    }
    if let Some(pkg) = text(tree, dir, "package.json") {
        let declared = serde_json::from_str::<Value>(pkg).ok().and_then(|v| {
            let m = v["packageManager"].as_str()?.split('@').next()?.to_owned();
            Some(m)
        });
        let manager = match declared.as_deref() {
            Some("pnpm") => Manager::Pnpm,
            Some("yarn") => Manager::Yarn,
            Some("bun") => Manager::Bun,
            Some(_) => Manager::Npm,
            None if lock_up(tree, dir, "pnpm-lock.yaml") => Manager::Pnpm,
            None if lock_up(tree, dir, "yarn.lock") => Manager::Yarn,
            None if lock_up(tree, dir, "bun.lock") || lock_up(tree, dir, "bun.lockb") => {
                Manager::Bun
            }
            None => Manager::Npm,
        };
        return Some(Kind::Node { manager });
    }
    let py = text(tree, dir, "pyproject.toml").unwrap_or_default();
    if !py.is_empty() || has(tree, dir, "setup.py") || has(tree, dir, "setup.cfg") {
        let tool = if has(tree, dir, "uv.lock") || py.contains("[tool.uv") {
            PyTool::Uv
        } else if has(tree, dir, "poetry.lock") || py.contains("[tool.poetry") {
            PyTool::Poetry
        } else {
            PyTool::Pip
        };
        return Some(Kind::Python { tool });
    }
    None
}

/// A lock file in the package dir or in the repo root (a workspace keeps one lock at the root).
fn lock_up(tree: &Tree, dir: &str, file: &str) -> bool {
    has(tree, dir, file) || has(tree, ".", file)
}

fn member(tree: &Tree, root: &Package, dir: &str) -> bool {
    let inside = root.dir == "." || dir.starts_with(&format!("{}/", root.dir));
    let workspace = match root.kind {
        Kind::Cargo => {
            text(tree, &root.dir, "Cargo.toml").is_some_and(|t| t.contains("[workspace]"))
        }
        Kind::Node { .. } => {
            text(tree, &root.dir, "package.json").is_some_and(|t| t.contains("\"workspaces\""))
                || has(tree, &root.dir, "pnpm-workspace.yaml")
        }
        _ => false,
    };
    inside && workspace
}

fn task(cmd: impl Into<String>, source: Source, from: String) -> Task {
    Task {
        cmd: cmd.into(),
        source,
        from,
    }
}

fn py_run(tool: PyTool, cmd: &str) -> String {
    match tool {
        PyTool::Uv => format!("uv run {cmd}"),
        PyTool::Poetry => format!("poetry run {cmd}"),
        PyTool::Pip => format!("python -m {cmd}"),
    }
}

fn node_bin(m: Manager, bin: &str) -> String {
    match m {
        Manager::Npm => format!("npx {bin}"),
        Manager::Pnpm => format!("pnpm exec {bin}"),
        Manager::Yarn => format!("yarn {bin}"),
        Manager::Bun => format!("bunx {bin}"),
    }
}

fn manager_name(m: Manager) -> &'static str {
    match m {
        Manager::Npm => "npm",
        Manager::Pnpm => "pnpm",
        Manager::Yarn => "yarn",
        Manager::Bun => "bun",
    }
}

fn defaults(tree: &Tree, pkg: &mut Package) {
    let dir = pkg.dir.clone();
    let d = |f: &str| join(&dir, f);
    match pkg.kind {
        Kind::Cargo => {
            let ws = text(tree, &dir, "Cargo.toml").is_some_and(|t| t.contains("[workspace]"));
            let all = if ws { " --workspace" } else { "" };
            pkg.add(
                TaskName::Test,
                task(format!("cargo test{all}"), Source::Default, d("Cargo.toml")),
            );
            pkg.add(
                TaskName::Build,
                task(
                    format!("cargo build{all}"),
                    Source::Default,
                    d("Cargo.toml"),
                ),
            );
            pkg.add(
                TaskName::Lint,
                task(
                    format!("cargo clippy{all} --all-targets"),
                    Source::Default,
                    d("Cargo.toml"),
                ),
            );
        }
        Kind::Go => {
            pkg.add(
                TaskName::Test,
                task("go test ./...", Source::Default, d("go.mod")),
            );
            pkg.add(
                TaskName::Build,
                task("go build ./...", Source::Default, d("go.mod")),
            );
            pkg.add(
                TaskName::Lint,
                task("go vet ./...", Source::Default, d("go.mod")),
            );
        }
        Kind::Python { tool } => {
            let py = text(tree, &dir, "pyproject.toml").unwrap_or_default();
            let cfg = text(tree, &dir, "setup.cfg").unwrap_or_default();
            let tox = text(tree, &dir, "tox.ini").unwrap_or_default();
            let pytest = has(tree, &dir, "tests/")
                || has(tree, &dir, "test/")
                || has(tree, &dir, "pytest.ini")
                || py.contains("[tool.pytest")
                || cfg.contains("[tool:pytest]")
                || tox.contains("[pytest]");
            if pytest {
                let from = if py.is_empty() {
                    d("setup.py")
                } else {
                    d("pyproject.toml")
                };
                pkg.add(
                    TaskName::Test,
                    task(py_run(tool, "pytest"), Source::Default, from),
                );
            }
            if has(tree, &dir, "alembic.ini") {
                pkg.add(
                    TaskName::Migrate,
                    task(
                        py_run(tool, "alembic upgrade head"),
                        Source::Default,
                        d("alembic.ini"),
                    ),
                );
            }
            if has(tree, &dir, "manage.py") {
                let python = match tool {
                    PyTool::Pip => "python manage.py migrate".to_owned(),
                    t => py_run(t, "python manage.py migrate"),
                };
                pkg.add(
                    TaskName::Migrate,
                    task(python, Source::Default, d("manage.py")),
                );
            }
        }
        Kind::Node { manager } => {
            if has(tree, &dir, "prisma/schema.prisma") {
                pkg.add(
                    TaskName::Migrate,
                    task(
                        node_bin(manager, "prisma migrate deploy"),
                        Source::Default,
                        d("prisma/schema.prisma"),
                    ),
                );
            }
        }
    }
    if has(tree, &dir, "bin/rails") {
        pkg.add(
            TaskName::Migrate,
            task("bin/rails db:prepare", Source::Default, d("bin/rails")),
        );
    }
}

fn scripts(tree: &Tree, pkg: &mut Package) {
    let dir = pkg.dir.clone();
    if let Kind::Node { manager } = pkg.kind {
        let Some(v) =
            text(tree, &dir, "package.json").and_then(|t| serde_json::from_str::<Value>(t).ok())
        else {
            return;
        };
        let m = manager_name(manager);
        for (name, keys) in [
            (TaskName::Test, &["test"][..]),
            (TaskName::Build, &["build"]),
            (TaskName::Lint, &["lint"]),
            (
                TaskName::Migrate,
                &["migrate", "db:migrate", "migrate:latest"],
            ),
        ] {
            let Some(key) = keys.iter().find(|k| {
                v["scripts"][**k]
                    .as_str()
                    .is_some_and(|s| !s.contains("no test specified"))
            }) else {
                continue;
            };
            let cmd = if *key == "test" {
                format!("{m} test")
            } else {
                format!("{m} run {key}")
            };
            pkg.add(name, task(cmd, Source::Script, join(&dir, "package.json")));
        }
    }
    if let Kind::Python { .. } = pkg.kind {
        let Some(t) =
            text(tree, &dir, "pyproject.toml").and_then(|t| t.parse::<toml::Table>().ok())
        else {
            return;
        };
        let tool = &t.get("tool");
        let tables: [(&[&str], &str); 3] = [
            (&["poe", "tasks"], "poe"),
            (&["pdm", "scripts"], "pdm run"),
            (&["hatch", "envs", "default", "scripts"], "hatch run"),
        ];
        for (path, run) in tables {
            let mut node = tool.and_then(|v| v.as_table());
            for p in path {
                node = node.and_then(|n| n.get(*p)).and_then(|v| v.as_table());
            }
            let Some(node) = node else {
                continue;
            };
            for (name, key) in [
                (TaskName::Test, "test"),
                (TaskName::Lint, "lint"),
                (TaskName::Migrate, "migrate"),
            ] {
                if node.contains_key(key) {
                    pkg.add(
                        name,
                        task(
                            format!("{run} {key}"),
                            Source::Script,
                            join(&dir, "pyproject.toml"),
                        ),
                    );
                }
            }
        }
    }
}

const RUNNER_NAMES: [(TaskName, &[&str]); 4] = [
    (TaskName::Test, &["test", "tests"]),
    (TaskName::Build, &["build"]),
    (TaskName::Lint, &["lint"]),
    (TaskName::Migrate, &["migrate", "db-migrate"]),
];

/// The recipe names of a justfile or Makefile: `name args...:` at the start of a line, not `name := value`.
fn recipes(text: &str) -> Vec<String> {
    text.lines()
        .filter(|l| !l.starts_with([' ', '\t', '#', '.']) && !l.contains(":=") && !l.contains("::"))
        .filter_map(|l| {
            let head = l.split(':').next()?;
            let name = head.split_whitespace().next()?.trim_start_matches('@');
            (l.contains(':') && !name.is_empty()).then(|| name.to_owned())
        })
        .collect()
}

fn runners(tree: &Tree, pkg: &mut Package) {
    let dir = pkg.dir.clone();
    let mut found: Vec<(String, Vec<String>, &str)> = Vec::new();
    for f in ["justfile", "Justfile", ".justfile"] {
        if let Some(t) = text(tree, &dir, f) {
            found.push((join(&dir, f), recipes(t), "just"));
        }
    }
    for f in ["Makefile", "GNUmakefile"] {
        if let Some(t) = text(tree, &dir, f) {
            found.push((join(&dir, f), recipes(t), "make"));
        }
    }
    if let Some(v) =
        text(tree, &dir, "Taskfile.yml").and_then(|t| serde_norway::from_str::<Value>(t).ok())
    {
        let names = v["tasks"]
            .as_object()
            .map(|o| o.keys().cloned().collect())
            .unwrap_or_default();
        found.push((join(&dir, "Taskfile.yml"), names, "task"));
    }
    for f in [".mise.toml", "mise.toml"] {
        if let Some(t) = text(tree, &dir, f).and_then(|t| t.parse::<toml::Table>().ok()) {
            let names = t
                .get("tasks")
                .and_then(|v| v.as_table())
                .map(|o| o.keys().cloned().collect())
                .unwrap_or_default();
            found.push((join(&dir, f), names, "mise run"));
        }
    }
    for (from, names, run) in found {
        for (task_name, aliases) in RUNNER_NAMES {
            if let Some(n) = aliases.iter().find(|a| names.iter().any(|n| n == *a)) {
                pkg.add(
                    task_name,
                    task(format!("{run} {n}"), Source::Runner, from.clone()),
                );
            }
        }
    }
}

/// Test and build commands a line runs, by the tool it starts with.
fn command_task(line: &str) -> Option<TaskName> {
    let l = line.trim().trim_start_matches("$ ").trim();
    let test = [
        "cargo test",
        "cargo nextest run",
        "go test",
        "pytest",
        "python -m pytest",
        "python3 -m pytest",
        "uv run pytest",
        "poetry run pytest",
        "tox",
        "nox",
        "npm test",
        "npm run test",
        "pnpm test",
        "pnpm run test",
        "yarn test",
        "yarn run test",
        "bun test",
        "bun run test",
        "just test",
        "make test",
        "make check",
    ];
    let starts = |p: &str| l == p || l.starts_with(&format!("{p} "));
    if test.iter().any(|p| starts(p)) {
        return Some(TaskName::Test);
    }
    None
}

fn ci(tree: &Tree, pkg: &mut Package) {
    for (path, text) in tree.range(".github/workflows/".to_owned()..) {
        if !path.starts_with(".github/workflows/") {
            break;
        }
        let Ok(v) = serde_norway::from_str::<Value>(text) else {
            continue;
        };
        let jobs = v["jobs"].as_object().into_iter().flatten();
        let lines = jobs.flat_map(|(_, j)| {
            j["steps"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|s| s["working-directory"].is_null())
                .filter_map(|s| s["run"].as_str())
                .flat_map(str::lines)
                .map(str::to_owned)
                .collect::<Vec<_>>()
        });
        for line in lines {
            if line.contains("${{") || line.contains('$') && line.contains("{{") {
                continue;
            }
            if let Some(name) = command_task(&line)
                && !pkg
                    .tasks
                    .get(&name)
                    .into_iter()
                    .flatten()
                    .any(|t| t.source == Source::Ci)
            {
                pkg.add(name, task(line.trim(), Source::Ci, path.clone()));
            }
        }
    }
}

fn docs(tree: &Tree, pkg: &mut Package) {
    let readmes = tree
        .iter()
        .filter(|(p, _)| !p.contains('/') && p.to_ascii_lowercase().starts_with("readme"));
    for (path, text) in readmes {
        let mut fenced = false;
        for line in text.lines() {
            if line.trim_start().starts_with("```") {
                fenced = !fenced;
                continue;
            }
            if !fenced {
                continue;
            }
            if let Some(name) = command_task(line)
                && !pkg
                    .tasks
                    .get(&name)
                    .into_iter()
                    .flatten()
                    .any(|t| t.source == Source::Docs)
            {
                let cmd = line.trim().trim_start_matches("$ ").trim();
                pkg.add(name, task(cmd, Source::Docs, path.clone()));
            }
        }
    }
}

fn manifest(tree: &Tree, pkg: &mut Package) {
    let Some(t) = tree
        .get("rizr.toml")
        .and_then(|t| t.parse::<toml::Table>().ok())
    else {
        return;
    };
    let Some(tasks) = t.get("tasks").and_then(|v| v.as_table()) else {
        return;
    };
    let scoped = tasks.get(&pkg.dir).and_then(|v| v.as_table());
    let table = if pkg.dir == "." { Some(tasks) } else { scoped };
    for (name, key) in [
        (TaskName::Build, "build"),
        (TaskName::Test, "test"),
        (TaskName::Lint, "lint"),
        (TaskName::Migrate, "migrate"),
    ] {
        if let Some(cmd) = table.and_then(|t| t.get(key)).and_then(|v| v.as_str()) {
            pkg.add(name, task(cmd, Source::Manifest, "rizr.toml".into()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(files: &[(&str, &str)]) -> Tree {
        files
            .iter()
            .map(|(p, t)| ((*p).to_owned(), (*t).to_owned()))
            .collect()
    }

    fn test_cmd(t: &Tree) -> (String, Source) {
        let p = detect(t);
        let Some(task) = p[0].task(TaskName::Test) else {
            panic!("no test task");
        };
        (task.cmd.clone(), task.source)
    }

    #[test]
    fn a_cargo_workspace_tests_from_its_root_and_ci_wins_over_the_default() {
        let t = tree(&[
            ("Cargo.toml", "[workspace]\nmembers = [\"crates/*\"]\n"),
            ("crates/a/Cargo.toml", "[package]\nname = \"a\"\n"),
            (
                ".github/workflows/ci.yml",
                "jobs:\n  test:\n    steps:\n      - run: cargo test --all-features ${{ matrix.x }}\n      - run: |\n          cargo build\n          cargo test --workspace --locked\n",
            ),
        ]);
        assert_eq!(detect(&t).len(), 1);
        assert_eq!(
            test_cmd(&t),
            ("cargo test --workspace --locked".into(), Source::Ci)
        );
    }

    #[test]
    fn node_scripts_use_the_lock_files_manager_and_skip_the_npm_placeholder() {
        let t = tree(&[
            (
                "package.json",
                r#"{"scripts": {"test": "vitest run", "build": "tsc"}}"#,
            ),
            ("pnpm-lock.yaml", ""),
        ]);
        assert_eq!(test_cmd(&t), ("pnpm test".into(), Source::Script));
        let t = tree(&[(
            "package.json",
            r#"{"scripts": {"test": "echo \"Error: no test specified\" && exit 1"}}"#,
        )]);
        assert!(detect(&t)[0].task(TaskName::Test).is_none());
    }

    #[test]
    fn a_runner_recipe_beats_the_tool_default_and_docs_beat_the_runner() {
        let mut t = tree(&[
            (
                "pyproject.toml",
                "[project]\nname = \"x\"\n[tool.pytest.ini_options]\n",
            ),
            ("uv.lock", ""),
            (
                "justfile",
                "set shell := [\"bash\"]\n\n# run tests\ntest *args:\n    uv run pytest {{args}}\n",
            ),
        ]);
        assert_eq!(test_cmd(&t), ("just test".into(), Source::Runner));
        t.insert(
            "README.md".into(),
            "Run:\n```sh\n$ uv run pytest -q\n```\n".into(),
        );
        assert_eq!(test_cmd(&t), ("uv run pytest -q".into(), Source::Docs));
        t.insert(
            "rizr.toml".into(),
            "version = 1\n[tasks]\ntest = \"make ci\"\n".into(),
        );
        assert_eq!(test_cmd(&t), ("make ci".into(), Source::Manifest));
    }

    #[test]
    fn python_migrations_and_tests_use_the_project_tool() {
        let t = tree(&[
            ("backend/pyproject.toml", "[tool.poetry]\nname = \"api\"\n"),
            ("backend/tests/", ""),
            ("backend/alembic.ini", ""),
        ]);
        let p = &detect(&t)[0];
        assert_eq!(p.dir, "backend");
        assert_eq!(
            p.task(TaskName::Test).map(|t| t.cmd.as_str()),
            Some("poetry run pytest")
        );
        assert_eq!(
            p.task(TaskName::Migrate).map(|t| t.cmd.as_str()),
            Some("poetry run alembic upgrade head")
        );
    }

    #[test]
    fn a_makefile_variable_is_not_a_recipe() {
        assert_eq!(
            recipes("GO ?= go\nX := 1\ntest: build\n\tgo test ./...\n.PHONY: test\n"),
            ["test"]
        );
    }
}
