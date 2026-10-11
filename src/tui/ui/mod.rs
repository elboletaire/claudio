//! Rendering: tab bar, session pane, status bar and modal popups.
//!
//! Everything here is a pure function of [`App`]; the tab-fitting and text
//! helpers are shared with the app (tab hit-testing) and the wizard.

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};
use ratatui::Frame;

use crate::proto::{SessionKind, SessionState};

mod confirm;
mod git;
mod stats;
mod status;

use confirm::draw_confirm;
use git::draw_git;
use stats::draw_proxy_stats;
use status::{draw_status_machine, draw_status_session};

use super::app::{App, Modal, Mode, SessionView};
use super::selection::Selection;
use super::fmt::{abbreviate_home, fmt_age, str_width, tail, truncate};
use super::wizard::{resume_label, Wizard};

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
/// Glyph shown when a remote session's connection has dropped.
const RECONNECTING: &str = "⇄";

/// Draw the whole UI.
pub fn draw(frame: &mut Frame, app: &App) {
    match app.mode {
        Mode::Manager => draw_manager(frame, app),
        Mode::Plain => draw_plain(frame, app),
    }
}

fn draw_manager(frame: &mut Frame, app: &App) {
    let [tabs, pane, status] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(0),
        Constraint::Length(2),
    ])
    .areas(frame.area());
    draw_tabs(frame, app, tabs);
    draw_pane(frame, app, pane);
    let [status_session, status_machine] =
        Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(status);
    draw_status_session(frame, app, status_session);
    draw_status_machine(frame, app, status_machine);
    draw_modal(frame, app, pane);
}

/// `--plain`: the session fills the screen. A notice is a one-line overlay
/// at the bottom right until it times out; popups cover it.
fn draw_plain(frame: &mut Frame, app: &App) {
    let area = frame.area();
    draw_pane(frame, app, area);
    if let Some(notice) = &app.notice {
        let text = format!(" {} ", truncate(&notice.text, (area.width as usize).saturating_sub(2)));
        let width = (str_width(&text) as u16).min(area.width);
        if let Some(y) = area.bottom().checked_sub(1).filter(|_| width > 0) {
            let rect = Rect {
                x: area.right() - width,
                y,
                width,
                height: 1,
            };
            frame.render_widget(Clear, rect);
            frame.render_widget(
                Paragraph::new(text).style(Style::default().add_modifier(Modifier::REVERSED)),
                rect,
            );
        }
    }
    draw_modal(frame, app, area);
}

/// The open popup, if any, over `pane` (the area the history viewer fills).
fn draw_modal(frame: &mut Frame, app: &App, pane: Rect) {
    match &app.modal {
        Some(Modal::Rename { input, .. }) => draw_rename(frame, input),
        Some(Modal::Wizard(w)) => {
            let toggle_key = app
                .keymap
                .key_for(super::keymap::Action::ToggleHidden)
                .unwrap_or_default();
            draw_wizard(frame, w, app.now, &toggle_key)
        }
        Some(Modal::ProxyStats(view)) => draw_proxy_stats(frame, app, view),
        Some(Modal::Overview { selected, filter }) => draw_overview(frame, app, *selected, filter),
        Some(Modal::Help) => draw_help(frame, app),
        Some(Modal::Confirm(prompt)) => draw_confirm(frame, prompt),
        Some(Modal::Git(view)) => draw_git(frame, view, app.now, pane),
        None => {}
    }
}

// ── Tab bar ───────────────────────────────────────────────────────────────────

/// The state glyph for a tab, animated by `tick` while working.
///
/// `reconnecting` is `true` when the session's host connection has dropped —
/// shown as `⇄` (dim) regardless of the last known session state.
pub fn glyph(state: SessionState, tick: usize, reconnecting: bool) -> (&'static str, Style) {
    let s = Style::default();
    if reconnecting {
        return (RECONNECTING, s.add_modifier(Modifier::DIM));
    }
    match state {
        SessionState::Working => (SPINNER[tick % SPINNER.len()], s.fg(Color::Cyan)),
        SessionState::NeedsApproval => ("◆", s.fg(Color::Red)),
        SessionState::NeedsInput => ("?", s.fg(Color::Yellow)),
        SessionState::Idle => ("✓", s.fg(Color::Green)),
        SessionState::Error => ("✗", s.fg(Color::Red)),
        SessionState::Exited => ("○", s.add_modifier(Modifier::DIM)),
        SessionState::Starting | SessionState::Unknown => ("·", s),
    }
}

