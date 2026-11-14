//! VISION section 7 Trust: an untrusted repo with hostile tool configs runs nothing before the prompt.
#![allow(clippy::unwrap_used, reason = "a test panics on a broken precondition")]

use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};

#[test]
fn hostile_untrusted_repo_starts_no_process() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, bin, home) = (
        tmp.path().join("repo"),
        tmp.path().join("bin"),
        tmp.path().join("home"),
    );
    let marker = tmp.path().join("ran");
    for d in [&repo, &bin, &home, &repo.join(".cargo")] {
        std::fs::create_dir_all(d).unwrap();
    }
    let w = |p: &std::path::Path, s: &str| std::fs::write(p, s).unwrap();
    w(
        &repo.join(".mise.toml"),
        &format!("[hooks]\nenter = \"touch {}\"\n", marker.display()),
    );
    w(
        &repo.join(".cargo/config.toml"),
        &format!(
            "[build]\nrustc-wrapper = \"/bin/sh -c 'touch {}'\"\n",
            marker.display()
        ),
    );
    w(
        &repo.join("compose.yml"),
        &format!(
            "services:\n  x:\n    build: .\n    command: touch {m}\n    healthcheck: {{test: [CMD, touch, {m}]}}\n",
            m = marker.display()
        ),
    );
    w(&repo.join(".env"), "COMPOSE_FILE=compose.yml\n");
    // Every tool rizr could start before trust records that it ran.
    for tool in [
        "docker",
        "podman",
        "docker-compose",
        "mise",
        "cargo",
        "npm",
        "go",
        "sh",
        "bash",
        "env",
    ] {
        let p = bin.join(tool);
        w(
            &p,
            &format!("#!/bin/sh\necho {tool} >> {}\n", marker.display()),
        );
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    for args in [
        &["up"][..],
        &["up", "--json"],
        &["down"],
        &["trust"],
        &["trust", "--yes"],
    ] {
        let out = Command::new(env!("CARGO_BIN_EXE_rizr"))
            .args(args)
            .current_dir(&repo)
            .env_clear()
            .env("PATH", &bin)
            .env("HOME", &home)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert_eq!(
            out.status.code(),
            Some(3),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            !marker.exists(),
            "{args:?} ran: {}",
            std::fs::read_to_string(&marker).unwrap()
        );
    }
    assert!(!home.join(".local/share/rizr/trust.json").exists());
}

/// After a person trusted a repo once, an agent re-trusts changed files with `--yes`, without a terminal.
#[test]
fn trust_yes_works_only_after_a_first_trust() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, home) = (tmp.path().join("repo"), tmp.path().join("home"));
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(
        repo.join("compose.yml"),
        "services:\n  x:\n    image: alpine\n",
    )
    .unwrap();
    let rizr = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_rizr"))
            .args(args)
            .current_dir(&repo)
            .env_clear()
            .env("HOME", &home)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    };
    assert_eq!(rizr(&["trust", "--yes"]).status.code(), Some(3));
    let store = home.join(".local/share/rizr/trust.json");
    std::fs::create_dir_all(store.parent().unwrap()).unwrap();
    let root = repo.canonicalize().unwrap();
    std::fs::write(&store, format!("{{\"{}\": \"stale\"}}", root.display())).unwrap();
    let refused = rizr(&["up", "--json"]);
    assert!(
        String::from_utf8_lossy(&refused.stdout).contains("rizr trust --yes"),
        "{}",
        String::from_utf8_lossy(&refused.stdout)
    );
    let out = rizr(&["trust", "--yes"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!std::fs::read_to_string(&store).unwrap().contains("stale"));
}

/// Local edits at the commit a person trusted run without a re-trust; after HEAD moves (pull, checkout) rizr asks.
#[test]
fn local_edits_run_but_a_moved_head_asks() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, home) = (tmp.path().join("repo"), tmp.path().join("home"));
    std::fs::create_dir_all(repo.join(".git/refs/heads")).unwrap();
    std::fs::write(repo.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    std::fs::write(repo.join(".git/refs/heads/main"), "aaa\n").unwrap();
    std::fs::write(
        repo.join("compose.yml"),
        "services:\n  x:\n    image: alpine\n",
    )
    .unwrap();
    let store = home.join(".local/share/rizr/trust.json");
    std::fs::create_dir_all(store.parent().unwrap()).unwrap();
    let root = repo.canonicalize().unwrap();
    let pin = |head: &str| {
        std::fs::write(
            &store,
            format!(
                "{{\"{}\": {{\"digest\": \"before-the-edit\", \"head\": \"{head}\"}}}}",
                root.display()
            ),
        )
        .unwrap();
    };
    let plan = || {
        let o = Command::new(env!("CARGO_BIN_EXE_rizr"))
            .args(["plan", "--json"])
            .current_dir(&repo)
            .env_clear()
            .env("HOME", &home)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        format!(
            "{}{}",
            String::from_utf8_lossy(&o.stdout),
            String::from_utf8_lossy(&o.stderr)
        )
    };
    pin("aaa");
    let out = plan();
    assert!(!out.contains("untrusted"), "{out}");
    assert!(
        !std::fs::read_to_string(&store)
            .unwrap()
            .contains("before-the-edit"),
        "the edit is now the trusted digest"
    );
    pin("aaa");
    std::fs::write(repo.join(".git/refs/heads/main"), "bbb\n").unwrap();
    let out = plan();
    assert!(out.contains("untrusted"), "{out}");
}
