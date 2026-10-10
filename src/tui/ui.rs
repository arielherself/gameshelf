use std::time::Duration;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Gauge, List, ListItem, Paragraph, Tabs, Wrap};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use super::gamepad::{Layout as PadLayout, Pad};
use super::graphics::SlotState;
use super::{App, AvailEntry, InputKind, Level, Load, Modal, Pane, RuntimeScreen, Screen, Sort, View};
use crate::art::ArtKind;
use crate::lockfile::{Game, MatchState};
use crate::runtime::{self, Phase};
use crate::util::{format_ago, format_bytes, format_duration, now, tilde};

/// Colours. Body text uses the terminal's own foreground so both light and
/// dark themes work; these are for accents and de-emphasis.
mod theme {
    use ratatui::style::Color;

    pub const ACCENT: Color = Color::Rgb(203, 166, 247);
    pub const SUBTLE: Color = Color::Rgb(147, 153, 178);
    pub const MUTED: Color = Color::Rgb(108, 112, 134);
    pub const RULE: Color = Color::Rgb(69, 71, 90);
    pub const SELECTED_BG: Color = Color::Rgb(49, 50, 68);
    pub const SELECTED_FG: Color = Color::Rgb(205, 214, 244);
    pub const ON_ACCENT: Color = Color::Rgb(24, 24, 37);
    pub const GREEN: Color = Color::Rgb(166, 227, 161);
    pub const YELLOW: Color = Color::Rgb(249, 226, 175);
    pub const RED: Color = Color::Rgb(243, 139, 168);
}

const SPINNER: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];

pub fn draw(f: &mut Frame, app: &mut App) {
    app.overlay = None;
    let [top, _, main, status, keys] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(5),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(f.area());
    draw_topbar(f, app, top);
    match app.screen {
        Screen::Games => match app.view {
            View::List => draw_library(f, app, main),
            View::Shelf => draw_shelf(f, app, main),
        },
        Screen::Runtimes => draw_runtimes(f, app, main),
    }
    draw_status(f, app, status);
    draw_keys(f, app, keys);
    let pad = pad_hints(app);
    if let Some(modal) = &mut app.modal {
        app.overlay = Some(draw_modal(f, modal, pad));
    }
}

// -- helpers ----------------------------------------------------------------

/// The controller to show hints for, when the last input came from one.
fn pad_hints(app: &App) -> Option<PadLayout> {
    app.pad().filter(|_| app.pad_input).map(|(_, layout)| *layout)
}

/// A controller button as a key badge, in the button's colour.
fn pad_badge(layout: PadLayout, pad: Pad) -> Span<'static> {
    let (label, color) = layout.label(pad);
    let fg = color.unwrap_or(theme::SELECTED_FG);
    Span::styled(format!(" {label} "), Style::new().bg(theme::SELECTED_BG).fg(fg).bold())
}

fn key_badge(key: &str) -> Span<'static> {
    Span::styled(format!(" {key} "), Style::new().bg(theme::SELECTED_BG).fg(theme::SELECTED_FG))
}

/// Cut `s` to `width` display columns, ending in `…` if shortened.
fn truncate(s: &str, width: usize) -> String {
    if s.width() <= width {
        return s.to_string();
    }
    let mut out = String::new();
    let mut w = 0;
    for c in s.chars() {
        let cw = c.width().unwrap_or(0);
        if w + cw + 1 > width {
            break;
        }
        out.push(c);
        w += cw;
    }
    out.push('…');
    out
}

fn panel(title: impl Into<Line<'static>>, focused: bool) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(if focused { theme::ACCENT } else { theme::RULE }))
        .title(title)
}

/// Stable, pleasant colour for a game without artwork.
fn name_color(name: &str) -> Color {
    let hash = name.bytes().fold(2166136261u32, |h, b| (h ^ b as u32).wrapping_mul(16777619));
    let hue = (hash % 360) as f64;
    let (s, l) = (0.55, 0.72);
    let c = (1.0 - (2.0 * l - 1.0f64).abs()) * s;
    let x = c * (1.0 - ((hue / 60.0) % 2.0 - 1.0).abs());
    let m = l - c / 2.0;
    let (r, g, b) = match (hue / 60.0) as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    Color::Rgb(((r + m) * 255.0) as u8, ((g + m) * 255.0) as u8, ((b + m) * 255.0) as u8)
}

fn game_accent(app: &App, dir: &str, game: &Game) -> Color {
    game.appid
        .and_then(|id| app.gfx.accent(id))
        .unwrap_or_else(|| if game.appid.is_some() { theme::ACCENT } else { name_color(dir) })
}

fn release_year(date: &str) -> Option<&str> {
    date.split(|c: char| !c.is_ascii_digit()).find(|p| p.len() == 4)
}

/// "Developer · 2018 · Action, Adventure"
fn meta_line(game: &Game) -> Option<String> {
    let s = game.steam.as_ref()?;
    let mut parts = Vec::new();
    if let Some(d) = s.developers.first() {
        parts.push(d.clone());
    }
    if let Some(y) = release_year(&s.release_date) {
        parts.push(y.to_string());
    }
    if !s.genres.is_empty() {
        parts.push(s.genres.iter().take(3).cloned().collect::<Vec<_>>().join(", "));
    }
    (!parts.is_empty()).then(|| parts.join("  ·  "))
}

