//! Instance-scoped, immutable tool generations and explicit project bindings.
use super::{Service, dependency::Requirement, now_ms, require};
use crate::{
    instance::Paths,
    storage::{Error, Result},
};
use rusqlite::{OptionalExtension, TransactionBehavior, params};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::{Read, Seek, SeekFrom},
    os::unix::{
        ffi::OsStringExt,
        fs::{MetadataExt, PermissionsExt},
    },
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, Serialize)]
pub struct Tool {
    pub id: i64,
    pub name: String,
    pub version: String,
    pub platform: String,
    pub architecture: String,
    pub components: Vec<String>,
    pub path: PathBuf,
    pub provenance: String,
    pub row_version: i64,
    pub binding_version: Option<i64>,
    pub bound: bool,
    pub ready: bool,
}

pub struct ResolvedTools {
    pub env: Vec<(String, String)>,
    pub fingerprint: String,
    pub description: String,
    pub writable_roots: Vec<PathBuf>,
    pub installations: Vec<PathBuf>,
    _leases: Vec<File>,
    _android_runtime: Option<crate::scratch::Scratch>,
}

pub struct Registration<'a> {
    pub name: &'a str,
    pub version: &'a str,
    pub platform: &'a str,
    pub architecture: &'a str,
    pub components: &'a [String],
    pub path: &'a Path,
    pub managed: bool,
}

fn has_android_image(tools: &[Tool]) -> bool {
    tools.iter().any(|tool| {
        tool.name == "android-sdk"
            && tool
                .components
                .iter()
                .any(|component| component == "system-images;android-36;google_apis;x86_64")
    })
}

fn add_android_template(
    runtime: &Path,
    has_image: bool,
    env: &mut Vec<(String, String)>,
) -> Result<()> {
    if has_image {
        let template = runtime.join("avd-template.json");
        fs::write(&template, super::provision::avd_template())?;
        env.push((
            "KTASK_ANDROID_AVD_TEMPLATE".into(),
            template.to_string_lossy().into_owned(),
        ));
    }
    Ok(())
}

fn add_android_sdk_bindings(
    tool: &Tool,
    env: &mut Vec<(String, String)>,
    sdk_bins: &mut Vec<PathBuf>,
) {
    let root = tool.path.to_string_lossy().into_owned();
    env.push(("ANDROID_HOME".into(), root.clone()));
    env.push(("ANDROID_SDK_ROOT".into(), root));
    for relative in ["platform-tools", "cmdline-tools/latest/bin", "emulator"] {
        let path = tool.path.join(relative);
        if path.is_dir() {
            sdk_bins.push(path);
        }
    }
    for component in &tool.components {
        if component.starts_with("cmdline-tools;") {
            let path = tool.path.join(component.replace(';', "/")).join("bin");
            if path.is_dir() && !sdk_bins.contains(&path) {
                sdk_bins.push(path);
            }
        }
    }
    if let Ok(packages) = fs::read_dir(tool.path.join("cmdline-tools")) {
        let mut packages = packages
            .flatten()
            .map(|entry| entry.path())
            .collect::<Vec<_>>();
        packages.sort();
        for package in packages {
            let bin = package.join("bin");
            if (bin.join("android").is_file() || bin.join("sdkmanager").is_file())
                && !sdk_bins.contains(&bin)
            {
                sdk_bins.push(bin);
            }
        }
    }
}

fn name(value: &str) -> Result<String> {
    let normalized = value.trim().to_ascii_lowercase();
    require(
        !normalized.is_empty()
            && normalized.len() <= 80
            && normalized
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'+')),
        "Tool name must use letters, numbers, dot, dash, plus or underscore",
    )?;
    Ok(normalized)
}

fn components(values: &[String]) -> Result<Vec<String>> {
    require(
        values.len() <= 32,
        "A tool generation may declare at most 32 components",
    )?;
    let mut values = values.to_vec();
    for value in &values {
        require(
            !value.is_empty()
                && value.len() <= 160
                && value.split(';').all(|part| {
                    !part.is_empty()
                        && part != "."
                        && part != ".."
                        && !part.contains('/')
                        && !part.contains('\\')
                }),
            "Invalid tool component",
        )?;
    }
    values.sort();
    values.dedup();
    Ok(values)
}

