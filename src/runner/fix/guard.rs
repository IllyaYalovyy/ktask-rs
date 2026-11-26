//! The `git` an agent fix finds first on its PATH: it refuses what would discard work or
//! rewrite history, records the refusal, and otherwise is Git itself. The supervisor also
//! checks refs and stashes afterwards, so another Git binary cannot lose anything silently.
use crate::runner::callback;
use std::{os::unix::process::CommandExt, process::Command, process::ExitCode};

#[derive(Debug, PartialEq, Eq)]
enum Verdict {
    Run,
    /// Never allowed in a fix: left for the owner.
    Refused(&'static str),
    /// Allowed with a message naming the fix.
    Unnamed,
}

const DISCARDS: &str = "it discards uncommitted or stashed work";
const REWRITES: &str = "it rewrites or removes history";
const FORCES: &str = "it force-pushes or deletes on the remote";

/// Options of a command, up to `--`; a short cluster counts for each of its letters.
fn has(rest: &[String], long: &[&str], short: &[char]) -> bool {
    rest.iter().take_while(|arg| *arg != "--").any(|arg| {
        if let Some(name) = arg.strip_prefix("--") {
            let name = name.split('=').next().unwrap_or(name);
            long.contains(&name)
        } else {
            arg.strip_prefix('-')
                .is_some_and(|letters| letters.chars().any(|c| short.contains(&c)))
        }
    })
}

fn message(rest: &[String]) -> String {
    let mut message = String::new();
    let mut args = rest.iter();
    while let Some(arg) = args.next() {
        let value = if arg == "--message"
            || (arg.starts_with('-') && !arg.starts_with("--") && arg.ends_with('m'))
        {
            args.next().map(String::as_str)
        } else {
            arg.strip_prefix("--message=")
                .or_else(|| arg.strip_prefix("-m"))
        };
        message.push_str(value.unwrap_or_default());
        message.push('\n');
    }
    message.to_lowercase()
}

fn verdict(args: &[String], fix: i64) -> Verdict {
    use Verdict::{Refused, Run, Unnamed};
    let mut args = args.iter();
    let command = loop {
        let Some(arg) = args.next() else {
            return Run;
        };
        if matches!(
            arg.as_str(),
            "-C" | "-c" | "--git-dir" | "--work-tree" | "--namespace"
        ) {
            args.next();
        } else if !arg.starts_with('-') {
            break arg.as_str();
        }
    };
    let rest: Vec<String> = args.cloned().collect();
    let any = |words: &[&str]| rest.iter().any(|arg| words.contains(&arg.as_str()));
    let named = || {
        let wanted = format!("agent fix #{fix}");
        if message(&rest).contains(&wanted) {
            Run
        } else {
            Unnamed
        }
    };
    let refuse_if = |forbidden: bool, why: &'static str| if forbidden { Refused(why) } else { Run };
    match command {
        "reset" | "clean" => Refused(DISCARDS),
        "rebase" | "filter-branch" | "filter-repo" | "replace" | "update-ref" | "prune" => {
            Refused(REWRITES)
        }
        "restore" => refuse_if(
            !has(&rest, &["staged"], &['S']) || has(&rest, &["worktree"], &['W']),
            DISCARDS,
        ),
        "checkout" | "switch" => refuse_if(
            has(&rest, &["force", "discard-changes"], &['f', 'B', 'C']) || any(&["--", "."]),
            DISCARDS,
        ),
        "commit" if has(&rest, &["amend"], &[]) => Refused(REWRITES),
        "commit" => named(),
        "push" => refuse_if(
            has(
                &rest,
                &[
                    "force",
                    "force-with-lease",
                    "force-if-includes",
                    "mirror",
                    "delete",
                    "prune",
                ],
                &['f', 'd'],
            ) || rest
                .iter()
                .any(|arg| arg.starts_with('+') || arg.starts_with(':')),
            FORCES,
        ),
        "stash" => match rest.iter().find(|arg| !arg.starts_with('-')) {
            Some(action) if matches!(action.as_str(), "drop" | "clear") => Refused(DISCARDS),
            Some(action)
                if matches!(
                    action.as_str(),
                    "list" | "show" | "pop" | "apply" | "branch" | "create" | "store"
                ) && rest.first() == Some(action) =>
            {
                Run
            }
            _ => named(),
        },
        "branch" => refuse_if(has(&rest, &["force"], &['D', 'M', 'C', 'f']), REWRITES),
        "tag" => refuse_if(has(&rest, &["delete", "force"], &['d', 'f']), REWRITES),
        "reflog" => refuse_if(any(&["expire", "delete"]), REWRITES),
        "gc" => refuse_if(has(&rest, &["prune"], &[]), REWRITES),
        "rm" => refuse_if(has(&rest, &["force"], &['f']), DISCARDS),
        "worktree" => refuse_if(any(&["remove"]) && has(&rest, &["force"], &['f']), DISCARDS),
        "merge" | "cherry-pick" | "revert" | "am" => {
            refuse_if(has(&rest, &["abort"], &[]), DISCARDS)
        }
        "pull" => refuse_if(has(&rest, &["rebase", "force"], &['r', 'f']), REWRITES),
        "read-tree" | "checkout-index" => {
            refuse_if(has(&rest, &["reset", "force"], &['f']), DISCARDS)
        }
        _ => Run,
    }
}

/// The `__fix_git REAL_GIT ARGS...` process.
pub(crate) fn main() -> ExitCode {
    let mut arguments = std::env::args().skip(2);
    let Some(git) = arguments.next() else {
        return ExitCode::FAILURE;
    };
    let args: Vec<String> = arguments.collect();
    let fix = std::env::var("KTASK_FIX_ID")
        .ok()
        .and_then(|id| id.parse().ok())
        .unwrap_or(0);
    match verdict(&args, fix) {
        Verdict::Run => {
            let error = Command::new(git).args(&args).exec();
            note!("git: {error}");
            ExitCode::FAILURE
        }
        Verdict::Refused(why) => {
            let line = format!("git {}", args.join(" "));
            let token = std::env::var("KTASK_FIX_TOKEN").unwrap_or_default();
            let recorded =
                callback::fix_command(&token, &["refused-git".into(), line.clone(), why.into()]);
            note!("ktask: refused `{line}`: {why}. It is left for the owner.");
            if let Err(error) = recorded {
                note!("ktask: the refusal could not be recorded: {error}");
            }
            ExitCode::FAILURE
        }
        Verdict::Unnamed => {
            note!(
                "ktask: name the fix in the message, for example -m 'ktask: agent fix #{fix}: ...'"
            );
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn judged(line: &str) -> Verdict {
        let args: Vec<String> = line.split(' ').map(str::to_owned).collect();
        verdict(&args, 4)
    }

    #[test]
    fn nothing_discards_work_or_rewrites_history() {
        for line in [
            "reset --hard HEAD",
            "-C /tmp reset",
            "-c a=b clean -fdx",
            "restore file",
            "checkout -- file",
            "checkout -f main",
            "switch --discard-changes main",
            "rebase main",
            "commit --amend -m agent",
            "push --force origin main",
            "push -uf origin main",
            "push origin +main",
            "push origin :old",
            "push --force-with-lease",
            "stash drop",
            "stash clear",
            "branch -D old",
            "tag -d v1",
            "reflog expire --all",
            "rm -f file",
            "merge --abort",
            "pull --rebase",
        ] {
            assert!(matches!(judged(line), Verdict::Refused(_)), "{line}");
        }
        for line in [
            "status",
            "log --oneline",
            "add file",
            "restore --staged file",
            "checkout main",
            "push origin main",
            "stash pop",
            "stash list",
            "branch new",
            "rm --cached file",
            "--version",
        ] {
            assert_eq!(judged(line), Verdict::Run, "{line}");
        }
    }

    #[test]
    fn commits_and_stashes_name_the_fix() {
        for line in ["commit -m wip", "commit", "stash", "stash push -u"] {
            assert_eq!(judged(line), Verdict::Unnamed, "{line}");
        }
        let args = |items: &[&str]| -> Vec<String> {
            items.iter().map(|item| (*item).to_owned()).collect()
        };
        for items in [
            &["commit", "-m", "ktask: agent fix #4: keep the notes"][..],
            &["commit", "-am", "agent fix #4"],
            &["commit", "--message=Agent fix #4: x"],
            &["stash", "push", "-u", "-m", "ktask: agent fix #4: aside"],
        ] {
            assert_eq!(verdict(&args(items), 4), Verdict::Run, "{items:?}");
        }
    }
}