/// The glyph for a session's tab or overview row: `$` (bold green) for a
/// terminal, else its state glyph. A remote claude whose host connection has
/// dropped shows `⇄` instead of its last known state.
pub fn view_glyph(view: &SessionView, tick: usize) -> (&'static str, Style) {
    if view.kind == SessionKind::Shell {
        return (
            "$",
            Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
        );
    }
    let reconnecting = view.host != "local"
        && !view.attached
        && matches!(view.state, SessionState::Unknown);
    glyph(view.state, tick, reconnecting)
}

/// Proxy badge shown in the tab bar when the session uses a proxy.
const PROXY_BADGE: &str = "⇅";

/// The text of one tab with its label cut to `cap` columns:
/// ` {n} {glyph} {label} `, or ` {n} {glyph} ` when no label fits.
pub fn tab_parts(index: usize, label: &str, cap: usize) -> (String, String) {
    let label = truncate(label, cap);
    let suffix = if label.is_empty() {
        " ".to_owned()
    } else {
        format!(" {label} ")
    };
    (format!(" {index} "), suffix)
}

/// Like [`tab_parts`] but appends a proxy badge when `proxied` is true.
///
/// Kept for completeness; the tab bar no longer shows the badge (it was
/// removed to reduce visual noise — the status bar shows proxy info instead).
#[allow(dead_code)]
pub fn tab_parts_proxy(index: usize, label: &str, cap: usize, proxied: bool) -> (String, String) {
    let (prefix, mut suffix) = tab_parts(index, label, cap);
    if proxied {
        // Insert badge before the trailing space.
        let trimmed = suffix.trim_end_matches(' ');
        suffix = format!("{trimmed}{PROXY_BADGE} ");
    }
    (prefix, suffix)
}

/// Per-tab label widths so that every tab fits in `width` columns (tabs are
/// separated by one column). Inactive tabs shrink first, all to a common
/// cap; the active tab shrinks only once the others have no label left.
pub fn fit_tabs(labels: &[usize], active: Option<usize>, width: usize) -> Vec<usize> {
    let total = |caps: &[usize]| -> usize {
        let tabs: usize = caps
            .iter()
            .enumerate()
            .map(|(i, &cap)| {
                let digits = (i + 1).to_string().len();
                // " n " + glyph + " " [+ label + " "]
                digits + 4 + if cap > 0 { cap + 1 } else { 0 }
            })
            .sum();
        tabs + caps.len().saturating_sub(1)
    };
    let mut caps = labels.to_vec();
    if total(&caps) <= width {
        return caps;
    }
    let inactive_max = labels
        .iter()
        .enumerate()
        .filter(|(i, _)| Some(*i) != active)
        .map(|(_, &w)| w)
        .max();
    for cap in (0..inactive_max.unwrap_or(0)).rev() {
        for (i, c) in caps.iter_mut().enumerate() {
            if Some(i) != active {
                *c = labels[i].min(cap);
            }
        }
        if total(&caps) <= width {
            return caps;
        }
    }
    if let Some(a) = active.filter(|&a| a < caps.len()) {
        for cap in (0..labels[a]).rev() {
            caps[a] = cap;
            if total(&caps) <= width {
                return caps;
            }
        }
    }
    caps
}

/// The tab label for a session: `label@host` for remote sessions, and for
/// every terminal (`term@local`), where the host is part of what it is.
pub fn tab_label(view: &SessionView) -> String {
    let base = view.label();
    if view.host == "local" && view.kind == SessionKind::Claude {
        base
    } else {
        format!("{base}@{}", view.host)
    }
}

