use crate::support::{self, Result, Terminal};
use ktask_rs_mk3::{
    application::{
        Definition, Kind, Service, VersionedTask, dependency::Requirement, tools::Registration,
    },
    instance::Paths,
    storage::Database,
};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

const BIN: &str = env!("CARGO_BIN_EXE_ktask-dev");

fn cli(root: &Path, project: &Path, args: &[&str]) -> Result<Output> {
    Ok(Command::new(BIN)
        .env_remove("KTASK_WORKER_CALLBACK")
        .env_remove("KTASK_CALLBACK_DIR")
        .env("HOME", root)
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_STATE_HOME", root.join("state"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_RUNTIME_DIR", root.join("runtime"))
        .args(["--json", "--project"])
        .arg(project)
        .args(args)
        .output()?)
}

#[test]
fn cli_provision_plan_consent_and_policy_have_json_contracts() -> Result {
    let sandbox = support::sandbox()?;
    let root = sandbox.path();
    let project = root.join("project");
    fs::create_dir(&project)?;
    support::empty_repository(&project)?;
    let paths = Paths {
        config: root.join("config/ktask-dev"),
        state: root.join("state/ktask-dev"),
        cache: root.join("cache/ktask-dev"),
        data: root.join("data/ktask-dev"),
        runtime: root.join("runtime/ktask-dev"),
        executable: BIN.into(),
    };
    let source = root.join("source-sdk");
    fs::create_dir_all(source.join("cmdline-tools/latest/bin"))?;
    fs::create_dir_all(source.join("licenses"))?;
    support::write_executable(
        &source.join("cmdline-tools/latest/bin/sdkmanager"),
        b"#!/bin/sh\n[ \"$1\" = --version ] && { echo 16.0; exit 0; }\nexit 2\n",
    )?;
    fs::write(
        source.join("licenses/android-sdk-license"),
        ktask_rs_mk3::application::provision::ANDROID_SDK_LICENSE_SHA1,
    )?;
    let mut service = Service::open(Database::open(&paths)?, &project, None)?;
    let id = service.register_tool(
        &paths,
        &Registration {
            name: "android-sdk",
            version: "cmdline-tools-16",
            platform: std::env::consts::OS,
            architecture: std::env::consts::ARCH,
            components: &[],
            path: &source,
            managed: false,
        },
    )?;
    drop(service);
    let id = id.to_string();
    let plan = cli(
        root,
        &project,
        &[
            "tool",
            "provision-plan",
            "android-36-google-apis-x86_64",
            &id,
        ],
    )?;
    assert!(
        plan.status.success(),
        "{}",
        String::from_utf8_lossy(&plan.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&plan.stdout)?;
    assert_eq!(json["data"]["plan"]["enabled"], false);
    assert_eq!(json["data"]["plan"]["consented"], false);
    let ai = cli(
        root,
        &project,
        &[
            "--actor-kind",
            "ai",
            "--actor",
            "worker",
            "tool",
            "provision-mode",
            "automatic",
        ],
    )?;
    assert!(!ai.status.success());
    assert!(String::from_utf8_lossy(&ai.stderr).contains("Only a human actor"));
    for command in [
        vec![
            "tool",
            "provision-consent",
            "android-36-google-apis-x86_64",
            &id,
        ],
        vec!["tool", "provision-mode", "automatic"],
    ] {
        let output = cli(root, &project, &command)?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let plan = cli(
        root,
        &project,
        &[
            "tool",
            "provision-plan",
            "android-36-google-apis-x86_64",
            &id,
        ],
    )?;
    let json: serde_json::Value = serde_json::from_slice(&plan.stdout)?;
    assert_eq!(json["data"]["plan"]["enabled"], true);
    assert_eq!(json["data"]["plan"]["consented"], true);
    assert!(
        json["data"]["plan"]["packages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item == "system-images;android-36;google_apis;x86_64")
    );
    let status = cli(root, &project, &["tool", "provision-status"])?;
    assert!(status.status.success());
    let status_json: serde_json::Value = serde_json::from_slice(&status.stdout)?;
    assert!(status_json["data"]["status"].is_null());
    Ok(())
}

#[test]
fn published_sdk_rechecks_original_dependency_without_starting_a_new_session() -> Result {
    let sandbox = support::sandbox()?;
    let root = sandbox.path();
    let project = root.join("project");
    fs::create_dir(&project)?;
    support::empty_repository(&project)?;
    let paths = Paths {
        config: root.join("config/ktask-dev"),
        state: root.join("state/ktask-dev"),
        cache: root.join("cache/ktask-dev"),
        data: root.join("data/ktask-dev"),
        runtime: root.join("runtime/ktask-dev"),
        executable: BIN.into(),
    };
    let mut service = Service::open(Database::open(&paths)?, &project, None)?;
    service.create(
        &[Definition {
            title: "Android dependency".into(),
            goal: "Build app".into(),
            kind: Kind::Ai,
            category: None,
            lenses: None,
            criteria: vec!["Uses SDK".into()],
            models: std::array::from_fn(|_| None),
            attempt_limit: None,
            timeout_ms: None,
            stage_timeout_ms: None,
        }],
        None,
        true,
    )?;
    let task = service.current_task()?.ok_or("task")?;
    service.declare_requirement(
        VersionedTask {
            id: task.id,
            version: task.version,
        },
        &Requirement {
            kind: "sdk_component".into(),
            item: "system-images;android-36;google_apis;x86_64".into(),
            version: Some("123".into()),
            scope: "project".into(),
            stage: "baseline".into(),
            reproduction: "Missing exact Android image".into(),
        },
    )?;
    let task = service.current_task()?.ok_or("task")?;
    assert!(!service.admit_dependencies_for_run(
        VersionedTask {
            id: task.id,
            version: task.version
        },
        &paths.cache.join("workers"),
        &paths.executable,
        &paths,
    )?);
    let sdk = paths.data.join("tools/fixture-sdk");
    let image = sdk.join("system-images/android-36/google_apis/x86_64");
    fs::create_dir_all(&image)?;
    fs::write(
        image.join("source.properties"),
        "Pkg.Revision=123\nAndroidVersion.ApiLevel=36\nSystemImage.TagId=google_apis\nSystemImage.Abi=x86_64\n",
    )?;
    fs::write(image.join("system.img"), "fixture")?;
    fs::create_dir_all(sdk.join("licenses"))?;
    fs::write(
        sdk.join("licenses/android-sdk-license"),
        ktask_rs_mk3::application::provision::ANDROID_SDK_LICENSE_SHA1,
    )?;
    fs::create_dir_all(sdk.join("platform-tools"))?;
    fs::create_dir_all(sdk.join("emulator"))?;
    support::write_executable(&sdk.join("platform-tools/adb"), b"#!/bin/sh\necho adb\n")?;
    support::write_executable(
        &sdk.join("emulator/emulator"),
        b"#!/bin/sh\necho emulator\n",
    )?;
    let id = service.register_tool(
        &paths,
        &Registration {
            name: "android-sdk",
            version: "fixture",
            platform: std::env::consts::OS,
            architecture: std::env::consts::ARCH,
            components: &["system-images;android-36;google_apis;x86_64".into()],
            path: &sdk,
            managed: true,
        },
    )?;
    let target = service.provision_target()?;
    service.bind_tool("android-sdk", id, Some(0))?;
    let (rechecked, resumed) =
        ktask_rs_mk3::runner::continue_after_provision(&mut service, &paths, target)?;
    assert!(rechecked && !resumed);
    assert_eq!(service.current_task()?.ok_or("task")?.id, task.id);
    assert!(
        service
            .dependencies(task.id)?
            .iter()
            .all(|dependency| dependency.state == "satisfied")
    );
    assert!(service.execution_session()?.is_none());
    Ok(())
}

#[test]
fn stopped_and_rerun_session_cannot_continue_stale_android_install() -> Result {
    for original_queue in [false, true] {
        let sandbox = support::sandbox()?;
        let root = sandbox.path();
        let project = root.join("project");
        fs::create_dir(&project)?;
        support::empty_repository(&project)?;
        let paths = Paths {
            config: root.join("config/ktask-dev"),
            state: root.join("state/ktask-dev"),
            cache: root.join("cache/ktask-dev"),
            data: root.join("data/ktask-dev"),
            runtime: root.join("runtime/ktask-dev"),
            executable: BIN.into(),
        };
        let mut service = Service::open(Database::open(&paths)?, &project, None)?;
        let isolated_sdk = root.join("isolated-source-sdk");
        fs::create_dir_all(isolated_sdk.join("cmdline-tools/latest/bin"))?;
        support::write_executable(
            &isolated_sdk.join("cmdline-tools/latest/bin/sdkmanager"),
            b"#!/bin/sh\necho 16.0\n",
        )?;
        let sdk_id = service.register_tool(
            &paths,
            &Registration {
                name: "android-sdk",
                version: "fixture",
                platform: std::env::consts::OS,
                architecture: std::env::consts::ARCH,
                components: &[],
                path: &isolated_sdk,
                managed: false,
            },
        )?;
        service.bind_tool("android-sdk", sdk_id, Some(0))?;
        service.create(
            &[Definition {
                title: "Waiting Android task".into(),
                goal: "Use Android image".into(),
                kind: Kind::Ai,
                category: None,
                lenses: None,
                criteria: vec!["image".into()],
                models: std::array::from_fn(|_| None),
                attempt_limit: None,
                timeout_ms: None,
                stage_timeout_ms: None,
            }],
            None,
            true,
        )?;
        let task = service.current_task()?.ok_or("task")?;
        service.declare_requirement(
            VersionedTask {
                id: task.id,
                version: task.version,
            },
            &Requirement {
                kind: "sdk_component".into(),
                item: "system-images;android-36;google_apis;x86_64".into(),
                version: Some("7".into()),
                scope: "project".into(),
                stage: "execution".into(),
                reproduction: "Missing image".into(),
            },
        )?;
        let task = service.current_task()?.ok_or("task")?;
        assert!(!service.admit_dependencies_for_run(
            VersionedTask {
                id: task.id,
                version: task.version
            },
            &paths.cache.join("workers"),
            &paths.executable,
            &paths
        )?);
        service.start_execution_session(original_queue)?;
        service.execution_state("waiting", "Missing Android image")?;
        let target = service.provision_target()?;
        service.execution_state("stopped", "Owner stopped run")?;
        service.start_execution_session(original_queue)?;
        service.execution_state("waiting", "Replacement run")?;
        assert_eq!(
            ktask_rs_mk3::runner::continue_after_provision(&mut service, &paths, target)?,
            (false, false)
        );
        assert!(
            service
                .dependencies(task.id)?
                .iter()
                .any(|dependency| dependency.state == "waiting")
        );
    }
    Ok(())
}

#[test]
fn real_tools_plan_is_visible_from_config_by_keyboard_and_mouse() -> Result {
    for mouse in [false, true] {
        let sandbox = support::sandbox()?;
        let root = sandbox.path();
        let project = root.join("project");
        fs::create_dir(&project)?;
        support::empty_repository(&project)?;
        let paths = Paths {
            config: root.join("config/ktask-dev"),
            state: root.join("state/ktask-dev"),
            cache: root.join("cache/ktask-dev"),
            data: root.join("data/ktask-dev"),
            runtime: root.join("runtime/ktask-dev"),
            executable: BIN.into(),
        };
        let source = root.join("source-sdk");
        fs::create_dir_all(source.join("cmdline-tools/latest/bin"))?;
        fs::create_dir_all(source.join("licenses"))?;
        support::write_executable(
            &source.join("cmdline-tools/latest/bin/sdkmanager"),
            b"#!/bin/sh\n[ \"$1\" = --version ] && { echo 16.0; exit 0; }\nexit 2\n",
        )?;
        fs::write(
            source.join("licenses/android-sdk-license"),
            ktask_rs_mk3::application::provision::ANDROID_SDK_LICENSE_SHA1,
        )?;
        let mut service = Service::open(Database::open(&paths)?, &project, None)?;
        let id = service.register_tool(
            &paths,
            &Registration {
                name: "android-sdk",
                version: "cmdline-tools-16",
                platform: std::env::consts::OS,
                architecture: std::env::consts::ARCH,
                components: &[],
                path: &source,
                managed: false,
            },
        )?;
        service.create(
            &[Definition {
                title: "Android tool setup".into(),
                goal: "Build an Android app".into(),
                kind: Kind::Ai,
                category: None,
                lenses: None,
                criteria: vec!["Android checks can run".into()],
                models: std::array::from_fn(|_| None),
                attempt_limit: None,
                timeout_ms: None,
                stage_timeout_ms: None,
            }],
            None,
            true,
        )?;
        let current = service.current_task()?.ok_or("current task")?;
        service.declare_requirement(
            VersionedTask {
                id: current.id,
                version: current.version,
            },
            &Requirement {
                kind: "sdk_component".into(),
                item: "system-images;android-36;google_apis;x86_64".into(),
                version: Some("9999".into()),
                scope: "project".into(),
                stage: "execution".into(),
                reproduction: "Android image is absent".into(),
            },
        )?;
        let current = service.current_task()?.ok_or("current task")?;
        assert!(!service.admit_dependencies_for_run(
            VersionedTask {
                id: current.id,
                version: current.version
            },
            &paths.cache.join("workers"),
            std::path::Path::new(BIN),
            &paths
        )?);
        let project_id = service.project_id;
        drop(service);
        let mut terminal = Terminal::open_project(BIN, root, &project, 140, 45)?;
        let current_screen = terminal.screen("current dependency opens", |s| {
            s.contains("Current #1 · Needs dependency") && s.contains("t Tools and install plan")
        })?;
        if let Some(directory) = std::env::var_os("KTASK_ARTIFACT_DIR") {
            support::screen_png(
                &current_screen,
                &PathBuf::from(directory).join(format!("provision-current-mouse-{mouse}.png")),
            )?;
        }
        if mouse {
            terminal.click("t Tools and install plan")?;
        } else {
            terminal.send("t")?;
        }
        let current_tools =
            terminal.screen("current tools opens", |s| s.contains("Project tools"))?;
        if let Some(directory) = std::env::var_os("KTASK_ARTIFACT_DIR") {
            support::screen_png(
                &current_tools,
                &PathBuf::from(directory)
                    .join(format!("provision-current-tools-mouse-{mouse}.png")),
            )?;
        }
        terminal.click("Cancel")?;
        terminal.screen("current tools closes", |s| !s.contains("Project tools"))?;
        if mouse {
            terminal.click("4 Config")?;
            let config =
                terminal.screen("configuration opened", |s| s.contains("Configuration"))?;
            println!("PROVISION CONFIG\n{config}");
            terminal.click("t Tools")?;
        } else {
            terminal.send("4t")?;
        }
        let tools = terminal.screen("tools form", |s| s.contains("Project tools"))?;
        assert!(tools.contains("Provisioning: manual") && tools.contains(&format!("#{id}")));
        if mouse {
            for (from, to) in [
                ("< Register >", "< Bind >"),
                ("< Bind >", "< Unbind >"),
                ("< Unbind >", "< Refresh external >"),
                ("< Refresh external >", "< Plan >"),
            ] {
                terminal.click(from)?;
                terminal.screen("plan choice", |s| s.contains(to))?;
            }
            terminal.click("Apply")?;
        } else {
            terminal.send("    ")?;
            terminal.screen("plan choice", |s| s.contains("< Plan >"))?;
            terminal.send("\x13")?;
        }
        let screen = terminal.screen("reviewed install plan", |s| {
            s.contains("Android install plan")
        })?;
        println!("PROVISION PLAN mouse={mouse}\n{screen}");
        assert!(
            screen.contains("Stage capacity")
                && screen.contains("License consent")
                && screen.contains("system-images")
        );
        if let Some(directory) = std::env::var_os("KTASK_ARTIFACT_DIR") {
            support::screen_png(
                &screen,
                &PathBuf::from(directory).join(format!("provision-plan-mouse-{mouse}.png")),
            )?;
        }
        terminal.click("Close")?;
        terminal.screen("plan closed", |s| !s.contains("Android install plan"))?;
        let open_tools = |terminal: &mut Terminal| -> Result {
            if mouse {
                terminal.click("t Tools")?;
            } else {
                terminal.send("t")?;
            }
            terminal.screen("tools controls", |s| s.contains("Project tools"))?;
            Ok(())
        };
        let choose = |terminal: &mut Terminal, index: usize, label: &str| -> Result {
            if mouse {
                let labels = [
                    "Register",
                    "Bind",
                    "Unbind",
                    "Refresh external",
                    "Plan",
                    "Enable automatic",
                    "Use manual",
                    "Consent",
                    "Install",
                    "Progress",
                    "Cancel install",
                ];
                for (position, prior) in labels.iter().enumerate().take(index) {
                    terminal.click(&format!("< {prior} >"))?;
                    terminal.screen("next tool action", |s| {
                        s.contains(&format!("< {} >", labels[position + 1]))
                    })?;
                }
                terminal.screen("chosen tool action", |s| {
                    s.contains(&format!("< {label} >"))
                })?;
                terminal.click("Apply")?;
            } else {
                terminal.send(&" ".repeat(index))?;
                terminal.screen("chosen tool action", |s| {
                    s.contains(&format!("< {label} >"))
                })?;
                terminal.send("\x13")?;
            }
            Ok(())
        };
        open_tools(&mut terminal)?;
        choose(&mut terminal, 5, "Enable automatic")?;
        terminal.screen("automatic mode enabled", |s| {
            s.contains("Provisioning enabled")
        })?;
        open_tools(&mut terminal)?;
        terminal.screen("automatic mode shown", |s| {
            s.contains("Provisioning: automatic enabled")
        })?;
        choose(&mut terminal, 7, "Consent")?;
        terminal.screen("human consent recorded", |s| {
            s.contains("Recorded human license consent")
        })?;
        let stage_root = paths.data.join("provision-staging");
        let active_stage = ktask_rs_mk3::scratch::Scratch::new(&stage_root)?;
        let status_root = paths.state.join("provision");
        fs::create_dir_all(&status_root)?;
        let operation_lock = fs::File::create(status_root.join(format!("{project_id}.lock")))?;
        operation_lock.lock()?;
        let operation = format!("{project_id}-fixture");
        fs::write(
            status_root.join(format!("{project_id}.json")),
            format!(
                r#"{{"state":"running","recipe":"android-36-google-apis-x86_64","operation":"{operation}","stage_bytes":2048,"stage_entries":4,"download_cache_bytes":4096,"download_cache_entries":2,"message":"Downloading reviewed Android packages"}}"#
            ),
        )?;
        let status_path = status_root.join(format!("{project_id}.json"));
        let mut queued: serde_json::Value = serde_json::from_slice(&fs::read(&status_path)?)?;
        queued["state"] = "queued".into();
        fs::write(&status_path, serde_json::to_vec(&queued)?)?;
        let queued_reply = cli(root, &project, &["tool", "provision-status"])?;
        assert!(queued_reply.status.success());
        let queued_json: serde_json::Value = serde_json::from_slice(&queued_reply.stdout)?;
        assert_eq!(queued_json["data"]["status"]["state"], "queued");
        let cancel_reply = cli(root, &project, &["tool", "provision-cancel"])?;
        assert!(cancel_reply.status.success());
        assert!(stage_root.join(format!("cancel-{operation}")).exists());
        fs::remove_file(stage_root.join(format!("cancel-{operation}")))?;
        queued["state"] = "running".into();
        fs::write(&status_path, serde_json::to_vec(&queued)?)?;
        open_tools(&mut terminal)?;
        choose(&mut terminal, 9, "Progress")?;
        let progress = terminal.screen("installer progress view", |s| {
            s.contains("Android install progress")
                && s.contains("Downloading reviewed Android packages")
        })?;
        if let Some(directory) = std::env::var_os("KTASK_ARTIFACT_DIR") {
            support::screen_png(
                &progress,
                &PathBuf::from(directory).join(format!("provision-progress-mouse-{mouse}.png")),
            )?;
        }
        terminal.click("Close")?;
        open_tools(&mut terminal)?;
        choose(&mut terminal, 10, "Cancel install")?;
        terminal.screen("installer cancellation requested", |s| {
            s.contains("Provisioning cancellation requested")
        })?;
        assert!(stage_root.join(format!("cancel-{operation}")).exists());
        drop(active_stage);
        open_tools(&mut terminal)?;
        choose(&mut terminal, 6, "Use manual")?;
        terminal.screen("manual mode restored", |s| {
            s.contains("Manual tool setup selected")
        })?;
        open_tools(&mut terminal)?;
        choose(&mut terminal, 8, "Install")?;
        let stopped = terminal.screen("install stays in dependency wait", |s| {
            s.contains("Android provisioning stopped") && s.contains("Needs dependency")
        })?;
        if let Some(directory) = std::env::var_os("KTASK_ARTIFACT_DIR") {
            support::screen_png(
                &stopped,
                &PathBuf::from(directory).join(format!("provision-install-wait-mouse-{mouse}.png")),
            )?;
        }
        terminal.send("q")?;
        terminal.finish()?;
    }
    Ok(())
}

#[test]
fn real_binary_installs_fixture_and_rechecks_waiting_task_in_pty() -> Result {
    let sandbox = support::sandbox()?;
    let root = sandbox.path();
    let project = root.join("project");
    fs::create_dir(&project)?;
    support::empty_repository(&project)?;
    let paths = Paths {
        config: root.join("config/ktask-dev"),
        state: root.join("state/ktask-dev"),
        cache: root.join("cache/ktask-dev"),
        data: root.join("data/ktask-dev"),
        runtime: root.join("runtime/ktask-dev"),
        executable: BIN.into(),
    };
    let source = root.join("fixture-source");
    fs::create_dir_all(source.join("cmdline-tools/latest/bin"))?;
    fs::create_dir_all(source.join("licenses"))?;
    fs::write(
        source.join("licenses/android-sdk-license"),
        ktask_rs_mk3::application::provision::ANDROID_SDK_LICENSE_SHA1,
    )?;
    support::write_executable(&source.join("cmdline-tools/latest/bin/sdkmanager"), br#"#!/bin/sh
set -eu
if [ "$1" = --version ]; then echo 16.0; exit 0; fi
case "$1" in --sdk_root=*) ;; *) exit 23 ;; esac
if [ -f "$XDG_CACHE_HOME/cancel-mode" ]; then sleep 20; else sleep 2; fi
mkdir -p "$ANDROID_HOME/platform-tools" "$ANDROID_HOME/emulator" "$ANDROID_HOME/platforms/android-36" "$ANDROID_HOME/system-images/android-36/google_apis/x86_64"
printf 'Pkg.Revision=37.0.1\n' > "$ANDROID_HOME/platform-tools/source.properties"
printf 'Pkg.Revision=37.2.13\n' > "$ANDROID_HOME/emulator/source.properties"
printf 'Pkg.Revision=2\nAndroidVersion.ApiLevel=36\n' > "$ANDROID_HOME/platforms/android-36/source.properties"
printf 'Pkg.Revision=7\nAndroidVersion.ApiLevel=36\nSystemImage.TagId=google_apis\nSystemImage.Abi=x86_64\n' > "$ANDROID_HOME/system-images/android-36/google_apis/x86_64/source.properties"
printf '#!/bin/sh\nexit 0\n' > "$ANDROID_HOME/platform-tools/adb"
printf '#!/bin/sh\nexit 0\n' > "$ANDROID_HOME/emulator/emulator"
chmod +x "$ANDROID_HOME/platform-tools/adb" "$ANDROID_HOME/emulator/emulator"
truncate -s 131072 "$ANDROID_HOME/system-images/android-36/google_apis/x86_64/system.img"
"#)?;
    let mut service = Service::open(Database::open(&paths)?, &project, None)?;
    let source_id = service.register_tool(
        &paths,
        &Registration {
            name: "android-sdk",
            version: "cmdline-tools-16",
            platform: std::env::consts::OS,
            architecture: std::env::consts::ARCH,
            components: &[],
            path: &source,
            managed: false,
        },
    )?;
    service.set_provision_enabled(true)?;
    service.consent_to_provision("android-36-google-apis-x86_64", source_id)?;
    service.create(
        &[Definition {
            title: "Fixture Android wait".into(),
            goal: "Exercise local SDK provisioning".into(),
            kind: Kind::Ai,
            category: None,
            lenses: None,
            criteria: vec!["image available".into()],
            models: std::array::from_fn(|_| None),
            attempt_limit: None,
            timeout_ms: None,
            stage_timeout_ms: None,
        }],
        None,
        true,
    )?;
    let task = service.current_task()?.ok_or("task")?;
    service.declare_requirement(
        VersionedTask {
            id: task.id,
            version: task.version,
        },
        &Requirement {
            kind: "sdk_component".into(),
            item: "system-images;android-36;google_apis;x86_64".into(),
            version: Some("7".into()),
            scope: "project".into(),
            stage: "execution".into(),
            reproduction: "Fixture image absent".into(),
        },
    )?;
    let task = service.current_task()?.ok_or("task")?;
    let source_binding = service.bind_tool("android-sdk", source_id, Some(0))?;
    assert!(!service.admit_dependencies_for_run(
        VersionedTask {
            id: task.id,
            version: task.version
        },
        &paths.cache.join("workers"),
        &paths.executable,
        &paths
    )?);
    service.unbind_tool("android-sdk", source_binding)?;
    drop(service);
    let fake_kvm = root.join("fake-kvm");
    fs::write(&fake_kvm, [])?;
    let cancel_marker = paths.data.join("provision-download-cache/cancel-mode");
    fs::create_dir_all(cancel_marker.parent().ok_or("download cache path")?)?;
    fs::write(&cancel_marker, b"cancel first operation")?;
    let wrapper = root.join("ktask-with-fixture-kvm");
    support::write_executable(
        &wrapper,
        format!(
            "#!/bin/sh\nexec /usr/bin/bwrap --dev-bind / / --bind '{}' /dev/kvm -- '{}' \"$@\"\n",
            fake_kvm.display(),
            BIN,
        )
        .as_bytes(),
    )?;
    let mut terminal = Terminal::open_project(
        wrapper.to_str().ok_or("wrapper path")?,
        root,
        &project,
        140,
        45,
    )?;
    terminal.screen("waiting task before install", |s| {
        s.contains("Needs dependency")
    })?;
    terminal.send("4t")?;
    terminal.screen("tools setup", |s| s.contains("Project tools"))?;
    terminal.send("        ")?;
    terminal.screen("install selected", |s| s.contains("< Install >"))?;
    terminal.send("\x13")?;
    let started = terminal.screen("real installer started", |s| {
        s.contains("Android installer started")
    })?;
    if let Some(directory) = std::env::var_os("KTASK_ARTIFACT_DIR") {
        support::screen_png(
            &started,
            &PathBuf::from(directory).join("provision-real-installer-started.png"),
        )?;
    }
    terminal.send("t")?;
    terminal.screen("installer tools", |s| s.contains("Project tools"))?;
    terminal.send("         ")?;
    terminal.screen("progress selected", |s| s.contains("< Progress >"))?;
    terminal.send("\x13")?;
    let progress = terminal.screen("real installer progress", |s| {
        s.contains("Android install progress") && s.contains("Installing fixed Android packages")
    })?;
    if let Some(directory) = std::env::var_os("KTASK_ARTIFACT_DIR") {
        support::screen_png(
            &progress,
            &PathBuf::from(directory).join("provision-real-installer-progress.png"),
        )?;
    }
    terminal.click("Close")?;
    terminal.send("t")?;
    terminal.screen("tools for cancellation", |s| s.contains("Project tools"))?;
    terminal.send("          ")?;
    terminal.screen("cancel selected", |s| s.contains("< Cancel install >"))?;
    terminal.send("\x13")?;
    let cancelled = terminal.screen("real installer cancelled", |s| {
        s.contains("Android provisioning stopped") && s.contains("Needs dependency")
    })?;
    if let Some(directory) = std::env::var_os("KTASK_ARTIFACT_DIR") {
        support::screen_png(
            &cancelled,
            &PathBuf::from(directory).join("provision-real-installer-cancelled.png"),
        )?;
    }
    assert!(
        !paths
            .data
            .join(format!(
                "tools/android-36-google-apis-x86_64-source-{source_id}"
            ))
            .exists()
    );
    fs::remove_file(&cancel_marker)?;
    terminal.send("t")?;
    terminal.screen("tools for retry", |s| s.contains("Project tools"))?;
    terminal.send("        ")?;
    terminal.screen("retry install selected", |s| s.contains("< Install >"))?;
    terminal.send("\x13")?;
    terminal.screen("real installer retry started", |s| {
        s.contains("Android installer started")
    })?;
    terminal.send("1")?;
    let ready = terminal.screen("installed SDK rechecked", |s| {
        s.contains("Current #1 · Pending")
            && s.contains("dependency rechecked: true; resumed: false")
    })?;
    if let Some(directory) = std::env::var_os("KTASK_ARTIFACT_DIR") {
        support::screen_png(
            &ready,
            &PathBuf::from(directory).join("provision-real-installer-ready.png"),
        )?;
    }
    terminal.send("q")?;
    terminal.finish()?;
    let service = Service::open(Database::open(&paths)?, &project, None)?;
    let sdk = service
        .tools()?
        .into_iter()
        .find(|tool| tool.bound && tool.provenance == "managed")
        .ok_or("managed SDK binding")?;
    let image = sdk
        .path
        .join("system-images/android-36/google_apis/x86_64/system.img");
    assert_eq!(fs::metadata(&image)?.len(), 131072);
    assert!(!image.starts_with(paths.cache.join("workers")));
    Ok(())
}
