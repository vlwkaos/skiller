use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::model::EffectiveMode;

const PROJECTION_POLICY: &str = "<!-- skiller:projection-policy -->\nTreat this installed skill directory as read-only. Write only to explicit project, state, cache, or catalog authoring paths defined by this skill.\n";

pub fn apply_invocation_mode(skill_root: &Path, mode: EffectiveMode) -> Result<()> {
    let skill_md = skill_root.join("SKILL.md");
    let raw = std::fs::read_to_string(&skill_md)
        .with_context(|| format!("reading {}", skill_md.display()))?;
    let (disable_model, user_invocable) = match mode {
        EffectiveMode::Enable => (None, None),
        EffectiveMode::Manual => (Some("true"), None),
        EffectiveMode::Dependency => (None, Some("false")),
    };
    let updated = set_frontmatter_field(
        &set_frontmatter_field(&raw, "disable-model-invocation", disable_model)?,
        "user-invocable",
        user_invocable,
    )?;
    std::fs::write(&skill_md, updated)
        .with_context(|| format!("writing {}", skill_md.display()))?;
    set_codex_policy(
        &skill_root.join("agents/openai.yaml"),
        mode != EffectiveMode::Manual,
    )
}

pub fn apply_projected_identity(
    skill_root: &Path,
    installed_name: &str,
    scope: Option<&str>,
    description: &str,
) -> Result<()> {
    let skill_md = skill_root.join("SKILL.md");
    let raw = std::fs::read_to_string(&skill_md)
        .with_context(|| format!("reading {}", skill_md.display()))?;
    let description = match scope {
        Some(scope) => format!("[{scope}] {description}"),
        None => description.to_owned(),
    };
    let quoted = serde_json::to_string(&description)?;
    let mut updated = set_frontmatter_field(
        &set_frontmatter_field(&raw, "name", Some(installed_name))?,
        "description",
        Some(&quoted),
    )?;
    if !updated.contains("<!-- skiller:projection-policy -->") {
        if !updated.ends_with("\n\n") {
            updated.push('\n');
        }
        updated.push_str(PROJECTION_POLICY);
    }
    std::fs::write(&skill_md, updated).with_context(|| format!("writing {}", skill_md.display()))
}

fn set_frontmatter_field(raw: &str, key: &str, value: Option<&str>) -> Result<String> {
    let lines: Vec<&str> = raw.lines().collect();
    if lines.first().copied() != Some("---") {
        bail!("SKILL.md is missing YAML frontmatter");
    }
    let closing = lines
        .iter()
        .enumerate()
        .skip(1)
        .find_map(|(index, line)| (*line == "---").then_some(index))
        .context("SKILL.md frontmatter is not closed")?;
    let matches: Vec<usize> = lines[..closing]
        .iter()
        .enumerate()
        .filter_map(|(index, line)| {
            line.split_once(':')
                .filter(|(candidate, _)| {
                    candidate.trim() == key && !candidate.starts_with(char::is_whitespace)
                })
                .map(|_| index)
        })
        .collect();
    if matches.len() > 1 {
        bail!("SKILL.md contains duplicate {key} fields");
    }
    let mut output = Vec::with_capacity(lines.len() + 1);
    let mut index = 0usize;
    while index < lines.len() {
        let line = lines[index];
        if matches.first() == Some(&index) {
            if let Some(value) = value {
                output.push(format!("{key}: {value}"));
            }
            index += 1;
            while index < closing
                && (lines[index].is_empty() || lines[index].starts_with(char::is_whitespace))
            {
                index += 1;
            }
            continue;
        }
        if index == closing
            && matches.is_empty()
            && let Some(value) = value
        {
            output.push(format!("{key}: {value}"));
        }
        output.push(line.to_owned());
        index += 1;
    }
    Ok(format!("{}\n", output.join("\n")))
}

