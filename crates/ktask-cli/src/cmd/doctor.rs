//! Doctor command: check provider availability, git, toolchain, etc.

use crate::{json, render};
use ktask_core::RunOutcome;
use serde::Serialize;
use std::fmt::Write;
use std::path::PathBuf;
use std::process::Command;

#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "lowercase")]
enum Status {
    Pass,
    Fail,
}

#[derive(Debug, Clone, Serialize)]
#[allow(clippy::struct_field_names)]
struct Check {
    check: String,
    status: Status,
    detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    remedy: Option<String>,
}

pub(crate) fn run(json_output: bool) -> RunOutcome {
    let checks = vec![
        check_git_availability(),
        check_git_version(),
        check_toolchain(),
        check_state_dir_permissions(),
        check_provider_availability(),
    ];

    let has_failures = checks.iter().any(|c| c.status == Status::Fail);

    if json_output {
        let _ = json::emit_json(&checks);
    } else {
        for check in &checks {
            let status_str = match check.status {
                Status::Pass => "PASS",
                Status::Fail => "FAIL",
            };

            let mut line = format!("{}: {status_str} {}", check.check, check.detail);
            if let Some(remedy) = &check.remedy {
                let _ = write!(line, " [{remedy}]");
            }

            render::out(format_args!("{line}"));
        }
    }

    if has_failures {
        RunOutcome::TaskFailed {
            task: ktask_core::TaskId::new(0),
        }
    } else {
        RunOutcome::Drained
    }
}

fn check_git_availability() -> Check {
    match Command::new("git").arg("--version").output() {
        Ok(output) if output.status.success() => Check {
            check: "git_availability".to_string(),
            status: Status::Pass,
            detail: "git is available".to_string(),
            remedy: None,
        },
        _ => Check {
            check: "git_availability".to_string(),
            status: Status::Fail,
            detail: "git not found".to_string(),
            remedy: Some("install git".to_string()),
        },
    }
}

fn check_git_version() -> Check {
    match Command::new("git").arg("--version").output() {
        Ok(output) if output.status.success() => {
            let version = String::from_utf8_lossy(&output.stdout);
            Check {
                check: "git_version".to_string(),
                status: Status::Pass,
                detail: version.trim().to_string(),
                remedy: None,
            }
        }
        _ => Check {
            check: "git_version".to_string(),
            status: Status::Fail,
            detail: "unable to determine git version".to_string(),
            remedy: Some("verify git is installed and working".to_string()),
        },
    }
}

fn check_toolchain() -> Check {
    match Command::new("rustc").arg("--version").output() {
        Ok(output) if output.status.success() => {
            let version = String::from_utf8_lossy(&output.stdout);
            Check {
                check: "toolchain".to_string(),
                status: Status::Pass,
                detail: version.trim().to_string(),
                remedy: None,
            }
        }
        _ => Check {
            check: "toolchain".to_string(),
            status: Status::Fail,
            detail: "Rust toolchain not found".to_string(),
            remedy: Some("install Rust via rustup".to_string()),
        },
    }
}

fn check_state_dir_permissions() -> Check {
    let state_dir = get_state_dir();

    if let Some(dir) = &state_dir {
        match std::fs::metadata(dir) {
            Ok(metadata) => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;

                    let perms = metadata.permissions();
                    let mode = perms.mode();
                    let is_writable = (mode & 0o200) != 0;

                    if is_writable {
                        Check {
                            check: "state_dir_permissions".to_string(),
                            status: Status::Pass,
                            detail: format!("state dir is writable at {}", dir.display()),
                            remedy: None,
                        }
                    } else {
                        Check {
                            check: "state_dir_permissions".to_string(),
                            status: Status::Fail,
                            detail: format!("state dir {} is not writable", dir.display()),
                            remedy: Some(format!("fix permissions: chmod u+w {}", dir.display())),
                        }
                    }
                }
                #[cfg(not(unix))]
                {
                    Check {
                        check: "state_dir_permissions".to_string(),
                        status: Status::Pass,
                        detail: format!("state dir exists at {}", dir.display()),
                        remedy: None,
                    }
                }
            }
            Err(_) => Check {
                check: "state_dir_permissions".to_string(),
                status: Status::Fail,
                detail: format!(
                    "state dir {} does not exist or is not accessible",
                    dir.display()
                ),
                remedy: Some(format!("create directory: mkdir -p {}", dir.display())),
            },
        }
    } else {
        Check {
            check: "state_dir_permissions".to_string(),
            status: Status::Fail,
            detail: "unable to determine state directory".to_string(),
            remedy: Some("set XDG_STATE_HOME or HOME environment variable".to_string()),
        }
    }
}

