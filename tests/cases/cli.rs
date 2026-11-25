use crate::support::{self, Result, Terminal};
use serde_json::Value;
use std::{
    path::Path,
    process::{Command, Output},
};

pub(super) const DEV: &str = env!("CARGO_BIN_EXE_ktask-dev");
const PROD: &str = env!("CARGO_BIN_EXE_ktask");

fn complete_android_image_fixture(sdk: &Path, revision: &str) -> Result {
    let image = sdk.join("system-images/android-36/google_apis/x86_64");
    std::fs::create_dir_all(&image)?;
    std::fs::write(
        image.join("source.properties"),
        format!(
            "Pkg.Revision={revision}\nAndroidVersion.ApiLevel=36\nSystemImage.TagId=google_apis\nSystemImage.Abi=x86_64\n"
        ),
    )?;
    std::fs::write(image.join("system.img"), b"fake system image")?;
    std::fs::create_dir_all(sdk.join("licenses"))?;
    std::fs::write(
        sdk.join("licenses/android-sdk-license"),
        ktask_rs_mk3::application::provision::ANDROID_SDK_LICENSE_SHA1,
    )?;
    for (relative, body) in [
        (
            "platform-tools/adb",
            b"#!/bin/sh\necho 'adb 2.0'\n".as_slice(),
        ),
        (
            "emulator/emulator",
            b"#!/bin/sh\necho 'emulator 2.0'\n".as_slice(),
        ),
    ] {
        let binary = sdk.join(relative);
        if !binary.exists() {
            std::fs::create_dir_all(binary.parent().ok_or("Android binary parent")?)?;
            support::write_executable(&binary, body)?;
        }
    }
    Ok(())
}

#[test]
fn external_android_image_requires_reviewed_license_hash_on_recheck() -> Result {
    let scratch = support::sandbox()?;
    let root = scratch.path();
    support::empty_repository(root)?;
    add(root, "External Android license")?;
    let component = "system-images;android-36;google_apis;x86_64";
    json(
        root,
        &[
            "require",
            "1",
            "--kind",
            "sdk_component",
            "--item",
            component,
            "--version",
            "7",
            "--scope",
            "project",
            "--stage",
            "execution",
            "--reproduction",
            "Android image missing",
        ],
        0,
    )?;
    assert_eq!(
        json(root, &["dependency-recheck", "1"], 0)?["data"]["passed"],
        false
    );
    let sdk = root.join("external-sdk");
    complete_android_image_fixture(&sdk, "7")?;
    let registered = json(
        root,
        &[
            "tool",
            "register",
            "android-sdk",
            "36",
            std::env::consts::OS,
            std::env::consts::ARCH,
            sdk.to_str().ok_or("SDK path")?,
            "--component",
            component,
        ],
        0,
    )?;
    let id = registered["data"]["id"].as_i64().ok_or("SDK ID")?;
    json(
        root,
        &[
            "tool",
            "bind",
            "android-sdk",
            &id.to_string(),
            "--if-version",
            "0",
        ],
        0,
    )?;
    let license = sdk.join("licenses/android-sdk-license");
    let mut generation_version = 1;
    for invalid in ["", "unrelated", "0123456789abcdef0123456789abcdef01234567"] {
        std::fs::write(&license, invalid)?;
        json(
            root,
            &[
                "tool",
                "refresh",
                &id.to_string(),
                "36",
                "--component",
                component,
                "--if-version",
                &generation_version.to_string(),
            ],
            0,
        )?;
        generation_version += 1;
        let checked = json(root, &["dependency-recheck", "1"], 0)?;
        assert_eq!(checked["data"]["passed"], false, "{invalid:?}: {checked}");
    }
    std::fs::write(
        &license,
        ktask_rs_mk3::application::provision::ANDROID_SDK_LICENSE_SHA1,
    )?;
    json(
        root,
        &[
            "tool",
            "refresh",
            &id.to_string(),
            "36",
            "--component",
            component,
            "--if-version",
            &generation_version.to_string(),
        ],
        0,
    )?;
    assert_eq!(
        json(root, &["dependency-recheck", "1"], 0)?["data"]["passed"],
        true
    );
    Ok(())
}

#[test]
fn candidate_sdk_probe_uses_bound_java_and_protects_external_and_managed_installations() -> Result {
    for managed in [false, true] {
        candidate_sdk_probe_case(managed)?;
    }
    Ok(())
}

fn candidate_sdk_probe_case(managed: bool) -> Result {
    let scratch = support::sandbox()?;
    let root = scratch.path();
    support::empty_repository(root)?;
    add(root, "SDK needs bound Java")?;

    let java = root.join("java-install");
    std::fs::create_dir_all(java.join("bin"))?;
    support::write_executable(&java.join("bin/java"), b"#!/bin/sh\necho 'java 99.0'\n")?;
    let java_id = json(
        root,
        &[
            "tool",
            "register",
            "java",
            "99.0",
            std::env::consts::OS,
            std::env::consts::ARCH,
            java.to_str().ok_or("Java path")?,
        ],
        0,
    )?["data"]["id"]
        .as_i64()
        .ok_or("Java ID")?;
    json(
        root,
        &[
            "tool",
            "bind",
            "java",
            &java_id.to_string(),
            "--if-version",
            "0",
        ],
        0,
    )?;

    let bad = if managed {
        root.join(".local/share/ktask-dev/tools/sdk-write")
    } else {
        root.join("sdk-write")
    };
    let bad_binary = bad.join("cmdline-tools/latest/bin/sdkmanager");
    std::fs::create_dir_all(bad_binary.parent().ok_or("SDK binary parent")?)?;
    support::write_executable(&bad_binary, format!(
        "#!/bin/sh\nset -e\n[ \"$(java --version)\" = 'java 99.0' ]\n[ \"$ANDROID_SDK_ROOT\" = '{}' ]\nprintf started > \"$ANDROID_USER_HOME/candidate-started\"\nif (printf modified > \"$ANDROID_SDK_ROOT/unwanted\") 2>/dev/null; then\n  printf allowed > \"$ANDROID_USER_HOME/candidate-write\"\n  echo 'sdkmanager 2.0'\nelse\n  printf denied > \"$ANDROID_USER_HOME/candidate-write\"\n  exit 1\nfi\n",
        bad.display(),
    ).as_bytes())?;
    let mut register = vec![
        "tool",
        "register",
        "android-sdk",
        "36",
        std::env::consts::OS,
        std::env::consts::ARCH,
        bad.to_str().ok_or("SDK path")?,
        "--component",
        "cmdline-tools;latest",
    ];
    if managed {
        register.push("--managed");
    }
    let bad_id = json(root, &register, 0)?["data"]["id"]
        .as_i64()
        .ok_or("SDK ID")?;
    json(
        root,
        &[
            "require",
            "1",
            "--kind",
            "executable",
            "--item",
            "sdkmanager",
            "--version",
            "2.0",
            "--scope",
            "project",
            "--stage",
            "baseline",
            "--reproduction",
            "SDK needs project-bound Java",
        ],
        0,
    )?;
    assert_eq!(
        json(root, &["dependency-recheck", "1"], 0)?["data"]["passed"],
        false
    );
    let runtime = root.join("cache/ktask-dev/project-runtime/1/android");
    assert_eq!(
        std::fs::read_to_string(runtime.join("candidate-started"))?,
        "started"
    );
    assert_eq!(
        std::fs::read_to_string(runtime.join("candidate-write"))?,
        "denied"
    );
    assert!(!bad.join("unwanted").exists());
    assert!(
        json(root, &["tool", "list"], 0)?["data"]["tools"]
            .as_array()
            .ok_or("tools")?
            .iter()
            .any(|tool| tool["id"] == bad_id && !tool["bound"].as_bool().unwrap())
    );

    let good = root.join("sdk-protected");
    let good_binary = good.join("cmdline-tools/latest/bin/sdkmanager");
    std::fs::create_dir_all(good_binary.parent().ok_or("SDK binary parent")?)?;
    support::write_executable(&good_binary, format!(
        "#!/bin/sh\nset -e\n[ \"$(java --version)\" = 'java 99.0' ]\n[ \"$ANDROID_SDK_ROOT\" = '{}' ]\nprintf probe > \"$ANDROID_USER_HOME/startup\"\necho 'sdkmanager 2.0'\n",
        good.display(),
    ).as_bytes())?;
    let good_id = json(
        root,
        &[
            "tool",
            "register",
            "android-sdk",
            "36",
            std::env::consts::OS,
            std::env::consts::ARCH,
            good.to_str().ok_or("SDK path")?,
            "--component",
            "cmdline-tools;latest",
        ],
        0,
    )?["data"]["id"]
        .as_i64()
        .ok_or("good SDK ID")?;
    let checked = json(root, &["dependency-recheck", "1"], 0)?;
    assert_eq!(checked["data"]["passed"], true, "{checked}");
    assert!(
        json(root, &["tool", "list"], 0)?["data"]["tools"]
            .as_array()
            .ok_or("tools")?
            .iter()
            .any(|tool| tool["id"] == good_id && tool["bound"] == true)
    );
    assert_eq!(std::fs::read_to_string(runtime.join("startup"))?, "probe");
    Ok(())
}

#[test]
fn automatic_android_selection_checks_component_and_binary_versions() -> Result {
    let scratch = support::sandbox()?;
    let root = scratch.path();
    support::empty_repository(root)?;
    json(
        root,
        &[
            "add",
            "--title",
            "Android versions",
            "--goal",
            "Use matching SDK",
            "--criterion",
            "All versions match",
        ],
        0,
    )?;
    let component = "system-images;android-36;google_apis;x86_64";
    let mut ids = Vec::new();
    for (label, component_version, binary_version) in [
        ("old-binaries", "2.0", "1.0"),
        ("old-component", "1.0", "2.0"),
        ("compatible", "2.0", "2.0"),
    ] {
        let sdk = root.join(format!("sdk-{label}"));
        for (relative, binary) in [
            ("platform-tools/adb", "adb"),
            ("emulator/emulator", "emulator"),
            ("cmdline-tools/latest/bin/sdkmanager", "sdkmanager"),
        ] {
            let file = sdk.join(relative);
            std::fs::create_dir_all(file.parent().ok_or("binary parent")?)?;
            support::write_executable(
                &file,
                format!("#!/bin/sh\necho '{binary} {binary_version}'\n").as_bytes(),
            )?;
        }
        complete_android_image_fixture(&sdk, component_version)?;
        let registered = json(
            root,
            &[
                "tool",
                "register",
                "android-sdk",
                "36",
                std::env::consts::OS,
                std::env::consts::ARCH,
                sdk.to_str().ok_or("SDK path")?,
                "--component",
                component,
                "--component",
                "platform-tools",
                "--component",
                "emulator",
                "--component",
                "cmdline-tools;latest",
            ],
            0,
        )?;
        ids.push(registered["data"]["id"].as_i64().ok_or("generation id")?);
    }
    for (kind, item) in [
        ("executable", "sdkmanager"),
        ("sdk_component", component),
        ("executable", "emulator"),
        ("executable", "adb"),
    ] {
        json(
            root,
            &[
                "require",
                "1",
                "--kind",
                kind,
                "--item",
                item,
                "--version",
                "2.0",
                "--scope",
                "project",
                "--stage",
                "baseline",
                "--reproduction",
                "wrong version",
            ],
            0,
        )?;
    }
    let checked = json(root, &["dependency-recheck", "1"], 0)?;
    assert_eq!(checked["data"]["passed"], true, "{checked}");
    let tools = json(root, &["tool", "list"], 0)?;
    assert_eq!(tools["data"]["tools"][2]["id"], ids[2]);
    assert_eq!(tools["data"]["tools"][2]["binding_version"], 1);
    assert!(tools["data"]["tools"][0]["binding_version"].is_null());
    assert!(tools["data"]["tools"][1]["binding_version"].is_null());
    Ok(())
}