/// The tab texts as laid out for a bar `width` columns wide.
///
/// Tabs show only the session label (e.g. `api@devbox`). Age and proxy badges
/// are intentionally omitted to keep the bar uncluttered — the status bar
/// already shows host:cwd, state, and proxy info for the active session.
pub fn tab_titles(
    sessions: &[SessionView],
    active: Option<usize>,
    width: u16,
    _now: u64,
) -> Vec<(String, String)> {
    let labels: Vec<String> = sessions.iter().map(tab_label).collect();
    let label_widths: Vec<usize> = labels.iter().map(|l| str_width(l)).collect();
    let caps = fit_tabs(&label_widths, active, width as usize);
    labels
        .iter()
        .zip(caps)
        .enumerate()
        .map(|(i, (l, cap))| tab_parts(i + 1, l, cap))
        .collect()
}

/// Which tab is at column `col` of the bar, given its titles.
pub fn tab_at(titles: &[(String, String)], col: u16) -> Option<usize> {
    let col = col as usize;
    let mut x = 0;
    for (i, (prefix, suffix)) in titles.iter().enumerate() {
        let w = str_width(prefix) + 1 + str_width(suffix);
        if (x..x + w).contains(&col) {
            return Some(i);
        }
        x += w + 1;
    }
    None
}

fn draw_tabs(frame: &mut Frame, app: &App, area: Rect) {
    let titles = tab_titles(&app.sessions, app.active, area.width, app.now);
    let mut spans = Vec::new();
    for (i, ((prefix, suffix), view)) in titles.into_iter().zip(&app.sessions).enumerate() {
        if i > 0 {
            spans.push(Span::styled(
                "│",
                Style::default().add_modifier(Modifier::DIM),
            ));
        }
        let (g, gstyle) = view_glyph(view, app.tick);
        let base = if Some(i) == app.active {
            Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
        } else {
            // Steady color signals attention — no blinking which terminals
            // render inconsistently and users find aggressive.
            match view.state {
                SessionState::NeedsApproval | SessionState::Error => {
                    Style::default().fg(Color::LightRed).add_modifier(Modifier::BOLD)
                }
                SessionState::NeedsInput => {
                    Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)
                }
                _ => Style::default(),
            }
        };
        spans.push(Span::styled(prefix, base));
        spans.push(Span::styled(g, base.patch(gstyle)));
        spans.push(Span::styled(suffix, base));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

// ── Pane ──────────────────────────────────────────────────────────────────────

fn draw_pane(frame: &mut Frame, app: &App, area: Rect) {
    let Some(view) = app.active_view() else {
        let hint = Paragraph::new("No sessions. Alt+n starts one, Alt+q quits.")
            .style(Style::default().add_modifier(Modifier::DIM))
            .centered();
        let y = area.y + area.height / 2;
        frame.render_widget(
            hint,
            Rect {
                y,
                height: 1.min(area.height),
                ..area
            },
        );
        return;
    };
    view.mirror.render(area, frame.buffer_mut());
    if let Some(sel) = app.selection.as_ref().filter(|s| s.session == view.id) {
        highlight_selection(frame.buffer_mut(), area, sel);
    }
    if app.modal.is_none() {
        if let Some((col, row)) = view.mirror.cursor() {
            if col < area.width && row < area.height {
                frame.set_cursor_position((area.x + col, area.y + row));
            }
        }
    }
}

/// Show the selected cells of the pane at `area` in reverse video (cells
/// that already are reversed are shown plain, so they stand out too).
fn highlight_selection(buf: &mut ratatui::buffer::Buffer, area: Rect, sel: &Selection) {
    let (start, end) = sel.bounds();
    for row in start.row..=end.row.min(area.height.saturating_sub(1)) {
        for col in 0..area.width {
            if !sel.contains(row, col) {
                continue;
            }
            if let Some(cell) = buf.cell_mut((area.x + col, area.y + row)) {
                cell.modifier.toggle(Modifier::REVERSED);
            }
        }
    }
}

// ── Modals ────────────────────────────────────────────────────────────────────

/// A `width`×`height` rectangle centered in `area` (clamped to it).
fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}

/// Clear `rect`, draw a titled border and return the inner area.
fn popup(frame: &mut Frame, rect: Rect, title: &str) -> Rect {
    let block = Block::bordered().title(format!(" {title} "));
    let inner = block.inner(rect);
    frame.render_widget(Clear, rect);
    frame.render_widget(block, rect);
    inner
}