fn short_playtime(secs: u64) -> String {
    match secs {
        0 => String::new(),
        s if s < 60 => "<1m".into(),
        s if s < 3600 => format!("{}m", s / 60),
        s => format!("{}h", s / 3600),
    }
}

// -- top bar ------------------------------------------------------------------

fn draw_topbar(f: &mut Frame, app: &App, area: Rect) {
    let mut left = vec![
        Span::styled(" ◆ gameshelf ", Style::new().bg(theme::ACCENT).fg(theme::ON_ACCENT).bold()),
        Span::raw("  "),
        Span::styled(tilde(&app.lib.root), Style::new().fg(theme::SUBTLE)),
    ];
    if app.screen == Screen::Runtimes {
        left.push(Span::styled("  ›  Proton runtimes", Style::new().fg(theme::SUBTLE)));
    }
    let mut right = Vec::new();
    let spinner = SPINNER[(app.tick as usize / 2) % SPINNER.len()];
    if app.lookup_running {
        right.push(Span::styled(format!("{spinner} matching on Steam   "), Style::new().fg(theme::ACCENT)));
    }
    if !app.running.is_empty() {
        right
            .push(Span::styled(format!("▶ {} running   ", app.running.len()), Style::new().fg(theme::GREEN)));
    }
    if app.offline {
        right.push(Span::styled("offline   ", Style::new().fg(theme::MUTED)));
    }
    if let Some((name, _)) = app.pad() {
        let color = if app.pad_input { theme::ACCENT } else { theme::MUTED };
        right.push(Span::styled(format!("🎮 {}   ", truncate(name, 28)), Style::new().fg(color)));
    }
    right.push(Span::styled(format!("{} games ", app.lock.games.len()), Style::new().fg(theme::MUTED)));
    f.render_widget(Paragraph::new(Line::from(left)), area);
    f.render_widget(Paragraph::new(Line::from(right).right_aligned()), area);
}

// -- library (list) view -----------------------------------------------------------

fn draw_library(f: &mut Frame, app: &mut App, area: Rect) {
    let sidebar_w = (area.width / 3).clamp(28, 40);
    let [sidebar, _, detail] =
        Layout::horizontal([Constraint::Length(sidebar_w), Constraint::Length(2), Constraint::Min(20)])
            .areas(area);
    draw_sidebar(f, app, sidebar);
    draw_detail(f, app, detail);
}

fn status_mark(app: &App, dir: &str, g: &Game) -> (&'static str, Color) {
    if app.is_running(dir) {
        ("▶", theme::GREEN)
    } else if !app.lib.game_dir(dir).is_dir() {
        ("✗", theme::RED)
    } else if g.match_state == MatchState::Pending {
        ("…", theme::MUTED)
    } else if g.appid.is_none() {
        ("?", theme::YELLOW)
    } else {
        (" ", theme::MUTED)
    }
}

fn draw_sidebar(f: &mut Frame, app: &mut App, area: Rect) {
    let [header, list_area] = Layout::vertical([Constraint::Length(2), Constraint::Min(1)]).areas(area);
    let mut head = vec![Span::styled("LIBRARY", Style::new().fg(theme::SUBTLE).bold())];
    if app.sort == Sort::Recent {
        head.push(Span::styled("  · recent", Style::new().fg(theme::MUTED)));
    }
    if !app.filter.is_empty() {
        head.push(Span::styled(format!("  /{}", app.filter), Style::new().fg(theme::ACCENT)));
    }
    f.render_widget(Paragraph::new(Line::from(head)), header);

    if app.rows.is_empty() {
        let msg = if app.lock.games.is_empty() {
            format!(
                "No games yet.\n\nEvery directory in {} is a game. Add one and press s.",
                tilde(&app.lib.root)
            )
        } else {
            "Nothing matches the filter.\nEsc clears it.".into()
        };
        f.render_widget(Paragraph::new(msg).fg(theme::MUTED).wrap(Wrap { trim: false }), list_area);
        return;
    }

    let width = list_area.width as usize;
    let selected = app.list_state.selected();
    let items: Vec<ListItem> = app
        .rows
        .iter()
        .enumerate()
        .map(|(i, dir)| {
            let g = &app.lock.games[dir];
            let is_sel = selected == Some(i);
            let (mark, mark_color) = status_mark(app, dir, g);
            let right = short_playtime(g.playtime_secs);
            let name_w = width.saturating_sub(5 + right.width() + 1);
            let name = truncate(g.display_name(dir), name_w);
            let bar = if is_sel { "▌" } else { " " };
            let accent = game_accent(app, dir, g);
            let name_style = if is_sel { Style::new().fg(theme::SELECTED_FG).bold() } else { Style::new() };
            let used = 1 + 2 + name.width();
            let gap = width.saturating_sub(used + right.width() + 1);
            ListItem::new(Line::from(vec![
                Span::styled(bar, Style::new().fg(accent)),
                Span::styled(format!("{mark} "), Style::new().fg(mark_color)),
                Span::styled(name, name_style),
                Span::raw(" ".repeat(gap)),
                Span::styled(right, Style::new().fg(theme::MUTED)),
                Span::raw(" "),
            ]))
        })
        .collect();
    let list = List::new(items).highlight_style(Style::new().bg(theme::SELECTED_BG));
    f.render_stateful_widget(list, list_area, &mut app.list_state);
}

