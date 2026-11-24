//! Workspace automation task runner (`cargo xtask`).

mod bench;

use std::env;
use std::process::{Command, ExitCode};

/// Ordered quality gates run by `cargo xtask ci`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Gate {
    Fmt,
    Clippy,
    Test,
    Doctest,
    Docs,
    Msrv,
    Deny,
}

impl Gate {
    const ALL: [Self; 7] = [
        Self::Fmt,
        Self::Clippy,
        Self::Test,
        Self::Doctest,
        Self::Docs,
        Self::Msrv,
        Self::Deny,
    ];

    const fn name(self) -> &'static str {
        match self {
            Self::Fmt => "fmt",
            Self::Clippy => "clippy",
            Self::Test => "test",
            Self::Doctest => "doctest",
            Self::Docs => "docs",
            Self::Msrv => "msrv",
            Self::Deny => "deny",
        }
    }

    fn parse(raw: &str) -> Option<Self> {
        match raw {
            "fmt" => Some(Self::Fmt),
            "clippy" => Some(Self::Clippy),
            "test" => Some(Self::Test),
            "doctest" => Some(Self::Doctest),
            "docs" => Some(Self::Docs),
            "msrv" => Some(Self::Msrv),
            "deny" => Some(Self::Deny),
            _ => None,
        }
    }
}

const MSRV: &str = "1.96";

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None | Some("-h" | "--help" | "help") => {
            print_usage();
            ExitCode::SUCCESS
        }
        Some("ci") => run_ci(&args[1..]),
        Some("bench") => bench::run(&args[1..]),
        Some("codegen") => {
            eprintln!("xtask: subcommand `{}` is not implemented yet", args[0]);
            ExitCode::FAILURE
        }
        Some(other) => {
            eprintln!("xtask: unknown subcommand `{other}`");
            print_usage();
            ExitCode::FAILURE
        }
    }
}

fn print_usage() {
    println!("auditstream xtask");
    println!();
    println!("Planned subcommands:");
    println!("  ci [gate]  Run all local quality gates or one gate:");
    println!("             fmt, clippy, test, doctest, docs, msrv, deny");
    println!("  codegen    Regenerate schemas and artifacts");
    println!("  bench      Run pgbench capture benchmarks");
}

fn run_ci(args: &[String]) -> ExitCode {
    let gates: Vec<Gate> = match args {
        [] => Gate::ALL.to_vec(),
        [single] => {
            let Some(gate) = Gate::parse(single) else {
                eprintln!(
                    "xtask ci: unknown gate `{single}` (expected one of: fmt, clippy, test, doctest, docs, msrv, deny)"
                );
                return ExitCode::FAILURE;
            };
            vec![gate]
        }
        _ => {
            eprintln!("xtask ci: expected at most one gate name");
            return ExitCode::FAILURE;
        }
    };

    for gate in gates {
        eprintln!("==> xtask ci: running gate `{}`", gate.name());
        if let Err(err) = run_gate(gate) {
            eprintln!("==> xtask ci: gate `{}` failed: {err}", gate.name());
            return ExitCode::FAILURE;
        }
    }

    ExitCode::SUCCESS
}

fn run_gate(gate: Gate) -> Result<(), String> {
    match gate {
        Gate::Fmt => run_cmd("cargo", &["fmt", "--all", "--check"], &[]),
        Gate::Clippy => run_cmd(
            "cargo",
            &[
                "clippy",
                "--workspace",
                "--all-targets",
                "--locked",
                "--",
                "-D",
                "warnings",
            ],
            &[],
        ),
        Gate::Test => run_cmd(
            "cargo",
            &[
                "nextest",
                "run",
                "--workspace",
                "--locked",
                "--profile",
                "ci",
            ],
            &[],
        ),
        Gate::Doctest => run_cmd("cargo", &["test", "--doc", "--workspace", "--locked"], &[]),
        Gate::Docs => run_cmd(
            "cargo",
            &["doc", "--workspace", "--no-deps", "--locked"],
            &[("RUSTDOCFLAGS", "-D warnings")],
        ),
        Gate::Msrv => {
            ensure_msrv_toolchain()?;
            run_cmd(
                "cargo",
                &["hack", "check", "--rust-version", "--workspace", "--locked"],
                &[],
            )
        }
        Gate::Deny => run_cmd("cargo", &["deny", "check"], &[]),
    }
}

fn ensure_msrv_toolchain() -> Result<(), String> {
    let output = Command::new("rustup")
        .args(["toolchain", "list"])
        .output()
        .map_err(|err| format!("failed to run `rustup toolchain list`: {err}"))?;

    if !output.status.success() {
        return Err(format!(
            "`rustup toolchain list` exited with status {}",
            output.status
        ));
    }

    let installed = String::from_utf8_lossy(&output.stdout)
        .lines()
        .any(|line| line.starts_with(MSRV));

    if !installed {
        run_cmd(
            "rustup",
            &["toolchain", "install", MSRV, "--profile", "minimal"],
            &[],
        )?;
    }

    Ok(())
}

fn run_cmd(program: &str, args: &[&str], extra_env: &[(&str, &str)]) -> Result<(), String> {
    let mut cmd = Command::new(program);
    cmd.args(args);
    for (key, value) in extra_env {
        cmd.env(key, value);
    }

    let status = cmd
        .status()
        .map_err(|err| format!("failed to spawn `{program}`: {err}"))?;

    if status.success() {
        Ok(())
    } else {
        Err(format!(
            "`{program} {}` exited with status {status}",
            args.join(" ")
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::Gate;

    #[test]
    fn gate_order_matches_handbook() {
        let names: Vec<&str> = Gate::ALL.into_iter().map(Gate::name).collect();
        assert_eq!(
            names,
            ["fmt", "clippy", "test", "doctest", "docs", "msrv", "deny"]
        );
    }

    #[test]
    fn gate_parse_round_trips_all_and_rejects_unknown() {
        for gate in Gate::ALL {
            assert_eq!(Gate::parse(gate.name()), Some(gate));
        }
        assert_eq!(Gate::parse("unknown"), None);
    }
}