/// Render an input line and place the cursor after it.
fn draw_input(frame: &mut Frame, area: Rect, prompt: &str, input: &str) {
    if area.height == 0 {
        return;
    }
    let line = Rect { height: 1, ..area };
    // Keep the end of a long input visible.
    let room = (area.width as usize).saturating_sub(str_width(prompt) + 1);
    let shown = tail(input, room);
    frame.render_widget(Paragraph::new(format!("{prompt}{shown}")), line);
    let x = area.x + (str_width(prompt) + str_width(&shown)) as u16;
    frame.set_cursor_position((x.min(area.right().saturating_sub(1)), area.y));
}

fn draw_rename(frame: &mut Frame, input: &str) {
    let area = frame.area();
    let rect = centered(area, 60.min(area.width.saturating_sub(4)), 3);
    let inner = popup(frame, rect, "Rename session (Enter save · Esc cancel)");
    draw_input(frame, inner, "", input);
}

/// Render `rows` into `area`, highlighting `selected` and scrolling to keep
/// it visible.
/// `rows` with `selected` highlighted and scrolled into view; `None` for a
/// list without a selection (an unfocused section).
fn draw_list(frame: &mut Frame, area: Rect, rows: &[String], selected: Option<usize>) {
    let visible = area.height as usize;
    if visible == 0 {
        return;
    }
    let offset = selected.map_or(0, |i| i.saturating_sub(visible - 1));
    let lines: Vec<Line> = rows
        .iter()
        .enumerate()
        .skip(offset)
        .take(visible)
        .map(|(i, row)| {
            let text = truncate(row, area.width as usize);
            if Some(i) == selected {
                Line::styled(text, Style::default().add_modifier(Modifier::REVERSED))
            } else {
                Line::raw(text)
            }
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), area);
}

