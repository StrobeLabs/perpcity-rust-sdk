//! Building and loading rustdoc's JSON for the crate.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use rustdoc_types::{Crate, FORMAT_VERSION};

/// The nightly whose rustdoc emits the format `rustdoc-types` expects; CI
/// pins it through `DESIGN_TOOLCHAIN`, locally `nightly` is tried and the
/// format check below says when it has moved on.
pub const TOOLCHAIN: &str = "nightly-2026-04-23";

/// Run rustdoc with JSON output and load the crate.
pub fn load(root: &Path) -> Result<Crate> {
    let toolchain = env::var("DESIGN_TOOLCHAIN").unwrap_or_else(|_| "nightly".to_string());
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
        ])
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
        bail!(
            "rustdoc failed under toolchain {toolchain} (install it with `rustup toolchain install {toolchain}`)"
        );
    }
    let path: PathBuf = root.join("target/doc/perpcity_sdk.json");
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
