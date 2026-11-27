//! A navigation packet, not a transcript dump. Full evidence stays in the archive.
use super::*;
use rusqlite::OptionalExtension;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs::File,
    io::{BufWriter, Write},
    path::{Path, PathBuf},
};

const PREVIEW_BYTES: usize = 12 * 1024;

/// What a project's check names its report of new, changed and removed screens.
pub(crate) const SCREEN_REPORT: &str = "changed-screens.txt";

/// A project keeps an approved set of terminal screens as `tests/golden/*.screens`, and its
/// check compares every recorded screen with it.
pub(crate) fn has_golden_screens(project: &Path) -> bool {
    std::fs::read_dir(project.join("tests/golden")).is_ok_and(|entries| {
        entries
            .flatten()
            .any(|entry| entry.path().extension().is_some_and(|ext| ext == "screens"))
    })
}

/// What the checks of the current source left to say about its screens.
enum ScreenReport {
    /// The report of new, changed and removed screens, as an artifact.
    Available { id: i64, path: PathBuf },
    /// Their evidence was archived completely and holds no report.
    NothingChanged,
    /// A report cannot be read, or may be among files that were not archived.
    Unavailable,
}

/// What judges are told about the screens of a project with a golden set.
fn screen_evidence(report: &ScreenReport) -> String {
    let report = match report {
        ScreenReport::Available { id, path } => format!(
            "The changed-screens report, artifact #{id} at {}, is the only screen evidence: it holds the before and after text of every new, changed or removed screen. Page or search it with the evidence command and judge those new and changed screens against the criteria; a removed screen needs a reason in the change.",
            path.display()
        ),
        ScreenReport::NothingChanged => "No changed-screens report is archived for the current source tree, whose check evidence is complete: that is how the check says that no screen is new, changed or removed, so there is no screen to review.".into(),
        ScreenReport::Unavailable => "The changed-screens report of the current source tree is unavailable: it is missing, pruned, or the check's evidence is incomplete. New and changed screens could not be reviewed, and this missing evidence blocks acceptance.".into(),
    };
    format!(
        "Screen evidence: this project keeps an approved set of terminal screens in tests/golden, and its check compares every recorded screen with it. Screens that match the approved set were verified in code against it; do not reread them, the .screens files or their diff. {report}\n"
    )
}

fn record(out: &mut impl Write, value: &Value) -> Result<()> {
    serde_json::to_writer(&mut *out, value).map_err(|e| Error::Conflict(e.to_string()))?;
    out.write_all(b"\n")?;
    Ok(())
}

