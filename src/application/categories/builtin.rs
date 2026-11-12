//! The built-in categories. They are the defaults every project starts with; a user or a
//! project overrides one by saving a category of the same name.
use super::{Category, Scope, Templates};

pub const CODE: &str = "code";
pub const DOCUMENTS: &str = "documents";
pub const ART: &str = "art";

const TASK_BLOCK: &str =
    "\nTask: {{title}}\nGoal: {{goal}}\nCriteria:\n{{criteria}}\nContext:\n{{context}}";

const CODE_EXECUTION: &str = "Own this high-level task end to end: inspect the repository, infer the necessary implementation details, choose a sound design and implement it. Keep planning brief and inside this execution; no separate specification or breakdown is required. Make routine engineering decisions autonomously, stay within the goal and constraints, and report consequential assumptions with evidence. Preserve unrelated work.";
const CODE_REVIEW: &str = "Independently review the change as a senior engineer holding the Clean Code bar: intention-revealing names; small functions that do one thing at one level of abstraction; no duplication of logic the project already has; no dead or commented-out code; comments only where the code cannot say it, never narration or noise; error handling that does not obscure the logic; no magic values; consistency with the project's existing structure and conventions; tests that are fast, independent, repeatable and readable, with none removed, weakened or ignored; no scope creep beyond the task. The change must leave the code at least as clean as it found it. Before answering, review your own findings: for each one, state its concrete impact. Mark a finding critical only when it harms correctness, behavior, safety or data, performance, or long-term maintenance, for example a test that cannot fail or does not exercise what it claims, logic duplicated instead of reused, structure that makes the next change harder, or a criterion without proof. Each critical finding must identify file:line, a reachable trigger or demonstrated maintenance cost, the consequence, and a focused correction. Review the full diff once and report all substantiated blockers together. Diagnostic changes that a repair plan authorizes are judged on their merits, not rejected merely for touching another file.";
const CODE_VALIDATION: &str = "Independently validate that the objective was achieved as written, not a narrower version of it, and that the evidence is complete: every configured supervisor check either ran and exited 0 or has a recorded passing result reused for the same verified tree and check definition (see the supervisor check evidence, source step and transcripts in Context), no test was skipped, ignored, removed or weakened compared with the baseline, and every acceptance criterion has concrete proof in the transcripts, rendered screens or repository. Compare current_counts with previous_checkpoint_counts for each check: fewer passed tests, more ignored tests, or missing current counts after a counted checkpoint require repair. Do not trust executor claims; fail unmet criteria or missing evidence, naming exactly what is missing. Report anything that does not affect a criterion or the evidence as a minor finding: it never costs a fix round.";
const CODE_REMEDIATION: &str = "Remediate the reported problems of this code task. Hold the Clean Code bar. Fix each problem at its cause with the smallest sound change, without narrowing the goal, and preserve useful work. Never skip, weaken or delete a test to make a check pass. Resolve implementation details yourself.";

const DOCS_EXECUTION: &str = "This is a docs task: the deliverable is documents, plans or other non-code files. Verify facts by reading the code and existing docs, and leave the repository consistent: working links and references, tables of contents and indexes up to date. Independent reviewers judge the written result against the goal and criteria for accuracy, completeness, consistency and clarity. Own the task end to end: inspect the repository, infer the necessary details, and preserve unrelated work.";
const DOCS_REVIEW: &str = "This is a docs task. Independently review the change as a senior technical editor, judging the written result against the task's goal and criteria: accuracy against the code and the existing docs; completeness; consistency with the code, terminology and existing docs; clarity and concision, with no filler, narration or duplicated explanation. A checked-in change must leave the repository consistent: working links and references, an up-to-date table of contents or index, and no stale mentions of what changed. Verify claims by reading the code and docs. Before answering, review your own findings: for each one, state its concrete impact. Mark a finding critical only when the document is wrong or misleading, omits something a criterion requires, contradicts the code or existing docs, leaves a broken link or reference, or a criterion lacks proof. Each critical finding must identify file:line, the consequence, and a focused correction. Review the full change once and report all substantiated blockers together.";
const DOCS_VALIDATION: &str = "This is a docs task. Independently validate that the objective was achieved as written, not a narrower version of it: every acceptance criterion has concrete proof in the written result and the repository, the content matches the code and existing docs, and a checked-in change leaves links, references and tables of contents consistent. Do not require check transcripts or test counts. Do not trust executor claims; fail unmet criteria or missing evidence, naming exactly what is missing. Report anything that does not affect a criterion as a minor finding: it never costs a fix round.";
const DOCS_REMEDIATION: &str = "This is a docs task: hold the accuracy, completeness, consistency and clarity bar for the written result, and require links, references and tables of contents to stay consistent. Fix each problem at its cause with the smallest sound edit, without narrowing the goal, and preserve useful work.";