fn draw_wizard(frame: &mut Frame, w: &Wizard, now: u64, toggle_key: &str) {
    let area = frame.area();
    let rect = centered(
        area,
        90.min(area.width.saturating_sub(4)),
        22.min(area.height.saturating_sub(2)),
    );

    // Step 0: first screen (LOCAL / REMOTE sections).
    if let Some(hs) = &w.host_step {
        use super::wizard::HostSection;
        let inner = popup(
            frame,
            rect,
            "New session (↑/↓ move · Tab switch section · Enter pick · Esc cancel)",
        );
        if let Some(host) = &hs.connecting {
            frame.render_widget(
                Paragraph::new(format!("Connecting to {host}…"))
                    .style(Style::default().fg(Color::Cyan)),
                inner,
            );
            return;
        }
        let height = inner.height as usize;
        // Layout: filter (2) + LOCAL label (1) + local items + REMOTE label (1) + remote items.
        let local_count = hs.local_len().min((height.saturating_sub(4)) / 2);
        let [filter_area, rest] =
            ratatui::layout::Layout::vertical([Constraint::Length(2), Constraint::Min(0)])
                .areas(inner);
        draw_input(frame, filter_area, "filter> ", &hs.input);
        // Render LOCAL section header.
        let sections = ratatui::layout::Layout::vertical([
            Constraint::Length(1),              // LOCAL header
            Constraint::Length(local_count as u16), // local items
            Constraint::Length(1),              // REMOTE header
            Constraint::Min(0),                 // remote items
        ])
        .split(rest);
        let local_focused = hs.focus == HostSection::Local;
        let remote_focused = hs.focus == HostSection::Remote;
        let local_hdr_style = if local_focused {
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::DIM)
        };
        let remote_hdr_style = if remote_focused {
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
        } else {
            Style::default().add_modifier(Modifier::DIM)
        };
        frame.render_widget(
            Paragraph::new("── LOCAL ──────────────────────────────────────────────────")
                .style(local_hdr_style),
            sections[0],
        );
        // LOCAL items: "Explore local dirs…" + recent dirs, badged like the
        // directory list.
        let local_area = sections[1];
        let visible = local_area.height as usize;
        let selected = local_focused.then_some(hs.local_selected);
        let width = local_area.width as usize;
        let explore = truncate("  Explore local dirs…", width);
        let explore = if selected == Some(0) {
            Line::styled(explore, Style::default().add_modifier(Modifier::REVERSED))
        } else {
            Line::raw(explore)
        };
        let offset = selected.map_or(0, |i| i.saturating_sub(visible.saturating_sub(1)));
        let lines: Vec<Line> = std::iter::once(explore)
            .chain(hs.local_items.iter().enumerate().map(|(i, dir)| {
                let mut row = dir_row(w, dir, selected == Some(i + 1), width.saturating_sub(2), now);
                row.spans.insert(0, Span::raw("  "));
                row
            }))
            .skip(offset)
            .take(visible)
            .collect();
        frame.render_widget(Paragraph::new(lines), local_area);
        frame.render_widget(
            Paragraph::new("── REMOTE ─────────────────────────────────────────────────")
                .style(remote_hdr_style),
            sections[2],
        );
        // REMOTE items (plus optional synthetic "connect to <query>" row).
        let mut remote_rows: Vec<String> = hs.items.iter().map(|h| format!("  {h}")).collect();
        if hs.connect_raw {
            remote_rows.push(format!("  connect to {}", hs.input.trim()));
        }
        draw_list(
            frame,
            sections[3],
            &remote_rows,
            remote_focused.then_some(hs.selected),
        );
        return;
    }

    // Step 2: resume picker.
    if let Some(step) = &w.resume {
        // Proxy toggle row takes 1 line when profiles are available.
        let has_proxy = w.proxy_options.len() > 1;
        let inner = popup(
            frame,
            rect,
            "New session: resume? (Enter pick · type to filter · Esc back · ←/→ proxy)",
        );
        // Layout: cwd · [proxy] · filter · list.
        let mut constraints = vec![
            Constraint::Length(1), // cwd
            Constraint::Length(1), // filter
        ];
        if has_proxy {
            constraints.insert(1, Constraint::Length(1)); // proxy before filter
        }
        constraints.push(Constraint::Min(0)); // list
        let areas = Layout::vertical(constraints).split(inner);
        let cwd = Paragraph::new(w.display(&step.cwd))
            .style(Style::default().add_modifier(Modifier::DIM));
        frame.render_widget(cwd, areas[0]);
        let (filter_idx, list_idx) = if has_proxy {
            let sel = w
                .proxy_options
                .get(w.proxy_selected)
                .map(String::as_str)
                .unwrap_or("none");
            let proxy_line = format!("Proxy: ◀ {} ▶", sel);
            frame.render_widget(
                Paragraph::new(proxy_line).style(Style::default().fg(Color::Cyan)),
                areas[1],
            );
            (2, 3)
        } else {
            (1, 2)
        };
        draw_input(frame, areas[filter_idx], "filter> ", &step.filter);
        let filtered = step.filtered_sessions();
        let rows: Vec<String> = std::iter::once("+ New session".to_owned())
            .chain(filtered.iter().map(|s| resume_label(s, now)))
            .collect();
        draw_list(frame, areas[list_idx], &rows, Some(step.selected));
        return;
    }

    // Step 1: directory picker.
    let title = if w.host == "local" {
        "New session: directory (↑/↓ move · Tab open · Enter pick · Esc cancel)".to_owned()
    } else {
        format!(
            "New session on {} (↑/↓ move · Tab open · Enter pick · Esc cancel)",
            w.host
        )
    };
    let inner = popup(frame, rect, &title);
    let [head, list] = Layout::vertical([Constraint::Length(2), Constraint::Min(0)]).areas(inner);
    if let Some(dir) = &w.pending {
        frame.render_widget(
            Paragraph::new(format!("Looking for sessions in {}…", w.display(dir))),
            head,
        );
        return;
    }
    draw_input(frame, head, "> ", &w.input);
    // Second head row: browse-mode key hints and the hidden-dirs toggle state.
    if head.height > 1 {
        let state = if w.hidden_visible() { "on" } else { "off" };
        let hint = if w.browsing() {
            "→ open · ← up · Enter start session · "
        } else {
            ""
        };
        frame.render_widget(
            Paragraph::new(format!("{hint}{toggle_key} hidden: {state}"))
                .style(Style::default().add_modifier(Modifier::DIM)),
            Rect { y: head.y + 1, height: 1, ..head },
        );
    }
    draw_dir_list(frame, list, w, now);
}