fn check_provider_availability() -> Check {
    let providers_to_check = vec!["claude", "dummy"];

    for provider in providers_to_check {
        if Command::new(provider).arg("--version").output().is_ok() {
            return Check {
                check: "provider_availability".to_string(),
                status: Status::Pass,
                detail: format!("provider '{provider}' is available"),
                remedy: None,
            };
        }
    }

    Check {
        check: "provider_availability".to_string(),
        status: Status::Fail,
        detail: "no supported provider available (claude or dummy)".to_string(),
        remedy: Some("install Claude CLI or use dummy provider".to_string()),
    }
}

fn get_state_dir() -> Option<PathBuf> {
    if let Ok(xdg_state) = std::env::var("XDG_STATE_HOME") {
        Some(PathBuf::from(xdg_state).join("ktask-rs"))
    } else if let Ok(home) = std::env::var("HOME") {
        Some(PathBuf::from(home).join(".local/share/ktask-rs"))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_git_availability_structure() {
        let check = check_git_availability();
        assert_eq!(check.check, "git_availability");
        assert!(matches!(check.status, Status::Pass | Status::Fail));
        assert!(!check.detail.is_empty());
    }

    #[test]
    fn check_git_version_structure() {
        let check = check_git_version();
        assert_eq!(check.check, "git_version");
        assert!(matches!(check.status, Status::Pass | Status::Fail));
        assert!(!check.detail.is_empty());
    }

    #[test]
    fn check_toolchain_structure() {
        let check = check_toolchain();
        assert_eq!(check.check, "toolchain");
        assert!(matches!(check.status, Status::Pass | Status::Fail));
        assert!(!check.detail.is_empty());
    }

    #[test]
    fn check_state_dir_permissions_structure() {
        let check = check_state_dir_permissions();
        assert_eq!(check.check, "state_dir_permissions");
        assert!(matches!(check.status, Status::Pass | Status::Fail));
        assert!(!check.detail.is_empty());
    }

    #[test]
    fn check_provider_availability_structure() {
        let check = check_provider_availability();
        assert_eq!(check.check, "provider_availability");
        assert!(matches!(check.status, Status::Pass | Status::Fail));
        assert!(!check.detail.is_empty());
    }

    #[test]
    fn check_serialize_to_json() {
        let check = Check {
            check: "test".to_string(),
            status: Status::Pass,
            detail: "test detail".to_string(),
            remedy: None,
        };
        let json = serde_json::to_string(&check).unwrap();
        assert!(json.contains("\"check\":\"test\""));
        assert!(json.contains("\"status\":\"pass\""));
        assert!(json.contains("\"detail\":\"test detail\""));
        assert!(!json.contains("remedy"));
    }

    #[test]
    fn check_serialize_with_remedy() {
        let check = Check {
            check: "test".to_string(),
            status: Status::Fail,
            detail: "test detail".to_string(),
            remedy: Some("fix this".to_string()),
        };
        let json = serde_json::to_string(&check).unwrap();
        assert!(json.contains("\"remedy\":\"fix this\""));
    }

    #[test]
    fn status_serializes_correctly() {
        assert_eq!(serde_json::to_string(&Status::Pass).unwrap(), "\"pass\"");
        assert_eq!(serde_json::to_string(&Status::Fail).unwrap(), "\"fail\"");
    }

    #[test]
    fn get_state_dir_returns_xdg_state_home() {
        // This test can't control XDG_STATE_HOME in the test environment,
        // but we can verify the function doesn't panic
        let _dir = get_state_dir();
    }
}