const ART_EXECUTION: &str = "This is an art task: the deliverable is visual or audio assets, such as images, icons, illustrations, sprites, textures or sound, or the source files and scripts that produce them. ktask supplies no image-generation or image-editing tool, and this task does not either: work only with tools and assets that already exist in the environment (editors, scripts, converters, renderers, files in the repository). When the brief needs something you cannot produce with them, deliver what you can verify (for example SVG or a generator script) and say in the summary exactly what is missing; never claim to have generated, opened or seen an image you did not. Follow the format, dimensions, palette, naming and location the criteria name, keep sources next to the exports, and record the origin and license of anything reused. Own the task end to end and preserve unrelated assets.";
const ART_REVIEW: &str = "This is an art task. Independently review the delivered assets as an art director, against the goal and criteria: open the actual deliverable files and any rendered or recorded images in the evidence at useful resolution, never judge from file names or the executor's description. Check each criterion's subject, style, composition, palette, dimensions, format, transparency, naming and location; that files open and are valid; consistency with the existing assets and style guide; and that anything reused has a recorded origin and license. A placeholder presented as final is a defect. When a criterion needs a visual check and no image could be opened, fail it as missing visual evidence rather than guessing. Do not ask for code tests or builds. Before answering, review your own findings: for each one, state its concrete impact. Mark a finding critical only when a criterion is unmet, a deliverable is missing, unreadable or invalid, or the work misrepresents what was produced. Each critical finding must identify the file or screen, the consequence, and a focused correction. Review all deliverables once and report all substantiated blockers together.";
const ART_VALIDATION: &str = "This is an art task. Independently validate that the objective was achieved as written from the actual deliverable files and the visual evidence available to you (images and rendered screens listed in Context, opened at useful resolution), not from descriptions. Every criterion needs concrete proof: an opened file, a measured property such as format or dimensions, or a recorded rendering. Where visual evidence is not available to you, fail the criterion and name exactly what is missing instead of guessing. Do not require code checks or test counts, and do not require any tool capability ktask does not provide, including image generation. Report anything that does not affect a criterion as a minor finding: it never costs a fix round.";
const ART_REMEDIATION: &str = "This is an art task: judge the deliverable against the brief, not against code quality. Fix each problem in the assets at its cause with the smallest sound change, using only tools that exist in the environment and never claiming a capability ktask does not supply, such as image generation. Say what could not be fixed. Without narrowing the goal, preserve useful and approved assets.";

fn templates(execution: &str, review: &str, validation: &str, remediation: &str) -> Templates {
    let with_task = |text: &str| format!("{text}{TASK_BLOCK}");
    Templates {
        execution: with_task(execution),
        review: with_task(review),
        validation: with_task(validation),
        remediation: with_task(remediation),
    }
}

/// The built-in categories, in the order lists show them.
pub fn all() -> Vec<Category> {
    let built = |name: &str, description: &str, checks: bool, templates: Templates| Category {
        name: name.into(),
        description: description.into(),
        checks,
        templates,
        scope: Scope::BuiltIn,
    };
    vec![
        built(
            CODE,
            "Source code: project checks gate it and the diff is held to the Clean Code bar.",
            true,
            templates(
                CODE_EXECUTION,
                CODE_REVIEW,
                CODE_VALIDATION,
                CODE_REMEDIATION,
            ),
        ),
        built(
            DOCUMENTS,
            "Documents, plans and other non-code files: no project checks.",
            false,
            templates(
                DOCS_EXECUTION,
                DOCS_REVIEW,
                DOCS_VALIDATION,
                DOCS_REMEDIATION,
            ),
        ),
        built(
            ART,
            "Images, icons, sound and other assets: no project checks; judged on the deliverables.",
            false,
            templates(ART_EXECUTION, ART_REVIEW, ART_VALIDATION, ART_REMEDIATION),
        ),
    ]
}

pub fn named(name: &str) -> Option<Category> {
    all()
        .into_iter()
        .find(|c| c.name.eq_ignore_ascii_case(name))
}
