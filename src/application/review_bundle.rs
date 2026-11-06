//! A navigation packet, not a transcript dump. Full evidence stays in the archive.
use super::*;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs::File,
    io::{BufWriter, Write},
    path::Path,
};

const PREVIEW_BYTES: usize = 12 * 1024;

fn record(out: &mut impl Write, value: &Value) -> Result<()> {
    serde_json::to_writer(&mut *out, value).map_err(|e| Error::Conflict(e.to_string()))?;
    out.write_all(b"\n")?;
    Ok(())
}

impl Service {
    /// Materialize a read-only snapshot for one model turn. No log payload is read here.
    /// Every index row links to complete evidence; missing and historical records stay explicit.
    pub fn review_bundle(
        &self,
        attempt: i64,
        archive: &Path,
        folder: &Path,
        tree: &str,
        context: &str,
    ) -> Result<String> {
        let db = self.db.connection();
        let (task, revision): (i64, i64) = db.query_row(
            "SELECT task_id,revision FROM attempts WHERE id=?1 AND project_id=?2",
            params![attempt, self.project_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        std::fs::create_dir_all(folder)?;
        std::fs::write(folder.join("workflow.txt"), context)?;
        let mut artifacts = BufWriter::new(File::create(folder.join("artifacts.jsonl"))?);
        let mut statement = db.prepare("SELECT ar.id,ar.attempt_id,ar.name,ar.media_type,ar.size_bytes,ar.checksum_sha256,ar.source_tree,ar.relative_path,ar.pruned_at_ms FROM artifacts ar JOIN attempts a ON a.id=ar.attempt_id WHERE a.project_id=?1 AND a.task_id=?2 ORDER BY ar.id DESC")?;
        let mut artifact_count = 0;
        let mut matching = 0;
        for row in statement.query_map(params![self.project_id, task], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, Option<String>>(5)?,
                r.get::<_, Option<String>>(6)?,
                r.get::<_, String>(7)?,
                r.get::<_, Option<i64>>(8)?,
            ))
        })? {
            let (id, origin, name, media, bytes, sha, source, relative, pruned) = row?;
            let path = if pruned.is_none() {
                super::queries::retained_artifact_path(archive, &relative)
            } else {
                None
            };
            let same_tree = source.as_deref() == Some(tree);
            artifact_count += 1;
            matching += usize::from(same_tree && path.is_some());
            record(
                &mut artifacts,
                &json!({"id":id,"attempt":origin,"name":name,"media_type":media,
                "bytes":bytes,"sha256":sha,"source_tree":source,"matches_current_tree":same_tree,
                "availability":if pruned.is_some(){"pruned"}else if path.is_some(){"available"}else{"missing"},"path":path}),
            )?;
        }
        artifacts.flush()?;
        let mut steps = BufWriter::new(File::create(folder.join("steps.jsonl"))?);
        let mut statement = db.prepare("SELECT s.id,s.attempt_id,s.role,s.state,s.check_name,s.command,s.exit_code,s.source_tree,s.reused_from_step_id,s.report,s.passed_count,s.failed_count,s.ignored_count FROM attempt_steps s JOIN attempts a ON a.id=s.attempt_id WHERE a.project_id=?1 AND a.task_id=?2 ORDER BY s.id")?;
        let mut latest = BTreeMap::new();
        let mut step_count = 0;
        for row in statement.query_map(params![self.project_id, task], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, Option<String>>(5)?,
                r.get::<_, Option<i64>>(6)?,
                r.get::<_, Option<String>>(7)?,
                r.get::<_, Option<i64>>(8)?,
                r.get::<_, Option<String>>(9)?,
                r.get::<_, Option<i64>>(10)?,
                r.get::<_, Option<i64>>(11)?,
                r.get::<_, Option<i64>>(12)?,
            ))
        })? {
            let (
                id,
                origin,
                role,
                state,
                name,
                command,
                exit,
                source,
                reused,
                report,
                passed,
                failed,
                ignored,
            ) = row?;
            let path = folder.join(format!("step-{id}.txt"));
            if let Some(report) = &report {
                std::fs::write(&path, report)?;
            }
            let item = json!({"step":id,"attempt":origin,"role":role,"state":state,"check":name,
                "command":command,"exit_code":exit,"source_tree":source,"matches_current_tree":source.as_deref()==Some(tree),
                "reused_from_step":reused,"counts":{"passed":passed,"failed":failed,"ignored":ignored},
                "report":report.as_ref().map(|_|&path),
                "authority":if role=="execution"{"executor claim, not independent proof"}else if role=="check"{"supervisor result; counts may cover only recognized suites"}else{"independent assessment of its recorded scope; recheck after changes"}});
            record(&mut steps, &item)?;
            step_count += 1;
            if state != "running" {
                latest.insert((role, name), item);
            }
        }
        steps.flush()?;
        let mut criteria = BufWriter::new(File::create(folder.join("criteria.jsonl"))?);
        let mut statement = db.prepare("SELECT ordinal,description FROM acceptance_criteria WHERE task_id=?1 AND revision=?2 ORDER BY ordinal")?;
        let mut criterion_count = 0;
        for row in statement.query_map(params![task, revision], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        })? {
            let (ordinal, description) = row?;
            let mut claims=db.prepare("SELECT s.id,s.attempt_id,s.role,r.state,r.evidence FROM criterion_results r JOIN attempt_steps s ON s.id=r.step_id WHERE s.task_id=?1 AND s.revision=?2 AND r.criterion_ordinal=?3 ORDER BY s.id DESC")?;
            let mut latest_claims = BTreeMap::new();
            for row in claims.query_map(params![task, revision, ordinal], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, Option<String>>(4)?,
                ))
            })? {
                let (step, origin, role, state, evidence) = row?;
                if latest_claims.contains_key(&role) {
                    continue;
                }
                let path = folder.join(format!("criterion-{ordinal}-step-{step}.txt"));
                if let Some(evidence) = &evidence {
                    std::fs::write(&path, evidence)?;
                }
                latest_claims.insert(role.clone(),json!({"step":step,"attempt":origin,"role":role,"state":state,"evidence":evidence.as_ref().map(|_|path),"independently_verified":false}));
            }
            record(
                &mut criteria,
                &json!({"ordinal":ordinal,"requirement":description,"latest_assessments":latest_claims,"rule":"Verify the cited source and version; a claim or prior assessment alone cannot satisfy this criterion."}),
            )?;
            criterion_count += 1;
        }
        criteria.flush()?;
        let mut latest_file = BufWriter::new(File::create(folder.join("latest.jsonl"))?);
        let mut preview = String::new();
        let mut shown = 0;
        for item in latest.values() {
            record(&mut latest_file, item)?;
            let line = format!("{}\n", item);
            if preview.len() + line.len() <= PREVIEW_BYTES {
                preview.push_str(&line);
                shown += 1;
            }
        }
        latest_file.flush()?;
        Ok(format!(
            "Evidence packet: task #{task}, attempt #{attempt}, revision {revision}, current source tree {tree}.\nRead-only index directory: {}\n{criterion_count} criteria in criteria.jsonl; {} latest role/check records in latest.jsonl ({shown} shown below); {step_count} historical steps in steps.jsonl; {artifact_count} artifacts in artifacts.jsonl ({matching} available with matching tree).\nStart with criteria.jsonl and latest.jsonl. Inspect every criterion's proof and every unresolved critical finding; reports are complete files, not summaries. Earlier findings are hypotheses until checked against the current diff. A missing, pruned, stale or unverified source is NOT proof. Matching tree alone does not prove the check definition matches: retain the supervisor's reuse guards.\nFull review scope, owner answers and authorized repair scope: workflow.txt. Search that file for 'Review scope', 'Recorded answers', and 'Repair plan'; do not replay it wholesale. The task criteria and mandatory review bar are unchanged.\nUse the supervisor's evidence command to page/search these files; open required PNG paths with your image viewer. Do not concatenate archives into context. Actual visual inspection is mandatory for visual criteria.\nLatest results (not a substitute for source inspection):\n{preview}",
            folder.display(),
            latest.len()
        ))
    }
}
