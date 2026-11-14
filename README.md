# rizr

One control plane for any project. Clone a supported repo, run `rizr up`, and get its stack running in one
command. `rizr why <thing>` explains what runs and why.

Status: early development (phase D1).

## Install

Linux, x86_64 or aarch64. Needs docker with compose v2.

    arch=$(uname -m)
    tag=$(curl -fsSL https://api.github.com/repos/Andromeda-Desktop/rizr/releases/latest | grep -o '"tag_name": *"[^"]*"' | cut -d'"' -f4)
    curl -fLO "https://github.com/Andromeda-Desktop/rizr/releases/download/$tag/rizr-$tag-$arch-unknown-linux-musl"
    curl -fLO "https://github.com/Andromeda-Desktop/rizr/releases/download/$tag/rizr-$tag-$arch-unknown-linux-musl.sha256"
    sha256sum -c "rizr-$tag-$arch-unknown-linux-musl.sha256"
    install -m 755 "rizr-$tag-$arch-unknown-linux-musl" ~/.local/bin/rizr

Then, in a repo with a compose file: `rizr up`. `rizr doctor` checks the host first.

## Build

    cargo build --release

Developer gate: `just check` (format, clippy, tests, cargo-deny).

## License

Apache-2.0. See [LICENSE](LICENSE).