fn draw_detail(f: &mut Frame, app: &mut App, area: Rect) {
    let Some((dir, game)) = app.selected_game().map(|(d, g)| (d.to_string(), g.clone())) else {
        return;
    };
    let accent = game_accent(app, &dir, &game);
    let artwork = app.gfx.enabled() && game.appid.is_some();
    let mut rest = area;

    // Hero banner across the top, cropped to fill.
    if let (true, Some(appid)) = (artwork, game.appid) {
        let natural = (area.width as f64 / (ArtKind::Hero.aspect() * app.gfx.cell_ratio())).round() as u16;
        let rows = natural.min(area.height * 2 / 5).max(4);
        let hero = Rect { height: rows.min(area.height), ..area };
        let state = app.gfx.slot(appid, ArtKind::Hero, hero);
        if state == SlotState::Loading {
            draw_loading(f, hero, app.tick);
        }
        if state != SlotState::Missing {
            rest = Rect { y: hero.bottom() + 1, height: area.height.saturating_sub(hero.height + 1), ..area };
        }
    }

    // Cover on the left of the info column.
    let mut info = rest;
    if let (true, Some(appid)) = (artwork, game.appid) {
        let cover_area = Rect { width: rest.width / 3, height: rest.height.min(22), ..rest };
        let cover = app.gfx.fit(ArtKind::Cover, cover_area);
        let cover = Rect { x: rest.x, ..cover };
        let state =
            if cover.width >= 6 { app.gfx.slot(appid, ArtKind::Cover, cover) } else { SlotState::Missing };
        if state == SlotState::Loading {
            draw_loading(f, cover, app.tick);
        }
        if state != SlotState::Missing {
            info = Rect { x: cover.right() + 3, width: rest.width.saturating_sub(cover.width + 3), ..rest };
        }
    }

    let lines = info_lines(app, &dir, &game, accent, info.width);
    f.render_widget(Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }), info);
}

fn section(title: &str, width: u16) -> Line<'static> {
    let rule = "─".repeat((width as usize).saturating_sub(title.width() + 1));
    Line::from(vec![
        Span::styled(title.to_string(), Style::new().fg(theme::SUBTLE).bold()),
        Span::styled(format!(" {rule}"), Style::new().fg(theme::RULE)),
    ])
}

fn kv(k: &str, v: impl Into<String>) -> Line<'static> {
    Line::from(vec![Span::styled(format!("{k:<12}"), Style::new().fg(theme::MUTED)), Span::raw(v.into())])
}

fn kv_styled(k: &str, v: impl Into<String>, style: Style) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{k:<12}"), Style::new().fg(theme::MUTED)),
        Span::styled(v.into(), style),
    ])
}

fn info_lines(app: &App, dir: &str, g: &Game, accent: Color, width: u16) -> Vec<Line<'static>> {
    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(Span::styled(g.display_name(dir).to_string(), Style::new().fg(accent).bold())));
    if let Some(meta) = meta_line(g) {
        lines.push(Line::from(Span::styled(meta, Style::new().fg(theme::SUBTLE))));
    }
    let source = match (g.appid, g.match_state) {
        (Some(id), MatchState::Manual) => {
            Span::styled(format!("Steam {id} · set by hand"), Style::new().fg(theme::MUTED))
        }
        (Some(id), _) => Span::styled(format!("Steam {id}"), Style::new().fg(theme::MUTED)),
        (None, MatchState::Pending) if app.offline => {
            Span::styled("not looked up (offline)", Style::new().fg(theme::MUTED))
        }
        (None, MatchState::Pending) => Span::styled("looking up on Steam…", Style::new().fg(theme::MUTED)),
        (None, _) => {
            Span::styled("No Steam match — m to search, a to set an app id", Style::new().fg(theme::YELLOW))
        }
    };
    lines.push(Line::from(vec![Span::styled(format!("{dir}  ·  "), Style::new().fg(theme::MUTED)), source]));
    lines.push(Line::default());

    // Primary action.
    let mut action = Vec::new();
    if let Some(s) = app.running.get(dir) {
        action.push(Span::styled(" ● RUNNING ", Style::new().bg(theme::GREEN).fg(theme::ON_ACCENT).bold()));
        action.push(Span::styled(
            format!("  {} · x to stop", format_duration(now().saturating_sub(s.started))),
            Style::new().fg(theme::SUBTLE),
        ));
    } else if app.launching.contains_key(dir) {
        action.push(Span::styled(" ● STARTING ", Style::new().bg(theme::GREEN).fg(theme::ON_ACCENT).bold()));
    } else {
        action.push(Span::styled(" ▶ PLAY ", Style::new().bg(accent).fg(theme::ON_ACCENT).bold()));
        match pad_hints(app) {
            Some(style) => {
                action.push(Span::raw("  "));
                action.push(pad_badge(style, Pad::South));
            }
            None => action.push(Span::styled("  Enter", Style::new().fg(theme::MUTED))),
        }
    }
    match g.last_played {
        Some(t) => action.push(Span::styled(
            format!("     {} played · last {}", format_duration(g.playtime_secs), format_ago(t, now())),
            Style::new().fg(theme::SUBTLE),
        )),
        None => action.push(Span::styled("     never played", Style::new().fg(theme::MUTED))),
    }
    lines.push(Line::from(action));
    if !app.lib.game_dir(dir).is_dir() {
        lines.push(Line::from(Span::styled(
            "✗ directory missing from the library",
            Style::new().fg(theme::RED),
        )));
    }

    if let Some(desc) = g.steam.as_ref().map(|s| s.short_description.as_str()).filter(|d| !d.is_empty()) {
        lines.push(Line::default());
        lines.push(Line::from(desc.to_string()));
    }

    lines.push(Line::default());
    lines.push(section("LAUNCH", width));
    match &g.executable {
        Some(e) => {
            let exists = app.lib.executable_path(dir, g).is_some_and(|p| p.is_file());
            lines.push(kv_styled(
                "Executable",
                e.clone(),
                if exists { Style::new() } else { Style::new().fg(theme::RED) },
            ));
        }
        None => lines.push(kv_styled("Executable", "none found — press e", Style::new().fg(theme::RED))),
    }
    let shared: Vec<&str> = app
        .lock
        .games
        .iter()
        .filter(|(d, o)| o.prefix == g.prefix && d.as_str() != dir)
        .map(|(d, _)| d.as_str())
        .collect();
    let mut prefix = vec![Span::raw(g.prefix.clone())];
    if !app.lib.prefix_path(&g.prefix).exists() {
        prefix.push(Span::styled("  new", Style::new().fg(theme::MUTED)));
    }
    if !shared.is_empty() {
        prefix.push(Span::styled(
            format!("  shared with {}", shared.join(", ")),
            Style::new().fg(theme::MUTED),
        ));
    }
    let mut prefix_line = vec![Span::styled(format!("{:<12}", "Prefix"), Style::new().fg(theme::MUTED))];
    prefix_line.extend(prefix);
    lines.push(Line::from(prefix_line));
    match runtime::resolve(&app.lib, &app.lock, g) {
        Ok((name, _)) if g.runtime.is_some() => lines.push(kv("Runtime", name)),
        Ok((name, _)) => lines.push(Line::from(vec![
            Span::styled(format!("{:<12}", "Runtime"), Style::new().fg(theme::MUTED)),
            Span::raw(name),
            Span::styled("  default", Style::new().fg(theme::MUTED)),
        ])),
        Err(e) => lines.push(kv_styled("Runtime", format!("{e:#}"), Style::new().fg(theme::RED))),
    }
    if !g.args.is_empty() {
        lines.push(kv("Arguments", shlex::try_join(g.args.iter().map(String::as_str)).unwrap_or_default()));
    }
    for (k, v) in &g.env {
        lines.push(kv("Env", format!("{k}={v}")));
    }
    if let Some(id) = &g.game_id {
        lines.push(kv("GAMEID", id.clone()));
    }
    let log = crate::launch::log_path(dir);
    if log.exists() {
        lines.push(kv_styled("Log", tilde(&log), Style::new().fg(theme::SUBTLE)));
    }
    lines
}