#[test]
fn standalone_android_executables_and_mixed_sdk_bindings_resolve() -> Result {
    for binary in ["adb", "emulator", "sdkmanager"] {
        let scratch = support::sandbox()?;
        let root = scratch.path();
        support::empty_repository(root)?;
        json(
            root,
            &[
                "add",
                "--title",
                "Standalone Android tool",
                "--goal",
                "Use registered tool",
                "--criterion",
                "Tool works",
            ],
            0,
        )?;
        let install = root.join(format!("standalone-{binary}"));
        std::fs::create_dir_all(install.join("bin"))?;
        support::write_executable(
            &install.join("bin").join(binary),
            format!("#!/bin/sh\necho '{binary} 2.0'\n").as_bytes(),
        )?;
        json(
            root,
            &[
                "tool",
                "register",
                binary,
                "2.0",
                std::env::consts::OS,
                std::env::consts::ARCH,
                install.to_str().ok_or("tool path")?,
            ],
            0,
        )?;
        json(
            root,
            &[
                "require",
                "1",
                "--kind",
                "executable",
                "--item",
                binary,
                "--version",
                "2.0",
                "--scope",
                "project",
                "--stage",
                "baseline",
                "--reproduction",
                "not on inherited PATH",
            ],
            0,
        )?;
        assert_eq!(
            json(root, &["dependency-recheck", "1"], 0)?["data"]["passed"],
            true
        );
        assert_eq!(
            json(root, &["tool", "list"], 0)?["data"]["tools"][0]["binding_version"],
            1
        );
    }
    let scratch = support::sandbox()?;
    let root = scratch.path();
    support::empty_repository(root)?;
    json(
        root,
        &[
            "add",
            "--title",
            "Mixed Android tools",
            "--goal",
            "Use component and standalone adb",
            "--criterion",
            "Both work",
        ],
        0,
    )?;
    let sdk = root.join("sdk");
    let component = "system-images;android-36;google_apis;x86_64";
    std::fs::create_dir_all(sdk.join("platform-tools"))?;
    support::write_executable(
        &sdk.join("platform-tools/adb"),
        b"#!/bin/sh\necho 'adb 1.0'\n",
    )?;
    complete_android_image_fixture(&sdk, "2.0")?;
    json(
        root,
        &[
            "tool",
            "register",
            "android-sdk",
            "36",
            std::env::consts::OS,
            std::env::consts::ARCH,
            sdk.to_str().ok_or("SDK path")?,
            "--component",
            component,
            "--component",
            "platform-tools",
        ],
        0,
    )?;
    let standalone = root.join("adb");
    std::fs::create_dir_all(standalone.join("bin"))?;
    support::write_executable(&standalone.join("bin/adb"), b"#!/bin/sh\necho 'adb 2.0'\n")?;
    json(
        root,
        &[
            "tool",
            "register",
            "adb",
            "2.0",
            std::env::consts::OS,
            std::env::consts::ARCH,
            standalone.to_str().ok_or("adb path")?,
        ],
        0,
    )?;
    for (kind, item) in [("sdk_component", component), ("executable", "adb")] {
        json(
            root,
            &[
                "require",
                "1",
                "--kind",
                kind,
                "--item",
                item,
                "--version",
                "2.0",
                "--scope",
                "project",
                "--stage",
                "baseline",
                "--reproduction",
                "missing",
            ],
            0,
        )?;
    }
    assert_eq!(
        json(root, &["dependency-recheck", "1"], 0)?["data"]["passed"],
        true
    );
    let list = json(root, &["tool", "list"], 0)?;
    assert!(
        list["data"]["tools"]
            .as_array()
            .ok_or("tools")?
            .iter()
            .all(|tool| tool["binding_version"].is_number()),
        "{list}"
    );
    Ok(())
}

#[test]
fn registered_android_sdk_satisfies_project_dependency_without_installing() -> Result {
    let scratch = support::sandbox()?;
    let root = scratch.path();
    support::empty_repository(root)?;
    json(
        root,
        &[
            "add",
            "--title",
            "Android fixture",
            "--goal",
            "Use existing SDK",
            "--criterion",
            "Component available",
        ],
        0,
    )?;
    let component = "system-images;android-36;google_apis;x86_64";
    let sdk = root.join("Android/Sdk");
    complete_android_image_fixture(&sdk, "1.0")?;
    let installed = sdk.join(component.replace(';', "/"));
    json(
        root,
        &[
            "require",
            "1",
            "--kind",
            "sdk_component",
            "--item",
            component,
            "--version",
            "1.0",
            "--scope",
            "project",
            "--stage",
            "baseline",
            "--reproduction",
            "SDK component unavailable",
        ],
        0,
    )?;
    let registered = json(
        root,
        &[
            "tool",
            "register",
            "android-sdk",
            "36",
            std::env::consts::OS,
            std::env::consts::ARCH,
            sdk.to_str().ok_or("non-UTF8 fixture")?,
            "--component",
            component,
        ],
        0,
    )?;
    assert_eq!(registered["data"]["id"], 1);
    let checked = json(root, &["dependency-recheck", "1"], 0)?;
    assert_eq!(checked["data"]["passed"], true);
    let listed = json(root, &["tool", "list"], 0)?;
    assert_eq!(listed["data"]["tools"][0]["binding_version"], 1);
    assert!(installed.join("source.properties").exists());
    Ok(())
}

fn tool_registry_text(screen: &str) -> String {
    support::dialog_text(screen, "Project tools", Some("Tool action"))
}

#[test]
fn registered_tool_is_visible_in_real_config_by_keyboard_and_mouse() -> Result {
    let scratch = support::sandbox()?;
    let root = scratch.path();
    support::empty_repository(root)?;
    json(
        root,
        &[
            "add",
            "--title",
            "Inspect tools",
            "--goal",
            "Show registered tools",
            "--criterion",
            "Tools visible",
        ],
        0,
    )?;
    let install = root.join("external-rg");
    std::fs::create_dir_all(install.join("bin"))?;
    support::write_executable(&install.join("bin/rg"), b"#!/bin/sh\necho 'rg 14.1'\n")?;
    let registered = json(
        root,
        &[
            "tool",
            "register",
            "rg",
            "14.1",
            std::env::consts::OS,
            std::env::consts::ARCH,
            install.to_str().ok_or("non-UTF8 fixture")?,
        ],
        0,
    )?;
    let id = registered["data"]["id"].as_i64().ok_or("generation id")?;
    let mut terminal = Terminal::open_real(DEV, root, 110, 32)?;
    terminal.send("4")?;
    terminal.screen("Config tools keyboard entry", |s| s.contains("t Tools"))?;
    terminal.send("t")?;
    terminal.screen("Unbound tool registry", |s| {
        s.contains("Project tools") && s.contains("rg 14.1")
    })?;
    terminal.send(" ")?;
    terminal.screen("Keyboard Bind choice", |s| s.contains("< Bind >"))?;
    terminal.send("\trg")?;
    terminal.send("\t\t\t\t\t\t\t")?;
    terminal.send(&id.to_string())?;
    terminal.send("\x13")?;
    terminal.screen("Keyboard tool binding saved", |s| s.contains("Bound rg"))?;
    let listed = json(root, &["tool", "list"], 0)?;
    assert_eq!(listed["data"]["tools"][0]["ready"], true);
    assert_eq!(listed["data"]["tools"][0]["binding_version"], 1);
    terminal.send("t")?;
    let keyboard = terminal.screen("Tool registry keyboard", |s| {
        let registry = tool_registry_text(s);
        registry.contains("rg 14.1") && registry.contains("bound v1")
    })?;
    println!("TOOL REGISTRY KEYBOARD SCREEN\n{keyboard}");
    if let Some(directory) = std::env::var_os("KTASK_ARTIFACT_DIR") {
        support::screen_png(
            &keyboard,
            &std::path::PathBuf::from(directory).join("tool-registry-keyboard.png"),
        )?;
    }
    terminal.send("\x1b")?;
    terminal.screen("Tool dialog closed", |s| {
        s.contains("t Tools") && !s.contains("Project tools")
    })?;
    terminal.click("t Tools")?;
    let mouse = terminal.screen("Tool registry mouse", |s| {
        s.contains("Project tools") && s.contains("rg 14.1")
    })?;
    println!("TOOL REGISTRY MOUSE SCREEN\n{mouse}");
    if let Some(directory) = std::env::var_os("KTASK_ARTIFACT_DIR") {
        support::screen_png(
            &mouse,
            &std::path::PathBuf::from(directory).join("tool-registry-mouse.png"),
        )?;
    }
    terminal.click_after("Tool action", "Register")?;
    terminal.screen("Mouse Bind choice", |s| s.contains("< Bind >"))?;
    terminal.click_after("Tool action", "Bind")?;
    terminal.screen("Mouse Unbind choice", |s| s.contains("< Unbind >"))?;
    terminal.click("Tool name")?;
    terminal.send("rg")?;
    terminal.click("Expected version (binding or generation)")?;
    terminal.send("\x011")?;
    terminal.click("[Apply]")?;
    terminal.screen("Mouse tool unbinding saved", |s| s.contains("Unbound rg"))?;
    assert!(json(root, &["tool", "list"], 0)?["data"]["tools"][0]["binding_version"].is_null());
    terminal.send("q")?;
    terminal.finish()?;
    Ok(())
}

#[test]
fn real_tool_dialog_runs_every_mutation_by_keyboard_and_mouse() -> Result {
    for mouse in [false, true] {
        let scratch = support::sandbox()?;
        let root = scratch.path();
        support::empty_repository(root)?;
        let install = root.join("external-rg");
        std::fs::create_dir_all(install.join("bin"))?;
        support::write_executable(&install.join("bin/rg"), b"#!/bin/sh\necho 'rg 14.1'\n")?;
        let mut terminal = Terminal::open_real(DEV, root, 110, 32)?;
        terminal.send("4")?;
        terminal.screen("Config", |s| s.contains("t Tools"))?;
        let open = |t: &mut Terminal| -> Result {
            if mouse {
                t.click("t Tools")?;
            } else {
                t.send("t")?;
            }
            t.screen("Tool dialog opened", |s| s.contains("Tool action"))?;
            Ok(())
        };
        let apply = |t: &mut Terminal| -> Result {
            if mouse {
                t.click("[Apply]")
            } else {
                t.send("\x13")
            }
        };
        let save_screen = |name: &str, screen: &str| -> Result {
            println!("{name}\n{screen}");
            if let Some(directory) = std::env::var_os("KTASK_ARTIFACT_DIR") {
                support::screen_png(
                    screen,
                    &std::path::PathBuf::from(directory)
                        .join(format!("tool-{name}-mouse-{mouse}.png")),
                )?;
            }
            Ok(())
        };
        let inspect = |t: &mut Terminal, name: &str, expected: &str| -> Result {
            open(t)?;
            let screen = t.screen("Tool registry state", |s| {
                let registry = tool_registry_text(s);
                registry.contains(expected) && registry.contains("ktask-test-scratch")
            })?;
            save_screen(name, &screen)?;
            t.send("\x1b")?;
            t.screen("Tool registry closed", |s| !s.contains("Tool action"))?;
            Ok(())
        };
        open(&mut terminal)?;
        terminal.screen("Empty tool registry", |s| {
            s.contains("No registered tool generations")
        })?;
        if mouse {
            terminal.click("Tool name")?;
            terminal.send("rg")?;
            terminal.click("Tool version")?;
            terminal.send("14.1")?;
            terminal.click("Installation path")?;
            terminal.send(install.to_str().ok_or("installation path")?)?;
        } else {
            terminal.send("\trg\t14.1\t\t\t")?;
            terminal.send(install.to_str().ok_or("installation path")?)?;
        }
        apply(&mut terminal)?;
        let screen = terminal.screen("Tool registered", |s| {
            s.contains("Registered tool generation #1")
        })?;
        save_screen("registered", &screen)?;
        inspect(&mut terminal, "registered-state", "rg 14.1")?;
        let id = json(root, &["tool", "list"], 0)?["data"]["tools"][0]["id"]
            .as_i64()
            .ok_or("generation id")?;

        open(&mut terminal)?;
        if mouse {
            terminal.click_after("Tool action", "Register")?;
            terminal.screen("Mouse Bind action", |s| s.contains("< Bind >"))?;
            terminal.click("Tool name")?;
            terminal.send("rg")?;
            terminal.click("Generation ID")?;
            terminal.send(&id.to_string())?;
        } else {
            terminal.send(" ")?;
            terminal.send("\trg")?;
            terminal.send(&"\t".repeat(7))?;
            terminal.send(&id.to_string())?;
        }
        apply(&mut terminal)?;
        let screen = terminal.screen("Tool bound", |s| s.contains("Bound rg"))?;
        save_screen("bound", &screen)?;
        inspect(&mut terminal, "bound-state", "bound v1")?;
        assert_eq!(
            json(root, &["tool", "list"], 0)?["data"]["tools"][0]["binding_version"],
            1
        );

        support::write_executable(&install.join("bin/rg"), b"#!/bin/sh\necho 'rg 14.2'\n")?;
        open(&mut terminal)?;
        let changed = terminal.screen("External tool changed", |s| {
            tool_registry_text(s).contains("changed or missing")
        })?;
        save_screen("changed", &changed)?;
        if mouse {
            for (choice, next) in [
                ("Register", "Bind"),
                ("Bind", "Unbind"),
                ("Unbind", "Refresh external"),
            ] {
                terminal.click_after("Tool action", choice)?;
                terminal.screen("Mouse tool action", |s| s.contains(&format!("< {next} >")))?;
            }
            terminal.click("Tool version")?;
            terminal.send("14.2")?;
            terminal.click("Generation ID")?;
            terminal.send(&id.to_string())?;
            terminal.click("Expected version (binding or generation)")?;
            terminal.send("\x011")?;
        } else {
            terminal.send("   ")?;
            terminal.send("\t\t14.2")?;
            terminal.send(&"\t".repeat(6))?;
            terminal.send(&id.to_string())?;
            terminal.send("\t\x011")?;
        }
        apply(&mut terminal)?;
        let screen = terminal.screen("Tool refreshed", |s| s.contains("Refreshed"))?;
        save_screen("refreshed", &screen)?;
        inspect(&mut terminal, "refreshed-state", "rg 14.2")?;
        assert_eq!(
            json(root, &["tool", "list"], 0)?["data"]["tools"][0]["ready"],
            true
        );

        open(&mut terminal)?;
        terminal.screen("Refreshed tool ready", |s| {
            let registry = tool_registry_text(s);
            registry.contains("rg 14.2") && registry.contains("ready")
        })?;
        if mouse {
            terminal.click_after("Tool action", "Register")?;
            terminal.screen("Mouse Bind action", |s| s.contains("< Bind >"))?;
            terminal.click_after("Tool action", "Bind")?;
            terminal.screen("Mouse Unbind action", |s| s.contains("< Unbind >"))?;
            terminal.click("Tool name")?;
            terminal.send("rg")?;
            terminal.click("Expected version (binding or generation)")?;
            terminal.send("\x011")?;
        } else {
            terminal.send("  ")?;
            terminal.send("\trg")?;
            terminal.send(&"\t".repeat(8))?;
            terminal.send("\x011")?;
        }
        apply(&mut terminal)?;
        let screen = terminal.screen("Tool unbound", |s| s.contains("Unbound rg"))?;
        save_screen("unbound", &screen)?;
        inspect(&mut terminal, "unbound-state", "external · ready")?;
        assert!(json(root, &["tool", "list"], 0)?["data"]["tools"][0]["binding_version"].is_null());
        terminal.send("q")?;
        terminal.finish()?;
    }
    Ok(())
}