/// Render the directory candidate list with right-aligned metadata badges.
///
/// Badges (right-aligned, only shown when metadata is available):
///   `⎇ <branch>` green   – git repo
///   `✻ <age>`   magenta  – last claude session
///   `↻`         cyan     – recently used via claudio
///   `↪`         dim      – symlink
///
/// Hidden directories are rendered with DIM. The selected row is reversed.
fn draw_dir_list(frame: &mut Frame, area: Rect, w: &Wizard, now: u64) {
    let visible = area.height as usize;
    if visible == 0 {
        return;
    }
    let selected = w.selected;
    let offset = selected.saturating_sub(visible - 1);
    let width = area.width as usize;

    // In browse mode row 0 is the "start here" pseudo-entry, ahead of the items.
    let here = w.here_dir().map(|dir| {
        let style = Style::default().fg(Color::Green).add_modifier(Modifier::BOLD);
        let style = if w.here_selected() {
            style.add_modifier(Modifier::REVERSED)
        } else {
            style
        };
        let text = truncate(&format!("▸ start here: {}", w.display(&dir)), width);
        let pad = " ".repeat(width.saturating_sub(str_width(&text)));
        Line::from(vec![Span::styled(text, style), Span::styled(pad, style)])
    });
    let first_item_row = usize::from(here.is_some());

    let lines: Vec<Line> = here
        .into_iter()
        .chain(w.items.iter().enumerate().map(|(i, path)| {
            dir_row(w, path, i + first_item_row == selected, width, now)
        }))
        .skip(offset)
        .take(visible)
        .collect();
    frame.render_widget(Paragraph::new(lines), area);
}

/// One directory row: the `~`-abbreviated path, then right-aligned badges
/// (symlink ↪, recently used ↻, claude activity ✻, git branch ⎇).
fn dir_row(w: &Wizard, path: &str, is_selected: bool, width: usize, now: u64) -> Line<'static> {
    let meta = w.meta.get(path);
    let display = w.display(path);
    let is_hidden = meta.map_or(false, |m| m.hidden);

    // Build badge text pieces (right-aligned).
    let mut badges: Vec<(String, Style)> = Vec::new();
    if let Some(m) = meta {
        if m.symlink {
            badges.push((" ↪".to_owned(), Style::default().add_modifier(Modifier::DIM)));
        }
        if m.recently_used {
            badges.push((" ↻".to_owned(), Style::default().fg(Color::Cyan)));
        }
        if let Some(at) = m.claude_at {
            let age = fmt_age(now.saturating_sub(at));
            badges.push((
                format!(" ✻ {age}"),
                Style::default().fg(Color::Magenta),
            ));
        }
        if let Some(branch) = &m.git {
            badges.push((
                format!(" ⎇ {branch}"),
                Style::default().fg(Color::Green),
            ));
        }
    }

    let badge_width: usize = badges.iter().map(|(s, _)| str_width(s)).sum();
    let label_max = width.saturating_sub(badge_width);
    let label = truncate(&display, label_max);
    let label_w = str_width(&label);

    let base_style = if is_selected {
        Style::default().add_modifier(Modifier::REVERSED)
    } else if is_hidden {
        Style::default().add_modifier(Modifier::DIM)
    } else {
        Style::default()
    };

    let mut spans = vec![Span::styled(label, base_style)];
    // Padding between label and badges.
    let pad = width.saturating_sub(label_w + badge_width);
    if pad > 0 {
        spans.push(Span::styled(" ".repeat(pad), base_style));
    }
    for (text, badge_style) in badges {
        let style = if is_selected {
            // Merge reversed background onto badge colour.
            badge_style.patch(Style::default().add_modifier(Modifier::REVERSED))
        } else {
            badge_style
        };
        spans.push(Span::styled(text, style));
    }
    Line::from(spans)
}

// ── Overview popup ────────────────────────────────────────────────────────────

