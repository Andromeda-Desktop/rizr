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
    for args in [&["up"][..], &["up", "--json"], &["down"], &["trust"]] {
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
