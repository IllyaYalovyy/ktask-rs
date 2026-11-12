//! Prompt templates: `{{name}}` placeholders over task fields and runtime context.
//!
//! Rendering is a single pass over the template. Values are inserted as they are and never
//! scanned again, so a goal that mentions `{{context}}` reaches the model as typed. `\{{`
//! writes a literal `{{`; a single brace is always literal. Placeholders that name nothing
//! are rejected when a category is saved, never at run time.
/// A template problem, worded for the person who wrote the template.
pub type Result<T> = std::result::Result<T, String>;

/// One documented placeholder.
pub struct Placeholder {
    pub name: &'static str,
    /// Filled by the supervisor when a turn starts; a preview shows a labelled stand-in.
    pub runtime: bool,
    pub description: &'static str,
}

pub const PLACEHOLDERS: [Placeholder; 5] = [
    Placeholder {
        name: "title",
        runtime: false,
        description: "the task title",
    },
    Placeholder {
        name: "goal",
        runtime: false,
        description: "the task goal, exactly as written",
    },
    Placeholder {
        name: "criteria",
        runtime: false,
        description: "the acceptance criteria, numbered from 1",
    },
    Placeholder {
        name: "category",
        runtime: false,
        description: "the name of the work category",
    },
    Placeholder {
        name: "context",
        runtime: true,
        description: "supervisor evidence for this turn: review scope, check results, earlier findings, repair plan, answers",
    },
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Var {
    Title,
    Goal,
    Criteria,
    Category,
    Context,
}

impl Var {
    fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "title" => Self::Title,
            "goal" => Self::Goal,
            "criteria" => Self::Criteria,
            "category" => Self::Category,
            "context" => Self::Context,
            _ => return None,
        })
    }
}

enum Token {
    Text(String),
    Var(Var),
}

/// The names a template may use, for messages.
pub fn names() -> String {
    PLACEHOLDERS
        .iter()
        .map(|p| format!("{{{{{}}}}}", p.name))
        .collect::<Vec<_>>()
        .join(", ")
}

fn parse(template: &str) -> Result<Vec<Token>> {
    let mut tokens = Vec::new();
    let mut text = String::new();
    let mut rest = template;
    while let Some(at) = rest.find("{{") {
        let (before, after) = rest.split_at(at);
        if let Some(escaped) = before.strip_suffix('\\') {
            text.push_str(escaped);
            text.push_str("{{");
            rest = &after[2..];
            continue;
        }
        text.push_str(before);
        let inner_and_rest = &after[2..];
        let end = inner_and_rest.find("}}").ok_or_else(|| {
            format!(
                "Unterminated placeholder near \"{}\"; close it with }}}} or write \\{{{{ for a literal",
                excerpt(after)
            )
        })?;
        let name = inner_and_rest[..end].trim();
        let var = Var::parse(name).ok_or_else(|| {
            format!(
                "Unknown placeholder {{{{{name}}}}}; valid placeholders are {}",
                names()
            )
        })?;
        if !text.is_empty() {
            tokens.push(Token::Text(std::mem::take(&mut text)));
        }
        tokens.push(Token::Var(var));
        rest = &inner_and_rest[end + 2..];
    }
    text.push_str(rest);
    if !text.is_empty() {
        tokens.push(Token::Text(text));
    }
    Ok(tokens)
}

fn excerpt(text: &str) -> String {
    text.chars().take(24).collect::<String>().replace('\n', " ")
}

/// Checks that every placeholder of `template` is documented and closed.
pub fn validate(template: &str) -> Result<()> {
    parse(template).map(drop)
}

/// The values a template receives.
pub struct Values<'a> {
    pub title: &'a str,
    pub goal: &'a str,
    /// Already numbered.
    pub criteria: &'a str,
    pub category: &'a str,
    pub context: &'a str,
}

/// Numbered acceptance criteria, one per line.
pub fn numbered(criteria: &[String]) -> String {
    criteria
        .iter()
        .enumerate()
        .map(|(i, c)| format!("{}. {c}", i + 1))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The template with its placeholders filled. Task fields and context are always delivered:
/// one the template leaves out is appended after it, context last.
pub fn render(template: &str, values: &Values<'_>) -> Result<String> {
    let tokens = parse(template)?;
    let mut used = [false; 5];
    let mut out = String::new();
    for token in &tokens {
        match token {
            Token::Text(text) => out.push_str(text),
            Token::Var(var) => {
                let (index, value) = match var {
                    Var::Title => (0, values.title),
                    Var::Goal => (1, values.goal),
                    Var::Criteria => (2, values.criteria),
                    Var::Category => (3, values.category),
                    Var::Context => (4, values.context),
                };
                used[index] = true;
                out.push_str(value);
            }
        }
    }
    let missing = [
        (0, "Task: ", values.title),
        (1, "Goal: ", values.goal),
        (2, "Criteria:\n", values.criteria),
        (4, "Context:\n", values.context),
    ];
    for (index, label, value) in missing {
        if !used[index] {
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str(label);
            out.push_str(value);
            out.push('\n');
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn values<'a>() -> Values<'a> {
        Values {
            title: "T",
            goal: "G {{context}} {{nope}}",
            criteria: "1. A\n2. B",
            category: "art",
            context: "CTX",
        }
    }

    #[test]
    fn placeholders_fill_in_one_pass_and_keep_task_text_literal() {
        let text = render(
            "{{ title }}|{{goal}}|{{criteria}}|{{category}}|{{context}}",
            &values(),
        )
        .unwrap();
        assert_eq!(text, "T|G {{context}} {{nope}}|1. A\n2. B|art|CTX");
    }

    #[test]
    fn missing_task_fields_and_context_are_appended_in_order() {
        let text = render("Do the work.", &values()).unwrap();
        assert_eq!(
            text,
            "Do the work.\nTask: T\nGoal: G {{context}} {{nope}}\nCriteria:\n1. A\n2. B\nContext:\nCTX\n"
        );
        let partial = render("{{goal}} {{context}}", &values()).unwrap();
        assert_eq!(
            partial,
            "G {{context}} {{nope}} CTX\nTask: T\nCriteria:\n1. A\n2. B\n"
        );
    }

    #[test]
    fn escaped_and_single_braces_are_literal() {
        let text = render("\\{{title}} {x} }} \\n {{title}}", &values()).unwrap();
        assert!(text.starts_with("{{title}} {x} }} \\n T"), "{text}");
    }

    #[test]
    fn invalid_placeholders_are_reported_with_the_valid_names() {
        for bad in ["{{titel}}", "{{ }}", "{{title", "ok {{a b}}"] {
            let error = validate(bad).unwrap_err();
            assert!(
                error.contains("placeholder") && error.contains("{{criteria}}")
                    || error.contains("Unterminated"),
                "{bad}: {error}"
            );
        }
        assert!(validate("{{title}} {{goal}} {{criteria}} {{category}} {{context}}").is_ok());
        assert!(validate("no placeholders {at all}").is_ok());
    }
}
