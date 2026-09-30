//! Building and loading rustdoc's JSON for the crate.

use std::env;
use std::fs;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};
use rustdoc_types::{Crate, FORMAT_VERSION};

/// The nightly whose rustdoc emits the format `rustdoc-types` expects. CI
/// pins it through `DESIGN_TOOLCHAIN`; locally it is used when installed,
/// else plain `nightly` is tried and the format check says when that has
/// moved on.
pub const TOOLCHAIN: &str = "nightly-2026-04-23";

/// Where the nightly build goes: its own target directory, so it never
/// invalidates the stable build's artifacts and `CARGO_TARGET_DIR` does not
/// move the JSON from under the tool.
pub fn target_dir(root: &Path) -> std::path::PathBuf {
    root.join("target/design")
}

/// Run rustdoc with JSON output and load the crate.
pub fn load(root: &Path) -> Result<Crate> {
    let toolchain = match env::var("DESIGN_TOOLCHAIN") {
        Ok(t) => t,
        Err(_) if installed(TOOLCHAIN) => TOOLCHAIN.to_string(),
        Err(_) => {
            eprintln!(
                "note: {TOOLCHAIN} is not installed (`rustup toolchain install {TOOLCHAIN}`); trying `nightly`"
            );
            "nightly".to_string()
        }
    };
    let target = target_dir(root);
    let status = Command::new("rustup")
        .args([
            "run",
            &toolchain,
            "cargo",
            "doc",
            "--no-deps",
            "--all-features",
            "--package",
            "perpcity-sdk",
            "--target-dir",
        ])
        .arg(&target)
        // Private items too: a private field or helper is evidence that a
        // type is built from another, and the surface is filtered here.
        .env(
            "RUSTDOCFLAGS",
            "-Z unstable-options --output-format json --document-private-items",
        )
        .current_dir(root)
        .status()
        .context("running rustdoc; is rustup installed?")?;
    if !status.success() {
        bail!("rustdoc failed under toolchain {toolchain}");
    }
    let path = target.join("doc/perpcity_sdk.json");
    let text = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let krate: Crate = serde_json::from_str(&text).context("parsing rustdoc JSON")?;
    if krate.format_version != FORMAT_VERSION {
        bail!(
            "rustdoc JSON format {} from toolchain {toolchain}, but rustdoc-types expects {}; use {TOOLCHAIN} (DESIGN_TOOLCHAIN) or bump the two together",
            krate.format_version,
            FORMAT_VERSION
        );
    }
    Ok(krate)
}

/// Whether rustup has the toolchain.
fn installed(toolchain: &str) -> bool {
    Command::new("rustup")
        .args(["run", toolchain, "rustc", "--version"])
        .output()
        .is_ok_and(|o| o.status.success())
}