fn set_codex_policy(path: &Path, allow_implicit_invocation: bool) -> Result<()> {
    let parent = path.parent().context("Codex sidecar has no parent")?;
    crate::paths::ensure_real_dir(parent)?;
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if allow_implicit_invocation {
                return Ok(());
            }
            return std::fs::write(path, "policy:\n  allow_implicit_invocation: false\n")
                .with_context(|| format!("writing {}", path.display()));
        }
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    let mut lines: Vec<String> = raw.lines().map(str::to_owned).collect();
    let allow_lines: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter_map(|(index, line)| {
            line.trim_start()
                .starts_with("allow_implicit_invocation:")
                .then_some(index)
        })
        .collect();
    if allow_lines.len() > 1 {
        bail!("Codex sidecar contains duplicate allow_implicit_invocation fields");
    }
    if let Some(index) = allow_lines.first() {
        let indent = lines[*index].len() - lines[*index].trim_start().len();
        lines[*index] = format!(
            "{}allow_implicit_invocation: {allow_implicit_invocation}",
            " ".repeat(indent)
        );
    } else if !allow_implicit_invocation {
        let policy_lines: Vec<usize> = lines
            .iter()
            .enumerate()
            .filter_map(|(index, line)| (line == "policy:").then_some(index))
            .collect();
        if policy_lines.len() > 1 {
            bail!("Codex sidecar contains duplicate policy mappings");
        }
        if let Some(index) = policy_lines.first() {
            lines.insert(index + 1, "  allow_implicit_invocation: false".to_owned());
        } else {
            if !lines.is_empty() && !lines.last().is_some_and(String::is_empty) {
                lines.push(String::new());
            }
            lines.extend([
                "policy:".to_owned(),
                "  allow_implicit_invocation: false".to_owned(),
            ]);
        }
    }
    std::fs::write(path, format!("{}\n", lines.join("\n")))
        .with_context(|| format!("writing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontmatter_field_is_inserted_replaced_or_removed() {
        let raw = "---\nname: old\ndescription: Test\nuser-invocable: false\n---\nBody\n";
        let updated = set_frontmatter_field(raw, "disable-model-invocation", Some("true")).unwrap();
        assert!(updated.contains("disable-model-invocation: true\n---"));
        assert!(
            set_frontmatter_field(&updated, "name", Some("new-scope"))
                .unwrap()
                .contains("name: new-scope")
        );
        assert!(
            !set_frontmatter_field(&updated, "user-invocable", None)
                .unwrap()
                .contains("user-invocable")
        );
    }

    #[test]
    fn projected_identity_keeps_name_clean_and_prefixes_scope_description() {
        let base = std::env::current_dir()
            .unwrap()
            .join("target/test-work/projected-identity");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(
            base.join("SKILL.md"),
            "---\nname: commit\ndescription: >-\n  Create safe commits\n  without losing state.\n---\n",
        )
        .unwrap();
        apply_projected_identity(&base, "commit", Some("engineering"), "Create safe commits")
            .unwrap();
        let raw = std::fs::read_to_string(base.join("SKILL.md")).unwrap();
        assert!(raw.contains("name: commit\n"));
        assert!(raw.contains("description: \"[engineering] Create safe commits\""));
        assert!(raw.contains("Treat this installed skill directory as read-only."));
        assert!(!raw.contains("without losing state"));
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn effective_modes_map_to_independent_frontmatter_capabilities() {
        let raw = "---\nname: test\ndescription: Test\ndisable-model-invocation: true\nuser-invocable: false\n---\nBody\n";
        let transform = |mode| {
            let (disable_model, user_invocable) = match mode {
                EffectiveMode::Enable => (None, None),
                EffectiveMode::Manual => (Some("true"), None),
                EffectiveMode::Dependency => (None, Some("false")),
            };
            set_frontmatter_field(
                &set_frontmatter_field(raw, "disable-model-invocation", disable_model).unwrap(),
                "user-invocable",
                user_invocable,
            )
            .unwrap()
        };
        let enabled = transform(EffectiveMode::Enable);
        assert!(!enabled.contains("disable-model-invocation"));
        assert!(!enabled.contains("user-invocable"));

        let manual = transform(EffectiveMode::Manual);
        assert!(manual.contains("disable-model-invocation: true"));
        assert!(!manual.contains("user-invocable"));

        let dependency = transform(EffectiveMode::Dependency);
        assert!(!dependency.contains("disable-model-invocation"));
        assert!(dependency.contains("user-invocable: false"));
    }
}
