//! Repo tooling. `cargo xtask design` reads the design nodes' type tables,
//! resolves every name against rustdoc's JSON, verifies what the tables
//! claim against the real signatures, checks the graph-level invariants,
//! and draws the type graph as an interactive page.

mod design;
mod index;
mod invariants;
mod nodes;
mod page;
mod report;
mod rustdoc;

use std::env;
use std::process::ExitCode;

use anyhow::{Result, bail};

const HELP: &str = "\
cargo xtask design [--check] [--fmt] [--report] [--open]

  --check   verify every table link resolves, every claim matches a
            signature, and the enforced invariants hold (the CI job)
  --fmt     rewrite the tables' link targets to their canonical files
  --report  print the graph's numbers and the reported invariants
  --open    write target/design/index.html and open it
";

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
    for flag in &flags {
        match *flag {
            "--check" => opts.check = true,
            "--fmt" => opts.fmt = true,
            "--report" => opts.report = true,
            "--open" => opts.open = true,
            "--help" | "-h" => {
                print!("{HELP}");
                return Ok(true);
            }
            other => bail!("unknown flag `{other}`\n{HELP}"),
        }
    }
    if !(opts.check || opts.fmt || opts.report || opts.open) {
        opts.check = true;
    }
    design::run(opts)
}
