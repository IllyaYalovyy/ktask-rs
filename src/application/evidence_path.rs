//! The one parser of evidence paths. Publication, accounting, deletion and the empty-directory
//! sweep all resolve `artifacts.relative_path` through it, component by component.

/// `evidence/attempt-<id>/<name>` or `evidence/attempt-<id>/<directory>/<name>`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct EvidencePath<'a> {
    pub attempt: i64,
    pub attempt_directory: &'a str,
    /// The step directory of a published file; absent for a check transcript.
    pub directory: Option<&'a str>,
    pub name: &'a str,
}

impl<'a> EvidencePath<'a> {
    /// Every component is a plain name: no empty, `.` or `..` part and no other root.
    pub fn parse(relative: &'a str) -> Option<Self> {
        let parts: Vec<_> = relative.split('/').collect();
        let (attempt_directory, directory, name) = match parts.as_slice() {
            ["evidence", attempt, name] => (*attempt, None, *name),
            ["evidence", attempt, directory, name] => (*attempt, Some(*directory), *name),
            _ => return None,
        };
        let attempt = canonical_number(attempt_directory.strip_prefix("attempt-")?)?;
        (directory.is_none_or(plain) && plain(name)).then_some(Self {
            attempt: i64::try_from(attempt).ok()?,
            attempt_directory,
            directory,
            name,
        })
    }

    /// The directories from the state root to the file's parent.
    pub fn parents(&self) -> impl Iterator<Item = &'a str> {
        ["evidence", self.attempt_directory]
            .into_iter()
            .chain(self.directory)
    }

    /// The step of `step-<n>.log` or of a file inside `step-<n>/`.
    pub fn step(&self) -> Option<u64> {
        match self.directory {
            Some(directory) => canonical_number(directory.strip_prefix("step-")?),
            None => canonical_number(self.name.strip_prefix("step-")?.strip_suffix(".log")?),
        }
    }

    /// Only names ktask writes itself: check transcripts and `file-<pid>-<index>` copies.
    /// Retention deletes nothing else.
    pub fn generated(&self) -> bool {
        self.step().is_some()
            && self.directory.is_none_or(|_| {
                self.name
                    .strip_prefix("file-")
                    .and_then(|name| name.split_once('-'))
                    .is_some_and(|(pid, index)| {
                        canonical_number(pid).is_some_and(|pid| u32::try_from(pid).is_ok())
                            && canonical_number(index).is_some()
                    })
            })
    }
}

fn plain(part: &str) -> bool {
    !matches!(part, "" | "." | "..") && !part.contains('\0')
}

/// Decimal digits exactly as ktask formats them, so a name resolves to one id.
fn canonical_number(text: &str) -> Option<u64> {
    text.parse::<u64>()
        .ok()
        .filter(|number| number.to_string() == text)
}

#[cfg(test)]
mod tests {
    use super::EvidencePath;

    #[test]
    fn parses_generated_paths_and_rejects_everything_else() {
        let transcript = EvidencePath::parse("evidence/attempt-7/step-12.log").unwrap();
        assert_eq!(
            (
                transcript.attempt,
                transcript.step(),
                transcript.generated()
            ),
            (7, Some(12), true)
        );
        let file = EvidencePath::parse("evidence/attempt-7/step-3/file-41-0").unwrap();
        assert_eq!(
            file.parents().collect::<Vec<_>>(),
            ["evidence", "attempt-7", "step-3"]
        );
        assert!(file.generated());
        let named = EvidencePath::parse("evidence/attempt-7/proof.txt").unwrap();
        assert!(named.step().is_none() && !named.generated());
        assert!(
            !EvidencePath::parse("evidence/attempt-7/step-3/empty-1")
                .unwrap()
                .generated()
        );
        for unsafe_path in [
            "unsafe/step-101.log",
            "evidence/attempt-07/step-1.log",
            "evidence/attempt-+7/step-1.log",
            "evidence/attempt-7/../step-1.log",
            "evidence/attempt-7//step-1.log",
            "evidence/attempt-7/step-1/./file-1-0",
            "/evidence/attempt-7/step-1.log",
            "evidence/attempt-7/step-1/file-1-0/extra",
            "evidence/attempt-7",
        ] {
            assert_eq!(EvidencePath::parse(unsafe_path), None, "{unsafe_path}");
        }
    }
}
