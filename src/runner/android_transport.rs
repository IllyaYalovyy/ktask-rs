//! A guardian-owned Android command broker. The provider keeps API access; its
//! network-restricted commands exchange bounded files with this private namespace.
use crate::storage::{Error, Result};
use nix::{
    sys::signal::{Signal, killpg},
    unistd::Pid,
};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::unix::{fs::OpenOptionsExt, process::CommandExt},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    thread,
    time::{Duration, Instant},
};

const MAX_REQUEST: u64 = 64 * 1024;
const MAX_OUTPUT: u64 = 64 * 1024;
// JSON encodes each byte as at most three digits and a separator. Both streams
// retain their 64 KiB payload limits; this only bounds the response envelope.
pub(crate) const MAX_RESPONSE: u64 = 8 * MAX_OUTPUT + 4096;
static NEXT: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Serialize, Deserialize)]
struct Spec {
    root: PathBuf,
    sdk: PathBuf,
    project: PathBuf,
    runtime: PathBuf,
    adb_port: u16,
    emulator_port: u16,
    serial: String,
    secret: String,
}

#[derive(Serialize, Deserialize)]
struct Request {
    secret: String,
    operation: String,
    args: Vec<String>,
    cwd: PathBuf,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Output {
    pub code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

#[derive(Clone)]
pub struct Endpoint {
    root: PathBuf,
    secret: String,
    sdk: PathBuf,
}

pub struct Transport {
    child: Child,
    endpoint: Endpoint,
}

fn env_path(env: &[(String, String)], name: &str) -> Option<PathBuf> {
    env.iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| PathBuf::from(value))
}

fn env_number(env: &[(String, String)], name: &str) -> Result<u16> {
    env.iter()
        .find(|(key, _)| key == name)
        .and_then(|(_, value)| value.parse().ok())
        .ok_or_else(|| {
            Error::Conflict(format!(
                "Android runtime has no {name}; Recheck the bound SDK"
            ))
        })
}

impl Transport {
    pub fn start(
        executable: &Path,
        project: &Path,
        scratch: &Path,
        env: &[(String, String)],
    ) -> Result<Option<Self>> {
        let Some(sdk) = env_path(env, "ANDROID_HOME") else {
            return Ok(None);
        };
        let project = project.canonicalize()?;
        let runtime = env_path(env, "KTASK_ANDROID_RUNTIME_ROOT").ok_or_else(|| {
            Error::Conflict("Android runtime is missing; Recheck the bound SDK".into())
        })?;
        let root = scratch.join("android-transport");
        fs::create_dir(&root)?;
        let mut random = [0u8; 32];
        File::open("/dev/urandom")?.read_exact(&mut random)?;
        let secret = random
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let spec = Spec {
            root: root.clone(),
            sdk: sdk.clone(),
            project: project.clone(),
            runtime: runtime.clone(),
            adb_port: env_number(env, "ANDROID_ADB_SERVER_PORT")?,
            emulator_port: env_number(env, "KTASK_EMULATOR_PORT")?,
            serial: env
                .iter()
                .find(|(key, _)| key == "ANDROID_SERIAL")
                .map(|(_, value)| value.clone())
                .ok_or_else(|| {
                    Error::Conflict("Android serial is missing; Recheck the bound SDK".into())
                })?,
            secret: secret.clone(),
        };
        let mut command = Command::new(super::executable("bwrap")?);
        let stderr_path = root.join("sidecar.stderr");
        command
            .args(["--ro-bind", "/", "/"])
            .arg("--bind")
            .arg(&project)
            .arg(&project)
            .arg("--bind")
            .arg(&runtime)
            .arg(&runtime)
            .arg("--bind")
            .arg(scratch)
            .arg(scratch)
            .arg("--ro-bind")
            .arg(&sdk)
            .arg(&sdk)
            .args([
                "--dev",
                "/dev",
                "--unshare-net",
                "--unshare-pid",
                "--proc",
                "/proc",
                "--die-with-parent",
            ]);
        if Path::new("/dev/kvm").exists() {
            command.args(["--dev-bind", "/dev/kvm", "/dev/kvm"]);
        }
        if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
            let personal = home.join(".android");
            if personal.is_dir() {
                command.arg("--tmpfs").arg(personal);
            }
        }
        command
            .arg("--")
            .arg(executable)
            .arg("__android_sidecar")
            .current_dir(&runtime)
            .env("TMPDIR", scratch)
            .env("HOME", &runtime)
            .env("ANDROID_HOME", &sdk)
            .env("ANDROID_SDK_ROOT", &sdk)
            .env("ANDROID_AVD_HOME", runtime.join("avd"))
            .env("ANDROID_USER_HOME", runtime.join("android-user"))
            .env(
                "ADB_SERVER_SOCKET",
                format!("tcp:localhost:{}", spec.adb_port),
            )
            .env("ANDROID_ADB_SERVER_PORT", spec.adb_port.to_string())
            .env("ANDROID_SERIAL", &spec.serial)
            .env("KTASK_EMULATOR_PORT", spec.emulator_port.to_string())
            .env("KTASK_ANDROID_RUNTIME_ROOT", &runtime)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(File::create(&stderr_path)?)
            .process_group(0);
        let mut child = command.spawn()?;
        if let Some(mut input) = child.stdin.take() {
            serde_json::to_writer(&mut input, &spec).map_err(io::Error::other)?;
        }
        let mut transport = Self {
            child,
            endpoint: Endpoint { root, secret, sdk },
        };
        let deadline = Instant::now() + Duration::from_secs(3);
        while !transport.endpoint.root.join("online").exists() {
            if Instant::now() >= deadline || transport.child.try_wait()?.is_some() {
                let detail = fs::read_to_string(&stderr_path).unwrap_or_default();
                return Err(Error::Conflict(format!(
                    "Private Android transport could not start: {}; verify bubblewrap networking and Recheck",
                    detail.trim()
                )));
            }
            thread::sleep(Duration::from_millis(20));
        }
        Ok(Some(transport))
    }