#[test]
fn dependency_recheck_requires_observed_version_and_stop_restores_pending() -> Result {
    let scratch = support::sandbox()?;
    let root = scratch.path();
    support::empty_repository(root)?;
    json(
        root,
        &[
            "add",
            "--title",
            "Requires a tool",
            "--goal",
            "Use it",
            "--criterion",
            "Version is present",
        ],
        0,
    )?;
    let executable = root.join("fixture-tool");
    let item = executable.to_str().ok_or("fixture path is not UTF-8")?;
    json(
        root,
        &[
            "require",
            "1",
            "--kind",
            "executable",
            "--item",
            item,
            "--version",
            "1.2.3",
            "--scope",
            "task",
            "--stage",
            "baseline",
            "--reproduction",
            "fixture-tool --version: command not found",
        ],
        0,
    )?;
    let repeated = json(
        root,
        &[
            "require",
            "1",
            "--kind",
            "executable",
            "--item",
            item,
            "--version",
            "1.2.3",
            "--scope",
            "task",
            "--stage",
            "baseline",
            "--reproduction",
            "fixture-tool --version: command not found",
        ],
        0,
    )?;
    assert_eq!(
        repeated["data"]["dependencies"].as_array().unwrap().len(),
        1
    );
    let malformed = json(
        root,
        &[
            "require",
            "1",
            "--kind",
            "capability",
            "--item",
            "relative-license",
            "--scope",
            "host",
            "--stage",
            "baseline",
            "--reproduction",
            "license missing",
        ],
        4,
    )?;
    assert!(
        malformed["error"]["message"]
            .as_str()
            .unwrap()
            .contains("absolute local path")
    );
    assert_eq!(
        json(root, &["current"], 0)?["data"]["dependencies"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let first = json(root, &["dependency-recheck", "1"], 0)?;
    assert_eq!(first["data"]["passed"], false);
    assert!(
        first["data"]["dependencies"][0]["observed"]
            .as_str()
            .unwrap()
            .starts_with("MISSING:")
    );
    assert_eq!(
        json(root, &["current"], 0)?["data"]["state"],
        "needs_dependency"
    );
    json(root, &["answer", "1", "--text", "installed"], 4)?;
    assert_eq!(
        json(root, &["current"], 0)?["data"]["state"],
        "needs_dependency"
    );
    support::write_executable(&executable, b"#!/bin/sh\necho 'fixture-tool 2.0.0'\n")?;
    let version = json(root, &["current"], 0)?["data"]["version"]
        .as_i64()
        .unwrap()
        .to_string();
    let wrong = json(
        root,
        &["dependency-recheck", "1", "--if-version", &version],
        0,
    )?;
    assert_eq!(wrong["data"]["passed"], false);
    assert!(
        wrong["data"]["dependencies"][0]["observed"]
            .as_str()
            .unwrap()
            .starts_with("WRONG_VERSION:")
    );
    json(
        root,
        &["dependency-recheck", "1", "--if-version", &version],
        4,
    )?;
    json(root, &["stop", "1"], 0)?;
    assert_eq!(json(root, &["current"], 0)?["data"]["state"], "pending");
    support::write_executable(&executable, b"#!/bin/sh\necho 'fixture-tool 1.2.3'\n")?;
    let ready = json(root, &["dependency-recheck", "1"], 0)?;
    assert_eq!(ready["data"]["passed"], true);
    assert_eq!(json(root, &["current"], 0)?["data"]["state"], "pending");
    Ok(())
}

#[test]
fn dependency_recheck_uses_project_cwd_relative_path_and_symlink_invocation_name() -> Result {
    use std::os::unix::fs::symlink;
    let scratch = support::sandbox()?;
    let root = scratch.path();
    let project = root.join("project");
    let outside = root.join("outside");
    std::fs::create_dir_all(project.join("bin"))?;
    std::fs::create_dir_all(&outside)?;
    support::empty_repository(&project)?;
    let executable = project.join("bin/toolchain");
    support::write_executable(&executable, b"#!/bin/sh\ncase \"$0\" in */dispatcher) echo 'dispatcher 1.2.3';; *) echo 'toolchain 9.9.9';; esac\n")?;
    symlink(&executable, project.join("bin/dispatcher"))?;
    let invoke = |args: &[&str]| -> Result<Value> {
        let output = process(DEV, root)
            .current_dir(&outside)
            .env("PATH", "./bin:/usr/bin:/bin")
            .arg("--project")
            .arg(&project)
            .arg("--json")
            .args(args)
            .output()?;
        json_result(output, 0)
    };
    invoke(&[
        "add",
        "--title",
        "Project toolchain",
        "--goal",
        "Use selected tool",
        "--criterion",
        "Tool is ready",
    ])?;
    invoke(&[
        "require",
        "1",
        "--kind",
        "executable",
        "--item",
        "dispatcher",
        "--version",
        "1.2.3",
        "--scope",
        "project",
        "--stage",
        "execution",
        "--reproduction",
        "dispatcher --version unavailable",
    ])?;
    let rechecked = invoke(&["dependency-recheck", "1"])?;
    assert_eq!(rechecked["data"]["passed"], true, "{rechecked}");
    assert!(
        rechecked["data"]["dependencies"][0]["observed"]
            .as_str()
            .unwrap()
            .contains("dispatcher 1.2.3")
    );
    Ok(())
}

#[test]
fn dependency_probes_bound_fifo_sdk_metadata_and_detached_version_descendants() -> Result {
    use nix::{sys::stat::Mode, unistd::mkfifo};
    use std::time::{Duration, Instant};
    for fixture in [
        "capability_fifo",
        "sdk_fifo",
        "sdk_oversize",
        "detached_version",
        "timeout_version",
        "denied_version",
        "denied_launch",
    ] {
        let scratch = support::sandbox()?;
        let root = scratch.path();
        support::empty_repository(root)?;
        add(root, "Bound local probe")?;
        let item = match fixture {
            "capability_fifo" => {
                let fifo = root.join("capability.fifo");
                mkfifo(&fifo, Mode::S_IRUSR | Mode::S_IWUSR)?;
                fifo.to_string_lossy().into_owned()
            }
            "sdk_fifo" | "sdk_oversize" => {
                let component = root.join("android-sdk/emulator");
                std::fs::create_dir_all(&component)?;
                let properties = component.join("source.properties");
                if fixture == "sdk_fifo" {
                    mkfifo(&properties, Mode::S_IRUSR | Mode::S_IWUSR)?;
                } else {
                    std::fs::write(&properties, vec![b'x'; 5000])?;
                }
                "emulator".into()
            }
            "timeout_version" => {
                let tool = root.join("timeout-tool");
                support::write_executable(&tool, b"#!/bin/sh\nsleep 30\n")?;
                tool.to_string_lossy().into_owned()
            }
            "denied_version" => {
                use std::os::unix::fs::PermissionsExt;
                let tool = root.join("denied-tool");
                support::write_executable(&tool, b"#!/bin/sh\necho 'denied-tool 1.2.3'\n")?;
                std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o600))?;
                tool.to_string_lossy().into_owned()
            }
            "denied_launch" => {
                use std::os::unix::fs::PermissionsExt;
                let tool = root.join("denied-launch");
                std::fs::copy("/usr/bin/true", &tool)?;
                std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o001))?;
                tool.to_string_lossy().into_owned()
            }
            _ => {
                let tool = root.join("detached-tool");
                support::write_executable(&tool, b"#!/bin/sh\nsetsid sh -c 'echo $$ > \"$HOME/descendant.pid\"; sleep 30' &\nwhile [ ! -f \"$HOME/descendant.pid\" ]; do sleep 0.01; done\necho 'detached-tool 1.2.3'\n")?;
                tool.to_string_lossy().into_owned()
            }
        };
        let kind = if fixture.starts_with("sdk_") {
            "sdk_component"
        } else if fixture == "capability_fifo" {
            "capability"
        } else {
            "executable"
        };
        let mut args = vec![
            "require",
            "1",
            "--kind",
            kind,
            "--item",
            &item,
            "--scope",
            "task",
            "--stage",
            "execution",
            "--reproduction",
            "fixture probe",
        ];
        if kind != "capability" {
            args.extend([
                "--version",
                if kind == "sdk_component" {
                    "1.0"
                } else {
                    "1.2.3"
                },
            ]);
        }
        json(root, &args, 0)?;
        let started = Instant::now();
        let output = process(DEV, root)
            .env("ANDROID_SDK_ROOT", root.join("android-sdk"))
            .arg("--json")
            .args(["dependency-recheck", "1"])
            .output()?;
        let reply = json_result(output, 0)?;
        assert!(
            started.elapsed() < Duration::from_secs(6),
            "{fixture} took too long"
        );
        let observed = reply["data"]["dependencies"][0]["observed"]
            .as_str()
            .unwrap();
        if fixture == "denied_launch" {
            assert!(observed.starts_with("DENIED:"), "{observed}");
        }
        if fixture == "detached_version" {
            assert_eq!(reply["data"]["passed"], true, "{observed}");
            let pid = std::fs::read_to_string(root.join("descendant.pid"))?;
            assert!(
                !std::path::Path::new(&format!("/proc/{}", pid.trim())).exists(),
                "detached probe child survived cleanup: {pid}"
            );
        } else {
            assert_eq!(reply["data"]["passed"], false, "{observed}");
            assert!(
                observed.starts_with("FAILED:")
                    || observed.starts_with("TRANSIENT:")
                    || (fixture == "denied_version" && observed.starts_with("DENIED:"))
                    || (fixture == "denied_launch" && observed.starts_with("DENIED:")),
                "{fixture}: {observed}"
            );
        }
    }
    Ok(())
}

