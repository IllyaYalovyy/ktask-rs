//! Versioned, escaped TSV task definitions. Not a backup of attempt/history records.
use super::*;
use std::{
    fs::{self, File},
    io::{Read, Write},
    path::Path,
};

const HEADER: &str = "ktask-tasks-v2";
/// v1 rows have no stage limit column; they still import.
const HEADER_V1: &str = "ktask-tasks-v1";
const MAX_BYTES: u64 = 4 * 1024 * 1024;

pub fn encode(definitions: &[Definition]) -> Result<String> {
    let mut lines = vec![HEADER.to_owned()];
    for d in definitions {
        d.validate()?;
        let mut fields = vec![
            d.kind.as_str().into(),
            d.title.clone(),
            d.goal.clone(),
            d.attempt_limit.map_or(String::new(), |n| n.to_string()),
            d.timeout_ms.map_or(String::new(), |n| n.to_string()),
            d.stage_timeout_ms.map_or(String::new(), |n| n.to_string()),
        ];
        for model in &d.models {
            if let Some(m) = model {
                fields.extend([
                    m.provider.clone(),
                    m.model.clone(),
                    m.effort.clone().unwrap_or_default(),
                ]);
            } else {
                fields.extend([String::new(), String::new(), String::new()]);
            }
        }
        fields.extend(d.criteria.iter().cloned());
        lines.push(
            fields
                .iter()
                .map(|s| escape(s))
                .collect::<Vec<_>>()
                .join("\t"),
        );
    }
    Ok(lines.join("\n") + "\n")
}
pub fn decode(text: &str) -> Result<Vec<Definition>> {
    let mut lines = text.lines();
    let stage = match lines.next() {
        Some(HEADER) => 1,
        Some(HEADER_V1) => 0,
        _ => {
            return Err(Error::Conflict(
                "Expected a ktask-tasks-v2 TSV export".into(),
            ));
        }
    };
    let mut result = Vec::new();
    for line in lines {
        let f = line.split('\t').map(unescape).collect::<Result<Vec<_>>>()?;
        require(f.len() >= 18 + stage, "Incomplete task definition")?;
        let kind = match f[0].as_str() {
            "ai" => Kind::Ai,
            "human" => Kind::Human,
            _ => return Err(Error::Conflict("Unknown task kind".into())),
        };
        let mut models = std::array::from_fn(|_| None);
        for (i, model) in models.iter_mut().enumerate() {
            let offset = 5 + stage + i * 3;
            if f[offset..offset + 3].iter().any(|s| !s.is_empty()) {
                *model = Some(ModelConfig {
                    provider: f[offset].clone(),
                    model: f[offset + 1].clone(),
                    effort: (!f[offset + 2].is_empty()).then(|| f[offset + 2].clone()),
                });
            }
        }
        let d = Definition {
            title: f[1].clone(),
            goal: f[2].clone(),
            kind,
            criteria: f[17 + stage..].to_vec(),
            models,
            attempt_limit: number(&f[3])?,
            timeout_ms: number(&f[4])?,
            stage_timeout_ms: if stage == 1 { number(&f[5])? } else { None },
        };
        d.validate()?;
        result.push(d);
    }
    require(!result.is_empty(), "Import contains no tasks")?;
    Ok(result)
}
fn number(s: &str) -> Result<Option<i64>> {
    if s.is_empty() {
        Ok(None)
    } else {
        s.parse()
            .map(Some)
            .map_err(|_| Error::Conflict("Invalid limit".into()))
    }
}
fn escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('\t', "\\t")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}
fn unescape(s: &str) -> Result<String> {
    let mut out = String::new();
    let mut chars = s.chars();
    while let Some(ch) = chars.next() {
        out.push(if ch == '\\' {
            match chars.next() {
                Some('\\') => '\\',
                Some('t') => '\t',
                Some('n') => '\n',
                Some('r') => '\r',
                _ => return Err(Error::Conflict("Invalid TSV escape".into())),
            }
        } else {
            ch
        });
    }
    Ok(out)
}
pub fn read(path: &Path) -> Result<Vec<u8>> {
    require(
        fs::metadata(path)?.is_file(),
        "Source must be a regular file",
    )?;
    let file = File::open(path)?;
    require(file.metadata()?.is_file(), "Source must be a regular file")?;
    let mut data = Vec::new();
    file.take(MAX_BYTES + 1).read_to_end(&mut data)?;
    require(data.len() as u64 <= MAX_BYTES, "File exceeds 4 MiB limit")?;
    Ok(data)
}
pub fn import(path: &Path) -> Result<Vec<Definition>> {
    let data = read(path)?;
    let text =
        std::str::from_utf8(&data).map_err(|_| Error::Conflict("Import must be UTF-8".into()))?;
    decode(text)
}
/// Same-directory atomic replacement; no truncation, no leftover temporary file on failure.
pub fn export(path: &Path, data: &str, expected: Option<&[u8]>) -> Result<()> {
    require(
        path.extension().is_some_and(|s| s == "tsv"),
        "Export destination must end in .tsv",
    )?;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let mut temp = tempfile::NamedTempFile::new_in(parent)?;
    temp.write_all(data.as_bytes())?;
    temp.as_file().sync_all()?;
    if let Some(expected) = expected {
        let meta = fs::symlink_metadata(path)?;
        require(
            meta.is_file() && !meta.file_type().is_symlink(),
            "Refusing to overwrite a non-regular file",
        )?;
        require(
            read(path)? == expected,
            "Destination changed since confirmation; export again",
        )?;
        temp.persist(path).map_err(|e| Error::Io(e.error))?;
    } else {
        temp.persist_noclobber(path)
            .map_err(|e| Error::Io(e.error))?;
    }
    File::open(parent)?.sync_all()?;
    Ok(())
}