fn probe(path: &Path, name: &str, components: &[String]) -> Result<String> {
    require(path.is_dir(), "Tool installation path is not a directory")?;
    let mut watched = vec![path.to_path_buf()];
    let bin = path.join("bin").join(name);
    if name != "android-sdk" {
        require(
            bin.is_file() && fs::metadata(&bin)?.permissions().mode() & 0o111 != 0,
            &format!("Missing {name} executable at {}", bin.display()),
        )?;
        watched.push(bin);
    }
    if name == "android-sdk" {
        for binary in [
            "platform-tools/adb",
            "cmdline-tools/latest/bin/sdkmanager",
            "bin/android",
        ] {
            let path = path.join(binary);
            if path.exists() {
                watched.push(path);
            }
        }
        if watched.len() == 1 {
            let packaged = path.join("cmdline-tools/latest/bin/android");
            if packaged.is_file() {
                watched.push(packaged);
            } else if let Ok(packages) = fs::read_dir(path.join("cmdline-tools")) {
                let mut packages = packages
                    .flatten()
                    .map(|package| package.path())
                    .collect::<Vec<_>>();
                packages.sort();
                let binary = ["android", "sdkmanager"].into_iter().find_map(|name| {
                    packages
                        .iter()
                        .map(|package| package.join("bin").join(name))
                        .find(|binary| binary.is_file())
                });
                if let Some(binary) = binary {
                    watched.push(binary);
                }
            }
        }
        require(
            !components.is_empty() || watched.len() > 1,
            "Android SDK registration needs an installed component or executable",
        )?;
    }
    for component in components {
        let entry = path.join(component.replace(';', "/"));
        require(
            entry.exists(),
            &format!("Missing tool component {component}"),
        )?;
        watched.push(entry.clone());
        let properties = entry.join("source.properties");
        if properties.exists() {
            watched.push(properties);
        }
        let payloads: &[&str] = if component == "emulator" {
            &["emulator", "qemu/linux-x86_64/qemu-system-x86_64"]
        } else if component == "platform-tools" {
            &["adb"]
        } else if component.starts_with("system-images;") {
            &["system.img", "ramdisk.img", "kernel-ranchu", "userdata.img"]
        } else if component.starts_with("cmdline-tools;") {
            &["bin/sdkmanager"]
        } else {
            &[]
        };
        for relative in payloads {
            let payload = entry.join(relative);
            if payload.is_file() {
                watched.push(payload);
            }
        }
        if component == "emulator"
            || component == "platform-tools"
            || component.starts_with("system-images;")
        {
            require(
                watched.iter().any(|item| {
                    item.starts_with(&entry)
                        && item != &entry
                        && item
                            .file_name()
                            .is_some_and(|name| name != "source.properties")
                }),
                &format!("Missing installed payload for {component}"),
            )?;
        }
    }
    let mut digest = Sha256::new();
    for item in watched {
        let metadata = fs::metadata(&item)?;
        digest.update(item.as_os_str().as_encoded_bytes());
        for part in [
            metadata.dev(),
            metadata.ino(),
            metadata.size(),
            metadata.mtime() as u64,
            metadata.mtime_nsec() as u64,
        ] {
            digest.update(part.to_le_bytes());
        }
        if metadata.is_file() {
            let mut file = File::open(&item)?;
            let mut sample = [0_u8; 4096];
            let count = file.read(&mut sample)?;
            digest.update(&sample[..count]);
            if metadata.len() > sample.len() as u64 {
                file.seek(SeekFrom::End(-(sample.len() as i64)))?;
                let count = file.read(&mut sample)?;
                digest.update(&sample[..count]);
            }
        }
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn candidate_binary(tool: &Tool, item: &str) -> Option<PathBuf> {
    if tool.name == "android-sdk" {
        if item == "android" {
            let root = tool.path.join("bin/android");
            if root.is_file() {
                return Some(root);
            }
            let latest = tool.path.join("cmdline-tools/latest/bin/android");
            if latest.is_file() {
                return Some(latest);
            }
            let mut bins = fs::read_dir(tool.path.join("cmdline-tools"))
                .ok()?
                .flatten()
                .map(|entry| entry.path().join("bin/android"))
                .collect::<Vec<_>>();
            bins.sort();
            return bins.into_iter().find(|binary| binary.is_file());
        }
        if item == "sdkmanager" {
            let latest = tool.path.join("cmdline-tools/latest/bin/sdkmanager");
            if latest.is_file() {
                return Some(latest);
            }
            let mut bins = fs::read_dir(tool.path.join("cmdline-tools"))
                .ok()?
                .flatten()
                .map(|entry| entry.path().join("bin/sdkmanager"))
                .collect::<Vec<_>>();
            bins.sort();
            return bins.into_iter().find(|binary| binary.is_file());
        }
        let relative = match item {
            "adb" => "platform-tools/adb",
            "emulator" => "emulator/emulator",
            _ => return None,
        };
        Some(tool.path.join(relative))
    } else if tool.name == item {
        Some(tool.path.join("bin").join(item))
    } else {
        None
    }
}

struct CandidateProbe<'a> {
    paths: &'a Paths,
    scratch: &'a Path,
    project: &'a Path,
    supervisor: &'a Path,
    network: bool,
}

fn candidate_matches(
    tool: &Tool,
    requirements: &[&Requirement],
    resolved: &ResolvedTools,
    context: &CandidateProbe<'_>,
) -> bool {
    if !tool.ready
        || tool.platform != std::env::consts::OS
        || tool.architecture != std::env::consts::ARCH
    {
        return false;
    }
    let policy = || {
        Some(super::dependency::ProbePolicy::Restricted {
            writable_roots: &resolved.writable_roots,
            installations: &resolved.installations,
            network: context.network,
        })
    };
    requirements.iter().all(|req| {
        let mut direct = (*req).clone();
        if req.kind == "sdk_component" {
            return tool.name == "android-sdk"
                && tool.components.contains(&req.item)
                && if req.item.starts_with("system-images;") {
                    super::dependency::probe(
                        &direct,
                        context.scratch,
                        context.project,
                        context.supervisor,
                        &resolved.env,
                        policy(),
                    )
                    .starts_with("OK:")
                } else {
                    super::dependency::probe_sdk_component_at(
                        &tool.path,
                        &req.item,
                        req.version.as_deref(),
                    )
                    .starts_with("OK:")
                };
        } else {
            let Some(binary) = candidate_binary(tool, &req.item) else {
                return false;
            };
            direct.item = binary.to_string_lossy().into_owned();
        }
        super::dependency::probe(
            &direct,
            context.scratch,
            context.project,
            context.supervisor,
            &resolved.env,
            policy(),
        )
        .starts_with("OK:")
    })
}

impl Service {
    fn verified_candidate(
        &self,
        tool: &Tool,
        requirements: &[&Requirement],
        context: &CandidateProbe<'_>,
    ) -> Option<ResolvedTools> {
        let resolved = self
            .resolve_tools_with_overlay(context.paths, Some(tool))
            .ok()?;
        if !candidate_matches(tool, requirements, &resolved, context) {
            return None;
        }
        // External owners can change a tree without taking our lease. Recheck
        // after the executable runs, then hold the returned leases through bind.
        self.resolve_tools_with_overlay(context.paths, Some(tool))
            .ok()
    }

    /// A declared local prerequisite can reuse a compatible registered generation.
    /// External registrations take precedence; an explicit binding is never replaced.
    pub(super) fn ensure_requirement_bindings(
        &mut self,
        task: i64,
        scratch_root: &Path,
        supervisor: &Path,
        paths: &Paths,
    ) -> Result<()> {
        let requirements = self.dependencies(task)?;
        if requirements.is_empty() {
            return Ok(());
        }
        let scratch = crate::scratch::Scratch::new(scratch_root)?;
        let project = self.project_root()?;
        let context = CandidateProbe {
            paths,
            scratch: scratch.path(),
            project: &project,
            supervisor,
            network: self.agent_network()?,
        };
        let mut candidates = self.tools()?;
        candidates.sort_by_key(|tool| (tool.provenance != "external", tool.id));
        let mut bound_names = candidates
            .iter()
            .filter(|tool| tool.bound)
            .map(|tool| tool.name.clone())
            .collect::<BTreeSet<_>>();
        let mut components = Vec::new();
        let mut executables: BTreeMap<String, Vec<Requirement>> = BTreeMap::new();
        for dependency in requirements {
            let req = dependency.requirement;
            if req.kind == "sdk_component" {
                components.push(req);
            } else if req.kind == "executable" && !req.item.contains('/') {
                executables.entry(name(&req.item)?).or_default().push(req);
            }
        }
        let android = ["adb", "emulator", "sdkmanager"];
        let android_requirements = executables
            .iter()
            .filter(|(name, _)| android.contains(&name.as_str()))
            .flat_map(|(_, reqs)| reqs.iter())
            .collect::<Vec<_>>();
        let component_requirements = components.iter().collect::<Vec<_>>();
        let all_sdk_requirements = component_requirements
            .iter()
            .copied()
            .chain(android_requirements.iter().copied())
            .collect::<Vec<_>>();
        let mut sdk = candidates
            .iter()
            .find(|tool| tool.name == "android-sdk" && tool.bound);
        if sdk.is_none() && !components.is_empty() {
            let selected = candidates
                .iter()
                .filter(|tool| tool.name == "android-sdk")
                .find_map(|tool| {
                    self.verified_candidate(tool, &all_sdk_requirements, &context)
                        .map(|guard| (tool, guard))
                })
                .or_else(|| {
                    candidates
                        .iter()
                        .filter(|tool| tool.name == "android-sdk")
                        .find_map(|tool| {
                            self.verified_candidate(tool, &component_requirements, &context)
                                .map(|guard| (tool, guard))
                        })
                });
            if let Some((tool, _guard)) = selected {
                self.bind_tool("android-sdk", tool.id, Some(0))?;
                sdk = Some(tool);
                bound_names.insert("android-sdk".into());
            }
        }
        for (name, requirements) in &executables {
            if bound_names.contains(name) {
                continue;
            }
            let requested = requirements.iter().collect::<Vec<_>>();
            if sdk.is_some_and(|tool| {
                self.verified_candidate(tool, &requested, &context)
                    .is_some()
            }) {
                continue;
            }
            let selected = candidates
                .iter()
                .filter(|tool| tool.name == *name)
                .find_map(|tool| {
                    self.verified_candidate(tool, &requested, &context)
                        .map(|guard| (tool, guard))
                });
            if let Some((tool, _guard)) = selected {
                self.bind_tool(name, tool.id, Some(0))?;
                bound_names.insert(name.clone());
            }
        }
        if sdk.is_none() && components.is_empty() {
            let remaining = android_requirements
                .iter()
                .filter(|req| !bound_names.contains(&req.item))
                .copied()
                .collect::<Vec<_>>();
            let selected = (!remaining.is_empty())
                .then(|| {
                    candidates
                        .iter()
                        .filter(|tool| tool.name == "android-sdk")
                        .find_map(|tool| {
                            self.verified_candidate(tool, &remaining, &context)
                                .map(|guard| (tool, guard))
                        })
                })
                .flatten();
            if let Some((tool, _guard)) = selected {
                self.bind_tool("android-sdk", tool.id, Some(0))?;
            }
        }
        Ok(())
    }

    pub fn register_tool(&mut self, paths: &Paths, input: &Registration<'_>) -> Result<i64> {
        let tool_name = name(input.name)?;
        for value in [input.version, input.platform, input.architecture] {
            require(
                !value.trim().is_empty() && value.len() <= 100 && !value.contains('\0'),
                "Version, platform and architecture are required",
            )?;
        }
        let platform = input.platform.trim().to_ascii_lowercase();
        let architecture = input.architecture.trim().to_ascii_lowercase();
        let component_set = components(input.components)?;
        let canonical = input.path.canonicalize()?;
        require(
            canonical.to_str().is_some(),
            "Tool paths must be UTF-8 for CLI JSON output",
        )?;
        let managed_root = paths.data.join("tools");
        if input.managed {
            fs::create_dir_all(&managed_root)?;
            require(
                canonical.starts_with(managed_root.canonicalize()?),
                "Managed generations must live below this instance's data/tools directory",
            )?;
        } else {
            require(
                !canonical.starts_with(&managed_root),
                "An instance-owned generation must be registered as managed",
            )?;
        }
        let signature = probe(&canonical, &tool_name, &component_set)?;
        let existing: Option<i64> = self
            .db
            .connection()
            .query_row(
                "SELECT id FROM tool_generations WHERE path=?1",
                [canonical.as_os_str().as_encoded_bytes()],
                |r| r.get(0),
            )
            .optional()?;
        require(
            existing.is_none(),
            "Installation path is already registered; generations are immutable",
        )?;
        self.db.connection().execute("INSERT INTO tool_generations(name,version,platform,architecture,components_json,path,provenance,observed_signature,created_at_ms) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![tool_name,input.version.trim(),platform,architecture,serde_json::to_string(&component_set).expect("component JSON"),canonical.as_os_str().as_encoded_bytes(),if input.managed {"managed"} else {"external"},signature,now_ms()])?;
        Ok(self.db.connection().last_insert_rowid())
    }

    pub fn tools(&self) -> Result<Vec<Tool>> {
        self.load_tools(false)
    }

    pub(super) fn unready_bound_tool(&self) -> Result<Option<(String, i64)>> {
        Ok(self
            .load_tools(true)?
            .into_iter()
            .find(|tool| !tool.ready)
            .map(|tool| (tool.name, tool.id)))
    }

    fn load_tools(&self, bound_only: bool) -> Result<Vec<Tool>> {
        let sql = format!(
            "SELECT g.id,g.name,g.version,g.platform,g.architecture,g.components_json,g.path,g.provenance,g.row_version,p.generation_id IS NOT NULL,p.row_version,g.observed_signature FROM tool_generations g LEFT JOIN project_tools p ON p.generation_id=g.id AND p.project_id=?1 {} ORDER BY g.name,g.version,g.id",
            if bound_only {
                "WHERE p.generation_id IS NOT NULL"
            } else {
                ""
            }
        );
        let mut statement = self.db.connection().prepare(&sql)?;
        let rows = statement
            .query_map([self.project_id], |r| {
                let raw: Vec<u8> = r.get(6)?;
                let json: String = r.get(5)?;
                let mut tool = Tool {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    version: r.get(2)?,
                    platform: r.get(3)?,
                    architecture: r.get(4)?,
                    components: serde_json::from_str(&json).map_err(|e| {
                        rusqlite::Error::FromSqlConversionFailure(
                            5,
                            rusqlite::types::Type::Text,
                            Box::new(e),
                        )
                    })?,
                    path: PathBuf::from(std::ffi::OsString::from_vec(raw)),
                    provenance: r.get(7)?,
                    row_version: r.get(8)?,
                    bound: r.get(9)?,
                    binding_version: r.get(10)?,
                    ready: false,
                };
                let signature: String = r.get(11)?;
                tool.ready = probe(&tool.path, &tool.name, &tool.components)
                    .is_ok_and(|current| current == signature);
                Ok(tool)
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Explicit re-admission after the owner changed a registered external tree. The
    /// registration is version guarded, and an active lease prevents the update.
    pub fn refresh_external_tool(
        &mut self,
        paths: &Paths,
        generation: i64,
        expected: i64,
        version: &str,
        component_set: &[String],
    ) -> Result<i64> {
        require(
            !version.trim().is_empty() && version.len() <= 100,
            "Tool version is required",
        )?;
        let component_set = components(component_set)?;
        let lease_root = paths.data.join("tools").join(".leases");
        fs::create_dir_all(&lease_root)?;
        let lease = crate::scratch::open_lock(&lease_root.join(format!("{generation}.lock")))?;
        require(lease.try_lock().is_ok(), "Tool generation is in use")?;
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let row: Option<(String, Vec<u8>, String, i64)> = tx
            .query_row(
                "SELECT name,path,provenance,row_version FROM tool_generations WHERE id=?1",
                [generation],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        let (tool_name, raw, provenance, current) =
            row.ok_or_else(|| Error::Conflict("Unknown tool generation".into()))?;
        require(
            provenance == "external",
            "Managed generations are immutable; register another generation",
        )?;
        require(
            current == expected,
            "Tool generation changed; inspect it again",
        )?;
        let path = PathBuf::from(std::ffi::OsString::from_vec(raw));
        let signature = probe(&path, &tool_name, &component_set)?;
        tx.execute("UPDATE tool_generations SET version=?2,components_json=?3,observed_signature=?4,row_version=row_version+1 WHERE id=?1",params![generation,version.trim(),serde_json::to_string(&component_set).expect("JSON"),signature])?;
        tx.commit()?;
        Ok(current + 1)
    }

    pub fn bind_tool(
        &mut self,
        tool_name: &str,
        generation: i64,
        if_version: Option<i64>,
    ) -> Result<i64> {
        self.bind_tool_at_epoch(tool_name, generation, if_version, None)
    }

    /// Provisioning pins the project epoch before it starts. The epoch also detects
    /// an intervening bind followed by unbind, when no binding row remains.
    pub(crate) fn tool_epoch(&self) -> Result<i64> {
        Ok(self
            .db
            .connection()
            .query_row(
                "SELECT epoch FROM project_tool_epochs WHERE project_id=?1",
                [self.project_id],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or(0))
    }

    pub(crate) fn bind_tool_at_epoch(
        &mut self,
        tool_name: &str,
        generation: i64,
        if_version: Option<i64>,
        expected_epoch: Option<i64>,
    ) -> Result<i64> {
        let tool_name = name(tool_name)?;
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let epoch_before: i64 = tx
            .query_row(
                "SELECT epoch FROM project_tool_epochs WHERE project_id=?1",
                [self.project_id],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or(0);
        require(
            expected_epoch.is_none_or(|expected| expected == epoch_before),
            "Tool binding changed during provisioning; published generation remains available",
        )?;
        let candidate: Option<(String, String, String)> = tx
            .query_row(
                "SELECT name,platform,architecture FROM tool_generations WHERE id=?1",
                [generation],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        require(
            candidate.as_ref().is_some_and(|(name, platform, arch)| {
                name == &tool_name
                    && platform == std::env::consts::OS
                    && arch == std::env::consts::ARCH
            }),
            "Unknown or incompatible generation for this tool and host",
        )?;
        let active: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM attempts a JOIN tasks t ON t.id=a.task_id WHERE t.project_id=?1 AND a.state='running')",[self.project_id],|r|r.get(0))?;
        require(
            !active,
            "Pause or finish the current attempt before changing tool bindings",
        )?;
        let previous: Option<i64> = tx
            .query_row(
                "SELECT row_version FROM project_tools WHERE project_id=?1 AND name=?2",
                params![self.project_id, tool_name],
                |r| r.get(0),
            )
            .optional()?;
        if previous.is_none() {
            let count: i64 = tx.query_row(
                "SELECT count(*) FROM project_tools WHERE project_id=?1",
                [self.project_id],
                |r| r.get(0),
            )?;
            require(count < 32, "A project may bind at most 32 tool generations")?;
        }
        require(
            if_version.is_none_or(|expected| previous.unwrap_or(0) == expected),
            "Tool binding changed; inspect it again",
        )?;
        tx.execute("INSERT INTO project_tool_epochs(project_id,epoch) VALUES (?1,1) ON CONFLICT(project_id) DO UPDATE SET epoch=epoch+1",[self.project_id])?;
        let epoch: i64 = tx.query_row(
            "SELECT epoch FROM project_tool_epochs WHERE project_id=?1",
            [self.project_id],
            |r| r.get(0),
        )?;
        tx.execute("INSERT INTO project_tools(project_id,name,generation_id,row_version) VALUES (?1,?2,?3,?4) ON CONFLICT(project_id,name) DO UPDATE SET generation_id=excluded.generation_id,row_version=excluded.row_version",params![self.project_id,tool_name,generation,epoch])?;
        let version = tx.query_row(
            "SELECT row_version FROM project_tools WHERE project_id=?1 AND name=?2",
            params![self.project_id, tool_name],
            |r| r.get(0),
        )?;
        tx.commit()?;
        Ok(version)
    }

    pub fn unbind_tool(&mut self, tool_name: &str, expected: i64) -> Result<()> {
        let tool_name = name(tool_name)?;
        let tx = self
            .db
            .connection_mut()
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let active: bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM attempts a JOIN tasks t ON t.id=a.task_id WHERE t.project_id=?1 AND a.state='running')",[self.project_id],|r|r.get(0))?;
        require(
            !active,
            "Pause or finish the current attempt before changing tool bindings",
        )?;
        require(
            tx.execute(
                "DELETE FROM project_tools WHERE project_id=?1 AND name=?2 AND row_version=?3",
                params![self.project_id, tool_name, expected],
            )? == 1,
            "Tool binding changed; inspect it again",
        )?;
        tx.execute("INSERT INTO project_tool_epochs(project_id,epoch) VALUES (?1,1) ON CONFLICT(project_id) DO UPDATE SET epoch=epoch+1",[self.project_id])?;
        tx.commit()?;
        Ok(())
    }

    pub fn resolve_tools(&self, paths: &Paths) -> Result<ResolvedTools> {
        self.resolve_tools_with_overlay(paths, None)
    }

    /// A file callback may use Android transport only while its original attempt
    /// still owns execution and the same registered SDK remains bound.
    pub(crate) fn authorize_android_transport(
        &self,
        task: i64,
        attempt: i64,
        token: &str,
        sdk: &Path,
    ) -> Result<()> {
        let owned: bool = self.db.connection().query_row(
            "SELECT EXISTS(SELECT 1 FROM attempts a JOIN tasks t ON t.id=a.task_id WHERE a.id=?1 AND a.task_id=?2 AND a.execution_token=?3 AND a.state='running' AND t.project_id=?4)",
            rusqlite::params![attempt, task, token, self.project_id], |row| row.get(0))?;
        require(
            owned,
            "Android transport belongs to a different or stopped attempt",
        )?;
        require(
            self.tools()?.iter().any(|tool| {
                tool.name == "android-sdk" && tool.bound && tool.ready && tool.path == sdk
            }),
            "Android transport SDK binding changed; Recheck before using adb",
        )
    }

    /// Resolve a prospective binding without writing it. The returned leases pin
    /// both the candidate and every already bound dependency for the probe's life.
    fn resolve_tools_with_overlay(
        &self,
        paths: &Paths,
        overlay: Option<&Tool>,
    ) -> Result<ResolvedTools> {
        let epoch: i64 = self
            .db
            .connection()
            .query_row(
                "SELECT epoch FROM project_tool_epochs WHERE project_id=?1",
                [self.project_id],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or(0);
        let mut tools = self.load_tools(true)?;
        if let Some(candidate) = overlay {
            match tools.iter().find(|bound| bound.name == candidate.name) {
                Some(bound) if bound.id == candidate.id => {}
                Some(_) => {
                    return Err(Error::Conflict(
                        "An explicit tool binding takes precedence".into(),
                    ));
                }
                None => tools.push(candidate.clone()),
            }
            tools.sort_by(|left, right| left.name.cmp(&right.name).then(left.id.cmp(&right.id)));
        }
        if tools.is_empty() {
            return Ok(ResolvedTools {
                env: vec![],
                fingerprint: if epoch == 0 {
                    "unbound".into()
                } else {
                    format!("unbound:{epoch}")
                },
                description: "No project tool bindings".into(),
                writable_roots: vec![],
                installations: vec![],
                _leases: vec![],
                _android_runtime: None,
            });
        }
        let has_android = tools.iter().any(|tool| tool.name == "android-sdk");
        require(
            !has_android || !self.agent_network()?,
            "Android adb transport isolation requires project agent network Off; set it in Configuration, then Recheck",
        )?;
        if has_android {
            crate::runner::sandbox::available_android_transport()?;
        }
        let has_android_image = has_android_image(&tools);
        let lease_root = paths.data.join("tools").join(".leases");
        fs::create_dir_all(&lease_root)?;
        let mut leases = Vec::new();
        let mut bins = Vec::new();
        let mut sdk_bins = Vec::new();
        let mut env = Vec::new();
        let mut digest = Sha256::new();
        digest.update(epoch.to_le_bytes());
        if has_android {
            digest.update(b"android-tool-path-v2");
        }
        let mut description = Vec::new();
        let mut installations = Vec::new();
        for tool in tools {
            require(
                tool.ready,
                &format!(
                    "Bound tool {} generation #{} is missing or changed; recheck after external setup",
                    tool.name, tool.id
                ),
            )?;
            let lease = crate::scratch::open_lock(&lease_root.join(format!("{}.lock", tool.id)))?;
            lease.lock_shared()?;
            leases.push(lease);
            let signature = probe(&tool.path, &tool.name, &tool.components)?;
            let registered: String = self.db.connection().query_row(
                "SELECT observed_signature FROM tool_generations WHERE id=?1",
                [tool.id],
                |row| row.get(0),
            )?;
            require(
                signature == registered,
                &format!(
                    "Bound tool {} generation #{} changed while resolving; recheck after external setup",
                    tool.name, tool.id
                ),
            )?;
            digest.update(format!(
                "{}:{}:{}:{}:{}:{}:{}:{}:{}",
                tool.id,
                tool.name,
                tool.version,
                tool.platform,
                tool.architecture,
                serde_json::to_string(&tool.components).expect("JSON"),
                tool.row_version,
                tool.binding_version.unwrap_or(0),
                signature
            ));
            let bin = tool.path.join("bin");
            if bin.is_dir() {
                if tool.name == "android-sdk" {
                    sdk_bins.push(bin);
                } else {
                    bins.push(bin);
                }
            }
            if tool.name == "android-sdk" {
                add_android_sdk_bindings(&tool, &mut env, &mut sdk_bins);
            }
            description.push(format!(
                "{} {} {} ({})",
                tool.name,
                tool.version,
                tool.path.display(),
                tool.provenance
            ));
            installations.push(tool.path);
        }
        bins.extend(sdk_bins);
        if !bins.is_empty() {
            let host = std::env::var_os("PATH").unwrap_or_default();
            let mut all = bins;
            all.extend(std::env::split_paths(&host));
            env.push((
                "PATH".into(),
                std::env::join_paths(all)
                    .map_err(|e| Error::Conflict(e.to_string()))?
                    .to_string_lossy()
                    .into_owned(),
            ));
        }
        let downloads = paths.cache.join("package-downloads");
        let runtime = paths
            .cache
            .join("project-runtime")
            .join(self.project_id.to_string());
        fs::create_dir_all(&downloads)?;
        fs::create_dir_all(&runtime)?;
        for (key, path) in [
            ("PIP_CACHE_DIR", downloads.join("pip")),
            ("npm_config_cache", downloads.join("npm")),
            ("PUB_CACHE", downloads.join("pub")),
            ("GRADLE_USER_HOME", runtime.join("gradle")),
            ("ANDROID_USER_HOME", runtime.join("android")),
            ("ANDROID_EMULATOR_HOME", runtime.join("android")),
        ] {
            fs::create_dir_all(&path)?;
            env.push((key.into(), path.to_string_lossy().into_owned()));
        }
        let mut android_runtime = None;
        let mut writable_roots = vec![downloads, runtime.clone()];
        if has_android {
            let owned = crate::scratch::Scratch::new(&runtime.join("android-avd-runs"))?;
            let wrappers = owned.path().join("bin");
            fs::create_dir(&wrappers)?;
            for name in ["adb", "emulator"] {
                let script = format!(
                    "#!/bin/sh\nexec \"$KTASK_ANDROID_CLIENT\" __android_client {name} \"$@\"\n"
                );
                fs::write(wrappers.join(name), script)?;
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(wrappers.join(name), fs::Permissions::from_mode(0o755))?;
            }
            env.push((
                "KTASK_ANDROID_WRAPPER_DIR".into(),
                wrappers.to_string_lossy().into_owned(),
            ));
            env.push((
                "KTASK_ANDROID_CLIENT".into(),
                paths.executable.to_string_lossy().into_owned(),
            ));
            let avd = owned.path().join("avd");
            fs::create_dir_all(&avd)?;
            add_android_template(owned.path(), has_android_image, &mut env)?;
            env.push((
                "ANDROID_AVD_HOME".into(),
                avd.to_string_lossy().into_owned(),
            ));
            env.push((
                "KTASK_ANDROID_RUNTIME_ROOT".into(),
                owned.path().to_string_lossy().into_owned(),
            ));
            let port_root = paths.runtime.join("adb-ports");
            fs::create_dir_all(&port_root)?;
            let mut port = None;
            for number in 35_000..45_000 {
                let lease = crate::scratch::open_lock(&port_root.join(format!("{number}.lock")))?;
                if lease.try_lock().is_ok()
                    && std::net::TcpListener::bind(("127.0.0.1", number)).is_ok()
                {
                    port = Some(number);
                    leases.push(lease);
                    break;
                }
            }
            let port = port.ok_or_else(|| {
                Error::Conflict("No isolated adb server port is available".into())
            })?;
            env.push(("ADB_SERVER_SOCKET".into(), format!("tcp:localhost:{port}")));
            env.push(("ANDROID_ADB_SERVER_PORT".into(), port.to_string()));
            let emulator_port_root = paths.runtime.join("emulator-ports");
            fs::create_dir_all(&emulator_port_root)?;
            let mut emulator_port = None;
            for number in (5554..=5682).step_by(2) {
                let lease =
                    crate::scratch::open_lock(&emulator_port_root.join(format!("{number}.lock")))?;
                if lease.try_lock().is_ok()
                    && std::net::TcpListener::bind(("127.0.0.1", number)).is_ok()
                    && std::net::TcpListener::bind(("127.0.0.1", number + 1)).is_ok()
                {
                    emulator_port = Some(number);
                    leases.push(lease);
                    break;
                }
            }
            let emulator_port = emulator_port.ok_or_else(|| {
                Error::Conflict("No exclusive Android emulator port pair is available".into())
            })?;
            env.push(("KTASK_EMULATOR_PORT".into(), emulator_port.to_string()));
            env.push(("ANDROID_SERIAL".into(), format!("emulator-{emulator_port}")));
            writable_roots.push(owned.path().to_path_buf());
            android_runtime = Some(owned);
        }
        Ok(ResolvedTools {
            env,
            fingerprint: format!("{:x}", digest.finalize()),
            description: description.join("; "),
            writable_roots,
            installations,
            _leases: leases,
            _android_runtime: android_runtime,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::{
        CheckCounts, Definition, Kind, ModelConfig, VersionedTask, dependency::Requirement,
        execution::Check,
    };
    use crate::{instance::Paths, scratch::Scratch, storage::Database};
    use std::os::unix::fs::PermissionsExt;

    fn fake_tool(root: &Path, version: &str) -> Result<PathBuf> {
        let path = root.join(format!("rg-{version}"));
        fs::create_dir_all(path.join("bin"))?;
        let binary = path.join("bin/rg");
        fs::write(&binary, format!("#!/bin/sh\necho 'rg {version}'\n"))?;
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755))?;
        Ok(path)
    }

    #[test]
    fn android_bindings_lease_distinct_adb_ports_and_reset_owned_avd_state() -> Result<()> {
        let sandbox = Scratch::new(&std::env::temp_dir().join("ktask-test-scratch"))?;
        let root = sandbox.path();
        let paths = Paths {
            config: root.join("config/ktask-dev"),
            state: root.join("state/ktask-dev"),
            cache: root.join("cache/ktask-dev"),
            data: root.join("data/ktask-dev"),
            runtime: root.join("run/ktask-dev"),
            executable: std::env::current_exe()?,
        };
        let sdk = root.join("sdk");
        fs::create_dir_all(sdk.join("platform-tools"))?;
        fs::write(sdk.join("platform-tools/adb"), b"adb fixture")?;
        let image = sdk.join("system-images/android-36/google_apis/x86_64");
        fs::create_dir_all(&image)?;
        fs::write(image.join("source.properties"), "Pkg.Revision=7\n")?;
        fs::write(image.join("system.img"), b"small fixture")?;
        let first_root = root.join("first");
        let second_root = root.join("second");
        fs::create_dir(&first_root)?;
        fs::create_dir(&second_root)?;
        let mut first = Service::open(Database::open_in(&paths.state)?, &first_root, None)?;
        let id = first.register_tool(
            &paths,
            &Registration {
                name: "android-sdk",
                version: "fixture-1",
                platform: std::env::consts::OS,
                architecture: std::env::consts::ARCH,
                components: &[
                    "platform-tools".into(),
                    "system-images;android-36;google_apis;x86_64".into(),
                ],
                path: &sdk,
                managed: false,
            },
        )?;
        let mut second = Service::open(Database::open_in(&paths.state)?, &second_root, None)?;
        first.bind_tool("android-sdk", id, Some(0))?;
        second.bind_tool("android-sdk", id, Some(0))?;
        let one = first.resolve_tools(&paths)?;
        let two = second.resolve_tools(&paths)?;
        let value = |env: &[(String, String)], key: &str| {
            env.iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value.clone())
                .unwrap()
        };
        let one_avd = PathBuf::from(value(&one.env, "ANDROID_AVD_HOME"));
        let two_avd = PathBuf::from(value(&two.env, "ANDROID_AVD_HOME"));
        assert!(one_avd.is_dir() && two_avd.is_dir() && one_avd != two_avd);
        let template_path = PathBuf::from(value(&one.env, "KTASK_ANDROID_AVD_TEMPLATE"));
        let template: serde_json::Value =
            serde_json::from_slice(&fs::read(template_path)?).expect("valid reviewed template");
        assert_eq!(
            template["system_image"],
            "system-images;android-36;google_apis;x86_64"
        );
        let one_socket = value(&one.env, "ADB_SERVER_SOCKET");
        let two_socket = value(&two.env, "ADB_SERVER_SOCKET");
        assert!(one_socket.starts_with("tcp:localhost:") && one_socket != two_socket);
        assert!(!one_socket.ends_with(":5037") && !two_socket.ends_with(":5037"));
        let one_port = value(&one.env, "KTASK_EMULATOR_PORT");
        let two_port = value(&two.env, "KTASK_EMULATOR_PORT");
        assert_ne!(one_port, two_port);
        assert_eq!(
            value(&one.env, "ANDROID_SERIAL"),
            format!("emulator-{one_port}")
        );
        assert_eq!(
            value(&two.env, "ANDROID_SERIAL"),
            format!("emulator-{two_port}")
        );
        fs::write(one_avd.join("disk.img"), "isolated")?;
        drop(one);
        assert!(!one_avd.exists() && two_avd.exists());
        drop(two);
        assert!(!two_avd.exists());
        Ok(())
    }

    #[test]
    fn versioned_android_cli_registers_and_resolves_without_latest_alias() -> Result<()> {
        let sandbox = Scratch::new(&std::env::temp_dir().join("ktask-test-scratch"))?;
        let root = sandbox.path();
        let paths = Paths {
            config: root.join("config"),
            state: root.join("state"),
            cache: root.join("cache"),
            data: root.join("data"),
            runtime: root.join("runtime"),
            executable: std::env::current_exe()?,
        };
        let project = root.join("project");
        fs::create_dir(&project)?;
        let sdk = root.join("sdk");
        fs::create_dir_all(sdk.join("cmdline-tools/16/bin"))?;
        let binary = sdk.join("cmdline-tools/16/bin/android");
        fs::write(&binary, b"#!/bin/sh\nexit 0\n")?;
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755))?;
        let mut service = Service::open(Database::open_in(&paths.state)?, &project, None)?;
        let id = service.register_tool(
            &paths,
            &Registration {
                name: "android-sdk",
                version: "cmdline-tools-16",
                platform: std::env::consts::OS,
                architecture: std::env::consts::ARCH,
                components: &[],
                path: &sdk,
                managed: false,
            },
        )?;
        service.bind_tool("android-sdk", id, Some(0))?;
        let registered = service
            .tools()?
            .into_iter()
            .find(|tool| tool.id == id)
            .expect("SDK");
        assert!(registered.ready);
        assert_eq!(
            candidate_binary(&registered, "android"),
            Some(binary.clone())
        );
        let resolved = service.resolve_tools(&paths)?;
        let path = resolved
            .env
            .iter()
            .find(|(name, _)| name == "PATH")
            .expect("PATH")
            .1
            .clone();
        assert!(std::env::split_paths(&path).any(|entry| entry == binary.parent().unwrap()));
        Ok(())
    }

    #[test]
    fn binding_revision_rejects_stale_clients_after_recreation() -> Result<()> {
        let sandbox = Scratch::new(&std::env::temp_dir().join("ktask-test-scratch"))?;
        let root = sandbox.path();
        let paths = Paths {
            config: root.join("config/ktask-dev"),
            state: root.join("state/ktask-dev"),
            cache: root.join("cache/ktask-dev"),
            data: root.join("data/ktask-dev"),
            runtime: root.join("run/ktask-dev"),
            executable: std::env::current_exe()?,
        };
        let project = root.join("project");
        fs::create_dir(&project)?;
        let mut service = Service::open(Database::open_in(&paths.state)?, &project, None)?;
        let mut ids = Vec::new();
        for version in ["14.1", "15.0"] {
            let install = fake_tool(root, version)?;
            ids.push(service.register_tool(
                &paths,
                &Registration {
                    name: "rg",
                    version,
                    platform: std::env::consts::OS,
                    architecture: std::env::consts::ARCH,
                    components: &[],
                    path: &install,
                    managed: false,
                },
            )?);
        }
        assert_eq!(service.bind_tool("rg", ids[0], Some(0))?, 1);
        service.unbind_tool("rg", 1)?;
        assert_eq!(service.bind_tool("rg", ids[1], Some(0))?, 3);
        assert!(service.unbind_tool("rg", 1).is_err());
        assert!(service.bind_tool("rg", ids[0], Some(1)).is_err());
        assert_eq!(
            service
                .tools()?
                .into_iter()
                .find(|tool| tool.bound)
                .unwrap()
                .id,
            ids[1]
        );
        Ok(())
    }

    #[test]
    fn sdk_selection_uses_all_requirements_and_payload_drift_is_observed() -> Result<()> {
        let sandbox = Scratch::new(&std::env::temp_dir().join("ktask-test-scratch"))?;
        let root = sandbox.path();
        let paths = Paths {
            config: root.join("config/ktask-dev"),
            state: root.join("state/ktask-dev"),
            cache: root.join("cache/ktask-dev"),
            data: root.join("data/ktask-dev"),
            runtime: root.join("run/ktask-dev"),
            executable: std::env::current_exe()?
                .parent()
                .and_then(Path::parent)
                .expect("target/debug/deps")
                .join("ktask-dev"),
        };
        let image = "system-images;android-36;google_apis;x86_64".to_string();
        let mut ids = Vec::new();
        for index in 0..2 {
            let install = root.join(format!("sdk-{index}"));
            fs::create_dir_all(install.join("emulator"))?;
            let emulator = install.join("emulator/emulator");
            fs::write(&emulator, b"#!/bin/sh\necho emulator\n")?;
            fs::set_permissions(&emulator, fs::Permissions::from_mode(0o755))?;
            let mut components = vec!["emulator".to_string()];
            if index == 1 {
                fs::create_dir_all(install.join("platform-tools"))?;
                fs::create_dir_all(install.join("licenses"))?;
                fs::write(
                    install.join("licenses/android-sdk-license"),
                    crate::application::provision::ANDROID_SDK_LICENSE_SHA1,
                )?;
                let adb = install.join("platform-tools/adb");
                fs::write(&adb, b"#!/bin/sh\necho adb\n")?;
                fs::set_permissions(&adb, fs::Permissions::from_mode(0o755))?;
                let directory = install.join(image.replace(';', "/"));
                fs::create_dir_all(&directory)?;
                fs::write(directory.join("source.properties"), b"Pkg.Revision=1.0\nAndroidVersion.ApiLevel=36\nSystemImage.TagId=google_apis\nSystemImage.Abi=x86_64\n")?;
                fs::write(directory.join("system.img"), b"system image one")?;
                components.push(image.clone());
            }
            let project = root.join("registry-project");
            fs::create_dir_all(&project)?;
            let mut service = Service::open(Database::open_in(&paths.state)?, &project, None)?;
            ids.push(service.register_tool(
                &paths,
                &Registration {
                    name: "android-sdk",
                    version: "36",
                    platform: std::env::consts::OS,
                    architecture: std::env::consts::ARCH,
                    components: &components,
                    path: &install,
                    managed: false,
                },
            )?);
        }
        for (project_number, order) in [["emulator", image.as_str()], [image.as_str(), "emulator"]]
            .iter()
            .enumerate()
        {
            let project = root.join(format!("project-{project_number}"));
            fs::create_dir(&project)?;
            let mut service = Service::open(Database::open_in(&paths.state)?, &project, None)?;
            let definition = Definition {
                title: "Android work".into(),
                goal: "Use installed components".into(),
                criteria: vec!["Components ready".into()],
                kind: Kind::Ai,
                category: None,
                lenses: None,
                models: std::array::from_fn(|_| None),
                attempt_limit: Some(3),
                timeout_ms: None,
                stage_timeout_ms: None,
            };
            let task = service.create(&[definition], None, true)?[0];
            for component in order {
                let version = service.current_task()?.unwrap().version;
                service.declare_requirement(
                    VersionedTask { id: task, version },
                    &Requirement {
                        kind: "sdk_component".into(),
                        item: (*component).into(),
                        version: None,
                        scope: "project".into(),
                        stage: "execution".into(),
                        reproduction: "component required".into(),
                    },
                )?;
            }
            service.ensure_requirement_bindings(
                task,
                &root.join("cache/workers"),
                &paths.executable,
                &paths,
            )?;
            assert_eq!(
                service
                    .tools()?
                    .into_iter()
                    .find(|tool| tool.bound)
                    .unwrap()
                    .id,
                ids[1]
            );
        }
        // Metadata and a registered signature alone do not admit a broken
        // external emulator for a new image requirement.
        let broken = root.join("sdk-1/emulator/emulator");
        fs::write(&broken, b"#!/bin/sh\nexit 17\n")?;
        let registry_project = root.join("registry-project");
        let mut registry =
            Service::open(Database::open_in(&paths.state)?, &registry_project, None)?;
        registry.refresh_external_tool(
            &paths,
            ids[1],
            1,
            "36",
            &["emulator".into(), image.clone()],
        )?;
        let project = root.join("project-broken-external");
        fs::create_dir(&project)?;
        let mut waiting = Service::open(Database::open_in(&paths.state)?, &project, None)?;
        let task = waiting.create(
            &[Definition {
                title: "External Android admission".into(),
                goal: "Use image".into(),
                criteria: vec!["working emulator".into()],
                kind: Kind::Ai,
                category: None,
                lenses: None,
                models: std::array::from_fn(|_| None),
                attempt_limit: None,
                timeout_ms: None,
                stage_timeout_ms: None,
            }],
            None,
            true,
        )?[0];
        let version = waiting.current_task()?.unwrap().version;
        waiting.declare_requirement(
            VersionedTask { id: task, version },
            &Requirement {
                kind: "sdk_component".into(),
                item: image.clone(),
                version: None,
                scope: "project".into(),
                stage: "execution".into(),
                reproduction: "image required".into(),
            },
        )?;
        waiting.ensure_requirement_bindings(
            task,
            &root.join("cache/workers"),
            &paths.executable,
            &paths,
        )?;
        assert!(waiting.tools()?.iter().all(|tool| !tool.bound));
        let project = root.join("registry-project");
        let mut service = Service::open(Database::open_in(&paths.state)?, &project, None)?;
        let image_path = root
            .join("sdk-1")
            .join(image.replace(';', "/"))
            .join("system.img");
        fs::write(&image_path, b"system image two")?;
        assert!(
            !service
                .tools()?
                .into_iter()
                .find(|tool| tool.id == ids[1])
                .unwrap()
                .ready
        );
        service.refresh_external_tool(&paths, ids[1], 2, "36", &["emulator".into(), image])?;
        assert!(
            service
                .tools()?
                .into_iter()
                .find(|tool| tool.id == ids[1])
                .unwrap()
                .ready
        );
        Ok(())
    }

    #[test]
    fn missing_bound_generation_enters_dependency_wait_until_explicit_recheck() -> Result<()> {
        let sandbox = Scratch::new(&std::env::temp_dir().join("ktask-test-scratch"))?;
        let root = sandbox.path();
        let paths = Paths {
            config: root.join("config/ktask-dev"),
            state: root.join("state/ktask-dev"),
            cache: root.join("cache/ktask-dev"),
            data: root.join("data/ktask-dev"),
            runtime: root.join("run/ktask-dev"),
            executable: std::env::current_exe()?,
        };
        let project = root.join("project");
        fs::create_dir(&project)?;
        let mut service = Service::open(Database::open_in(&paths.state)?, &project, None)?;
        let install = fake_tool(root, "14.1")?;
        let id = service.register_tool(
            &paths,
            &Registration {
                name: "rg",
                version: "14.1",
                platform: std::env::consts::OS,
                architecture: std::env::consts::ARCH,
                components: &[],
                path: &install,
                managed: false,
            },
        )?;
        service.bind_tool("rg", id, Some(0))?;
        let definition = Definition {
            title: "Missing tool".into(),
            goal: "Wait for setup".into(),
            criteria: vec!["Ready".into()],
            kind: Kind::Ai,
            category: None,
            lenses: None,
            models: std::array::from_fn(|_| None),
            attempt_limit: Some(3),
            timeout_ms: Some(600_000),
            stage_timeout_ms: None,
        };
        let task = service.create(&[definition], None, true)?[0];
        fs::remove_file(install.join("bin/rg"))?;
        let target = VersionedTask {
            id: task,
            version: service.current_task()?.expect("current").version,
        };
        assert!(!service.admit_dependencies_for_run(
            target,
            &paths.cache.join("workers"),
            &paths.executable,
            &paths
        )?);
        assert_eq!(
            service.dependencies(task)?[0].requirement.kind,
            "registered_tool"
        );
        assert_eq!(service.dependencies(task)?[0].state, "waiting");
        fs::write(install.join("bin/rg"), b"#!/bin/sh\necho 'rg 14.1'\n")?;
        fs::set_permissions(install.join("bin/rg"), fs::Permissions::from_mode(0o755))?;
        service.refresh_external_tool(&paths, id, 1, "14.1", &[])?;
        let target = VersionedTask {
            id: task,
            version: service.current_task()?.expect("current").version,
        };
        assert!(service.recheck_dependencies(
            target,
            &paths.cache.join("workers"),
            &paths.executable,
            &paths
        )?);
        assert_eq!(service.dependencies(task)?[0].state, "satisfied");
        Ok(())
    }

    #[test]
    fn check_reuse_requires_the_recorded_tool_fingerprint() -> Result<()> {
        let sandbox = Scratch::new(&std::env::temp_dir().join("ktask-test-scratch"))?;
        let root = sandbox.path();
        let paths = Paths {
            config: root.join("config/ktask-dev"),
            state: root.join("state/ktask-dev"),
            cache: root.join("cache/ktask-dev"),
            data: root.join("data/ktask-dev"),
            runtime: root.join("run/ktask-dev"),
            executable: std::env::current_exe()?,
        };
        let project = root.join("project");
        fs::create_dir(&project)?;
        let mut service = Service::open(Database::open_in(&paths.state)?, &project, None)?;
        let install = fake_tool(root, "14.1")?;
        let id = service.register_tool(
            &paths,
            &Registration {
                name: "rg",
                version: "14.1",
                platform: std::env::consts::OS,
                architecture: std::env::consts::ARCH,
                components: &[],
                path: &install,
                managed: false,
            },
        )?;
        service.bind_tool("rg", id, Some(0))?;
        service.save_check(
            &[],
            &crate::application::configuration::ProjectCheck {
                id: None,
                name: "fixture".into(),
                command: "true".into(),
                directory: ".".into(),
                timeout_ms: 1000,
                enabled: true,
            },
        )?;
        let check = Check {
            id: service.project_checks()?[0].id.expect("saved check"),
            name: "fixture".into(),
            command: "true".into(),
            directory: ".".into(),
            timeout_ms: 1000,
        };
        let definition = Definition {
            title: "Fingerprint proof".into(),
            goal: "Reuse a verified check".into(),
            criteria: vec!["Check proof".into()],
            kind: Kind::Ai,
            category: None,
            lenses: None,
            models: std::array::from_fn(|_| {
                Some(ModelConfig {
                    provider: "codex".into(),
                    model: "gpt-6-sol".into(),
                    effort: None,
                })
            }),
            attempt_limit: Some(3),
            timeout_ms: Some(600_000),
            stage_timeout_ms: None,
        };
        let task = service.create(&[definition], None, true)?[0];
        let run = service.begin_run(
            VersionedTask {
                id: task,
                version: 1,
            },
            "fixture",
            "0".repeat(40).as_str(),
            false,
        )?;
        let resolved = service.resolve_tools(&paths)?;
        let original = resolved.fingerprint.clone();
        let step = service.start_check(&run, &check, &original)?;
        service.finish_check(
            &step,
            true,
            "passed",
            Some(0),
            Some(CheckCounts {
                passed: 1,
                failed: 0,
                ignored: 0,
            }),
        )?;
        let tree = "a".repeat(40);
        service.record_check_tree(&step, &tree)?;
        assert!(service.reusable_check(&check, &tree, &original)?.is_some());
        drop(resolved);
        service.finish_run(&run, "succeeded", None, "finished", None)?;
        let staged = install.join("bin/updated-rg");
        fs::write(&staged, b"#!/bin/sh\necho 'rg 14.2'\n")?;
        fs::set_permissions(&staged, fs::Permissions::from_mode(0o755))?;
        fs::rename(staged, install.join("bin/rg"))?;
        assert!(service.resolve_tools(&paths).is_err());
        service.refresh_external_tool(&paths, id, 1, "14.2", &[])?;
        let changed = service.resolve_tools(&paths)?;
        assert_ne!(original, changed.fingerprint);
        assert!(
            service
                .reusable_check(&check, &tree, &changed.fingerprint)?
                .is_none()
        );
        Ok(())
    }

    #[test]
    fn instance_shares_immutable_generations_across_projects_and_scratch_cleanup() -> Result<()> {
        let sandbox = Scratch::new(&std::env::temp_dir().join("ktask-test-scratch"))?;
        let root = sandbox.path();
        let paths = Paths {
            config: root.join("config/ktask-dev"),
            state: root.join("state/ktask-dev"),
            cache: root.join("cache/ktask-dev"),
            data: root.join("data/ktask-dev"),
            runtime: root.join("run/ktask-dev"),
            executable: std::env::current_exe()?,
        };
        let first = fake_tool(root, "14.1")?;
        let second = fake_tool(root, "15.0")?;
        let managed = fake_tool(&paths.data.join("tools"), "16.0")?;
        let mut projects = Vec::new();
        for n in 0..3 {
            let project = root.join(format!("project-{n}"));
            fs::create_dir(&project)?;
            projects.push(Service::open(
                Database::open_in(&paths.state)?,
                &project,
                None,
            )?);
        }
        let ordinary = projects[0].resolve_tools(&paths)?;
        assert!(ordinary.env.is_empty() && ordinary.writable_roots.is_empty());
        assert_eq!(ordinary.fingerprint, "unbound");
        let first_id = projects[0].register_tool(
            &paths,
            &Registration {
                name: "RG",
                version: "14.1",
                platform: std::env::consts::OS,
                architecture: std::env::consts::ARCH,
                components: &[],
                path: &first,
                managed: false,
            },
        )?;
        let second_id = projects[0].register_tool(
            &paths,
            &Registration {
                name: "rg",
                version: "15.0",
                platform: std::env::consts::OS,
                architecture: std::env::consts::ARCH,
                components: &[],
                path: &second,
                managed: false,
            },
        )?;
        let managed_id = projects[0].register_tool(
            &paths,
            &Registration {
                name: "rg",
                version: "16.0",
                platform: std::env::consts::OS,
                architecture: std::env::consts::ARCH,
                components: &[],
                path: &managed,
                managed: true,
            },
        )?;
        assert!(
            projects[0]
                .tools()?
                .iter()
                .any(|tool| tool.id == managed_id && tool.provenance == "managed")
        );
        projects[0].bind_tool("rg", first_id, Some(0))?;
        projects[1].bind_tool("rg", first_id, Some(0))?;
        let candidate = projects[2]
            .tools()?
            .into_iter()
            .find(|tool| tool.id == first_id)
            .expect("shared candidate");
        let probing = projects[2].resolve_tools_with_overlay(&paths, Some(&candidate))?;
        assert!(probing.installations.contains(&first));
        assert!(
            projects[0]
                .refresh_external_tool(&paths, first_id, 1, "14.2", &[])
                .is_err(),
            "a candidate probe in another project retains its generation lease"
        );
        drop(probing);
        projects[2].bind_tool("rg", second_id, Some(0))?;
        let a = projects[0].resolve_tools(&paths)?;
        let b = projects[1].resolve_tools(&paths)?;
        let c = projects[2].resolve_tools(&paths)?;
        assert_eq!(a.fingerprint, b.fingerprint);
        assert_ne!(a.fingerprint, c.fingerprint);
        assert!(a.env.iter().any(|(key, value)| key == "PATH"
            && value.starts_with(&first.join("bin").to_string_lossy().to_string())));
        let disposable = Scratch::new(&paths.cache.join("workers"))?;
        fs::write(disposable.path().join("build-output"), b"temporary")?;
        disposable.close()?;
        assert!(first.join("bin/rg").exists());
        assert!(managed.join("bin/rg").exists());
        assert!(
            projects[0]
                .refresh_external_tool(&paths, first_id, 1, "14.2", &[])
                .is_err(),
            "live leases prevent refresh"
        );
        drop((a, b, c));
        let staged = first.join("bin/new-rg");
        fs::write(&staged, b"#!/bin/sh\necho 'rg 14.2'\n")?;
        fs::set_permissions(&staged, fs::Permissions::from_mode(0o755))?;
        fs::rename(staged, first.join("bin/rg"))?;
        assert!(
            !projects[0]
                .tools()?
                .iter()
                .find(|t| t.id == first_id)
                .expect("registered")
                .ready
        );
        assert!(projects[0].resolve_tools(&paths).is_err());
        projects[0].refresh_external_tool(&paths, first_id, 1, "14.2", &[])?;
        let changed = projects[0].resolve_tools(&paths)?;
        assert_ne!(
            changed.fingerprint,
            projects[2].resolve_tools(&paths)?.fingerprint
        );
        drop(changed);
        projects[0].unbind_tool("rg", 1)?;
        let unbound = projects[0].resolve_tools(&paths)?;
        assert!(unbound.env.is_empty());
        assert_eq!(unbound.fingerprint, "unbound:2");
        projects[0].bind_tool("rg", first_id, Some(0))?;
        assert_ne!(projects[0].resolve_tools(&paths)?.fingerprint, "unbound:2");
        let other = root.join("state/ktask");
        let production = Service::open(Database::open_in(&other)?, &root.join("project-0"), None)?;
        assert!(
            production.tools()?.is_empty(),
            "instance registry stays isolated"
        );
        Ok(())
    }
}