#[test]
fn executable_lookup_is_inside_the_guardian_deadline() -> Result {
    use std::time::{Duration, Instant};
    let scratch = support::sandbox()?;
    let root = scratch.path();
    support::empty_repository(root)?;
    add(root, "Bound executable lookup")?;
    let tool = root.join("lookup-stall");
    support::write_executable(&tool, b"#!/bin/sh\necho 'lookup-stall 1.2.3'\n")?;
    let source = root.join("lookup-shim.c");
    std::fs::write(
        &source,
        r#"
#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>
static int block(const char *path) {
    if (path && strstr(path, "lookup-stall")) { sleep(30); errno = ENOENT; return 1; }
    return 0;
}
int stat(const char *path, struct stat *buf) {
    if (block(path)) return -1;
    int (*real_stat)(const char *, struct stat *) = dlsym(RTLD_NEXT, "stat");
    return real_stat(path, buf);
}
int statx(int dirfd, const char *path, int flags, unsigned int mask, struct statx *buf) {
    if (block(path)) return -1;
    int (*real_statx)(int, const char *, int, unsigned int, struct statx *) = dlsym(RTLD_NEXT, "statx");
    return real_statx(dirfd, path, flags, mask, buf);
}
int fstatat(int dirfd, const char *path, struct stat *buf, int flags) {
    if (block(path)) return -1;
    int (*real_fstatat)(int, const char *, struct stat *, int) = dlsym(RTLD_NEXT, "fstatat");
    return real_fstatat(dirfd, path, buf, flags);
}
"#,
    )?;
    let library = root.join("lookup-shim.so");
    let compiled = Command::new("cc")
        .args(["-shared", "-fPIC", "-o"])
        .arg(&library)
        .arg(&source)
        .arg("-ldl")
        .output()?;
    assert!(
        compiled.status.success(),
        "{}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    json(
        root,
        &[
            "require",
            "1",
            "--kind",
            "executable",
            "--item",
            tool.to_str().unwrap(),
            "--version",
            "1.2.3",
            "--scope",
            "task",
            "--stage",
            "execution",
            "--reproduction",
            "metadata lookup stalled",
        ],
        0,
    )?;
    let started = Instant::now();
    let reply = json_result(
        process(DEV, root)
            .env("LD_PRELOAD", &library)
            .arg("--json")
            .args(["dependency-recheck", "1"])
            .output()?,
        0,
    )?;
    assert!(
        started.elapsed() < Duration::from_secs(6),
        "lookup escaped guardian deadline"
    );
    let observed = reply["data"]["dependencies"][0]["observed"]
        .as_str()
        .unwrap();
    assert!(observed.contains("timed out after 3 seconds"), "{observed}");
    assert_eq!(reply["data"]["passed"], false);
    Ok(())
}

