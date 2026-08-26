use std::io::{self, IsTerminal, Write};

use anyhow::{Context, Result, bail};
use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::{Event, KeyCode, KeyEventKind, read};
use crossterm::execute;
use crossterm::style::{Attribute, Print, SetAttribute};
use crossterm::terminal::{
    Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode,
    enable_raw_mode, size,
};
use unicode_width::UnicodeWidthChar;

use crate::config_ui::{ConfigRow, cycle_selection, row_editable, toggle_gitignore};
use crate::model::{EffectiveMode, ProjectConfig, SelectionMode, SkillSelection};

pub(crate) enum ConfigTuiResult {
    Save,
    Cancel,
}

struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(io::stdout(), Show, LeaveAlternateScreen);
        let _ = disable_raw_mode();
    }
}

pub(crate) fn run(
    rows: &[ConfigRow],
    manifest: &mut ProjectConfig,
    global_scope: bool,
) -> Result<ConfigTuiResult> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        bail!(
            "interactive config requires a terminal; run `skiller config` directly for JSON or use --set for mutation"
        );
    }
    enable_raw_mode().context("enabling terminal raw mode")?;
    let _guard = TerminalGuard;
    execute!(io::stdout(), EnterAlternateScreen, Hide).context("opening configuration screen")?;
    let mut selected = 0usize;

    loop {
        draw(rows, manifest, global_scope, selected)?;
        let Event::Key(key) = read().context("reading terminal input")? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => selected = selected.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => {
                selected = (selected + 1).min(rows.len().saturating_sub(1));
            }
            KeyCode::PageUp => selected = selected.saturating_sub(10),
            KeyCode::PageDown => selected = (selected + 10).min(rows.len().saturating_sub(1)),
            KeyCode::Home => selected = 0,
            KeyCode::End => selected = rows.len().saturating_sub(1),
            KeyCode::Char(' ') if !rows.is_empty() && row_editable(&rows[selected]) => {
                cycle_selection(manifest, &rows[selected].key)
            }
            KeyCode::Char('i')
                if !global_scope
                    && !rows.is_empty()
                    && row_editable(&rows[selected])
                    && manifest.skills.contains_key(&rows[selected].key) =>
            {
                toggle_gitignore(manifest, &rows[selected].key);
            }
            KeyCode::Char('s') | KeyCode::Enter => return Ok(ConfigTuiResult::Save),
            KeyCode::Esc | KeyCode::Char('q') => return Ok(ConfigTuiResult::Cancel),
            _ => {}
        }
    }
}

fn draw(
    rows: &[ConfigRow],
    manifest: &ProjectConfig,
    global_scope: bool,
    selected: usize,
) -> Result<()> {
    let (width, height) = size().context("reading terminal size")?;
    let lines = view_lines(
        rows,
        manifest,
        global_scope,
        selected,
        width as usize,
        height as usize,
    );
    let mut output = io::stdout();
    execute!(output, MoveTo(0, 0), Clear(ClearType::All))?;
    for (index, line) in lines.iter().enumerate() {
        execute!(output, MoveTo(0, index as u16))?;
        if index == 0 {
            execute!(output, SetAttribute(Attribute::Bold))?;
        } else if line.starts_with('›') {
            execute!(output, SetAttribute(Attribute::Reverse))?;
        }
        execute!(output, Print(line), SetAttribute(Attribute::Reset))?;
    }
    output.flush()?;
    Ok(())
}