    pub fn endpoint(&self) -> Endpoint {
        self.endpoint.clone()
    }
    pub fn hidden_path(&self) -> &Path {
        &self.endpoint.root
    }
}

impl Drop for Transport {
    fn drop(&mut self) {
        let _ = killpg(Pid::from_raw(self.child.id() as i32), Signal::SIGKILL);
        let _ = self.child.wait();
    }
}

impl Endpoint {
    pub fn sdk(&self) -> &Path {
        &self.sdk
    }
    pub fn exchange(
        &self,
        operation: &str,
        args: &[String],
        cwd: &Path,
        stop: &AtomicBool,
    ) -> Result<Output> {
        let name = format!(
            "request-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        let temporary = self.root.join(format!("{name}.tmp"));
        let ready = self.root.join(format!("{name}.ready"));
        let response = self.root.join(format!("{name}.response"));
        let bytes = serde_json::to_vec(&Request {
            secret: self.secret.clone(),
            operation: operation.into(),
            args: args.to_vec(),
            cwd: cwd.to_path_buf(),
        })
        .map_err(io::Error::other)?;
        if bytes.len() as u64 > MAX_REQUEST {
            return Err(Error::Conflict("Android command exceeds 64 KiB".into()));
        }
        let mut file = new_file(&temporary)?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(temporary, ready)?;
        let deadline = Instant::now() + Duration::from_secs(120);
        loop {
            if let Ok(mut file) = OpenOptions::new()
                .read(true)
                .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
                .open(&response)
            {
                if !file.metadata()?.is_file() {
                    return Err(Error::Conflict("Invalid Android response file".into()));
                }
                let mut bytes = Vec::new();
                (&mut file).take(MAX_RESPONSE + 1).read_to_end(&mut bytes)?;
                let _ = fs::remove_file(response);
                if bytes.len() as u64 > MAX_RESPONSE {
                    return Err(Error::Conflict("Android response exceeds capacity".into()));
                }
                return serde_json::from_slice(&bytes).map_err(|error| {
                    Error::Conflict(format!("Invalid Android response: {error}"))
                });
            }
            if stop.load(Ordering::Relaxed) {
                return Err(Error::Conflict("Android command cancelled".into()));
            }
            if Instant::now() >= deadline {
                return Err(Error::Conflict("Android command timed out".into()));
            }
            thread::sleep(Duration::from_millis(20));
        }
    }
}

fn new_file(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .custom_flags(nix::libc::O_NOFOLLOW)
        .open(path)
}

pub fn sidecar() -> Result<()> {
    let spec: Spec =
        serde_json::from_reader(io::stdin().take(MAX_REQUEST)).map_err(io::Error::other)?;
    fs::write(spec.root.join("online"), b"ready")?;
    let mut children = Vec::new();
    loop {
        for entry in fs::read_dir(&spec.root)?.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "ready") {
                continue;
            }
            let processing = path.with_extension("processing");
            if fs::rename(&path, &processing).is_err() {
                continue;
            }
            let result = handle(&spec, &processing, &mut children).unwrap_or_else(|error| Output {
                code: 1,
                stdout: Vec::new(),
                stderr: error.to_string().into_bytes(),
            });
            let writing = path.with_extension("writing");
            if let Ok(mut file) = new_file(&writing)
                && serde_json::to_writer(&mut file, &result).is_ok()
                && file.sync_all().is_ok()
            {
                let _ = fs::rename(writing, path.with_extension("response"));
            }
            let _ = fs::remove_file(processing);
        }
        children.retain_mut(|child: &mut Child| child.try_wait().ok().flatten().is_none());
        thread::sleep(Duration::from_millis(20));
    }
}

