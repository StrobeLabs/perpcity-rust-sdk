//! The `cargo xtask` entry point; the work is in the library.

use std::env;
use std::process::ExitCode;

use anyhow::{Result, bail};
use xtask::design;

const HELP: &str = "\
cargo xtask design [--check] [--fmt] [--report] [--diff <ref>] [--open]

  --check       verify every table link resolves, every claim matches a
                signature, and the enforced invariants hold (the CI job)
  --fmt         rewrite the tables' link targets to their canonical files
  --report      print the graph's numbers and the reported invariants
  --diff <ref>  build the graph at a git ref too and print what changed,
                as markdown (the design job posts it on every PR)
  --open        write target/design/index.html and open it
";

/// Exit non-zero when the check found problems or the tool failed.
fn main() -> ExitCode {
    match run() {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}

/// Parse the command line and dispatch; `Ok(false)` means problems were
/// printed.
fn run() -> Result<bool> {
    let args: Vec<String> = env::args().skip(1).collect();
    let Some(cmd) = args.first() else {
        print!("{HELP}");
        return Ok(true);
    };
    if cmd != "design" {
        bail!("unknown command `{cmd}`\n{HELP}");
    }
    let flags: Vec<&str> = args[1..].iter().map(String::as_str).collect();
    let mut opts = design::Options::default();
    let mut i = 0;
    while i < flags.len() {
        match flags[i] {
            "--check" => opts.check = true,
            "--fmt" => opts.fmt = true,
            "--report" => opts.report = true,
            "--open" => opts.open = true,
            "--diff" => {
                i += 1;
                let Some(r) = flags.get(i) else {
                    bail!("--diff needs a git ref\n{HELP}")
                };
                opts.diff = Some(r.to_string());
            }
            "--help" | "-h" => {
                print!("{HELP}");
                return Ok(true);
            }
            other => bail!("unknown flag `{other}`\n{HELP}"),
        }
        i += 1;
    }
    if !(opts.check || opts.fmt || opts.report || opts.open || opts.diff.is_some()) {
        opts.check = true;
    }
    design::run(opts)
}