pub(crate) fn view_lines(
    rows: &[ConfigRow],
    manifest: &ProjectConfig,
    global_scope: bool,
    selected: usize,
    width: usize,
    height: usize,
) -> Vec<String> {
    let width = width.max(1);
    let height = height.max(1);
    let selected = selected.min(rows.len().saturating_sub(1));
    let body_height = height.saturating_sub(7).max(1);
    let start = selected.saturating_sub(body_height.saturating_sub(1));
    let end = (start + body_height).min(rows.len());
    let title = if global_scope {
        "Skiller · Global Skills"
    } else {
        "Skiller · Project Skills"
    };
    let selected_count = manifest.skills.len();
    let attention = rows
        .iter()
        .filter(|row| {
            !matches!(
                row.sync,
                None | Some(crate::installer::ProjectionStatus::Synced)
            )
        })
        .count();
    let mut lines = vec![fit(title, width)];
    if let Some(row) = rows.get(selected) {
        lines.push(fit(
            &format!(
                "{} / {} · {selected_count} selected · {attention} attention",
                row.catalog, row.scope
            ),
            width,
        ));
        lines.push(fit("  Mode  Status       Skill → projection", width));
    } else {
        lines.push(fit("No catalog skills are available.", width));
        lines.push(String::new());
    }
    for (index, row) in rows[start..end].iter().enumerate() {
        let absolute = start + index;
        let selection = manifest.skills.get(&row.key);
        let mark = match selection.map(SkillSelection::mode) {
            Some(SelectionMode::Enable) => '●',
            Some(SelectionMode::Manual) => '◎',
            None => '○',
        };
        let status = if row.read_only {
            "STALE"
        } else {
            row.sync.map_or("NOT INSTALLED", |status| status.label())
        };
        lines.push(fit(
            &format!(
                "{}  {mark}    {status:<12} ${}:{} → {}{}",
                if absolute == selected { '›' } else { ' ' },
                row.scope,
                row.name,
                row.installed_name,
                if selection.is_some_and(SkillSelection::gitignore) {
                    " · ignored"
                } else {
                    ""
                },
            ),
            width,
        ));
    }
    while lines.len() < height.saturating_sub(3) {
        lines.push(String::new());
    }
    if let Some(row) = rows.get(selected) {
        let mode = match manifest.skills.get(&row.key).map(SkillSelection::mode) {
            Some(SelectionMode::Enable) => "Agent + Human",
            Some(SelectionMode::Manual) => "Human",
            None => "Off",
        };
        let effective = match row.installed_mode {
            Some(EffectiveMode::Enable) => "Agent + Human",
            Some(EffectiveMode::Manual) => "Human",
            Some(EffectiveMode::Dependency) => "Agent dependency",
            None => "not installed",
        };
        let guide = row.authoring.as_deref().map_or("", |path| {
            if matches!(
                row.sync,
                Some(
                    crate::installer::ProjectionStatus::KeepLocal
                        | crate::installer::ProjectionStatus::Conflict
                        | crate::installer::ProjectionStatus::OrphanedLocal
                )
            ) {
                path
            } else {
                ""
            }
        });
        lines.push(fit(
            &format!(
                "  {mode} · {effective} · {}{}",
                row.description,
                if guide.is_empty() {
                    String::new()
                } else {
                    format!(" · promote via {guide}")
                }
            ),
            width,
        ));
    }
    let actions = rows.get(selected).map_or("", |row| {
        if !row_editable(row) {
            ""
        } else if global_scope {
            "  Space mode"
        } else {
            "  Space mode  i Git-ignore"
        }
    });
    lines.push(fit(
        &format!("  ↑↓ navigate{actions}  Enter save  Esc cancel"),
        width,
    ));
    lines.truncate(height);
    lines
}

fn fit(value: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let current: usize = value
        .chars()
        .map(|character| character.width().unwrap_or(0))
        .sum();
    if current <= width {
        return value.to_owned();
    }
    if width == 1 {
        return "…".to_owned();
    }
    let mut used = 0usize;
    let mut output = String::new();
    for character in value.chars() {
        let character_width = character.width().unwrap_or(0);
        if used + character_width > width - 1 {
            break;
        }
        output.push(character);
        used += character_width;
    }
    output.push('…');
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::SCHEMA_VERSION;
    use std::collections::BTreeMap;

    fn row(name: &str, scope: &str, installed: bool) -> ConfigRow {
        ConfigRow {
            key: format!("pyg/{name}"),
            catalog: "pyg".to_owned(),
            scope: scope.to_owned(),
            scope_order: 0,
            name: name.to_owned(),
            installed_name: name.to_owned(),
            description: format!("Configure {name}"),
            selected: None,
            gitignore: false,
            installed,
            installed_mode: installed.then_some(EffectiveMode::Enable),
            required_by: Vec::new(),
            read_only: false,
            status: None,
            sync: None,
            authoring: None,
        }
    }

    #[test]
    fn view_groups_scoped_modes_and_bounds_every_line() {
        let rows = vec![
            row("develop", "engineering", true),
            row("memo", "knowledge", false),
        ];
        let mut manifest = ProjectConfig {
            version: SCHEMA_VERSION,
            skills: BTreeMap::new(),
            agents: crate::model::default_agents(),
        };
        cycle_selection(&mut manifest, "pyg/develop");
        let lines = view_lines(&rows, &manifest, true, 0, 52, 10);
        assert!(lines.iter().any(|line| line.contains("pyg / engineering")));
        assert!(lines.iter().any(|line| line.contains('●')));
        assert!(
            lines
                .iter()
                .any(|line| line.contains("$engineering:develop"))
        );
        assert!(lines.iter().any(|line| line.contains("Agent + Human")));
        assert!(lines.iter().all(|line| {
            line.chars()
                .map(|character| character.width().unwrap_or(0))
                .sum::<usize>()
                <= 52
        }));
    }

    #[test]
    fn divergent_project_rows_show_status_and_no_mutation_hint() {
        let mut divergent = row("develop", "engineering", true);
        divergent.sync = Some(crate::installer::ProjectionStatus::KeepLocal);
        divergent.authoring = Some("/catalog/skills/develop".to_owned());
        let lines = view_lines(&[divergent], &ProjectConfig::default(), false, 0, 100, 9);
        let rendered = lines.join("\n");
        assert!(rendered.contains("KEEP LOCAL"));
        assert!(rendered.contains("promote via /catalog/skills/develop"));
        assert!(!rendered.contains("Space mode"));
        assert!(!rendered.contains("Git-ignore"));
    }

    #[test]
    fn view_is_safe_when_empty_or_tiny() {
        assert_eq!(
            view_lines(&[], &ProjectConfig::default(), true, 0, 1, 1),
            vec!["…"]
        );
        let lines = view_lines(
            &[row("develop", "engineering", true)],
            &ProjectConfig::default(),
            false,
            0,
            8,
            4,
        );
        assert_eq!(lines.len(), 4);
        assert!(lines.iter().all(|line| {
            line.chars()
                .map(|character| character.width().unwrap_or(0))
                .sum::<usize>()
                <= 8
        }));
    }
}