fn handle(spec: &Spec, path: &Path, children: &mut Vec<Child>) -> Result<Output> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(Error::Conflict("Invalid Android request file".into()));
    }
    let mut bytes = Vec::new();
    (&mut file).take(MAX_REQUEST + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_REQUEST {
        return Err(Error::Conflict("Android request exceeds capacity".into()));
    }
    let request: Request = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
    if request.secret != spec.secret {
        return Err(Error::Conflict("Android transport token mismatch".into()));
    }
    let cwd = owned_cwd(spec, &request.cwd)?;
    match request.operation.as_str() {
        "adb" => {
            validate_adb(spec, &request.args, &cwd)?;
            run_bounded(
                &spec.sdk.join("platform-tools/adb"),
                &request.args,
                &spec.root,
                &cwd,
            )
        }
        "emulator" => {
            validate_emulator(spec, &request.args)?;
            let log = OpenOptions::new()
                .create(true)
                .append(true)
                .open(spec.runtime.join("emulator.log"))?;
            let child = Command::new(spec.sdk.join("emulator/emulator"))
                .args(&request.args)
                .env("ANDROID_AVD_HOME", spec.runtime.join("avd"))
                .env("ANDROID_SERIAL", &spec.serial)
                .stdout(log.try_clone()?)
                .stderr(log)
                .spawn()?;
            children.push(child);
            Ok(Output {
                code: 0,
                stdout: Vec::new(),
                stderr: Vec::new(),
            })
        }
        _ => Err(Error::Conflict(
            "Only adb and emulator are available through Android transport".into(),
        )),
    }
}

fn validate_adb(spec: &Spec, args: &[String], cwd: &Path) -> Result<()> {
    let args = if args.first().is_some_and(|arg| arg == "-s") {
        if args.get(1) != Some(&spec.serial) {
            return Err(Error::Conflict(
                "Only the owned Android serial is authorized".into(),
            ));
        }
        &args[2..]
    } else {
        args
    };
    let Some(command) = args.first().map(String::as_str) else {
        return Err(Error::Conflict("Android adb command is required".into()));
    };
    if !matches!(
        command,
        "version"
            | "devices"
            | "start-server"
            | "kill-server"
            | "get-state"
            | "wait-for-device"
            | "shell"
            | "logcat"
            | "install"
            | "push"
            | "pull"
            | "emu"
    ) {
        return Err(Error::Conflict(
            "Android adb operation is not approved for the owned emulator".into(),
        ));
    }
    let operands = &args[1..];
    match command {
        "install" => {
            let files: Vec<_> = operands
                .iter()
                .filter(|arg| !matches!(arg.as_str(), "-r" | "-t" | "-d" | "-g"))
                .collect();
            if files.len() != 1 || files[0].starts_with('-') {
                return Err(Error::Conflict("Install needs one local APK".into()));
            }
            owned_local_path(spec, cwd, files[0], false)?;
        }
        "push" | "pull" => {
            if operands.len() != 2 {
                return Err(Error::Conflict(
                    "Android file transfer needs two paths".into(),
                ));
            }
            owned_local_path(
                spec,
                cwd,
                &operands[usize::from(command == "pull")],
                command == "pull",
            )?;
        }
        _ => {}
    }
    Ok(())
}

fn owned_cwd(spec: &Spec, cwd: &Path) -> Result<PathBuf> {
    if !cwd.is_absolute() {
        return Err(Error::Conflict(
            "Android caller directory must be absolute".into(),
        ));
    }
    let cwd = cwd.canonicalize()?;
    if !cwd.is_dir() || !authorized_root(spec, &cwd) {
        return Err(Error::Conflict(
            "Android caller directory is outside the project or owned scratch".into(),
        ));
    }
    Ok(cwd)
}