impl Service {
    /// Looks at the latest run of each check on `tree`, a reused result standing for the
    /// run it reused. Only a report one of them kept, or all their evidence collected
    /// completely without one, says anything about the screens.
    fn screen_report(&self, attempt: i64, archive: &Path, tree: &str) -> Result<ScreenReport> {
        let db = self.db.connection();
        let mut checks = db.prepare(
            "SELECT s.state,o.attempt_id,o.id FROM attempt_steps s
             JOIN attempt_steps o ON o.id=coalesce(s.reused_from_step_id,s.id)
             WHERE s.id IN (SELECT max(id) FROM attempt_steps
                 WHERE attempt_id=?1 AND role='check' AND source_tree=?2 GROUP BY check_name)
             ORDER BY s.id DESC",
        )?;
        let checks = checks
            .query_map(params![attempt, tree], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut complete = !checks.is_empty();
        for (state, origin, step) in checks {
            let prefix = format!("evidence/attempt-{origin}/step-{step}");
            let report = db
                .query_row(
                    "SELECT id,relative_path,pruned_at_ms IS NOT NULL FROM artifacts
                     WHERE attempt_id=?1 AND relative_path GLOB ?2 AND name=?3 ORDER BY id DESC",
                    params![origin, format!("{prefix}/*"), SCREEN_REPORT],
                    |r| Ok((r.get(0)?, r.get::<_, String>(1)?, r.get::<_, bool>(2)?)),
                )
                .optional()?;
            if let Some((id, relative, pruned)) = report {
                let path = (!pruned)
                    .then(|| super::queries::retained_artifact_path(archive, &relative))
                    .flatten()
                    .filter(|path| path.is_file());
                return Ok(path.map_or(ScreenReport::Unavailable, |path| {
                    ScreenReport::Available { id, path }
                }));
            }
            let collection: Option<String> = db
                .query_row(
                    "SELECT state FROM evidence_collections
                     WHERE attempt_id=?1 AND relative_prefix=?2 AND source_tree=?3 ORDER BY id DESC",
                    params![origin, prefix, tree],
                    |r| r.get(0),
                )
                .optional()?;
            complete &= state == "succeeded" && collection.as_deref() == Some("complete");
        }
        Ok(if complete {
            ScreenReport::NothingChanged
        } else {
            ScreenReport::Unavailable
        })
    }

    /// Materialize a read-only snapshot for one model turn. No log payload is read here.
    /// Every index row links to complete evidence; missing and historical records stay explicit.
    pub fn review_bundle(
        &self,
        attempt: i64,
        archive: &Path,
        folder: &Path,
        tree: &str,
        context: &str,
        golden_screens: bool,
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
        let screens = if golden_screens {
            screen_evidence(&self.screen_report(attempt, archive, tree)?)
        } else {
            String::new()
        };
        // Files a truncated collection did not archive are listed, never silently absent.
        let mut manifest = BufWriter::new(File::create(folder.join("manifest.jsonl"))?);
        let mut statement = db.prepare("SELECT c.attempt_id,m.name,m.size_bytes,m.checksum_sha256,m.omitted FROM evidence_manifest m JOIN evidence_collections c ON c.id=m.collection_id JOIN attempts a ON a.id=c.attempt_id WHERE a.project_id=?1 AND a.task_id=?2 ORDER BY c.id DESC,m.ordinal")?;
        let mut rows = statement.query(params![self.project_id, task])?;
        while let Some(row) = rows.next()? {
            let omitted: Option<String> = row.get(4)?;
            record(
                &mut manifest,
                &json!({"attempt":row.get::<_, i64>(0)?,"name":row.get::<_, String>(1)?,"bytes":row.get::<_, i64>(2)?,
                "sha256":row.get::<_, Option<String>>(3)?,"archived":omitted.is_none(),"not_archived_because":omitted}),
            )?;
        }
        manifest.flush()?;
        let truncated = super::collection::truncated(db, attempt)?
            .map_or_else(String::new, |evidence| {
                format!("{} The list is manifest.jsonl.\n", evidence.fact())
            });
        let mut steps = BufWriter::new(File::create(folder.join("steps.jsonl"))?);
        let mut statement = db.prepare("SELECT s.id,s.attempt_id,s.role,s.state,s.check_name,s.command,s.exit_code,s.source_tree,s.reused_from_step_id,s.report,s.passed_count,s.failed_count,s.ignored_count,s.environment_fingerprint FROM attempt_steps s JOIN attempts a ON a.id=s.attempt_id WHERE a.project_id=?1 AND a.task_id=?2 ORDER BY s.id")?;
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
                r.get::<_, Option<String>>(13)?,
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
                environment,
            ) = row?;
            let path = folder.join(format!("step-{id}.txt"));
            if let Some(report) = &report {
                std::fs::write(&path, report)?;
            }
            let item = json!({"step":id,"attempt":origin,"role":role,"state":state,"check":name,
                "command":command,"exit_code":exit,"source_tree":source,"matches_current_tree":source.as_deref()==Some(tree),"environment_fingerprint":environment,
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
            "Evidence packet: task #{task}, attempt #{attempt}, revision {revision}, current source tree {tree}.\nRead-only index directory: {}\n{criterion_count} criteria in criteria.jsonl; {} latest role/check records in latest.jsonl ({shown} shown below); {step_count} historical steps in steps.jsonl; {artifact_count} artifacts in artifacts.jsonl ({matching} available with matching tree).\nStart with criteria.jsonl and latest.jsonl. Inspect every criterion's proof and every unresolved critical finding; reports are complete files, not summaries. Earlier findings are hypotheses until checked against the current diff. A missing, pruned, stale or unverified source is NOT proof. Matching tree alone does not prove the check definition matches: retain the supervisor's reuse guards.\nFull review scope, owner answers and authorized repair scope: workflow.txt. Search that file for 'Review scope', 'Recorded answers', and 'Repair plan'; do not replay it wholesale. The task criteria and mandatory review bar are unchanged.\nUse the supervisor's evidence command to page/search these files; open required image paths with your image viewer. Do not concatenate archives into context. Actual inspection is mandatory for visual criteria: images in your viewer, recorded terminal screens as text.\n{screens}{truncated}Latest results (not a substitute for source inspection):\n{preview}",
            folder.display(),
            latest.len()
        ))
    }
}