// -- shelf (grid) view -------------------------------------------------------------

fn draw_shelf(f: &mut Frame, app: &mut App, area: Rect) {
    let [grid, _, info] =
        Layout::vertical([Constraint::Min(5), Constraint::Length(1), Constraint::Length(1)]).areas(area);
    if app.rows.is_empty() {
        f.render_widget(Paragraph::new("No games to show.").fg(theme::MUTED), grid);
        app.grid_cols = 1;
        return;
    }

    // Tile: a cover framed by one cell of border, then the title.
    let cover_w: u16 = if area.width >= 150 {
        20
    } else if area.width >= 100 {
        18
    } else {
        14
    };
    let cover_h = ((cover_w as f64 / (ArtKind::Cover.aspect() * app.gfx.cell_ratio())).round() as u16).max(4);
    let (tile_w, tile_h, gap) = (cover_w + 2, cover_h + 3, 2u16);
    let cols = ((grid.width + gap) / (tile_w + gap)).max(1) as usize;
    let visible_rows = (grid.height / tile_h).max(1) as usize;
    app.grid_cols = cols;

    let selected = app.list_state.selected().unwrap_or(0);
    let sel_row = selected / cols;
    if sel_row < app.grid_scroll {
        app.grid_scroll = sel_row;
    } else if sel_row >= app.grid_scroll + visible_rows {
        app.grid_scroll = sel_row + 1 - visible_rows;
    }
    let total_w = cols as u16 * tile_w + (cols as u16 - 1) * gap;
    let x0 = grid.x + grid.width.saturating_sub(total_w) / 2;

    let first = app.grid_scroll * cols;
    let last = (first + visible_rows * cols).min(app.rows.len());
    for idx in first..last {
        let dir = app.rows[idx].clone();
        let g = app.lock.games[&dir].clone();
        let (r, c) = ((idx - first) / cols, (idx - first) % cols);
        let tile = Rect {
            x: x0 + c as u16 * (tile_w + gap),
            y: grid.y + r as u16 * tile_h,
            width: tile_w,
            height: tile_h,
        };
        let is_sel = idx == selected;
        let accent = game_accent(app, &dir, &g);
        let frame = Rect { height: cover_h + 2, ..tile };
        if is_sel {
            f.render_widget(
                Block::bordered().border_type(BorderType::Rounded).border_style(Style::new().fg(accent)),
                frame,
            );
        }
        let cover = Rect { x: frame.x + 1, y: frame.y + 1, width: cover_w, height: cover_h };
        let state = match g.appid {
            Some(appid) => app.gfx.slot(appid, ArtKind::Cover, cover),
            None => SlotState::Missing,
        };
        if state != SlotState::Ready {
            draw_card(f, cover, g.display_name(&dir), name_color(&dir), state == SlotState::Loading);
        }

        let (mark, mark_color) = status_mark(app, &dir, &g);
        let title_w = tile_w as usize - 2;
        let name = truncate(g.display_name(&dir), title_w);
        let title_area = Rect { y: frame.bottom(), height: 1, ..tile };
        let style = if is_sel { Style::new().fg(accent).bold() } else { Style::new().fg(theme::SUBTLE) };
        let mut spans = Vec::new();
        if mark != " " {
            spans.push(Span::styled(format!("{mark} "), Style::new().fg(mark_color)));
        }
        spans.push(Span::styled(name, style));
        f.render_widget(Paragraph::new(Line::from(spans).centered()), title_area);
    }

    // Scroll hint and details of the selected game.
    let pages = app.rows.len().div_ceil(cols);
    if pages > visible_rows {
        let pos = format!("{}/{} ", sel_row + 1, pages);
        f.render_widget(Paragraph::new(Line::from(pos).right_aligned().fg(theme::MUTED)), info);
    }
    if let Some((dir, g)) = app.selected_game() {
        let mut parts = vec![Span::styled(g.display_name(dir).to_string(), Style::new().bold())];
        if let Some(meta) = meta_line(g) {
            parts.push(Span::styled(format!("   {meta}"), Style::new().fg(theme::SUBTLE)));
        }
        if g.playtime_secs > 0 {
            parts.push(Span::styled(
                format!("   {} played", format_duration(g.playtime_secs)),
                Style::new().fg(theme::MUTED),
            ));
        }
        f.render_widget(Paragraph::new(Line::from(parts)), info);
    }
}