#[test]
fn cli_rejects_hight_on_add_edit_defaults_and_import_without_writes() -> Result {
    let scratch = support::sandbox()?;
    let root = scratch.path();
    let bad = json(
        root,
        &[
            "model-default",
            "execution",
            "--provider",
            "codex",
            "--model",
            "executor",
            "--effort",
            "hight",
        ],
        4,
    )?;
    let message = bad["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("execution")
            && message.contains("hight")
            && message.contains("accepted values")
    );
    let bad = json(
        root,
        &[
            "add",
            "--title",
            "Bad",
            "--goal",
            "Goal",
            "--criterion",
            "Check",
            "--model",
            "execution=codex/executor",
            "--effort",
            "execution=hight",
        ],
        4,
    )?;
    assert!(bad["error"]["message"].as_str().unwrap().contains("hight"));
    assert_eq!(json(root, &["list"], 0)?["data"]["count"], 0);
    json(
        root,
        &[
            "add",
            "--title",
            "Good",
            "--goal",
            "Goal",
            "--criterion",
            "Check",
            "--model",
            "execution=codex/executor",
            "--effort",
            "execution=high",
        ],
        0,
    )?;
    let bad = json(root, &["edit", "1", "--effort", "execution=hight"], 4)?;
    assert!(bad["error"]["message"].as_str().unwrap().contains("hight"));
    assert_eq!(
        json(root, &["show", "1"], 0)?["data"]["definition"]["models"]["execution"]["effort"],
        "high"
    );
    json(root, &["export", "tasks.tsv"], 0)?;
    let path = root.join("tasks.tsv");
    let text = std::fs::read_to_string(&path)?;
    std::fs::write(&path, text.replacen("\thigh\t", "\thight\t", 1))?;
    let bad = json(root, &["import", "tasks.tsv"], 4)?;
    assert!(bad["error"]["message"].as_str().unwrap().contains("hight"));
    assert_eq!(json(root, &["list"], 0)?["data"]["count"], 1);
    Ok(())
}
pub(super) fn process(binary: &str, root: &Path) -> Command {
    let mut command = Command::new(binary);
    command
        .env_clear()
        .env("HOME", root)
        .env("XDG_STATE_HOME", root.join("state"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("XDG_RUNTIME_DIR", root.join("runtime"))
        .env("TMPDIR", root)
        .env("KTASK_INSTANCE", "production")
        .current_dir(root);
    command
}
fn run(root: &Path, args: &[&str]) -> std::io::Result<Output> {
    process(DEV, root).args(args).output()
}
pub(super) fn json_result(output: Output, exit: i32) -> Result<Value> {
    assert_eq!(
        output.status.code(),
        Some(exit),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let bytes = if exit == 0 {
        assert!(output.stderr.is_empty(), "{:?}", output.stderr);
        output.stdout
    } else {
        assert!(output.stdout.is_empty());
        output.stderr
    };
    let value: Value = serde_json::from_slice(&bytes)?;
    assert_eq!(value["schema_version"], 1);
    assert_eq!(value["ok"], exit == 0);
    Ok(value)
}
fn json(root: &Path, args: &[&str], exit: i32) -> Result<Value> {
    let output = process(DEV, root)
        .arg("--json")
        .args(args)
        .output()
        .map_err(|error| format!("launch ktask-dev with {args:?}: {error}"))?;
    json_result(output, exit)
}
fn add(root: &Path, title: &str) -> Result<i64> {
    let result = json(
        root,
        &[
            "add",
            "--title",
            title,
            "--goal",
            "Goal",
            "--criterion",
            "Check",
        ],
        0,
    )?;
    Ok(result["data"]["tasks"][0]["id"].as_i64().unwrap())
}

#[test]
fn cli_help_usage_and_read_commands_do_not_create_storage() -> Result {
    let scratch = support::sandbox()?;
    let root = scratch.path();
    for args in [
        vec!["--help"],
        vec!["add", "--help"],
        vec!["edit", "--help"],
        vec!["--version"],
    ] {
        let output = run(root, &args)?;
        assert!(output.status.success());
        assert!(output.stderr.is_empty());
        // The full help is the usage guide for scripts and agents.
        let guide = String::from_utf8_lossy(&output.stdout).contains("EXIT CODES");
        assert_eq!(guide, args == ["--help"], "{args:?}");
        println!(
            "HELP {}\n{}",
            args.join(" "),
            String::from_utf8(output.stdout)?
        );
    }
    for args in [
        vec!["list"],
        vec!["current"],
        vec!["show", "1"],
        vec!["stats"],
        vec!["events"],
        vec!["cancel", "1"],
        vec!["copy", "1"],
        vec!["done", "1"],
        vec!["run"],
        vec!["resume"],
        vec!["retry"],
    ] {
        assert_eq!(json(root, &args, 3)?["error"]["code"], "not_found");
    }
    for args in [
        vec!["add"],
        vec!["show", "0"],
        vec!["edit", "1"],
        vec!["cancel", "1", "--if-version", "0"],
        vec!["supersede", "1"],
        vec!["--demo", "list"],
        vec!["--paths", "list"],
        vec!["--actor-kind", "ai", "list"],
        vec!["events", "--limit", "1001"],
    ] {
        assert_eq!(json(root, &args, 2)?["error"]["code"], "usage");
    }
    // The commands the state machine replaced are gone, not aliases.
    for removed in [
        "reset-to-pending",
        "abandon",
        "recover",
        "resolve",
        "remediate",
        "ack",
        "delete",
        "restore",
        "reopen",
    ] {
        let refused = json(root, &[removed, "1"], 2)?;
        let message = refused["error"]["message"].as_str().unwrap();
        assert!(
            message.contains(&format!("unrecognized subcommand '{removed}'")),
            "{message}"
        );
    }
    let no_terminal = run(root, &[])?;
    assert_eq!(no_terminal.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&no_terminal.stderr).contains("terminal"));
    assert!(!root.join("state").exists());
    assert!(!root.join("cache").exists());
    Ok(())
}

#[test]
fn cli_crud_order_versions_cancel_copy_and_human_completion() -> Result {
    let scratch = support::sandbox()?;
    let root = scratch.path();
    assert_eq!(add(root, "First")?, 1);
    json(
        root,
        &[
            "add",
            "--title",
            "Human",
            "--goal",
            "Inspect",
            "--criterion",
            "Approved",
            "--kind",
            "human",
            "--before",
            "1",
        ],
        0,
    )?;
    let current = json(root, &["current"], 0)?;
    assert_eq!(current["data"]["id"], 2);
    json(root, &["done", "2", "--if-version", "1"], 0)?;
    let confirmed = json(root, &["show", "2"], 0)?;
    assert_eq!(confirmed["data"]["attempts"], serde_json::json!([]));
    assert_eq!(confirmed["data"]["lifecycle"], "succeeded");
    assert_eq!(
        json(root, &["list", "--scope", "history"], 0)?["data"]["tasks"][0]["id"],
        2
    );
    json(
        root,
        &["edit", "1", "--title", "Edited", "--if-version", "1"],
        0,
    )?;
    assert_eq!(
        json(
            root,
            &["edit", "1", "--title", "Stale", "--if-version", "1"],
            4
        )?["error"]["code"],
        "conflict"
    );
    json(root, &["cancel", "1", "--if-version", "2"], 0)?;
    assert!(json(root, &["current"], 0)?["data"].is_null());
    let history = json(root, &["list", "--scope", "history"], 0)?;
    assert_eq!(history["data"]["count"], 2);
    assert_eq!(history["data"]["tasks"][0]["id"], 1);
    assert_eq!(history["data"]["tasks"][0]["state"], "cancelled");
    assert_eq!(
        json(root, &["show", "1"], 0)?["data"]["lifecycle"],
        "cancelled"
    );
    let copied = json(root, &["copy", "1", "--if-version", "3"], 0)?;
    assert_eq!(copied["data"]["tasks"][0]["id"], 3);
    assert_eq!(json(root, &["show", "3"], 0)?["data"]["title"], "Edited");
    assert_eq!(
        json(root, &["show", "1"], 0)?["data"]["lifecycle"],
        "cancelled"
    );
    add(root, "Last")?;
    json(root, &["move", "4", "--before", "3"], 0)?;
    assert_eq!(json(root, &["list"], 0)?["data"]["tasks"][0]["id"], 4);
    json(root, &["move", "4"], 0)?;
    assert_eq!(json(root, &["current"], 0)?["data"]["id"], 3);
    json(root, &["move", "3", "--before", "3"], 4)?;
    let output = run(root, &["list"])?;
    println!("PLAIN LIST\n{}", String::from_utf8(output.stdout)?);
    Ok(())
}

#[test]
fn cli_copies_done_task_as_new_pending_and_keeps_its_history() -> Result {
    let scratch = support::sandbox()?;
    let root = scratch.path();
    json(
        root,
        &[
            "add",
            "--title",
            "Human copy",
            "--goal",
            "Confirm work",
            "--criterion",
            "Recorded",
            "--kind",
            "human",
        ],
        0,
    )?;
    add(root, "Later")?;
    json(
        root,
        &[
            "done",
            "1",
            "--reason",
            "Checked by hand",
            "--if-version",
            "1",
        ],
        0,
    )?;
    let counts = |stats: &Value| {
        (
            stats["data"]["queue"].as_i64(),
            stats["data"]["history"].as_i64(),
            stats["data"]["succeeded"].as_i64(),
        )
    };
    let before = json(root, &["stats"], 0)?;
    assert_eq!(counts(&before), (Some(1), Some(1), Some(1)));
    assert_eq!(
        json(root, &["copy", "1", "--if-version", "1"], 4)?["error"]["code"],
        "conflict"
    );
    let copied = json(
        root,
        &["copy", "1", "--before", "2", "--if-version", "2"],
        0,
    )?;
    assert_eq!(
        copied["data"],
        serde_json::json!({"action":"copied","from":1,"tasks":[{"id":3,"version":1}]})
    );
    let copy = json(root, &["show", "3"], 0)?;
    assert_eq!(
        (
            copy["data"]["state"].as_str(),
            copy["data"]["kind"].as_str()
        ),
        (Some("pending"), Some("human"))
    );
    assert_eq!(copy["data"]["lifecycle_events"], serde_json::json!([]));
    let original = json(root, &["show", "1"], 0)?;
    assert_eq!(original["data"]["state"], "done");
    assert_eq!(
        original["data"]["lifecycle_events"][0]["reason"],
        "Checked by hand"
    );
    let during = json(root, &["stats"], 0)?;
    assert_eq!(counts(&during), (Some(2), Some(1), Some(1)));
    assert_eq!(during["data"]["usage"], before["data"]["usage"]);
    assert_eq!(json(root, &["current"], 0)?["data"]["id"], 3);
    json(root, &["done", "3", "--if-version", "1"], 0)?;
    let after = json(root, &["stats"], 0)?;
    assert_eq!(counts(&after), (Some(1), Some(2), Some(2)));
    assert_eq!(after["data"]["usage"], before["data"]["usage"]);
    let db = ktask_rs_mk3::storage::Database::open_in(&root.join("state/ktask-dev"))?;
    assert_eq!(
        db.connection().query_row(
            "SELECT task_id, related_task_id FROM task_links WHERE kind='derived_from'",
            [],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
        )?,
        (3, 1)
    );
    Ok(())
}

#[test]
fn cli_walks_the_task_state_machine_with_exact_refusals() -> Result {
    let scratch = support::sandbox()?;
    let root = scratch.path();
    let pending = ["Run", "Edit", "Move", "Cancel", "Supersede", "Mark done"];
    let closed = ["Copy to queue"];
    let listed = |scope: &str| -> Result<Vec<(i64, String, Vec<String>)>> {
        let list = json(root, &["list", "--scope", scope], 0)?;
        Ok(list["data"]["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|task| {
                let actions = task["actions"].as_array().unwrap();
                (
                    task["id"].as_i64().unwrap(),
                    task["state"].as_str().unwrap().to_owned(),
                    actions.iter().map(|a| a.as_str().unwrap().into()).collect(),
                )
            })
            .collect())
    };
    let row = |id: i64, state: &str, actions: &[&str]| {
        (
            id,
            state.to_owned(),
            actions.iter().map(|a| a.to_string()).collect::<Vec<_>>(),
        )
    };
    let refused = |args: &[&str], message: &str| -> Result {
        let error = json(root, args, 4)?;
        assert_eq!(error["error"]["code"], "conflict", "{args:?}");
        assert_eq!(error["error"]["message"], message, "{args:?}");
        Ok(())
    };
    add(root, "Unneeded")?;
    add(root, "Replacement")?;
    assert_eq!(
        listed("queue")?,
        [row(1, "pending", &pending), row(2, "pending", &pending)]
    );

    let receipt = json(root, &["cancel", "1", "--reason", "Not needed"], 0)?;
    assert_eq!(
        receipt["data"],
        serde_json::json!({"action":"cancelled","id":1,"version":2})
    );
    refused(
        &["cancel", "1"],
        "Cancel is not available for a cancelled task; it offers Copy to queue",
    )?;
    assert_eq!(listed("history")?, [row(1, "cancelled", &closed)]);

    let copied = json(root, &["copy", "1"], 0)?;
    assert_eq!(copied["data"]["tasks"][0]["id"], 3);
    assert_eq!(
        listed("queue")?,
        [row(2, "pending", &pending), row(3, "pending", &pending)]
    );
    refused(
        &["copy", "3"],
        "Copy to queue is not available for a pending task; it offers Run, Edit, Move, Cancel, Supersede, Mark done",
    )?;
    refused(
        &["supersede", "3", "--by", "3"],
        "A task cannot supersede itself",
    )?;
    refused(
        &["supersede", "3", "--by", "99"],
        "Task #99 is not in this project",
    )?;

    let receipt = json(
        root,
        &["supersede", "3", "--by", "2", "--reason", "Folded in"],
        0,
    )?;
    assert_eq!(
        receipt["data"],
        serde_json::json!({"action":"superseded","id":3,"version":2})
    );
    assert_eq!(
        json(root, &["show", "3"], 0)?["data"]["resolution_reason"],
        "Superseded by #2: Folded in"
    );
    refused(
        &["done", "3"],
        "Mark done is not available for a superseded task; it offers Copy to queue",
    )?;

    let receipt = json(root, &["done", "2", "--reason", "Verified by hand"], 0)?;
    assert_eq!(
        receipt["data"],
        serde_json::json!({"action":"done","id":2,"version":2,"stash":null})
    );
    refused(
        &["cancel", "2"],
        "Cancel is not available for a done task; it offers Copy to queue",
    )?;
    refused(
        &["supersede", "2", "--by", "1"],
        "Supersede is not available for a done task; it offers Copy to queue",
    )?;
    refused(
        &["stop", "2"],
        "Stop is not available for a done task; it offers Copy to queue",
    )?;
    json(
        root,
        &[
            "add",
            "--title",
            "Owner check",
            "--goal",
            "Look",
            "--criterion",
            "Seen",
            "--kind",
            "human",
        ],
        0,
    )?;
    refused(
        &["--actor-kind", "ai", "--actor", "planner", "done", "4"],
        "Only the owner can mark Human work done",
    )?;
    json(root, &["done", "4"], 0)?;
    assert_eq!(
        json(root, &["show", "4"], 0)?["data"]["lifecycle"],
        "succeeded"
    );
    assert_eq!(
        json(root, &["show", "2"], 0)?["data"]["lifecycle"],
        "succeeded_override"
    );
    assert_eq!(listed("queue")?, []);
    assert_eq!(
        listed("history")?,
        [
            row(4, "done", &closed),
            row(2, "done", &closed),
            row(3, "superseded", &closed),
            row(1, "cancelled", &closed)
        ]
    );
    let db = ktask_rs_mk3::storage::Database::open_in(&root.join("state/ktask-dev"))?;
    let links = db
        .connection()
        .prepare("SELECT task_id, related_task_id, kind FROM task_links ORDER BY kind")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<rusqlite::Result<Vec<(i64, i64, String)>>>()?;
    assert_eq!(
        links,
        [(3, 1, "derived_from".into()), (2, 3, "replaces".into())]
    );

    let plain = String::from_utf8(run(root, &["list", "--scope", "history"])?.stdout)?;
    assert!(
        plain.contains("\tDone\t")
            && plain.contains("\tSuperseded\t")
            && plain.contains("\tCancelled\t"),
        "{plain}"
    );
    let shown = String::from_utf8(run(root, &["show", "1"])?.stdout)?;
    assert!(
        shown.contains("· Cancelled ·") && shown.contains("\nActions: Copy to queue\n"),
        "{shown}"
    );
    println!("STATE MACHINE HISTORY\n{plain}\n{shown}");
    Ok(())
}

#[test]
fn cli_models_drafts_approval_rejection_and_actor_attribution() -> Result {
    let scratch = support::sandbox()?;
    let root = scratch.path();
    add(root, "Ready")?;
    json(
        root,
        &[
            "edit",
            "1",
            "--title",
            "Retitled",
            "--approval",
            "draft",
            "--model",
            "execution=codex/worker",
            "--effort",
            "execution=high",
            "--attempt-limit",
            "7",
            "--timeout-ms",
            "unlimited",
        ],
        0,
    )?;
    let proposed = json(root, &["show", "1"], 0)?;
    assert_eq!(proposed["data"]["title"], "Ready");
    assert_eq!(proposed["data"]["pending_revision"], 2);
    json(
        root,
        &["reject", "1", "--reason", "Keep the approved inputs"],
        0,
    )?;
    assert_eq!(
        json(root, &["show", "1"], 0)?["data"]["selected_revision"],
        1
    );
    json(
        root,
        &[
            "edit",
            "1",
            "--approval",
            "draft",
            "--model",
            "execution=codex/worker",
            "--effort",
            "execution=high",
        ],
        0,
    )?;
    json(
        root,
        &["edit", "1", "--pending", "--goal", "Refined draft"],
        0,
    )?;
    json(
        root,
        &[
            "--actor-kind",
            "ai",
            "--actor",
            "planner-session-7",
            "approve",
            "1",
        ],
        0,
    )?;
    let approved = json(root, &["show", "1"], 0)?;
    assert_eq!(
        approved["data"]["definition"]["models"]["execution"]["effort"],
        "high"
    );
    assert_eq!(
        approved["data"]["revisions"][3]["actor"],
        "ai: planner-session-7"
    );
    json(
        root,
        &[
            "edit",
            "1",
            "--model",
            "review=codex/reviewer",
            "--model",
            "validation=codex/validator",
            "--model",
            "remediation=codex/strong",
            "--effort",
            "execution=none",
        ],
        0,
    )?;
    let task = json(root, &["show", "1"], 0)?;
    for role in ["execution", "review", "validation", "remediation"] {
        assert_eq!(
            task["data"]["definition"]["models"][role]["provider"],
            "codex"
        );
    }
    assert!(task["data"]["definition"]["models"]["execution"]["effort"].is_null());
    json(root, &["edit", "1", "--clear-model", "review"], 0)?;
    assert!(json(root, &["show", "1"], 0)?["data"]["definition"]["models"]["review"].is_null());
    for args in [
        vec![
            "edit",
            "1",
            "--model",
            "execution=p/a",
            "--model",
            "execution=p/b",
        ],
        vec![
            "edit",
            "1",
            "--clear-model",
            "review",
            "--effort",
            "review=high",
        ],
        vec!["edit", "1", "--effort", "review=high"],
        vec!["edit", "1", "--model", "bad=p/m"],
    ] {
        json(root, &args, 2)?;
    }
    json(
        root,
        &[
            "add",
            "--title",
            "Draft Human",
            "--goal",
            "Review",
            "--criterion",
            "Done",
            "--kind",
            "human",
            "--approval",
            "draft",
            "--before",
            "1",
        ],
        0,
    )?;
    json(root, &["reject", "2", "--reason", "Unneeded"], 0)?;
    assert_eq!(json(root, &["current"], 0)?["data"]["id"], 1);
    Ok(())
}

#[test]
fn docs_kind_is_added_edited_listed_shown_and_round_trips_through_tsv() -> Result {
    let scratch = support::sandbox()?;
    let root = scratch.path();
    let new = |kind: &str, title: &str| -> Result<i64> {
        let reply = json(
            root,
            &[
                "add",
                "--kind",
                kind,
                "--title",
                title,
                "--goal",
                "Goal",
                "--criterion",
                "Check",
            ],
            0,
        )?;
        Ok(reply["data"]["tasks"][0]["id"].as_i64().unwrap())
    };
    let docs = new("docs", "Write the plan")?;
    let ai = new("ai", "Code it")?;
    assert_eq!(
        json(root, &["show", &docs.to_string()], 0)?["data"]["kind"],
        "docs"
    );
    assert_eq!(
        json(root, &["show", &docs.to_string()], 0)?["data"]["definition"]["kind"],
        "docs"
    );
    let listed = json(root, &["list", "--kind", "docs"], 0)?;
    assert_eq!(listed["data"]["count"], 1);
    assert_eq!(listed["data"]["tasks"][0]["kind"], "docs");
    let text = run(root, &["show", &docs.to_string()])?;
    assert!(text.status.success());
    assert!(String::from_utf8_lossy(&text.stdout).contains("docs"));
    let text = run(root, &["list"])?;
    assert!(String::from_utf8_lossy(&text.stdout).contains("docs"));
    // An AI task becomes docs by edit, and an invalid kind keeps the usage exit code.
    json(root, &["edit", &ai.to_string(), "--kind", "docs"], 0)?;
    assert_eq!(
        json(root, &["list", "--kind", "docs"], 0)?["data"]["count"],
        2
    );
    json(
        root,
        &[
            "add",
            "--kind",
            "pages",
            "--title",
            "T",
            "--goal",
            "G",
            "--criterion",
            "C",
        ],
        2,
    )?;
    // Export and import keep the kind.
    json(root, &["export", "tasks.tsv"], 0)?;
    let exported = std::fs::read_to_string(root.join("tasks.tsv"))?;
    assert!(exported.starts_with("ktask-tasks-v4\n"));
    assert_eq!(
        exported.lines().filter(|l| l.starts_with("docs\t")).count(),
        2
    );
    json(root, &["import", "tasks.tsv"], 0)?;
    assert_eq!(
        json(root, &["list", "--kind", "docs"], 0)?["data"]["count"],
        4
    );
    Ok(())
}

#[test]
fn cli_export_import_preview_overwrite_and_atomic_insertion() -> Result {
    let scratch = support::sandbox()?;
    let root = scratch.path();
    add(root, "Unicode 雪\nTab\tQuote\"\\")?;
    json(root, &["export", "tasks.tsv"], 0)?;
    let bytes = std::fs::read(root.join("tasks.tsv"))?;
    json(root, &["export", "tasks.tsv"], 4)?;
    assert_eq!(std::fs::read(root.join("tasks.tsv"))?, bytes);
    json(root, &["export", "tasks.tsv", "--overwrite"], 0)?;
    assert_eq!(
        json(root, &["import", "tasks.tsv", "--preview"], 0)?["data"]["count"],
        1
    );
    json(root, &["import", "tasks.tsv", "--before", "999"], 4)?;
    assert_eq!(json(root, &["list"], 0)?["data"]["count"], 1);
    json(root, &["import", "tasks.tsv", "--before", "1"], 0)?;
    assert_eq!(json(root, &["current"], 0)?["data"]["id"], 2);
    let another = root.join("another");
    std::fs::create_dir(&another)?;
    let preview = process(DEV, root)
        .current_dir(&another)
        .args(["--json", "import", "../tasks.tsv", "--preview"])
        .output()?;
    json_result(preview, 0)?;
    let db = ktask_rs_mk3::storage::Database::open_in(&root.join("state/ktask-dev"))?;
    assert_eq!(
        db.connection()
            .query_row("SELECT count(*) FROM projects", [], |r| r.get::<_, i64>(0))?,
        1
    );
    assert_eq!(
        db.connection()
            .query_row("SELECT origin FROM tasks WHERE id = 2", [], |r| r
                .get::<_, String>(0))?,
        "import"
    );
    Ok(())
}

#[test]
fn cli_json_errors_plain_output_and_events_are_machine_safe() -> Result {
    let scratch = support::sandbox()?;
    let root = scratch.path();
    add(root, "Quotes \" Unicode 雪\nEscape\u{1b}[2J")?;
    let showed = json(root, &["show", "1"], 0)?;
    assert!(showed["data"]["title"].as_str().unwrap().contains('\u{1b}'));
    let plain = run(root, &["show", "1"])?;
    assert!(!plain.stdout.contains(&0x1b));
    println!("PLAIN SHOW\n{}", String::from_utf8(plain.stdout)?);
    let missing = json(root, &["show", "999"], 3)?;
    println!("JSON ERROR\n{missing}");
    assert_eq!(
        json(root, &["import", "missing.tsv"], 1)?["error"]["code"],
        "io"
    );
    let events = json(root, &["events", "--limit", "1"], 0)?;
    assert_eq!(events["data"]["events"][0]["kind"], "task.created");
    let after = events["data"]["next_after"].to_string();
    assert_eq!(
        json(root, &["events", "--after", &after], 0)?["data"]["events"],
        serde_json::json!([])
    );
    let stats = json(root, &["stats"], 0)?;
    assert_eq!(stats["data"]["queue"], 1);
    assert!(stats["data"]["recorded_cost_microusd"].is_null());
    Ok(())
}

#[test]
fn cli_and_tui_share_commands_refresh_and_conflict_guards() -> Result {
    let scratch = support::sandbox()?;
    let root = scratch.path();
    let mut tui = Terminal::open_real(DEV, root, 70, 22)?;
    tui.screen("empty", |s| s.contains("No current task."))?;
    add(root, "Added through CLI")?;
    println!(
        "CLI → TUI\n{}",
        tui.screen("external addition", |s| s.contains("Added through CLI"))?
    );
    tui.click("e Edit")?;
    tui.screen("edit", |s| s.contains("Edit task"))?;
    json(
        root,
        &["edit", "1", "--title", "CLI wins", "--if-version", "1"],
        0,
    )?;
    tui.send("\x01Stale TUI change\r")?;
    println!(
        "CONFLICT\n{}",
        tui.screen("stale", |s| s.contains("Task changed elsewhere"))?
    );
    tui.send("\x1b")?;
    tui.screen("fresh", |s| s.contains("CLI wins"))?;
    tui.send("e\x01TUI wins\r")?;
    tui.screen("saved", |s| {
        s.contains("TUI wins") && s.contains("Saved to SQLite")
    })?;
    let task = json(root, &["show", "1"], 0)?;
    assert_eq!(task["data"]["title"], "TUI wins");
    assert_eq!(task["data"]["version"], 3);
    tui.send("\x03")?;
    tui.finish()?;
    Ok(())
}

#[test]
fn cli_concurrent_writers_cannot_overwrite_the_same_version() -> Result {
    let scratch = support::sandbox()?;
    let root = scratch.path();
    add(root, "Original")?;
    let results = std::thread::scope(|scope| {
        let a = scope.spawn(|| {
            run(
                root,
                &["--json", "edit", "1", "--title", "A", "--if-version", "1"],
            )
        });
        let b = scope.spawn(|| {
            run(
                root,
                &["--json", "edit", "1", "--title", "B", "--if-version", "1"],
            )
        });
        [a.join().unwrap(), b.join().unwrap()]
    });
    let mut codes = Vec::new();
    for result in results {
        let output = result?;
        let code = output.status.code().unwrap();
        codes.push(code);
        json_result(output, code)?;
    }
    codes.sort();
    assert_eq!(codes, [0, 4]);
    assert_eq!(
        json(root, &["show", "1"], 0)?["data"]["revisions"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    Ok(())
}

#[test]
fn cli_project_and_instance_isolation_survives_renaming_and_callbacks() -> Result {
    fn step<T, E: std::fmt::Display>(name: &str, result: std::result::Result<T, E>) -> Result<T> {
        result.map_err(|error| format!("{name}: {error}").into())
    }

    let scratch = support::sandbox()?;
    let root = scratch.path();
    step("add development task", add(root, "Dev only"))?;
    let production = step(
        "launch production binary",
        process(PROD, root).args(["--json", "list"]).output(),
    )?;
    json_result(production, 3)?;
    let renamed = root.join("ktask");
    step(
        "copy development binary",
        support::copy_executable(DEV, &renamed),
    )?;
    let paths = json_result(
        step(
            "launch renamed binary",
            process(renamed.to_str().unwrap(), root)
                .args(["--paths", "--json"])
                .output(),
        )?,
        0,
    )?;
    assert_eq!(paths["data"]["instance"], "ktask-dev");
    let callback = paths["data"]["executable"].as_str().unwrap();
    assert_eq!(
        json_result(
            step(
                "launch callback binary",
                process(callback, root).args(["list", "--json"]).output()
            )?,
            0
        )?["data"]["count"],
        1
    );
    let other = root.join("other");
    std::fs::create_dir(&other)?;
    json_result(
        step(
            "launch development binary for other project",
            process(DEV, root)
                .args(["--project", other.to_str().unwrap(), "--json", "list"])
                .output(),
        )?,
        3,
    )?;
    json(
        root,
        &[
            "--project",
            "other",
            "add",
            "--title",
            "Other",
            "--goal",
            "Goal",
            "--criterion",
            "Check",
        ],
        0,
    )?;
    json(root, &["--project", "other", "show", "1"], 3)?;
    assert_eq!(
        json(root, &["--project", "other", "list"], 0)?["data"]["tasks"][0]["title"],
        "Other"
    );
    assert_eq!(
        json(root, &["list"], 0)?["data"]["tasks"][0]["title"],
        "Dev only"
    );
    Ok(())
}

#[test]
fn cli_running_guards_and_marking_failed_done_keep_attempts_immutable() -> Result {
    use ktask_rs_mk3::storage::{Database, execution::StartAttempt};
    let scratch = support::sandbox()?;
    let root = scratch.path();
    // Mark done on a failed task saves the attempt's changes in Git first.
    support::empty_repository(root)?;
    json(
        root,
        &[
            "add",
            "--title",
            "Failure",
            "--goal",
            "Goal",
            "--criterion",
            "Check",
            "--model",
            "execution=codex/worker",
            "--model",
            "review=codex/reviewer",
            "--model",
            "validation=codex/validator",
            "--model",
            "remediation=codex/strong",
        ],
        0,
    )?;
    let mut db = Database::open_in(&root.join("state/ktask-dev"))?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_millis() as i64;
    let attempt = db.start_attempt(StartAttempt {
        task_id: 1,
        expected_version: 1,
        execution_token: "test-record-only",
        now_ms: now,
    })?;
    json(root, &["move", "1"], 4)?;
    assert_eq!(
        json(root, &["cancel", "1"], 4)?["error"]["message"],
        "Cancel is not available for a running task; it offers Pause, Stop, Restart, Mark done"
    );
    json(root, &["edit", "1", "--title", "Unsafe"], 4)?;
    json(
        root,
        &[
            "add",
            "--title",
            "Before running",
            "--goal",
            "Goal",
            "--criterion",
            "Check",
            "--before",
            "1",
        ],
        4,
    )?;
    db.connection().execute("UPDATE attempts SET state = 'failed', finished_at_ms = started_at_ms, outcome = 'Recorded failure', failure_category = 'check' WHERE id = ?1", [attempt])?;
    json(
        root,
        &[
            "edit",
            "1",
            "--model",
            "execution=codex/strong",
            "--effort",
            "execution=high",
        ],
        0,
    )?;
    let failed = json(root, &["show", "1"], 0)?;
    assert_eq!(failed["data"]["state"], "failed");
    assert_eq!(
        failed["data"]["actions"],
        serde_json::json!(["Retry", "Edit", "Stop", "Restart", "Mark done"])
    );
    let receipt = json(root, &["done", "1", "--reason", "Accepted evidence"], 0)?;
    assert!(receipt["data"]["stash"].is_null(), "{receipt}");
    let task = json(root, &["show", "1"], 0)?;
    assert_eq!(task["data"]["lifecycle"], "succeeded_override");
    assert_eq!(task["data"]["attempts"][0]["revision"], 1);
    assert_eq!(task["data"]["attempts"][0]["state"], "failed");
    assert_eq!(task["data"]["attempts"][0]["outcome"], "Recorded failure");
    assert_eq!(task["data"]["resolved_by"], "human: owner");
    assert_eq!(task["data"]["resolution_reason"], "Accepted evidence");
    json(
        root,
        &[
            "add",
            "--title",
            "Human",
            "--goal",
            "Goal",
            "--criterion",
            "Check",
            "--kind",
            "human",
        ],
        0,
    )?;
    json(root, &["done", "2"], 0)?;
    assert_eq!(
        json(root, &["show", "2"], 0)?["data"]["lifecycle"],
        "succeeded"
    );
    // An AI cannot confirm human work; the owner can, and AI tasks it marks are overrides.
    json(
        root,
        &[
            "add",
            "--title",
            "Human for AI",
            "--goal",
            "Goal",
            "--criterion",
            "Check",
            "--kind",
            "human",
        ],
        0,
    )?;
    let refused = json(
        root,
        &["--actor-kind", "ai", "--actor", "executor", "done", "3"],
        4,
    )?;
    assert_eq!(
        refused["error"]["message"],
        "Only the owner can mark Human work done"
    );
    assert_eq!(json(root, &["show", "3"], 0)?["data"]["state"], "pending");
    Ok(())
}

#[test]
fn cli_stop_and_done_leave_paused_prerequisite_index_and_worktree_alone() -> Result {
    use ktask_rs_mk3::storage::{Database, execution::StartAttempt};
    for a_state in ["failed", "paused"] {
        for action in ["stop", "done"] {
            let scratch = support::sandbox()?;
            let root = scratch.path();
            support::empty_repository(root)?;
            std::fs::write(root.join(".git/info/exclude"), "*\n!b-untracked.rs\n")?;
            let add = |title: &str, before: bool| -> Result {
                let mut args = vec![
                    "add",
                    "--title",
                    title,
                    "--goal",
                    "Goal",
                    "--criterion",
                    "Check",
                    "--model",
                    "execution=codex/worker",
                    "--model",
                    "review=codex/reviewer",
                    "--model",
                    "validation=codex/validator",
                    "--model",
                    "remediation=codex/strong",
                ];
                if before {
                    args.extend(["--before", "1"]);
                }
                json(root, &args, 0)?;
                Ok(())
            };
            add("Failed A", false)?;
            let mut db = Database::open_in(&root.join("state/ktask-dev"))?;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_millis() as i64;
            let a = db.start_attempt(StartAttempt {
                task_id: 1,
                expected_version: 1,
                execution_token: "fixture-a",
                now_ms: now,
            })?;
            db.connection().execute("UPDATE attempts SET state=?2,finished_at_ms=CASE WHEN ?2='failed' THEN started_at_ms ELSE NULL END,outcome=CASE WHEN ?2='failed' THEN 'Failed' ELSE NULL END,failure_category=CASE WHEN ?2='failed' THEN 'check' ELSE NULL END WHERE id=?1", rusqlite::params![a, a_state])?;
            add("Paused B", true)?;
            let later = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_millis() as i64;
            let b = db.start_attempt(StartAttempt {
                task_id: 2,
                expected_version: 1,
                execution_token: "fixture-b",
                now_ms: later,
            })?;
            db.connection()
                .execute("UPDATE attempts SET state='paused' WHERE id=?1", [b])?;
            db.connection().execute("INSERT INTO execution_sessions(project_id,mode,state,reason,updated_at_ms) VALUES (1,'queue','paused','Paused B',?1)", [later])?;
            let file = root.join("b-work.txt");
            std::fs::write(&file, "staged B\n")?;
            let output = Command::new("git")
                .arg("-C")
                .arg(root)
                .args(["add", "-f", "b-work.txt"])
                .output()?;
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            std::fs::write(&file, "unstaged B\n")?;
            std::fs::write(root.join("b-untracked.rs"), "untracked B\n")?;
            let before = Command::new("git")
                .arg("-C")
                .arg(root)
                .args(["status", "--porcelain"])
                .output()?
                .stdout;
            let receipt = json(root, &[action, "1", "--reason", "Owner verdict"], 0)?;
            assert!(receipt["data"]["stash"].is_null(), "{receipt}");
            let after = Command::new("git")
                .arg("-C")
                .arg(root)
                .args(["status", "--porcelain"])
                .output()?
                .stdout;
            assert_eq!(before, after, "{action} changed B's index or worktree");
            assert_eq!(std::fs::read_to_string(&file)?, "unstaged B\n");
            assert_eq!(
                std::fs::read_to_string(root.join("b-untracked.rs"))?,
                "untracked B\n"
            );
            let b_show = json(root, &["show", "2"], 0)?;
            assert_eq!(b_show["data"]["state"], "paused");
            assert_eq!(b_show["data"]["attempts"][0]["id"], b);
            assert!(
                b_show["data"]["actions"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|x| x == "Resume")
            );
            let session: String = db.connection().query_row(
                "SELECT state FROM execution_sessions WHERE project_id=1",
                [],
                |r| r.get(0),
            )?;
            assert_eq!(session, "paused");
        }
    }
    Ok(())
}

#[test]
fn cli_stop_and_done_stash_latest_attempt_behind_unstarted_prerequisite() -> Result {
    use ktask_rs_mk3::storage::{Database, execution::StartAttempt};
    for state in ["failed", "paused"] {
        for action in ["stop", "done"] {
            let scratch = support::sandbox()?;
            let root = scratch.path();
            support::empty_repository(root)?;
            std::fs::write(root.join(".git/info/exclude"), "*\n!a-untracked.rs\n")?;
            json(
                root,
                &[
                    "add",
                    "--title",
                    "A",
                    "--goal",
                    "Goal",
                    "--criterion",
                    "Check",
                    "--model",
                    "execution=codex/worker",
                    "--model",
                    "review=codex/reviewer",
                    "--model",
                    "validation=codex/validator",
                    "--model",
                    "remediation=codex/strong",
                ],
                0,
            )?;
            let mut db = Database::open_in(&root.join("state/ktask-dev"))?;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_millis() as i64;
            let attempt = db.start_attempt(StartAttempt {
                task_id: 1,
                expected_version: 1,
                execution_token: "fixture-a",
                now_ms: now,
            })?;
            db.connection().execute(
                "UPDATE attempts SET state=?2,finished_at_ms=CASE WHEN ?2='failed' THEN started_at_ms ELSE NULL END,outcome=CASE WHEN ?2='failed' THEN 'Failed' ELSE NULL END,failure_category=CASE WHEN ?2='failed' THEN 'check' ELSE NULL END WHERE id=?1",
                rusqlite::params![attempt, state],
            )?;
            json(
                root,
                &[
                    "add",
                    "--title",
                    "B",
                    "--goal",
                    "Goal",
                    "--criterion",
                    "Check",
                    "--model",
                    "execution=codex/worker",
                    "--model",
                    "review=codex/reviewer",
                    "--model",
                    "validation=codex/validator",
                    "--model",
                    "remediation=codex/strong",
                    "--before",
                    "1",
                ],
                0,
            )?;
            std::fs::write(root.join("a-work.txt"), "staged A\n")?;
            let staged = Command::new("git")
                .arg("-C")
                .arg(root)
                .args(["add", "-f", "a-work.txt"])
                .output()?;
            assert!(staged.status.success());
            std::fs::write(root.join("a-work.txt"), "unstaged A\n")?;
            std::fs::write(root.join("a-untracked.rs"), "untracked A\n")?;
            let receipt = json(root, &[action, "1", "--reason", "Owner verdict"], 0)?;
            assert_eq!(receipt["data"]["stash"], "ktask: task #1 attempt #1");
            let names = Command::new("git")
                .arg("-C")
                .arg(root)
                .args([
                    "stash",
                    "show",
                    "--include-untracked",
                    "--name-only",
                    "stash@{0}",
                ])
                .output()?;
            assert!(names.status.success());
            let names = String::from_utf8(names.stdout)?;
            assert!(
                names.contains("a-work.txt") && names.contains("a-untracked.rs"),
                "{names}"
            );
            let saved_content = |revision: &str| -> Result<String> {
                let shown = Command::new("git")
                    .arg("-C")
                    .arg(root)
                    .args(["show", revision])
                    .output()?;
                assert!(
                    shown.status.success(),
                    "{}",
                    String::from_utf8_lossy(&shown.stderr)
                );
                Ok(String::from_utf8(shown.stdout)?)
            };
            assert_eq!(saved_content("stash@{0}:a-work.txt")?, "unstaged A\n");
            assert_eq!(saved_content("stash@{0}^2:a-work.txt")?, "staged A\n");
            assert_eq!(
                saved_content("stash@{0}^3:a-untracked.rs")?,
                "untracked A\n"
            );
            assert_eq!(json(root, &["show", "2"], 0)?["data"]["state"], "pending");
            assert_eq!(
                json(root, &["show", "2"], 0)?["data"]["attempts"]
                    .as_array()
                    .unwrap()
                    .len(),
                0
            );
            assert!(!root.join("a-work.txt").exists());
            assert!(!root.join("a-untracked.rs").exists());
            println!("CLI UNSTARTED PREREQUISITE state={state} action={action} stash={names}");
        }
    }
    Ok(())
}

#[test]
fn self_host_script_configures_a_private_checkout_with_the_given_executable() -> Result {
    let scratch = support::sandbox()?;
    let root = scratch.path();
    let project = root.join("project");
    std::fs::create_dir(&project)?;
    std::fs::create_dir(project.join("src"))?;
    std::fs::write(project.join("README.md"), "baseline\n")?;
    let git = |args: &[&str]| -> Result {
        let output = Command::new("git")
            .arg("-C")
            .arg(&project)
            .args(args)
            .output()?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(())
    };
    git(&["init", "-q"])?;
    git(&["add", "README.md"])?;
    git(&[
        "-c",
        "user.name=Test",
        "-c",
        "user.email=test@example.com",
        "commit",
        "-qm",
        "baseline",
    ])?;
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/self-host.sh");
    let self_host = |project: &Path, instance: &str| -> std::io::Result<Output> {
        Command::new("bash")
            .arg(&script)
            .args(["--ktask", DEV, "--instance", instance, "--project"])
            .arg(project)
            .env_clear()
            .env("HOME", root)
            .env("XDG_STATE_HOME", root.join("state"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_CACHE_HOME", root.join("cache"))
            .env("XDG_RUNTIME_DIR", root.join("runtime"))
            .env("TMPDIR", root)
            .env("PATH", "/usr/bin:/bin")
            .env("KTASK_EXECUTION_MODEL", "cheap")
            .env("KTASK_EXECUTION_EFFORT", "low")
            .env("KTASK_REVIEW_EFFORT", "")
            .env("KTASK_CHECK_TIMEOUT_MS", "120000")
            .current_dir(root)
            .output()
    };
    let project_arg = project.to_str().unwrap();

    let output = self_host(&project, "ktask-dev")?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("Check saved: build"), "{stdout}");
    assert!(stdout.contains("Default saved for planning"), "{stdout}");
    let checks = json(root, &["--project", project_arg, "checks"], 0)?;
    assert_eq!(checks["data"]["checks"][0]["name"], "build");
    assert_eq!(checks["data"]["checks"][0]["command"], "make check");
    assert_eq!(checks["data"]["checks"][0]["timeout_ms"], 120000);

    // Rerunning is idempotent, and new tasks pick up the configured defaults.
    assert!(self_host(&project, "ktask-dev")?.status.success());
    json(
        root,
        &[
            "--project",
            project_arg,
            "add",
            "--title",
            "Self-hosted",
            "--goal",
            "Goal",
            "--criterion",
            "Check",
        ],
        0,
    )?;
    let shown = json(root, &["--project", project_arg, "show", "1"], 0)?;
    let models = &shown["data"]["definition"]["models"];
    assert_eq!(models["execution"]["provider"], "codex");
    assert_eq!(models["execution"]["model"], "cheap");
    assert_eq!(models["execution"]["effort"], "low");
    assert_eq!(models["review"]["model"], "gpt-6-astra");
    assert!(models["review"]["effort"].is_null(), "{models}");
    assert_eq!(models["validation"]["effort"], "high");
    assert_eq!(checks["data"]["checks"].as_array().map(Vec::len), Some(1));

    // A non-root directory and a build-output production executable are refused unchanged.
    let output = self_host(&project.join("src"), "ktask-dev")?;
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("Git repository root"));
    let output = self_host(&project, "ktask")?;
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("build output"));
    let checks = json(root, &["--project", project_arg, "checks"], 0)?;
    assert_eq!(checks["data"]["checks"][0]["timeout_ms"], 120000);
    Ok(())
}

#[test]
fn init_sets_up_new_projects_from_saved_defaults_and_their_own_files() -> Result {
    let sandbox = support::sandbox()?;
    let root = sandbox.path();
    std::fs::create_dir(root.join("bin"))?;
    support::write_executable(&root.join("bin/codex"), "#!/bin/sh\nexit 0\n")?;
    let repository = |name: &str, files: &[(&str, &str)]| -> Result<String> {
        let path = root.join(name);
        std::fs::create_dir(&path)?;
        support::empty_repository(&path)?;
        for (file, content) in files {
            std::fs::write(path.join(file), content)?;
        }
        Ok(path.to_string_lossy().into_owned())
    };
    let ktask = |project: &str, args: &[&str], exit: i32| {
        let output = process(DEV, root)
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", root.join("bin").display()),
            )
            .args(["--json", "--project", project])
            .args(args)
            .output()?;
        json_result(output, exit)
    };
    let first = repository("first", &[])?;
    for (role, model) in [
        ("execution", "gpt-6-sol"),
        ("planning", "gpt-6-sol"),
        ("review", "gpt-6-astra"),
        ("validation", "gpt-6-astra"),
        ("remediation", "gpt-6-astra"),
    ] {
        ktask(
            &first,
            &[
                "model-default",
                role,
                "--provider",
                "codex",
                "--model",
                model,
                "--new-projects",
            ],
            0,
        )?;
    }

    // A new project starts with the saved defaults and the check its Makefile defines.
    let second = repository(
        "second",
        &[("Makefile", "build:\n\tcc x.c\ncheck:\n\t./t\n")],
    )?;
    let ready = ktask(&second, &["init"], 0)?["data"].clone();
    assert_eq!(ready["checks"][0]["command"], "make check");
    assert_eq!(ready["check_source"], "detected from Makefile");
    assert_eq!(ready["models"]["review"], "codex/gpt-6-astra");
    assert_eq!(ready["models"]["execution"], "codex/gpt-6-sol");
    assert_eq!(ready["agent_network"], false);

    // Nothing to detect: init saves what it can and names the missing check.
    let third = repository("third", &[("README", "notes\n")])?;
    let missing = ktask(&third, &["init"], 4)?;
    let message = missing["error"]["message"].as_str().unwrap();
    assert!(
        message.contains("Project check: No enabled check"),
        "{message}"
    );
    let fixed = ktask(
        &third,
        &[
            "init",
            "--check-command",
            "test -f README",
            "--agent-network",
            "on",
        ],
        0,
    )?;
    assert_eq!(fixed["data"]["check_source"], "given");
    assert_eq!(fixed["data"]["agent_network"], true);

    // Role models are inherited, not copied: a project that overrides nothing follows the
    // user default (only tasks defined afterwards use it), and a judge may explicitly
    // reuse the execution model.
    ktask(
        &first,
        &[
            "model-default",
            "review",
            "--provider",
            "codex",
            "--model",
            "gpt-6-sol",
            "--new-projects",
        ],
        0,
    )?;
    assert_eq!(
        ktask(&second, &["init"], 0)?["data"]["models"]["review"],
        "codex/gpt-6-sol"
    );
    let fourth = repository("fourth", &[("Cargo.toml", "[package]\n")])?;
    let ready = ktask(&fourth, &["init"], 0)?;
    assert_eq!(ready["data"]["checks"][0]["command"], "cargo test");
    assert_eq!(ready["data"]["models"]["review"], "codex/gpt-6-sol");
    assert_eq!(ready["data"]["models"]["execution"], "codex/gpt-6-sol");

    // A project can only run from a Git repository root with a clean baseline.
    let plain = root.join("plain");
    std::fs::create_dir(&plain)?;
    let message = ktask(
        plain.to_str().unwrap(),
        &["init", "--check-command", "true"],
        4,
    )?;
    let message = message["error"]["message"].as_str().unwrap().to_owned();
    assert!(
        message.contains("Git repository: The project is not the root"),
        "{message}"
    );
    let dirty = repository("dirty", &[("notes.txt", "draft\n")])?;
    let git = |args: &[&str]| {
        Command::new("git")
            .args([
                "-C",
                &dirty,
                "-c",
                "user.name=T",
                "-c",
                "user.email=t@example.com",
            ])
            .args(args)
            .output()
    };
    git(&["add", "-f", "notes.txt"])?;
    git(&["commit", "-qm", "notes"])?;
    std::fs::write(Path::new(&dirty).join("notes.txt"), "edited\n")?;
    let message = ktask(&dirty, &["init", "--check-command", "true"], 4)?;
    let message = message["error"]["message"].as_str().unwrap().to_owned();
    assert!(
        message.contains("Worktree: Uncommitted changes"),
        "{message}"
    );
    Ok(())
}

#[test]
fn evidence_reader_pages_and_searches_without_creating_app_state() -> Result {
    let scratch = support::sandbox()?;
    let root = scratch.path();
    let file = root.join("large.log");
    let mut bytes = vec![b'x'; 3 * 1024 * 1024];
    bytes[10..15].copy_from_slice(b"ERROR");
    bytes[1024 * 1024 - 2..1024 * 1024 + 3].copy_from_slice(b"ERROR");
    std::fs::write(&file, bytes)?;
    let call = |args: &[&str]| -> Result<Value> {
        let output = Command::new(DEV)
            .env_clear()
            .env("HOME", root.join("no-home"))
            .args(["--json", "evidence", file.to_str().unwrap()])
            .args(args)
            .output()?;
        assert!(output.status.success(), "{output:?}");
        assert!(output.stdout.len() < 16384);
        Ok(serde_json::from_slice::<Value>(&output.stdout)?["data"].clone())
    };
    let page = call(&["--limit", "100"])?;
    assert_eq!(page["text"].as_str().unwrap().len(), 100);
    assert_eq!(page["next_offset"], 100);
    let first = call(&["--search", "ERROR"])?;
    assert_eq!(first["matches"][0]["offset"], 10);
    let next = first["next_offset"].to_string();
    let second = call(&["--search", "ERROR", "--offset", &next])?;
    assert_eq!(second["matches"][0]["offset"], 1024 * 1024 - 2);
    assert!(
        !root.join("no-home").exists(),
        "reader must not open or initialize app state"
    );
    Ok(())
}

struct Projects<'a>(&'a Path);