/// Render the overview / "mission control" popup.
/// One row per session: `index glyph label host cwd state age [⇅]`.
pub fn draw_overview(frame: &mut Frame, app: &App, selected: usize, filter: &str) {
    use super::interaction::overview_matches;
    let area = frame.area();
    let filtered: Vec<(usize, &super::app::SessionView)> = app
        .sessions
        .iter()
        .enumerate()
        .filter(|(_, v)| overview_matches(v, filter))
        .collect();
    let height = (filtered.len() as u16 + 5).min(area.height.saturating_sub(2));
    let rect = centered(area, 100.min(area.width.saturating_sub(2)), height);
    let inner = popup(
        frame,
        rect,
        "Overview (↑/↓ select · Enter switch · type to filter · Esc close)",
    );

    // Split into filter input (1 line) + list.
    let [filter_area, list_area] =
        ratatui::layout::Layout::vertical([Constraint::Length(1), Constraint::Min(0)])
            .areas(inner);
    draw_input(frame, filter_area, "filter> ", filter);

    let visible = list_area.height as usize;
    let offset = selected.saturating_sub(visible.saturating_sub(1));

    let rows: Vec<Line> = filtered
        .iter()
        .enumerate()
        .skip(offset)
        .take(visible)
        .map(|(fi, (i, v))| {
            let (g, _) = view_glyph(v, app.tick);
            let cwd = abbreviate_home(&v.cwd, &app.home);
            let host_part = if v.host == "local" {
                cwd
            } else {
                format!("{}:{}", v.host, cwd)
            };
            let age = fmt_age(app.now.saturating_sub(v.created_at));
            let proxy_badge = if v.proxy.is_some() { PROXY_BADGE } else { " " };
            let state = match v.kind {
                SessionKind::Claude => v.state.name(),
                SessionKind::Shell => "terminal",
            };
            let label = v.label();
            let text = format!(
                " {:2} {} {:<20} {:<30} {:>14} {:>5} {}",
                i + 1,
                g,
                truncate(&label, 20),
                truncate(&host_part, 30),
                state,
                age,
                proxy_badge
            );
            let base = if fi == selected {
                Style::default().add_modifier(Modifier::REVERSED)
            } else if v.state.wants_attention() {
                Style::default().add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            Line::styled(text, base)
        })
        .collect();
    frame.render_widget(Paragraph::new(rows), list_area);
}

// ── Help popup ────────────────────────────────────────────────────────────────

/// Render the help popup listing all key bindings.
pub fn draw_help(frame: &mut Frame, app: &App) {
    let entries = app.keymap.help_entries();
    let extra = if app.upgrade_notice.is_some() { 2 } else { 0 };
    let height = (entries.len() as u16 + 6 + extra).min(frame.area().height.saturating_sub(2));
    let area = frame.area();
    let rect = centered(area, 72.min(area.width.saturating_sub(2)), height);
    let title = match app.mode {
        Mode::Manager => "Manager keys (any key closes)",
        Mode::Plain => "claudio keys (any key closes)",
    };
    let inner = popup(frame, rect, title);

    let mut lines: Vec<Line> = entries
        .iter()
        .map(|(k, desc)| {
            Line::from(vec![
                Span::styled(
                    format!("  {:>14}  ", k),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::raw(*desc),
            ])
        })
        .collect();
    // Claude Code's own key, not ours: listed because it closes the tab.
    lines.push(Line::from(""));
    let dim = Style::default().add_modifier(Modifier::DIM);
    let (ends, rest) = match app.mode {
        Mode::Manager => ("exit claude; the tab closes", None),
        Mode::Plain => (
            "exit claude; claudio exits too",
            Some("every other key goes to claude"),
        ),
    };
    lines.push(Line::from(vec![
        Span::styled(format!("  {:>14}  ", "Ctrl+D twice"), dim),
        Span::styled(ends, dim),
    ]));
    if let Some(rest) = rest {
        lines.push(Line::from(Span::styled(format!("  {rest}"), dim)));
    }
    // Show upgrade notice at the bottom of the help popup when available.
    if let Some(tag) = &app.upgrade_notice {
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::styled(
                format!("  ↑ newer claudio is available: {tag}"),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::DIM),
            ),
            Span::raw("  — run `claudio upgrade`"),
        ]));
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

