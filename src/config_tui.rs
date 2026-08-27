use std::io::{self, IsTerminal, Write};

use anyhow::{Context, Result, bail};
use crossterm::cursor::{Hide, MoveTo, Show};
use crossterm::event::{Event, KeyCode, KeyEventKind, read};
use crossterm::style::{
    Attribute, Color, Print, ResetColor, SetAttribute, SetBackgroundColor, SetForegroundColor,
};
use crossterm::terminal::{
    Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode,
    enable_raw_mode, size,
};
use crossterm::{SynchronizedUpdate, execute, queue};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::config_ui::{ConfigRow, cycle_selection, row_editable, toggle_gitignore};
use crate::model::{EffectiveMode, ProjectConfig, SelectionMode, SkillSelection};

const WIDE_LAYOUT_MIN_WIDTH: usize = 72;
const THREE_COLUMN_MIN_WIDTH: usize = 96;
const MASTER_DETAIL_DIVIDER: &str = " │ ";

pub(crate) enum ConfigTuiResult {
    Save,
    Cancel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConfigScreen {
    Scopes,
    Skills,
}

#[derive(Debug, Clone, Copy)]
struct ConfigTuiState {
    screen: ConfigScreen,
    scope: usize,
    skill: usize,
}

impl Default for ConfigTuiState {
    fn default() -> Self {
        Self {
            screen: ConfigScreen::Scopes,
            scope: 0,
            skill: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputResult {
    Continue,
    Save,
    Cancel,
}

struct ScopeGroup<'a> {
    catalog: &'a str,
    scope: &'a str,
    rows: Vec<&'a ConfigRow>,
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
    let mut state = ConfigTuiState::default();

    loop {
        draw(rows, manifest, global_scope, state)?;
        let Event::Key(key) = read().context("reading terminal input")? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match handle_key(rows, manifest, global_scope, &mut state, key.code) {
            InputResult::Continue => {}
            InputResult::Save => return Ok(ConfigTuiResult::Save),
            InputResult::Cancel => return Ok(ConfigTuiResult::Cancel),
        }
    }
}

fn handle_key(
    rows: &[ConfigRow],
    manifest: &mut ProjectConfig,
    global_scope: bool,
    state: &mut ConfigTuiState,
    key: KeyCode,
) -> InputResult {
    let groups = scope_groups(rows);
    normalize_state(&groups, state);
    let previous_scope = state.scope;
    let item_count = match state.screen {
        ConfigScreen::Scopes => groups.len(),
        ConfigScreen::Skills => groups.get(state.scope).map_or(0, |group| group.rows.len()),
    };
    let selected = match state.screen {
        ConfigScreen::Scopes => &mut state.scope,
        ConfigScreen::Skills => &mut state.skill,
    };
    match key {
        KeyCode::Up | KeyCode::Char('k') => *selected = selected.saturating_sub(1),
        KeyCode::Down | KeyCode::Char('j') => {
            *selected = (*selected + 1).min(item_count.saturating_sub(1));
        }
        KeyCode::PageUp => *selected = selected.saturating_sub(10),
        KeyCode::PageDown => *selected = (*selected + 10).min(item_count.saturating_sub(1)),
        KeyCode::Home => *selected = 0,
        KeyCode::End => *selected = item_count.saturating_sub(1),
        KeyCode::Enter if state.screen == ConfigScreen::Scopes && item_count > 0 => {
            state.screen = ConfigScreen::Skills;
            state.skill = 0;
        }
        KeyCode::Char(' ')
            if state.screen == ConfigScreen::Skills
                && selected_skill(&groups, *state).is_some_and(row_editable) =>
        {
            if let Some(row) = selected_skill(&groups, *state) {
                cycle_selection(manifest, &row.key);
            }
        }
        KeyCode::Char('i')
            if state.screen == ConfigScreen::Skills
                && !global_scope
                && selected_skill(&groups, *state).is_some_and(row_editable) =>
        {
            if let Some(row) = selected_skill(&groups, *state)
                && manifest.skills.contains_key(&row.key)
            {
                toggle_gitignore(manifest, &row.key);
            }
        }
        KeyCode::Char('s') => return InputResult::Save,
        KeyCode::Esc | KeyCode::Char('q') if state.screen == ConfigScreen::Skills => {
            state.screen = ConfigScreen::Scopes;
        }
        KeyCode::Esc | KeyCode::Char('q') => return InputResult::Cancel,
        _ => {}
    }
    if state.screen == ConfigScreen::Scopes && state.scope != previous_scope {
        state.skill = 0;
    }
    InputResult::Continue
}

fn draw(
    rows: &[ConfigRow],
    manifest: &ProjectConfig,
    global_scope: bool,
    state: ConfigTuiState,
) -> Result<()> {
    let (width, height) = size().context("reading terminal size")?;
    let lines = view_lines(
        rows,
        manifest,
        global_scope,
        state,
        width as usize,
        height as usize,
    );
    let mut output = io::stdout();
    write_frame(
        &mut output,
        &lines,
        state,
        crate::output::color_enabled(true),
    )?;
    output.flush()?;
    Ok(())
}

fn write_frame(
    output: &mut impl Write,
    lines: &[String],
    state: ConfigTuiState,
    color: bool,
) -> Result<()> {
    output.sync_update(|output| -> Result<()> {
        queue!(output, MoveTo(0, 0))?;
        for (index, line) in lines.iter().enumerate() {
            queue!(
                output,
                MoveTo(0, index as u16),
                Clear(ClearType::CurrentLine)
            )?;
            if index == 0 {
                print_colored(
                    output,
                    line,
                    Some(crate::output::ACCENT),
                    true,
                    false,
                    color,
                )?;
                continue;
            }
            if (index == 1 && line.contains(" scopes · ")) || index + 1 == lines.len() {
                print_colored(
                    output,
                    line,
                    Some(crate::output::MUTED),
                    false,
                    false,
                    color,
                )?;
                continue;
            }
            let segments: Vec<_> = line.split(MASTER_DETAIL_DIVIDER).collect();
            for (segment_index, segment) in segments.iter().enumerate() {
                if segment_index > 0 {
                    print_colored(
                        output,
                        MASTER_DETAIL_DIVIDER,
                        Some(crate::output::MUTED),
                        false,
                        false,
                        color,
                    )?;
                }
                let trimmed = segment.trim();
                let selected = trimmed.starts_with('›');
                let context = trimmed.starts_with('•');
                let foreground = segment_color(trimmed, state);
                print_colored(
                    output,
                    segment,
                    foreground,
                    selected || context || is_heading(trimmed),
                    selected,
                    color,
                )?;
            }
        }
        queue!(
            output,
            MoveTo(0, lines.len() as u16),
            Clear(ClearType::FromCursorDown)
        )?;
        Ok(())
    })??;
    Ok(())
}

fn print_colored(
    output: &mut impl Write,
    text: &str,
    foreground: Option<Color>,
    bold: bool,
    selected: bool,
    color: bool,
) -> Result<()> {
    if selected {
        if color {
            queue!(
                output,
                SetBackgroundColor(crate::output::SELECTED_BG),
                SetForegroundColor(crate::output::SELECTED_TEXT)
            )?;
        } else {
            queue!(output, SetAttribute(Attribute::Reverse))?;
        }
    } else if color && let Some(foreground) = foreground {
        queue!(output, SetForegroundColor(foreground))?;
    }
    if bold {
        queue!(output, SetAttribute(Attribute::Bold))?;
    }
    queue!(
        output,
        Print(text),
        ResetColor,
        SetAttribute(Attribute::Reset)
    )?;
    Ok(())
}

fn segment_color(segment: &str, state: ConfigTuiState) -> Option<Color> {
    if segment.starts_with("Scopes") {
        return Some(if state.screen == ConfigScreen::Scopes {
            crate::output::ACCENT
        } else {
            crate::output::MUTED
        });
    }
    if segment.starts_with("Skills") {
        return Some(if state.screen == ConfigScreen::Skills {
            crate::output::ACCENT
        } else {
            crate::output::MUTED
        });
    }
    if matches!(segment, "Details" | "Description" | "Installed") {
        return Some(crate::output::ACCENT);
    }
    if segment == "Required" || segment.contains("Required by") || segment.contains('↳') {
        return Some(crate::output::WARNING);
    }
    if segment.contains("CONFLICT") || segment.contains("ORPHANED") {
        return Some(crate::output::ERROR);
    }
    if segment.contains("KEEP LOCAL")
        || segment.contains("DRIFT")
        || segment.contains("UPDATE")
        || segment.contains("REVIEW")
    {
        return Some(crate::output::WARNING);
    }
    if segment.contains('$') {
        return Some(scope_color(segment));
    }
    if segment.contains('●') {
        return Some(crate::output::SUCCESS);
    }
    if segment.contains('◎') {
        return Some(crate::output::WARNING);
    }
    if segment.contains('○') || segment.contains("Not installed") {
        return Some(crate::output::MUTED);
    }
    None
}

fn is_heading(segment: &str) -> bool {
    segment.starts_with("Scopes")
        || segment.starts_with("Skills")
        || matches!(
            segment,
            "Details" | "Description" | "Required" | "Installed"
        )
}

fn scope_color(scope: &str) -> Color {
    let identity = scope
        .split_once('$')
        .and_then(|(_, value)| value.split_whitespace().next())
        .unwrap_or(scope);
    let hash = identity.bytes().fold(0usize, |value, byte| {
        value.wrapping_mul(31).wrapping_add(byte as usize)
    });
    crate::output::SCOPE_PALETTE[hash % crate::output::SCOPE_PALETTE.len()]
}

// ^ README.md#configuration documents the scope-first selection and detail states projected here.
fn view_lines(
    rows: &[ConfigRow],
    manifest: &ProjectConfig,
    global_scope: bool,
    mut state: ConfigTuiState,
    width: usize,
    height: usize,
) -> Vec<String> {
    let width = width.max(1);
    let height = height.max(1);
    let groups = scope_groups(rows);
    normalize_state(&groups, &mut state);
    let base_title = if global_scope {
        "Skiller · Global Skills"
    } else {
        "Skiller · Project Skills"
    };
    let title = match state.screen {
        ConfigScreen::Scopes => base_title.to_owned(),
        ConfigScreen::Skills => groups.get(state.scope).map_or_else(
            || base_title.to_owned(),
            |group| format!("{base_title} · ${}", group.scope),
        ),
    };
    if height == 1 {
        return vec![fit(&title, width)];
    }

    let show_footer = height >= 3;
    let header_height = usize::from(height >= 4) + 1;
    let body_height = height.saturating_sub(header_height + usize::from(show_footer));
    let attention = rows.iter().filter(|row| sync_attention(row)).count();
    let summary = format!(
        "{} scopes · {} skills · {} configured · {attention} attention",
        groups.len(),
        rows.len(),
        manifest.skills.len()
    );
    let mut lines = vec![fit(&title, width)];
    if header_height == 2 {
        lines.push(fit(&summary, width));
    }
    let body = if width >= THREE_COLUMN_MIN_WIDTH && !groups.is_empty() {
        three_column_body(&groups, manifest, state, width, body_height)
    } else {
        match state.screen {
            ConfigScreen::Scopes => scope_body(&groups, manifest, state.scope, width, body_height),
            ConfigScreen::Skills => groups.get(state.scope).map_or_else(
                || vec![fit("No skills are available in this scope.", width)],
                |group| skill_body(group, manifest, state.skill, width, body_height),
            ),
        }
    };
    lines.extend(body.into_iter().take(body_height));
    while lines.len() < height.saturating_sub(usize::from(show_footer)) {
        lines.push(String::new());
    }
    if show_footer {
        lines.push(fit(
            &key_hint(&groups, manifest, global_scope, state, width),
            width,
        ));
    }
    lines.truncate(height);
    lines
}

fn scope_groups(rows: &[ConfigRow]) -> Vec<ScopeGroup<'_>> {
    let mut groups: Vec<ScopeGroup<'_>> = Vec::new();
    for row in rows {
        if let Some(group) = groups
            .last_mut()
            .filter(|group| group.catalog == row.catalog && group.scope == row.scope)
        {
            group.rows.push(row);
        } else {
            groups.push(ScopeGroup {
                catalog: &row.catalog,
                scope: &row.scope,
                rows: vec![row],
            });
        }
    }
    groups
}

fn normalize_state(groups: &[ScopeGroup<'_>], state: &mut ConfigTuiState) {
    state.scope = state.scope.min(groups.len().saturating_sub(1));
    state.skill = state.skill.min(
        groups
            .get(state.scope)
            .map_or(0, |group| group.rows.len())
            .saturating_sub(1),
    );
    if groups.is_empty() {
        state.screen = ConfigScreen::Scopes;
    }
}

fn selected_skill<'a>(
    groups: &'a [ScopeGroup<'a>],
    state: ConfigTuiState,
) -> Option<&'a ConfigRow> {
    groups
        .get(state.scope)
        .and_then(|group| group.rows.get(state.skill))
        .copied()
}