impl Projects<'_> {
    fn repository(&self, name: &str, files: &[(&str, &str)]) -> Result<String> {
        let path = self.0.join(name);
        std::fs::create_dir(&path)?;
        support::empty_repository(&path)?;
        for (file, content) in files {
            std::fs::write(path.join(file), content)?;
        }
        Ok(path.to_string_lossy().into_owned())
    }
    fn run(&self, project: &str, args: &[&str]) -> std::io::Result<std::process::Output> {
        process(DEV, self.0)
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", self.0.join("bin").display()),
            )
            .args(["--json", "--project", project])
            .args(args)
            .output()
    }
    fn ktask(&self, project: &str, args: &[&str], exit: i32) -> Result<Value> {
        json_result(self.run(project, args)?, exit)
    }
}

fn save_defaults_for_new_projects(projects: &Projects, first: &str) -> Result {
    for role in [
        "planning",
        "execution",
        "review",
        "validation",
        "remediation",
    ] {
        projects.ktask(
            first,
            &[
                "model-default",
                role,
                "--provider",
                "claude",
                "--model",
                "opus",
                "--effort",
                "high",
                "--new-projects",
            ],
            0,
        )?;
    }
    let before = projects.ktask(first, &["defaults"], 0)?["data"].clone();
    assert_eq!(before["applied"], false);
    assert_eq!(before["defaults"]["check_timeout_ms"], 600000);
    assert_eq!(before["defaults"]["delivery_mode"], "local");
    assert_eq!(before["models"]["review"]["effort"], "high");
    // Every changed model default advanced the version that guards the whole defaults.
    assert_eq!(before["defaults"]["version"], 5);
    let saved = projects.ktask(
        first,
        &[
            "defaults",
            "--agent-network",
            "on",
            "--check-before-task",
            "off",
            "--check-timeout-ms",
            "45000",
            "--run-mode",
            "one",
            "--planner-approval",
            "review-required",
            "--delivery-mode",
            "pr",
            "--if-version",
            "5",
        ],
        0,
    )?["data"]
        .clone();
    assert_eq!(saved["defaults"]["version"], 6);
    // A second writer changes only an effort; the edit based on version 6 is now stale and
    // neither the newer effort nor any setting changes.
    let effort = projects.ktask(
        first,
        &[
            "model-default",
            "review",
            "--provider",
            "claude",
            "--model",
            "opus",
            "--effort",
            "medium",
            "--new-projects",
        ],
        0,
    )?;
    assert_eq!(effort["data"]["effort"], "medium");
    let stale = projects.ktask(
        first,
        &[
            "defaults",
            "--check-timeout-ms",
            "1000",
            "--if-version",
            "6",
        ],
        4,
    )?;
    assert!(
        stale["error"]["message"]
            .as_str()
            .unwrap()
            .contains("version 7")
    );
    let after = projects.ktask(first, &["defaults"], 0)?["data"].clone();
    assert_eq!(after["defaults"]["version"], 7);
    assert_eq!(after["defaults"]["check_timeout_ms"], 45000);
    assert_eq!(after["models"]["review"]["effort"], "medium");
    // Saving the same model again is not a change and leaves an open edit current.
    projects.ktask(
        first,
        &[
            "model-default",
            "review",
            "--provider",
            "claude",
            "--model",
            "opus",
            "--effort",
            "medium",
            "--new-projects",
        ],
        0,
    )?;
    assert_eq!(
        projects.ktask(first, &["defaults"], 0)?["data"]["defaults"]["version"],
        7
    );
    // Previewing changes nothing in the registered project.
    let rows = saved["preview"].as_array().unwrap();
    let row = |name: &str| rows.iter().find(|r| r["setting"] == name).unwrap();
    assert_eq!(row("agent network")["current"], "off");
    assert_eq!(row("agent network")["default"], "on");
    assert_eq!(row("delivery mode")["default"], "pr");
    let init = projects.ktask(first, &["init", "--check-command", "true"], 0)?;
    assert_eq!(init["data"]["agent_network"], false);
    Ok(())
}

