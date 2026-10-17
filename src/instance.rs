use std::{env, io, io::Write, path::PathBuf};

/// Fixed by the executable's entry point, never by cwd, filename, profile, or environment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Instance {
    Development,
    Production,
}

impl Instance {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Development => "ktask-dev",
            Self::Production => "ktask",
        }
    }

    pub fn paths(self) -> io::Result<Paths> {
        let home = || {
            absolute_env("HOME")?
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "HOME is not set"))
        };
        let base = |variable, fallback: &str| -> io::Result<PathBuf> {
            absolute_env(variable)?.map_or_else(|| Ok(home()?.join(fallback)), Ok)
        };
        let state = base("XDG_STATE_HOME", ".local/state")?.join(self.name());
        Ok(Paths {
            config: base("XDG_CONFIG_HOME", ".config")?.join(self.name()),
            cache: base("XDG_CACHE_HOME", ".cache")?.join(self.name()),
            runtime: absolute_env("XDG_RUNTIME_DIR")?
                .map_or_else(|| state.join("run"), |dir| dir.join(self.name())),
            state,
            executable: env::current_exe()?,
        })
    }
}

#[derive(Debug)]
pub struct Paths {
    pub config: PathBuf,
    pub state: PathBuf,
    pub cache: PathBuf,
    pub runtime: PathBuf,
    pub executable: PathBuf,
}

impl Paths {
    pub fn print(&self, instance: Instance) -> io::Result<()> {
        let mut out = io::stdout().lock();
        writeln!(out, "instance\t{}", instance.name())?;
        for (label, path) in [
            ("executable", &self.executable),
            ("config", &self.config),
            ("state", &self.state),
            ("cache", &self.cache),
            ("runtime", &self.runtime),
        ] {
            writeln!(out, "{label}\t{}", path.display())?;
        }
        Ok(())
    }
}

fn absolute_env(variable: &str) -> io::Result<Option<PathBuf>> {
    match env::var_os(variable).filter(|value| !value.is_empty()) {
        Some(value) => {
            let path = PathBuf::from(value);
            if !path.is_absolute() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("{variable} must be an absolute path"),
                ));
            }
            Ok(Some(path))
        }
        None => Ok(None),
    }
}