fn authorized_root(spec: &Spec, path: &Path) -> bool {
    [&spec.project, spec.root.parent().unwrap_or(&spec.root)]
        .iter()
        .any(|root| path.starts_with(root))
}

fn owned_local_path(spec: &Spec, cwd: &Path, value: &str, output: bool) -> Result<()> {
    let path = Path::new(value);
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    let checked = if output && !path.exists() {
        path.parent()
            .ok_or_else(|| Error::Conflict("Invalid Android output path".into()))?
    } else {
        path.as_path()
    };
    let checked = checked.canonicalize()?;
    if !authorized_root(spec, &checked) {
        return Err(Error::Conflict(
            "Android local files must stay in the project or owned scratch".into(),
        ));
    }
    Ok(())
}

fn validate_emulator(spec: &Spec, args: &[String]) -> Result<()> {
    let mut avd = None;
    let mut port = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "-avd" | "-port" | "-gpu" => {
                let value = args.get(i + 1).ok_or_else(|| {
                    Error::Conflict("Android emulator option needs a value".into())
                })?;
                match args[i].as_str() {
                    "-avd" if avd.replace(value.as_str()).is_some() => {
                        return Err(Error::Conflict("Duplicate Android AVD".into()));
                    }
                    "-port" if port.replace(value.as_str()).is_some() => {
                        return Err(Error::Conflict("Duplicate Android emulator port".into()));
                    }
                    "-avd" | "-port" => {}
                    _ if !matches!(value.as_str(), "auto" | "host" | "swiftshader_indirect") => {
                        return Err(Error::Conflict("Unapproved emulator GPU mode".into()));
                    }
                    _ => {}
                }
                i += 2;
            }
            "-no-window" | "-no-audio" | "-no-boot-anim" | "-no-snapshot" | "-wipe-data" => i += 1,
            _ => return Err(Error::Conflict("Unapproved Android emulator option".into())),
        }
    }
    let avd = avd.ok_or_else(|| Error::Conflict("Select an owned AVD with -avd".into()))?;
    if avd.contains('/')
        || avd.contains('\\')
        || avd.contains("..")
        || !spec.runtime.join("avd").join(format!("{avd}.avd")).is_dir()
    {
        return Err(Error::Conflict(
            "Android AVD is outside the owned runtime".into(),
        ));
    }
    if port != Some(spec.emulator_port.to_string().as_str()) {
        return Err(Error::Conflict(
            "Use the leased Android emulator port".into(),
        ));
    }
    Ok(())
}

fn run_bounded(binary: &Path, args: &[String], root: &Path, cwd: &Path) -> Result<Output> {
    let stdout_path = root.join("adb.stdout");
    let stderr_path = root.join("adb.stderr");
    let stdout = File::create(&stdout_path)?;
    let stderr = File::create(&stderr_path)?;
    let mut child = Command::new(binary)
        .args(args)
        .current_dir(cwd)
        .stdout(stdout)
        .stderr(stderr)
        .spawn()?;
    let result = (|| {
        let deadline = Instant::now() + Duration::from_secs(115);
        let code = loop {
            if let Some(status) = child.try_wait()? {
                break status.code().unwrap_or(1);
            }
            if Instant::now() >= deadline
                || fs::metadata(&stdout_path)?.len() > MAX_OUTPUT
                || fs::metadata(&stderr_path)?.len() > MAX_OUTPUT
            {
                let _ = child.kill();
                let _ = child.wait();
                return Err(Error::Conflict(
                    "Android adb command exceeded time or output capacity".into(),
                ));
            }
            thread::sleep(Duration::from_millis(20));
        };
        Ok(Output {
            code,
            stdout: bounded_output(&stdout_path)?,
            stderr: bounded_output(&stderr_path)?,
        })
    })();
    let _ = fs::remove_file(stdout_path);
    let _ = fs::remove_file(stderr_path);
    result
}

fn bounded_output(path: &Path) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    File::open(path)?
        .take(MAX_OUTPUT + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_OUTPUT {
        return Err(Error::Conflict(
            "Android adb output exceeds 64 KiB per-stream capacity".into(),
        ));
    }
    Ok(bytes)
}
