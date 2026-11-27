//! The golden screen gate of `make check` and `make golden`, run from the project root.
//!
//! `verify DIR` compares the screens the tests recorded in DIR with `tests/golden` and
//! writes the changed-screens report against the approved set, the golden files committed
//! at the starting commit. It never writes a golden file. `update DIR` adopts DIR as the
//! golden set.
#[allow(dead_code)]
#[path = "../tests/support/gate.rs"]
mod gate;
#[allow(dead_code)]
#[path = "../tests/support/golden.rs"]
mod golden;

use std::{
    path::{Path, PathBuf},
    process::ExitCode,
};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let project = Path::new(".");
    let result = match args.as_slice() {
        ["verify", recording] => {
            // The supervisor's artifact directory when it collects evidence.
            let reports = std::env::var_os("KTASK_ARTIFACT_DIR")
                .map_or_else(|| PathBuf::from("target"), PathBuf::from);
            // The supervisor names the commit the work started from; HEAD is that commit
            // until the work is committed.
            let base = std::env::var("KTASK_BASE_COMMIT").unwrap_or_else(|_| "HEAD".into());
            gate::check(project, Path::new(recording), &reports, &base).map(|said| said.join("\n"))
        }
        ["update", recording] => gate::update(project, Path::new(recording)),
        _ => Err("usage: screens verify|update RECORDED_DIR".into()),
    };
    match result {
        Ok(said) => {
            println!("{said}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}
