use std::path::Path;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use rizr_core::Action;
use rizr_core::error::{Error, ErrorCode};
use rizr_core::view::{Drift, Receipt};

mod compose;
mod docker;
mod trust;

/// One control plane for any project: run a repo's stack with one command and see why it works.
#[derive(Parser)]
#[command(name = "rizr", version)]
struct Cli {
    /// Print machine-readable JSON.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    #[command(flatten)]
    Action(Action),
    /// Print the JSON Schema of the API: actions, errors, receipt and drift.
    Schema,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::Schema => {
            let schema = serde_json::json!({
                "action": schemars::schema_for!(Action),
                "error": schemars::schema_for!(Error),
                "receipt": schemars::schema_for!(Receipt),
                "drift": schemars::schema_for!(Drift),
            });
            println!("{schema:#}");
            ExitCode::SUCCESS
        }
        Command::Action(Action::Trust) => match trust::gate(Path::new("."), true) {
            Ok(i) => {
                eprintln!("trusted {} ({} files)", i.root.display(), i.files.len());
                ExitCode::SUCCESS
            }
            Err(e) => fail(cli.json, &e),
        },
        Command::Action(action) => {
            let i = match trust::gate(Path::new("."), !cli.json) {
                Ok(i) => i,
                Err(e) => return fail(cli.json, &e),
            };
            let Some(data) = trust::store_path().and_then(|p| p.parent().map(Path::to_path_buf))
            else {
                return fail(
                    cli.json,
                    &Error::new(ErrorCode::Usage, "HOME is not set", "set HOME", "rizr up"),
                );
            };
            let name = verb(&action);
            let stack = docker::Stack::new(i.root, &data);
            match docker::run(&stack, i.digest, action) {
                Ok(_) => {
                    eprintln!("{name}: done ({})", stack.dir.display());
                    ExitCode::SUCCESS
                }
                Err(e) => fail(cli.json, &e),
            }
        }
    }
}

fn verb(a: &Action) -> &'static str {
    match a {
        Action::Up { .. } => "up",
        Action::Down => "down",
        Action::Restart { .. } => "restart",
        Action::Trust => "trust",
    }
}

fn fail(json: bool, e: &Error) -> ExitCode {
    if json {
        println!("{}", serde_json::to_string(e).unwrap_or_default());
    } else {
        eprintln!("error: {}\nhint: {}\ntry: {}", e.message, e.hint, e.example);
    }
    ExitCode::from(e.code.class().exit_code())
}