fn new_projects_start_from_defaults(projects: &Projects) -> Result {
    let second = projects.repository("second", &[("Makefile", "check:\n\t./t\n")])?;
    let init = projects.ktask(&second, &["init"], 0)?["data"].clone();
    assert_eq!(init["agent_network"], true);
    assert_eq!(init["check_before_task"], false);
    assert_eq!(init["checks"][0]["timeout_ms"], 45000);
    assert_eq!(init["checks"][0]["command"], "make check");
    assert_eq!(init["delivery"]["mode"], "pr");
    assert_eq!(init["delivery"]["remote"], "origin");
    assert_eq!(init["models"]["execution"], "claude/opus");
    // What only the project can supply is named, never copied from another project.
    let remaining: Vec<&str> = init["remaining_setup"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["item"].as_str().unwrap())
        .collect();
    assert_eq!(remaining, ["Delivery destination", "Domain instructions"]);
    // Explicit options override the user defaults for this project only.
    let third = projects.repository("third", &[])?;
    let init = projects.ktask(
        &third,
        &[
            "init",
            "--check-command",
            "true",
            "--check-timeout-ms",
            "5000",
            "--agent-network",
            "off",
        ],
        0,
    )?["data"]
        .clone();
    assert_eq!(init["checks"][0]["timeout_ms"], 5000);
    assert_eq!(init["agent_network"], false);
    let fourth = projects.repository("fourth", &[])?;
    let missing = projects.ktask(&fourth, &["init"], 4)?;
    assert!(
        missing["error"]["message"]
            .as_str()
            .unwrap()
            .contains("Project check")
    );
    Ok(())
}