/// Placeholder while artwork loads: a faint frame with a spinner.
fn draw_loading(f: &mut Frame, area: Rect, tick: u64) {
    let block = Block::bordered().border_type(BorderType::Rounded).border_style(Style::new().fg(theme::RULE));
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.height > 0 {
        let spinner = SPINNER[(tick as usize / 2) % SPINNER.len()];
        let mid = Rect { y: inner.y + inner.height / 2, height: 1, ..inner };
        f.render_widget(Paragraph::new(Line::from(spinner).centered().fg(theme::MUTED)), mid);
    }
}

/// Stand-in for missing artwork: a framed card with the title.
fn draw_card(f: &mut Frame, area: Rect, name: &str, color: Color, loading: bool) {
    let color = if loading { theme::RULE } else { color };
    let block = Block::new()
        .borders(Borders::ALL)
        .border_type(BorderType::Plain)
        .border_style(Style::new().fg(color));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let text_style = if loading { Style::new().fg(theme::MUTED) } else { Style::new().fg(color).bold() };
    let lines: Vec<Line> = wrap_words(name, inner.width.saturating_sub(2) as usize)
        .into_iter()
        .map(|l| Line::from(Span::styled(l, text_style)).centered())
        .collect();
    let top = inner.y + inner.height.saturating_sub(lines.len() as u16) / 2;
    let text_area = Rect { y: top, height: inner.bottom().saturating_sub(top), ..inner };
    f.render_widget(Paragraph::new(lines), text_area);
}

