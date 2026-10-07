//! Fixes, at build time, which channel this binary belongs to and which commit it was built
//! from. The channel is `dev` unless `scripts/install-user.sh` asks for `user` through
//! `KTASK_RS_CHANNEL`; nothing at runtime decides it.

mod build_support;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::{env, fs, io};

use build_support::{Revision, channel, git};

fn main() -> ExitCode {
    match generate() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            // Nowhere left to report a failure to write to standard output.
            let _ = writeln!(io::stdout(), "cargo::error=ktask-rs build: {message}");
            ExitCode::FAILURE
        }
    }
}

fn generate() -> Result<(), String> {
    let mut out = io::stdout();
    let say = |out: &mut io::Stdout, line: &str| writeln!(out, "{line}").map_err(|e| e.to_string());
    say(&mut out, "cargo::rerun-if-env-changed=KTASK_RS_CHANNEL")?;
    let requested = env::var("KTASK_RS_CHANNEL").ok();
    let channel = channel(requested.as_deref())?;
    let package = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").ok_or("no CARGO_MANIFEST_DIR")?);
    let revision = Revision::read(&package);
    for path in watched(&package) {
        say(
            &mut out,
            &format!("cargo::rerun-if-changed={}", path.display()),
        )?;
    }
    let version = env::var("CARGO_PKG_VERSION").map_err(|e| e.to_string())?;
    let variant = if channel == "user" { "User" } else { "Dev" };
    let generated = format!(
        "pub(crate) const CHANNEL: ktask_core::Channel = ktask_core::Channel::{variant};\n\
         pub(crate) const VERSION: &str = \"{version} {channel} {}\";\n",
        revision.label()
    );
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").ok_or("no OUT_DIR")?);
    fs::write(out_dir.join("build_info.rs"), generated).map_err(|e| e.to_string())
}

/// The files whose change can change the revision: where `HEAD` points, the index, and the
/// sources and manifests that make a tree dirty. Without these cargo would keep the
/// commit of the first build.
fn watched(package: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    let git_path = |name: &str| {
        git(package, &["rev-parse", "--git-path", name]).map(|text| package.join(text.trim()))
    };
    paths.extend(git_path("HEAD"));
    paths.extend(git_path("index"));
    if let Some(reference) = git(package, &["symbolic-ref", "-q", "HEAD"]) {
        paths.extend(git_path(reference.trim()));
    }
    if let Some(top) = git(package, &["rev-parse", "--show-toplevel"]) {
        let top = PathBuf::from(top.trim());
        paths.extend(["crates", "Cargo.toml", "Cargo.lock"].map(|name| top.join(name)));
    }
    paths
}