fn three_column_body(
    groups: &[ScopeGroup<'_>],
    manifest: &ProjectConfig,
    state: ConfigTuiState,
    width: usize,
    height: usize,
) -> Vec<String> {
    if height == 0 {
        return Vec::new();
    }
    let usable = width.saturating_sub(MASTER_DETAIL_DIVIDER.width() * 2);
    let scope_width = (usable * 28 / 100).max(24);
    let skill_width = (usable * 34 / 100).max(28);
    let detail_width = usable.saturating_sub(scope_width + skill_width).max(1);
    let row_capacity = height.saturating_sub(1).max(1);
    let scope_start = window_start(groups.len(), state.scope, row_capacity);
    let scope_end = (scope_start + row_capacity).min(groups.len());
    let selected_group = &groups[state.scope];
    let skill_start = window_start(selected_group.rows.len(), state.skill, row_capacity);
    let skill_end = (skill_start + row_capacity).min(selected_group.rows.len());
    let catalogs = groups
        .iter()
        .map(|group| group.catalog)
        .collect::<std::collections::BTreeSet<_>>();

    let mut scopes = vec!["Scopes".to_owned()];
    for (offset, group) in groups[scope_start..scope_end].iter().enumerate() {
        let index = scope_start + offset;
        let label = if catalogs.len() == 1 {
            format!("{} ${}", scope_state_marker(group, manifest), group.scope)
        } else {
            format!(
                "{} {} / ${}",
                scope_state_marker(group, manifest),
                group.catalog,
                group.scope
            )
        };
        let configured = group
            .rows
            .iter()
            .filter(|row| manifest.skills.contains_key(&row.key))
            .count();
        scopes.push(aligned_row(
            if index == state.scope {
                if state.screen == ConfigScreen::Scopes {
                    '›'
                } else {
                    '•'
                }
            } else {
                ' '
            },
            &label,
            &format!("{configured}/{}", group.rows.len()),
            scope_width,
        ));
    }

    let mut skills = vec![format!("Skills · ${}", selected_group.scope)];
    for (offset, row) in selected_group.rows[skill_start..skill_end]
        .iter()
        .enumerate()
    {
        let index = skill_start + offset;
        skills.push(skill_row_with_marker(
            row,
            manifest,
            if index == state.skill {
                if state.screen == ConfigScreen::Skills {
                    '›'
                } else {
                    '•'
                }
            } else {
                ' '
            },
            skill_width,
        ));
    }

    let mut details = vec!["Details".to_owned()];
    details.extend(detail_lines(selected_group.rows[state.skill], detail_width));
    (0..height)
        .map(|index| {
            format!(
                "{}{}{}{}{}",
                pad(&scopes.get(index).cloned().unwrap_or_default(), scope_width),
                MASTER_DETAIL_DIVIDER,
                pad(&skills.get(index).cloned().unwrap_or_default(), skill_width),
                MASTER_DETAIL_DIVIDER,
                fit(
                    &details.get(index).cloned().unwrap_or_default(),
                    detail_width
                )
            )
        })
        .collect()
}

fn window_start(total: usize, selected: usize, capacity: usize) -> usize {
    selected
        .saturating_sub(capacity / 2)
        .min(total.saturating_sub(capacity))
}

fn scope_state_marker(group: &ScopeGroup<'_>, manifest: &ProjectConfig) -> char {
    let modes: Vec<_> = group
        .rows
        .iter()
        .map(|row| manifest.skills.get(&row.key).map(SkillSelection::mode))
        .collect();
    if modes
        .iter()
        .all(|mode| *mode == Some(SelectionMode::Enable))
    {
        '●'
    } else if modes
        .iter()
        .all(|mode| *mode == Some(SelectionMode::Manual))
    {
        '◎'
    } else if modes.iter().all(Option::is_none) {
        '○'
    } else {
        '◐'
    }
}

fn scope_body(
    groups: &[ScopeGroup<'_>],
    manifest: &ProjectConfig,
    selected: usize,
    width: usize,
    height: usize,
) -> Vec<String> {
    if groups.is_empty() {
        return vec![fit("No catalog skills are available.", width)];
    }
    let catalogs = groups
        .iter()
        .map(|group| group.catalog)
        .collect::<std::collections::BTreeSet<_>>();
    let capacity = height.max(1);
    let start = selected.saturating_sub(capacity.saturating_sub(1));
    let end = (start + capacity).min(groups.len());
    groups[start..end]
        .iter()
        .enumerate()
        .map(|(offset, group)| {
            let configured = group
                .rows
                .iter()
                .filter(|row| manifest.skills.contains_key(&row.key))
                .count();
            let label = if catalogs.len() == 1 {
                format!("${}", group.scope)
            } else {
                format!("{} / ${}", group.catalog, group.scope)
            };
            aligned_row(
                if start + offset == selected {
                    '›'
                } else {
                    ' '
                },
                &label,
                &format!(
                    "{} skill{} · {configured} configured",
                    group.rows.len(),
                    if group.rows.len() == 1 { "" } else { "s" }
                ),
                width,
            )
        })
        .collect()
}

fn skill_body(
    group: &ScopeGroup<'_>,
    manifest: &ProjectConfig,
    selected: usize,
    width: usize,
    height: usize,
) -> Vec<String> {
    if height == 0 {
        return Vec::new();
    }
    if width >= WIDE_LAYOUT_MIN_WIDTH {
        return wide_skill_body(group, manifest, selected, width, height);
    }
    stacked_skill_body(group, manifest, selected, width, height)
}

fn wide_skill_body(
    group: &ScopeGroup<'_>,
    manifest: &ProjectConfig,
    selected: usize,
    width: usize,
    height: usize,
) -> Vec<String> {
    let left_width = ((width - MASTER_DETAIL_DIVIDER.width()) / 2).max(24);
    let right_width = width
        .saturating_sub(left_width + MASTER_DETAIL_DIVIDER.width())
        .max(1);
    let capacity = height.saturating_sub(1).max(1);
    let start = selected.saturating_sub(capacity.saturating_sub(1));
    let end = (start + capacity).min(group.rows.len());
    let mut left = vec![fit(
        &format!("Skills  {}–{} of {}", start + 1, end, group.rows.len()),
        left_width,
    )];
    for (offset, row) in group.rows[start..end].iter().enumerate() {
        left.push(skill_row(
            row,
            manifest,
            start + offset == selected,
            left_width,
        ));
    }
    let mut right = vec!["Details".to_owned()];
    right.extend(detail_lines(group.rows[selected], right_width));
    (0..height)
        .map(|index| {
            format!(
                "{}{}{}",
                pad(&left.get(index).cloned().unwrap_or_default(), left_width),
                MASTER_DETAIL_DIVIDER,
                fit(&right.get(index).cloned().unwrap_or_default(), right_width)
            )
        })
        .collect()
}

fn stacked_skill_body(
    group: &ScopeGroup<'_>,
    manifest: &ProjectConfig,
    selected: usize,
    width: usize,
    height: usize,
) -> Vec<String> {
    let detail_budget = if height >= 10 { 7 } else { 0 };
    let list_height = height.saturating_sub(detail_budget + 1).max(1);
    let capacity = list_height.max(1);
    let start = selected.saturating_sub(capacity.saturating_sub(1));
    let end = (start + capacity).min(group.rows.len());
    let mut lines = vec![fit(
        &format!("Skills  {}–{} of {}", start + 1, end, group.rows.len()),
        width,
    )];
    for (offset, row) in group.rows[start..end].iter().enumerate() {
        lines.push(skill_row(row, manifest, start + offset == selected, width));
    }
    if detail_budget > 0 {
        lines.push(fit("Details", width));
        lines.extend(stacked_detail_lines(group.rows[selected], width));
    }
    lines.into_iter().take(height).collect()
}

fn skill_row(row: &ConfigRow, manifest: &ProjectConfig, selected: bool, width: usize) -> String {
    skill_row_with_marker(row, manifest, if selected { '›' } else { ' ' }, width)
}

fn skill_row_with_marker(
    row: &ConfigRow,
    manifest: &ProjectConfig,
    marker: char,
    width: usize,
) -> String {
    aligned_row(marker, &row.name, &configured_state(row, manifest), width)
}

fn aligned_row(marker: char, label: &str, value: &str, width: usize) -> String {
    let marker_width = 2;
    let gap = 2;
    let value_width = value.width();
    if marker_width + gap + value_width >= width {
        return fit(&format!("{marker} {label}  {value}"), width);
    }
    let label_width = width - marker_width - gap - value_width;
    format!(
        "{marker} {}{}{}",
        pad(label, label_width),
        " ".repeat(gap),
        value
    )
}

fn detail_lines(row: &ConfigRow, width: usize) -> Vec<String> {
    let mut lines = vec!["Description".to_owned()];
    lines.extend(wrap(&row.description, width, 2));
    lines.push(String::new());
    lines.push("Required".to_owned());
    lines.extend(wrap(&required_state(row), width, 2));
    lines.push(String::new());
    lines.push("Installed".to_owned());
    lines.extend(wrap(&installed_state(row), width, 2));
    lines
}

fn stacked_detail_lines(row: &ConfigRow, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    for (label, value) in [
        ("Description", row.description.clone()),
        ("Required", required_state(row)),
        ("Installed", installed_state(row)),
    ] {
        lines.extend(wrap(&format!("{label}: {value}"), width, 2));
    }
    lines
}

fn required_state(row: &ConfigRow) -> String {
    if row.required_by.is_empty() {
        "None".to_owned()
    } else {
        format!("By {}", row.required_by.join(", "))
    }
}

fn configured_state(row: &ConfigRow, manifest: &ProjectConfig) -> String {
    let selection = manifest.skills.get(&row.key);
    let (mark, mode) = match selection.map(SkillSelection::mode) {
        Some(SelectionMode::Enable) => ('●', "Agent + Human"),
        Some(SelectionMode::Manual) => ('◎', "Human"),
        None => ('○', "Off"),
    };
    format!(
        "{mark} {mode}{}",
        if selection.is_some_and(SkillSelection::gitignore) {
            " · ignored"
        } else {
            ""
        }
    )
}

fn installed_state(row: &ConfigRow) -> String {
    let effective = match row.installed_mode {
        Some(EffectiveMode::Enable) => "Agent + Human",
        Some(EffectiveMode::Manual) => "Human",
        Some(EffectiveMode::Dependency) => "Agent dependency",
        None => "Not installed",
    };
    let mut parts = vec![if row.installed {
        format!("{effective} as {}", row.installed_name)
    } else {
        effective.to_owned()
    }];
    let sync_attention = row
        .sync
        .filter(|status| !matches!(status, crate::installer::ProjectionStatus::Synced));
    if let Some(status) = sync_attention {
        parts.push(status.label().to_owned());
    } else if row.read_only {
        parts.push(
            row.status
                .as_deref()
                .map_or_else(|| "Read-only".to_owned(), str::to_uppercase),
        );
    }
    if let Some(path) = row.authoring.as_deref()
        && matches!(
            row.sync,
            Some(
                crate::installer::ProjectionStatus::KeepLocal
                    | crate::installer::ProjectionStatus::Conflict
                    | crate::installer::ProjectionStatus::OrphanedLocal
            )
        )
    {
        parts.push(format!("Promote via {path}"));
    }
    parts.join(" · ")
}

fn sync_attention(row: &ConfigRow) -> bool {
    !matches!(
        row.sync,
        None | Some(crate::installer::ProjectionStatus::Synced)
    )
}

fn key_hint(
    groups: &[ScopeGroup<'_>],
    manifest: &ProjectConfig,
    global_scope: bool,
    state: ConfigTuiState,
    width: usize,
) -> String {
    let navigation = "[↑/↓] Navigate";
    let save = "[S] Save";
    match state.screen {
        ConfigScreen::Scopes => {
            let mut hints = Vec::new();
            if !groups.is_empty() {
                hints.extend([navigation, "[Enter] Open"]);
            }
            hints.extend([save, "[Esc] Cancel"]);
            fit_hint(&hints, &["[Enter] Open", save, "[Esc] Cancel"], width)
        }
        ConfigScreen::Skills => {
            let row = selected_skill(groups, state);
            let mut hints = vec![navigation];
            let mut compact = Vec::new();
            if row.is_some_and(row_editable) {
                hints.push("[Space] Mode");
                compact.push("[Space] Mode");
                if !global_scope && row.is_some_and(|row| manifest.skills.contains_key(&row.key)) {
                    hints.push("[I] Git-ignore");
                    compact.push("[I] Ignore");
                }
            }
            hints.extend([save, "[Esc] Scopes"]);
            compact.extend([save, "[Esc] Scopes"]);
            fit_hint(&hints, &compact, width)
        }
    }
}

fn fit_hint(full: &[&str], compact: &[&str], width: usize) -> String {
    let full = full.join("  ");
    if full.width() <= width {
        return full;
    }
    let compact = compact.join("  ");
    if compact.width() <= width {
        compact
    } else {
        fit(&compact, width)
    }
}

fn wrap(value: &str, width: usize, maximum: usize) -> Vec<String> {
    if maximum == 0 {
        return Vec::new();
    }
    let words: Vec<_> = value.split_whitespace().collect();
    let mut lines = Vec::new();
    let mut index = 0;
    while index < words.len() && lines.len() < maximum {
        if lines.len() + 1 == maximum {
            lines.push(fit(&words[index..].join(" "), width));
            break;
        }
        let mut current = words[index].to_owned();
        index += 1;
        while index < words.len() {
            let candidate = format!("{current} {}", words[index]);
            if candidate.width() > width {
                break;
            }
            current = candidate;
            index += 1;
        }
        lines.push(fit(&current, width));
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

fn pad(value: &str, width: usize) -> String {
    let fitted = fit(value, width);
    format!(
        "{fitted}{}",
        " ".repeat(width.saturating_sub(fitted.width()))
    )
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

    fn state(screen: ConfigScreen, scope: usize, skill: usize) -> ConfigTuiState {
        ConfigTuiState {
            screen,
            scope,
            skill,
        }
    }

    #[test]
    fn scope_view_groups_skills_before_showing_skill_rows() {
        let rows = vec![
            row("develop", "engineering", true),
            row("simplify", "engineering", true),
            row("memo", "knowledge", false),
        ];
        let mut manifest = ProjectConfig::default();
        cycle_selection(&mut manifest, "pyg/develop");
        let rendered = view_lines(
            &rows,
            &manifest,
            true,
            state(ConfigScreen::Scopes, 0, 0),
            80,
            14,
        )
        .join("\n");
        assert!(rendered.contains("› $engineering"));
        assert!(rendered.contains("2 skills · 1 configured"));
        assert!(rendered.contains("$knowledge"));
        assert!(!rendered.contains("develop"));
        assert!(rendered.contains("[Enter] Open"));
    }

    #[test]
    fn wide_view_keeps_scopes_skills_and_details_visible_in_order() {
        let rows = vec![
            row("develop", "engineering", true),
            row("simplify", "engineering", false),
            row("memo", "knowledge", false),
        ];
        let lines = view_lines(
            &rows,
            &ProjectConfig::default(),
            true,
            state(ConfigScreen::Scopes, 0, 0),
            140,
            18,
        );
        let header = lines.iter().find(|line| line.contains("Scopes")).unwrap();
        let scopes = header.find("Scopes").unwrap();
        let skills = header.find("Skills · $engineering").unwrap();
        let details = header.find("Details").unwrap();
        assert!(scopes < skills && skills < details);
        let rendered = lines.join("\n");
        assert!(rendered.contains("› ○ $engineering"));
        assert!(rendered.contains("• develop"));
        assert!(rendered.contains("Configure develop"));
        assert!(lines.iter().all(|line| line.width() <= 140));
    }

    #[test]
    fn semantic_colors_are_stable_by_scope_identity_and_state_role() {
        assert_eq!(
            scope_color("› ○ $engineering 0/2"),
            scope_color("$engineering")
        );
        assert_eq!(
            segment_color(
                "develop  ● Agent + Human",
                state(ConfigScreen::Skills, 0, 0)
            ),
            Some(crate::output::SUCCESS)
        );
        assert_eq!(
            segment_color("Required by release", state(ConfigScreen::Skills, 0, 0)),
            Some(crate::output::WARNING)
        );
        assert_eq!(
            segment_color("CONFLICT", state(ConfigScreen::Skills, 0, 0)),
            Some(crate::output::ERROR)
        );
        assert_eq!(
            segment_color("develop  ○ Off", state(ConfigScreen::Skills, 0, 0)),
            Some(crate::output::MUTED)
        );
    }

    #[test]
    fn changing_scope_resets_the_previewed_skill() {
        let rows = vec![
            row("develop", "engineering", true),
            row("simplify", "engineering", false),
            row("memo", "knowledge", false),
        ];
        let mut manifest = ProjectConfig::default();
        let mut position = state(ConfigScreen::Scopes, 0, 1);
        handle_key(&rows, &mut manifest, true, &mut position, KeyCode::Down);
        assert_eq!((position.scope, position.skill), (1, 0));
    }

    #[test]
    fn scope_enter_and_escape_form_a_two_level_navigation_stack() {
        let rows = vec![row("develop", "engineering", true)];
        let mut manifest = ProjectConfig::default();
        let mut position = ConfigTuiState::default();
        assert_eq!(
            handle_key(&rows, &mut manifest, true, &mut position, KeyCode::Enter),
            InputResult::Continue
        );
        assert_eq!(position.screen, ConfigScreen::Skills);
        assert_eq!(
            handle_key(&rows, &mut manifest, true, &mut position, KeyCode::Esc),
            InputResult::Continue
        );
        assert_eq!(position.screen, ConfigScreen::Scopes);
        assert_eq!(
            handle_key(&rows, &mut manifest, true, &mut position, KeyCode::Esc),
            InputResult::Cancel
        );
    }

    #[test]
    fn skill_view_uses_one_line_rows_and_keeps_details_on_the_right() {
        let mut rows = vec![
            row("develop", "engineering", true),
            row("simplify", "engineering", false),
        ];
        rows[0].required_by = vec!["release".to_owned(), "skiller".to_owned()];
        let mut manifest = ProjectConfig {
            version: SCHEMA_VERSION,
            skills: BTreeMap::new(),
            agents: crate::model::default_agents(),
        };
        cycle_selection(&mut manifest, "pyg/develop");
        let lines = view_lines(
            &rows,
            &manifest,
            true,
            state(ConfigScreen::Skills, 0, 0),
            100,
            18,
        );
        let rendered = lines.join("\n");
        let selected = lines
            .iter()
            .find(|line| line.contains("› develop"))
            .unwrap();
        assert!(selected.contains("● Agent + Human"));
        assert!(!rendered.contains("Config  "));
        assert!(rendered.contains("│ Details"));
        assert!(rendered.contains("Description"));
        assert!(rendered.contains("By release, skiller"));
        assert!(rendered.contains("Agent + Human as develop"));
        assert!(lines.iter().all(|line| line.width() <= 100));
    }

    #[test]
    fn space_changes_only_the_selected_skill_inside_a_scope() {
        let rows = vec![row("develop", "engineering", false)];
        let mut manifest = ProjectConfig::default();
        let mut scope_position = ConfigTuiState::default();
        handle_key(
            &rows,
            &mut manifest,
            true,
            &mut scope_position,
            KeyCode::Char(' '),
        );
        assert!(manifest.skills.is_empty());
        scope_position.screen = ConfigScreen::Skills;
        handle_key(
            &rows,
            &mut manifest,
            true,
            &mut scope_position,
            KeyCode::Char(' '),
        );
        assert_eq!(manifest.skills["pyg/develop"].mode(), SelectionMode::Enable);
    }

    #[test]
    fn narrow_skill_view_keeps_rows_single_line_and_stacks_selected_details() {
        let rows = vec![row("develop", "engineering", true)];
        let lines = view_lines(
            &rows,
            &ProjectConfig::default(),
            true,
            state(ConfigScreen::Skills, 0, 0),
            52,
            16,
        );
        let rendered = lines.join("\n");
        assert!(!rendered.contains(MASTER_DETAIL_DIVIDER));
        assert!(
            lines
                .iter()
                .any(|line| line.starts_with("› develop") && line.contains("○ Off"))
        );
        assert!(rendered.contains("Description: Configure develop"));
        assert!(rendered.contains("Required: None"));
        assert!(rendered.contains("Installed: Agent + Human as develop"));
        assert!(lines.iter().all(|line| line.width() <= 52));
    }

    #[test]
    fn divergent_project_rows_show_status_and_no_mutation_hint() {
        let mut divergent = row("develop", "engineering", true);
        divergent.sync = Some(crate::installer::ProjectionStatus::KeepLocal);
        divergent.authoring = Some("/catalog/skills/develop".to_owned());
        let lines = view_lines(
            &[divergent],
            &ProjectConfig::default(),
            false,
            state(ConfigScreen::Skills, 0, 0),
            100,
            18,
        );
        let rendered = lines.join("\n");
        assert!(rendered.contains("KEEP LOCAL"));
        assert!(rendered.contains("/catalog/skills/develop"));
        assert!(!rendered.contains("[Space] Mode"));
        assert!(!rendered.contains("[I] Git-ignore"));
        assert!(rendered.contains("[S] Save"));
        assert!(rendered.contains("[Esc] Scopes"));
    }

    #[test]
    fn orphaned_local_detail_prefers_sync_state_over_generic_read_only_state() {
        let mut orphaned = row("retired", "other", true);
        orphaned.read_only = true;
        orphaned.status = Some("orphaned".to_owned());
        orphaned.sync = Some(crate::installer::ProjectionStatus::OrphanedLocal);
        let installed = installed_state(&orphaned);
        assert!(installed.contains("ORPHANED"));
        assert!(!installed.contains("STALE"));
    }

    #[test]
    fn project_gitignore_hint_requires_an_editable_configured_skill() {
        let skill = row("develop", "engineering", false);
        let rows = vec![skill];
        let groups = scope_groups(&rows);
        let position = state(ConfigScreen::Skills, 0, 0);
        let mut manifest = ProjectConfig::default();
        assert!(!key_hint(&groups, &manifest, false, position, 100).contains("Git-ignore"));
        cycle_selection(&mut manifest, "pyg/develop");
        assert!(key_hint(&groups, &manifest, false, position, 100).contains("[I] Git-ignore"));
        assert!(!key_hint(&groups, &manifest, true, position, 100).contains("Git-ignore"));
        let narrow = key_hint(&groups, &manifest, false, position, 52);
        assert!(narrow.contains("[Space] Mode"));
        assert!(narrow.contains("[I] Ignore"));
        assert!(narrow.contains("[S] Save"));
        assert!(narrow.contains("[Esc] Scopes"));
    }

    #[test]
    fn redraw_is_one_synchronized_frame_without_a_full_screen_blank() {
        let mut output = Vec::new();
        write_frame(
            &mut output,
            &["Skiller".to_owned(), "Scopes".to_owned()],
            ConfigTuiState::default(),
            false,
        )
        .unwrap();
        let frame = String::from_utf8(output).unwrap();
        assert!(frame.starts_with("\u{1b}[?2026h"));
        assert!(frame.ends_with("\u{1b}[?2026l"));
        assert!(frame.contains("\u{1b}[2K"));
        assert!(!frame.contains("\u{1b}[2J"));
    }

    #[test]
    fn view_is_safe_when_empty_or_tiny() {
        assert_eq!(
            view_lines(
                &[],
                &ProjectConfig::default(),
                true,
                ConfigTuiState::default(),
                1,
                1,
            ),
            vec!["…"]
        );
        let lines = view_lines(
            &[row("develop", "engineering", true)],
            &ProjectConfig::default(),
            false,
            ConfigTuiState::default(),
            8,
            4,
        );
        assert_eq!(lines.len(), 4);
        assert!(lines.iter().all(|line| line.width() <= 8));
    }
}