#[test]
fn defaults_command_saves_user_defaults_previews_and_applies_them_to_a_project() -> Result {
    let sandbox = support::sandbox()?;
    let root = sandbox.path();
    std::fs::create_dir(root.join("bin"))?;
    support::write_executable(&root.join("bin/claude"), "#!/bin/sh\nexit 0\n")?;
    let projects = Projects(root);
    let first = projects.repository("first", &[])?;
    save_defaults_for_new_projects(&projects, &first)?;
    new_projects_start_from_defaults(&projects)?;
    // Applying makes the registered project match, keeping its command and remote.
    let text = String::from_utf8(projects.run(&first, &["defaults"])?.stdout)?;
    assert!(text.contains("\"changes\":true"));
    let applied = projects.ktask(&first, &["defaults", "--apply"], 0)?["data"].clone();
    assert_eq!(applied["applied"], true);
    assert!(
        applied["preview"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["changes"] == false)
    );
    let init = projects.ktask(&first, &["init"], 0)?["data"].clone();
    assert_eq!(init["agent_network"], true);
    assert_eq!(init["check_before_task"], false);
    assert_eq!(init["checks"][0]["command"], "true");
    assert_eq!(init["checks"][0]["timeout_ms"], 45000);
    assert_eq!(init["delivery"]["remote"], "origin");
    Ok(())
}