// ── Wizard (proxy toggle) ─────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// The wizard's first screen, as drawn: LOCAL has the focus, and the
    /// unfocused REMOTE section must still list its hosts.
    #[test]
    fn wizard_first_screen_draws_both_sections() {
        use ratatui::{backend::TestBackend, Terminal};
        let mut cfg = crate::tui::test_support::config();
        cfg.ssh_hosts = vec!["prod-db".into(), "build-01".into()];
        cfg.recent_dirs.insert("local".into(), vec!["/home/u/web".into()]);
        let mut app = App::new(cfg);
        app.recover(&Default::default(), &[]);

        let mut term = Terminal::new(TestBackend::new(app.width, app.height)).unwrap();
        term.draw(|f| draw(f, &app)).unwrap();
        let buf = term.backend().buffer();
        let screen: String = (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
                    + "\n"
            })
            .collect();
        for expected in ["Explore local dirs", "~/web", "prod-db", "build-01"] {
            assert!(screen.contains(expected), "{expected} missing:\n{screen}");
        }
    }

    fn total_width(titles: &[(String, String)]) -> usize {
        titles
            .iter()
            .map(|(p, s)| str_width(p) + 1 + str_width(s))
            .sum::<usize>()
            + titles.len().saturating_sub(1)
    }

    #[test]
    fn tabs_fit_untouched_when_there_is_room() {
        assert_eq!(fit_tabs(&[5, 7, 3], Some(0), 200), vec![5, 7, 3]);
    }

    #[test]
    fn inactive_tabs_shrink_first_to_a_common_cap() {
        // Full: 3 × (5 + 10 + 1) + 2 separators = 50.
        let caps = fit_tabs(&[10, 10, 10], Some(1), 40);
        assert_eq!(caps[1], 10, "active keeps its label");
        assert!(caps[0] < 10 && caps[0] == caps[2]);
        // Short labels stay whole while long ones shrink.
        let caps = fit_tabs(&[2, 30, 30], Some(0), 40);
        assert_eq!(caps[0], 2);
        assert_eq!(caps[1], caps[2]);
    }

    #[test]
    fn active_shrinks_once_inactive_labels_are_gone() {
        // Inactive tabs without labels: 2 × 5 + separators 2 = 12; active
        // overhead 6 → 22 columns leave 4 for the active label.
        assert_eq!(fit_tabs(&[20, 20, 20], Some(1), 22), vec![0, 4, 0]);
    }

    #[test]
    fn fitted_titles_never_exceed_the_bar() {
        let labels: Vec<usize> = (0..8).map(|i| 5 + i * 3).collect();
        for width in [50usize, 80, 120] {
            let caps = fit_tabs(&labels, Some(3), width);
            let titles: Vec<(String, String)> = caps
                .iter()
                .enumerate()
                .map(|(i, &c)| tab_parts(i + 1, &"x".repeat(labels[i]), c))
                .collect();
            assert!(total_width(&titles) <= width, "width {width}: {titles:?}");
        }
    }

    #[test]
    fn tab_hit_testing_follows_layout() {
        let titles = vec![tab_parts(1, "api", 3), tab_parts(2, "docs", 4)];
        // " 1 ✓ api " is 9 columns, then a separator at 9.
        assert_eq!(tab_at(&titles, 0), Some(0));
        assert_eq!(tab_at(&titles, 8), Some(0));
        assert_eq!(tab_at(&titles, 9), None);
        assert_eq!(tab_at(&titles, 10), Some(1));
        assert_eq!(tab_at(&titles, 100), None);
    }

    #[test]
    fn tab_titles_show_label_only() {
        use uuid::Uuid;
        let now = 600u64;
        let make_view = |name: &str| SessionView {
            name: Some(name.to_owned()),
            ..SessionView::new(Uuid::new_v4(), "local", "/srv", SessionKind::Claude, (24, 80))
        };
        let sessions = vec![make_view("api"), make_view("docs")];
        // Wide bar: labels appear, no age.
        let titles_wide = tab_titles(&sessions, Some(0), 200, now);
        let wide_text: String = titles_wide.iter().map(|(p, s)| format!("{p}{s}")).collect();
        assert!(wide_text.contains("api"), "label should appear");
        assert!(!wide_text.contains("10m"), "age should not appear in tab bar");
        // Narrow bar: labels still present, just shorter.
        let titles_narrow = tab_titles(&sessions, Some(0), 20, now);
        let narrow_text: String = titles_narrow.iter().map(|(p, s)| format!("{p}{s}")).collect();
        assert!(str_width(&narrow_text) <= 21);
    }
}