fn wrap_words(s: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines: Vec<String> = Vec::new();
    let mut cur = String::new();
    for word in s.split_whitespace() {
        let word = truncate(word, width);
        if !cur.is_empty() && cur.width() + 1 + word.width() > width {
            lines.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push(' ');
        }
        cur.push_str(&word);
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    lines
}

// -- runtimes -----------------------------------------------------------------------

fn draw_runtimes(f: &mut Frame, app: &mut App, area: Rect) {
    let rs = &app.runtimes;
    let [top, bottom] =
        Layout::vertical([Constraint::Min(5), Constraint::Length(if rs.download.is_some() { 3 } else { 0 })])
            .areas(area);
    let [left, _, right] =
        Layout::horizontal([Constraint::Percentage(45), Constraint::Length(1), Constraint::Percentage(55)])
            .areas(top);
    let highlight = Style::new().bg(theme::SELECTED_BG).fg(theme::SELECTED_FG).bold();

    let default = app.lock.settings.default_runtime.as_deref();
    let newest = rs.installed.iter().find(|r| r.valid).map(|r| r.name.as_str());
    let installed: Vec<ListItem> = rs
        .installed
        .iter()
        .map(|r| {
            let mut spans = vec![Span::raw(r.name.clone())];
            if default == Some(r.name.as_str()) {
                spans.push(Span::styled("  default", Style::new().fg(theme::GREEN)));
            } else if default.is_none() && newest == Some(r.name.as_str()) {
                spans.push(Span::styled("  default (newest)", Style::new().fg(theme::GREEN)));
            }
            if r.link_target.is_some() {
                spans.push(Span::styled("  link", Style::new().fg(theme::MUTED)));
            }
            if !r.valid {
                spans.push(Span::styled("  broken", Style::new().fg(theme::RED)));
            }
            let users = runtime::users_of(&app.lock, &r.name).len();
            if users > 0 {
                spans.push(Span::styled(format!("  {users} game(s)"), Style::new().fg(theme::MUTED)));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();
    let focus_inst = rs.focus == Pane::Installed;
    let title = Line::from(vec![
        Span::styled(" Installed ", Style::new().bold()),
        Span::styled(format!("{} ", tilde(&app.lib.runtimes_dir())), Style::new().fg(theme::MUTED)),
    ]);
    if installed.is_empty() {
        f.render_widget(
            Paragraph::new("No runtimes yet.\n\nPick one on the right and press Enter to install it.")
                .wrap(Wrap { trim: false })
                .fg(theme::MUTED)
                .block(panel(title, focus_inst)),
            left,
        );
    } else {
        let list = List::new(installed)
            .block(panel(title, focus_inst))
            .highlight_style(highlight)
            .highlight_symbol("▌");
        f.render_stateful_widget(list, left, &mut app.runtimes.installed_state);
    }

    let rs = &app.runtimes;
    let outer = panel(Span::styled(" Available ", Style::new().bold()), rs.focus == Pane::Available);
    let inner = outer.inner(right);
    f.render_widget(outer, right);
    let [tabs_area, list_area] = Layout::vertical([Constraint::Length(2), Constraint::Min(1)]).areas(inner);
    let tabs = Tabs::new(RuntimeScreen::TABS.iter().map(|t| Line::from(*t)))
        .select(rs.tab)
        .style(Style::new().fg(theme::MUTED))
        .highlight_style(Style::new().fg(theme::ACCENT).bold().underlined())
        .divider(Span::styled("·", Style::new().fg(theme::RULE)));
    f.render_widget(tabs, tabs_area);

    let installed_names: Vec<&str> = rs.installed.iter().map(|r| r.name.as_str()).collect();
    let spinner = SPINNER[(app.tick as usize / 2) % SPINNER.len()];
    let status_text = match rs.source() {
        Some(src) => match rs.releases.get(src.key()) {
            None | Some(Load::Loading) => {
                Some(format!("{spinner} Fetching releases from github.com/{}…", src.repo()))
            }
            Some(Load::Failed(e)) => Some(format!("Couldn't list releases: {e}\n\nPress u to retry.")),
            Some(Load::Ready(l)) if l.is_empty() => Some("No releases for this architecture.".into()),
            Some(Load::Ready(_)) => None,
        },
        None => None,
    };
    let entries = rs.entries();
    if let Some(text) = status_text {
        f.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }).fg(theme::MUTED), list_area);
    } else if entries.is_empty() {
        f.render_widget(
            Paragraph::new("No Proton builds found in Steam's compatibilitytools.d.").fg(theme::MUTED),
            list_area,
        );
    } else {
        let items: Vec<ListItem> = entries
            .iter()
            .map(|e| match e {
                AvailEntry::Release(a) => {
                    let mut spans = vec![
                        Span::raw(a.name.clone()),
                        Span::styled(
                            format!("  {}  {}", a.published, format_bytes(a.size)),
                            Style::new().fg(theme::MUTED),
                        ),
                    ];
                    if installed_names.contains(&a.name.as_str()) {
                        spans.push(Span::styled("  installed", Style::new().fg(theme::GREEN)));
                    }
                    ListItem::new(Line::from(spans))
                }
                AvailEntry::Steam { name, path } => {
                    let mut spans = vec![
                        Span::raw(name.clone()),
                        Span::styled(format!("  {}", tilde(path)), Style::new().fg(theme::MUTED)),
                    ];
                    if installed_names.contains(&name.as_str()) {
                        spans.push(Span::styled("  linked", Style::new().fg(theme::GREEN)));
                    }
                    ListItem::new(Line::from(spans))
                }
            })
            .collect();
        let list = List::new(items).highlight_style(highlight).highlight_symbol("▌");
        f.render_stateful_widget(list, list_area, &mut app.runtimes.available_state);
    }

    if let Some(d) = &app.runtimes.download {
        let p = d.progress;
        let (label, ratio) = match p.phase {
            Phase::Downloading => (
                format!("Downloading {}  {} / {}", d.name, format_bytes(p.done), format_bytes(p.total)),
                p.done as f64 / p.total.max(1) as f64,
            ),
            Phase::Verifying => (format!("Verifying {}", d.name), 1.0),
            Phase::Extracting => (format!("Extracting {}", d.name), p.done as f64 / p.total.max(1) as f64),
        };
        let gauge = Gauge::default()
            .block(panel(Span::styled(" Install · c to cancel ", Style::new().bold()), false))
            .gauge_style(Style::new().fg(theme::ACCENT).bg(theme::SELECTED_BG))
            .ratio(ratio.clamp(0.0, 1.0))
            .label(Span::styled(label, Style::new().fg(theme::SELECTED_FG).bold()));
        f.render_widget(gauge, bottom);
    }
}

// -- status and keys -------------------------------------------------------------------

fn draw_status(f: &mut Frame, app: &App, area: Rect) {
    let Some((msg, level, at)) = &app.status else { return };
    // Info messages fade after a while; problems stay until replaced.
    if msg.is_empty() || (*level == Level::Info && at.elapsed() > Duration::from_secs(8)) {
        return;
    }
    let (icon, color) = match level {
        Level::Info => ("•", theme::SUBTLE),
        Level::Ok => ("✓", theme::GREEN),
        Level::Warn => ("!", theme::YELLOW),
        Level::Error => ("✗", theme::RED),
    };
    let line = Line::from(vec![
        Span::styled(format!(" {icon} "), Style::new().fg(color).bold()),
        Span::styled(msg.clone(), Style::new().fg(color)),
    ]);
    f.render_widget(Paragraph::new(line), area);
}

fn draw_keys(f: &mut Frame, app: &App, area: Rect) {
    if let Some(style) = pad_hints(app) {
        draw_pad_keys(f, app, style, area);
        return;
    }
    let keys: &[(&str, &str)] = match (&app.modal, app.screen) {
        (Some(Modal::Input { .. }), _) => &[("Enter", "ok"), ("Esc", "cancel")],
        (Some(Modal::Select { .. }), _) => &[("↑↓", "move"), ("Enter", "choose"), ("Esc", "cancel")],
        (Some(Modal::Confirm { .. }), _) => &[("y", "yes"), ("n", "no")],
        (Some(Modal::Help), _) => &[("any key", "close")],
        (None, Screen::Games) => &[
            ("⏎", "play"),
            ("x", "stop"),
            ("v", "view"),
            ("/", "filter"),
            ("m", "match"),
            ("a", "app id"),
            ("e", "exe"),
            ("p", "prefix"),
            ("r", "runtime"),
            ("R", "runtimes"),
            ("?", "help"),
            ("q", "quit"),
        ],
        (None, Screen::Runtimes) => &[
            ("Tab", "pane"),
            ("←→", "source"),
            ("⏎", "install/default"),
            ("d", "delete"),
            ("u", "refresh"),
            ("Esc", "back"),
        ],
    };
    let mut spans = vec![Span::raw(" ")];
    for (k, v) in keys {
        spans.push(key_badge(k));
        spans.push(Span::styled(format!(" {v}  "), Style::new().fg(theme::MUTED)));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Hints for controller input: the same actions, labelled with the buttons
/// of the connected controller's family.
fn draw_pad_keys(f: &mut Frame, app: &App, style: PadLayout, area: Rect) {
    use Pad::*;
    let keys: &[(&[Pad], &str)] = match (&app.modal, app.screen) {
        (Some(Modal::Input { .. }), _) => &[(&[South], "ok"), (&[East], "cancel")],
        (Some(Modal::Select { .. }), _) => &[(&[Up], "move"), (&[South], "choose"), (&[East], "cancel")],
        (Some(Modal::Confirm { .. }), _) => &[(&[South], "yes"), (&[East], "no")],
        (Some(Modal::Help), _) => &[(&[South], "close")],
        (None, Screen::Games) => &[
            (&[Up], "move"),
            (&[South], "play"),
            (&[West], "stop"),
            (&[North], "view"),
            (&[Lb, Rb], "page"),
            (&[Start], "menu"),
            (&[Select], "help"),
        ],
        (None, Screen::Runtimes) => &[
            (&[Up], "move · source"),
            (&[South], "install/default"),
            (&[West], "delete"),
            (&[North], "refresh"),
            (&[Lb, Rb], "pane"),
            (&[East], "back"),
        ],
    };
    let mut spans = vec![Span::raw(" ")];
    for (pads, v) in keys {
        for (i, pad) in pads.iter().enumerate() {
            if i > 0 {
                spans.push(Span::raw(" "));
            }
            spans.push(pad_badge(style, *pad));
        }
        spans.push(Span::styled(format!(" {v}  "), Style::new().fg(theme::MUTED)));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

// -- modals -----------------------------------------------------------------------

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width.saturating_sub(2));
    let h = height.min(area.height.saturating_sub(2));
    Rect { x: area.x + (area.width - w) / 2, y: area.y + (area.height - h) / 2, width: w, height: h }
}

fn modal_block(title: &str) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(theme::ACCENT))
        .title(Span::styled(format!(" {title} "), Style::new().fg(theme::ACCENT).bold()))
}

/// Draws the popup and returns the area it covers.
fn draw_modal(f: &mut Frame, modal: &mut Modal, pad: Option<PadLayout>) -> Rect {
    let area = f.area();
    match modal {
        Modal::Input { kind, title, hint, input } => {
            let width = (area.width * 3 / 5).max(50);
            let rect = if matches!(kind, InputKind::Filter) {
                // Keep the list visible while filtering.
                Rect { x: 1, y: area.height.saturating_sub(6), width: width.min(area.width - 2), height: 3 }
            } else {
                centered(area, width, 3)
            };
            f.render_widget(Clear, rect);
            let b = modal_block(title)
                .title_bottom(Line::from(Span::styled(format!(" {hint} "), Style::new().fg(theme::MUTED))));
            let inner = b.inner(rect);
            f.render_widget(Paragraph::new(input.text().to_string()).block(b), rect);
            let x = inner.x + Line::from(input.before_cursor()).width() as u16;
            f.set_cursor_position((x.min(inner.right().saturating_sub(1)), inner.y));
            rect
        }
        Modal::Select { title, items, state, .. } => {
            let longest = items.iter().map(|i| i.label.width() + i.detail.width() + 6).max().unwrap_or(20);
            let width = (longest as u16 + 5).clamp(44, area.width.saturating_sub(4));
            let rect = centered(area, width, items.len() as u16 + 2);
            f.render_widget(Clear, rect);
            let list_items: Vec<ListItem> = items
                .iter()
                .map(|i| {
                    let mut spans = vec![
                        Span::styled(if i.current { "● " } else { "  " }, Style::new().fg(theme::GREEN)),
                        Span::raw(i.label.clone()),
                    ];
                    if !i.detail.is_empty() {
                        spans.push(Span::styled(format!("  {}", i.detail), Style::new().fg(theme::MUTED)));
                    }
                    ListItem::new(Line::from(spans))
                })
                .collect();
            let list = List::new(list_items)
                .block(modal_block(title))
                .highlight_style(Style::new().bg(theme::SELECTED_BG).fg(theme::SELECTED_FG).bold())
                .highlight_symbol("▌");
            f.render_stateful_widget(list, rect, state);
            rect
        }
        Modal::Confirm { message, .. } => {
            let rect = centered(area, 58, message.lines().count() as u16 + 4);
            f.render_widget(Clear, rect);
            let mut lines: Vec<Line> = message.lines().map(|l| Line::from(l.to_string())).collect();
            lines.push(Line::default());
            lines.push(Line::from(match pad {
                Some(style) => vec![
                    pad_badge(style, Pad::South),
                    Span::raw(" yes   "),
                    pad_badge(style, Pad::East),
                    Span::raw(" no"),
                ],
                None => vec![
                    Span::styled(" y ", Style::new().bg(theme::ACCENT).fg(theme::ON_ACCENT).bold()),
                    Span::raw(" yes   "),
                    key_badge("n"),
                    Span::raw(" no"),
                ],
            }));
            f.render_widget(
                Paragraph::new(lines).wrap(Wrap { trim: false }).block(modal_block("Confirm")),
                rect,
            );
            rect
        }
        Modal::Help => {
            let rows: &[(&str, &str)] = &[
                ("GAMES", ""),
                ("Enter", "launch the selected game (detached; output goes to its log)"),
                ("x", "stop the selected game (kills it and its wine processes)"),
                ("v", "switch between the library list and the cover shelf"),
                ("j/k h/l g/G", "move (h/l and ←/→ move sideways on the shelf)"),
                ("/", "filter by name · Esc clears"),
                ("o", "sort by name / recently played"),
                ("s", "rescan the library for new directories"),
                ("m", "search Steam and pick the matching game"),
                ("a", "set the Steam app id by hand (empty = none, auto = re-match)"),
                ("e", "choose the executable"),
                ("p", "choose the wine prefix (new, shared, or custom path)"),
                ("r", "choose the Proton runtime for this game"),
                ("A", "edit launch arguments"),
                ("R", "manage Proton runtimes"),
                ("", ""),
                ("RUNTIMES", ""),
                ("Tab", "switch between installed and available"),
                ("←/→", "switch download source"),
                ("Enter", "install (available) · toggle library default (installed)"),
                ("d", "delete an installed runtime"),
                ("c", "cancel the running download"),
                ("u", "refresh the release list"),
                ("", ""),
                ("q / Ctrl-C", "quit"),
            ];
            let mut lines: Vec<Line> = rows
                .iter()
                .map(|(k, v)| {
                    if v.is_empty() {
                        Line::from(Span::styled(k.to_string(), Style::new().fg(theme::SUBTLE).bold()))
                    } else {
                        Line::from(vec![
                            Span::styled(format!("  {k:<14}"), Style::new().fg(theme::ACCENT)),
                            Span::raw(v.to_string()),
                        ])
                    }
                })
                .collect();
            // With a controller in use, its buttons come first so they're
            // visible even when the help doesn't fit.
            if let Some(style) = pad {
                let mut pad_lines = Vec::new();
                pad_lines.push(Line::from(Span::styled(
                    format!("CONTROLLER ({})", style.kind),
                    Style::new().fg(theme::SUBTLE).bold(),
                )));
                let pad_rows: &[(&[Pad], &str)] = &[
                    (&[Pad::Up], "move (D-pad or left stick)"),
                    (&[Pad::South], "play · confirm"),
                    (&[Pad::East], "back · cancel"),
                    (&[Pad::West], "stop the game"),
                    (&[Pad::North], "switch list / shelf"),
                    (&[Pad::Lb, Pad::Rb], "page (pane on the runtimes screen)"),
                    (&[Pad::Start], "menu: settings, matching, runtimes, quit…"),
                    (&[Pad::Select], "this help"),
                ];
                for (pads, what) in pad_rows {
                    let mut spans = vec![Span::raw("  ")];
                    let mut width = 2;
                    for p in *pads {
                        let badge = pad_badge(style, *p);
                        width += badge.width() + 1;
                        spans.push(badge);
                        spans.push(Span::raw(" "));
                    }
                    spans.push(Span::raw(" ".repeat(16usize.saturating_sub(width))));
                    spans.push(Span::raw(what.to_string()));
                    pad_lines.push(Line::from(spans));
                }
                pad_lines.push(Line::default());
                pad_lines.append(&mut lines);
                lines = pad_lines;
            }
            let rect = centered(area, 84, lines.len() as u16 + 2);
            f.render_widget(Clear, rect);
            f.render_widget(Paragraph::new(lines).block(modal_block("Help")), rect);
            rect
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncates_by_display_width() {
        assert_eq!(truncate("Celeste", 10), "Celeste");
        assert_eq!(truncate("Order of the Sinking Star", 10), "Order of …");
        assert_eq!(truncate("星之海洋传说", 7), "星之海…");
        assert_eq!(truncate("abc", 0), "…");
    }

    #[test]
    fn wraps_titles_for_cards() {
        assert_eq!(wrap_words("Order of the Sinking Star", 10), ["Order of", "the", "Sinking", "Star"]);
        assert_eq!(wrap_words("Supercalifragilistic", 6), ["Super…"]);
    }

    #[test]
    fn years_and_colors() {
        assert_eq!(release_year("8 Oct, 2026"), Some("2026"));
        assert_eq!(release_year("Jan 25, 2018"), Some("2018"));
        assert_eq!(release_year("Coming soon"), None);
        assert_eq!(name_color("Paws"), name_color("Paws"));
        assert_ne!(name_color("Paws"), name_color("Celeste"));
    }
}
