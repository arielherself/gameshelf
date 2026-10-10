use std::time::Duration;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Clear, Gauge, List, ListItem, Paragraph, Tabs, Wrap};

use super::{App, AvailEntry, InputKind, Level, Load, Modal, Pane, RuntimeScreen, Screen, Sort};
use crate::lockfile::MatchState;
use crate::runtime::{self, Phase};
use crate::util::{format_ago, format_bytes, format_duration, now, tilde};

const ACCENT: Color = Color::Cyan;
const SPINNER: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];

pub fn draw(f: &mut Frame, app: &mut App) {
    let [main, status, keys] =
        Layout::vertical([Constraint::Min(5), Constraint::Length(1), Constraint::Length(1)]).areas(f.area());
    match app.screen {
        Screen::Games => draw_games(f, app, main),
        Screen::Runtimes => draw_runtimes(f, app, main),
    }
    draw_status(f, app, status);
    draw_keys(f, app, keys);
    if let Some(modal) = &mut app.modal {
        draw_modal(f, modal);
    }
}

fn block(title: impl Into<Line<'static>>, focused: bool) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(if focused { Style::new().fg(ACCENT) } else { Style::new().dark_gray() })
        .title(title)
}

fn draw_games(f: &mut Frame, app: &mut App, area: Rect) {
    let [left, right] =
        Layout::horizontal([Constraint::Percentage(36), Constraint::Percentage(64)]).areas(area);

    let items: Vec<ListItem> = app
        .rows
        .iter()
        .map(|dir| {
            let g = &app.lock.games[dir];
            let (mark, style) = if app.is_running(dir) {
                ("▶ ", Style::new().green())
            } else if !app.lib.game_dir(dir).is_dir() {
                ("✗ ", Style::new().red())
            } else if g.match_state == MatchState::Pending {
                ("… ", Style::new().dark_gray())
            } else if g.appid.is_none() {
                ("? ", Style::new().yellow())
            } else {
                ("  ", Style::new())
            };
            ListItem::new(Line::from(vec![
                Span::styled(mark, style),
                Span::raw(g.display_name(dir).to_string()),
            ]))
        })
        .collect();

    let mut title = vec![Span::raw(" Games ").bold(), Span::raw(format!("{} ", app.rows.len())).dark_gray()];
    if !app.filter.is_empty() {
        title.push(Span::raw(format!("/{} ", app.filter)).fg(ACCENT));
    }
    if app.sort == Sort::Recent {
        title.push(Span::raw("· recent ").dark_gray());
    }
    let lib_title = Line::from(format!(" {} ", tilde(&app.lib.root))).dark_gray().right_aligned();
    let list = List::new(items)
        .block(block(Line::from(title.clone()), true).title_bottom(lib_title))
        .highlight_style(Style::new().bg(Color::DarkGray).add_modifier(Modifier::BOLD))
        .highlight_symbol("›");
    if app.rows.is_empty() {
        let msg = if app.lock.games.is_empty() {
            format!(
                "No games yet.\n\nEvery directory in {} is treated as a game.\nAdd one and press s to rescan.",
                tilde(&app.lib.root)
            )
        } else {
            "Nothing matches the filter (Esc clears it).".into()
        };
        f.render_widget(
            Paragraph::new(msg).wrap(Wrap { trim: false }).dark_gray().block(block(Line::from(title), true)),
            left,
        );
    } else {
        f.render_stateful_widget(list, left, &mut app.list_state);
    }

    draw_details(f, app, right);
}

fn kv(k: &str, v: impl Into<String>) -> Line<'static> {
    Line::from(vec![Span::raw(format!("{k:<12}")).dark_gray(), Span::raw(v.into())])
}

fn kv_styled(k: &str, v: impl Into<String>, style: Style) -> Line<'static> {
    Line::from(vec![Span::raw(format!("{k:<12}")).dark_gray(), Span::styled(v.into(), style)])
}

fn section(title: &str) -> Line<'static> {
    Line::from(Span::raw(format!("─ {title} ")).fg(ACCENT))
}

fn draw_details(f: &mut Frame, app: &App, area: Rect) {
    let Some((dir, g)) = app.selected_game() else {
        f.render_widget(block(" Details ", false), area);
        return;
    };
    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(g.display_name(dir).to_string()).bold().fg(Color::White));

    let appid = match (g.appid, g.match_state) {
        (Some(id), MatchState::Manual) => Span::raw(format!("Steam app {id} (manual)")),
        (Some(id), _) => Span::raw(format!("Steam app {id}")),
        (None, MatchState::Pending) if app.offline => Span::raw("not looked up (offline)").dark_gray(),
        (None, MatchState::Pending) => Span::raw("looking up on Steam…").dark_gray(),
        (None, _) => Span::raw("no Steam match — m to search, a to set an app id").yellow(),
    };
    lines.push(Line::from(vec![Span::raw(format!("{dir} · ")).dark_gray(), appid]));
    if let Some(s) = app.running.get(dir) {
        lines.push(Line::from(vec![
            Span::raw(format!("▶ running for {}", format_duration(now().saturating_sub(s.started))))
                .green()
                .bold(),
            Span::raw("  x to stop").dark_gray(),
        ]));
    } else if app.launching.contains_key(dir) {
        lines.push(Line::from("▶ starting…").green());
    }
    if !app.lib.game_dir(dir).is_dir() {
        lines.push(Line::from("✗ directory missing from the library").red());
    }

    if let Some(s) = &g.steam {
        lines.push(Line::default());
        if !s.developers.is_empty() {
            lines.push(kv("Developer", s.developers.join(", ")));
        }
        if !s.publishers.is_empty() && s.publishers != s.developers {
            lines.push(kv("Publisher", s.publishers.join(", ")));
        }
        if !s.release_date.is_empty() {
            lines.push(kv("Released", s.release_date.clone()));
        }
        if !s.genres.is_empty() {
            lines.push(kv("Genres", s.genres.join(", ")));
        }
        if !s.short_description.is_empty() {
            lines.push(Line::default());
            lines.push(Line::from(s.short_description.clone()).italic());
        }
    }

    lines.push(Line::default());
    lines.push(section("Launch"));
    match &g.executable {
        Some(e) => {
            let exists = app.lib.executable_path(dir, g).is_some_and(|p| p.is_file());
            lines.push(kv_styled(
                "Executable",
                e.clone(),
                if exists { Style::new() } else { Style::new().red() },
            ));
        }
        None => lines.push(kv_styled("Executable", "none found — press e", Style::new().red())),
    }
    let prefix_path = app.lib.prefix_path(&g.prefix);
    let shared: Vec<&str> = app
        .lock
        .games
        .iter()
        .filter(|(d, o)| o.prefix == g.prefix && d.as_str() != dir)
        .map(|(d, _)| d.as_str())
        .collect();
    let mut prefix = g.prefix.clone();
    if !prefix_path.exists() {
        prefix.push_str(" (new)");
    }
    if !shared.is_empty() {
        prefix.push_str(&format!(" · shared with {}", shared.join(", ")));
    }
    lines.push(kv("Prefix", prefix));
    match runtime::resolve(&app.lib, &app.lock, g) {
        Ok((name, _)) if g.runtime.is_some() => lines.push(kv("Runtime", name)),
        Ok((name, _)) => lines.push(Line::from(vec![
            Span::raw(format!("{:<12}", "Runtime")).dark_gray(),
            Span::raw(name),
            Span::raw(" (default)").dark_gray(),
        ])),
        Err(e) => lines.push(kv_styled("Runtime", format!("{e:#}"), Style::new().red())),
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

    lines.push(Line::default());
    lines.push(section("Activity"));
    match g.last_played {
        Some(t) => {
            lines.push(kv("Last played", format_ago(t, now())));
            lines.push(kv("Playtime", format_duration(g.playtime_secs)));
        }
        None => lines.push(Line::from("Never played").dark_gray()),
    }
    let log = crate::launch::log_path(dir);
    if log.exists() {
        lines.push(kv("Log", tilde(&log)));
    }

    f.render_widget(
        Paragraph::new(Text::from(lines)).wrap(Wrap { trim: false }).block(block(" Details ", false)),
        area,
    );
}

fn draw_runtimes(f: &mut Frame, app: &mut App, area: Rect) {
    let rs = &app.runtimes;
    let [top, bottom] =
        Layout::vertical([Constraint::Min(5), Constraint::Length(if rs.download.is_some() { 3 } else { 0 })])
            .areas(area);
    let [left, right] =
        Layout::horizontal([Constraint::Percentage(45), Constraint::Percentage(55)]).areas(top);

    // Installed
    let default = app.lock.settings.default_runtime.as_deref();
    let newest = rs.installed.iter().find(|r| r.valid).map(|r| r.name.as_str());
    let installed: Vec<ListItem> = rs
        .installed
        .iter()
        .map(|r| {
            let mut spans = vec![Span::raw(r.name.clone())];
            if default == Some(r.name.as_str()) {
                spans.push(Span::raw("  default").green());
            } else if default.is_none() && newest == Some(r.name.as_str()) {
                spans.push(Span::raw("  default (newest)").green());
            }
            if r.link_target.is_some() {
                spans.push(Span::raw("  link").dark_gray());
            }
            if !r.valid {
                spans.push(Span::raw("  broken").red());
            }
            let users = runtime::users_of(&app.lock, &r.name).len();
            if users > 0 {
                spans.push(Span::raw(format!("  {users} game(s)")).dark_gray());
            }
            ListItem::new(Line::from(spans))
        })
        .collect();
    let focus_inst = rs.focus == Pane::Installed;
    let title = format!(" Installed {} ", tilde(&app.lib.runtimes_dir()));
    if installed.is_empty() {
        f.render_widget(
            Paragraph::new("No runtimes yet.\n\nPick one on the right and press Enter to install it.")
                .wrap(Wrap { trim: false })
                .dark_gray()
                .block(block(title, focus_inst)),
            left,
        );
    } else {
        let list = List::new(installed)
            .block(block(title, focus_inst))
            .highlight_style(Style::new().bg(Color::DarkGray).add_modifier(Modifier::BOLD))
            .highlight_symbol("›");
        f.render_stateful_widget(list, left, &mut app.runtimes.installed_state);
    }

    // Available
    let rs = &app.runtimes;
    let focus_avail = rs.focus == Pane::Available;
    let outer = block(" Available ", focus_avail);
    let inner = outer.inner(right);
    f.render_widget(outer, right);
    let [tabs_area, list_area] = Layout::vertical([Constraint::Length(2), Constraint::Min(1)]).areas(inner);
    let tabs = Tabs::new(RuntimeScreen::TABS.iter().map(|t| Line::from(*t)))
        .select(rs.tab)
        .highlight_style(Style::new().fg(ACCENT).bold().underlined())
        .divider("│");
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
        f.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }).dark_gray(), list_area);
    } else if entries.is_empty() {
        f.render_widget(
            Paragraph::new("No Proton builds found in Steam's compatibilitytools.d.").dark_gray(),
            list_area,
        );
    } else {
        let items: Vec<ListItem> = entries
            .iter()
            .map(|e| match e {
                AvailEntry::Release(a) => {
                    let mut spans = vec![
                        Span::raw(a.name.clone()),
                        Span::raw(format!("  {}  {}", a.published, format_bytes(a.size))).dark_gray(),
                    ];
                    if installed_names.contains(&a.name.as_str()) {
                        spans.push(Span::raw("  installed").green());
                    }
                    ListItem::new(Line::from(spans))
                }
                AvailEntry::Steam { name, path } => {
                    let mut spans =
                        vec![Span::raw(name.clone()), Span::raw(format!("  {}", tilde(path))).dark_gray()];
                    if installed_names.contains(&name.as_str()) {
                        spans.push(Span::raw("  linked").green());
                    }
                    ListItem::new(Line::from(spans))
                }
            })
            .collect();
        let list = List::new(items)
            .highlight_style(Style::new().bg(Color::DarkGray).add_modifier(Modifier::BOLD))
            .highlight_symbol("›");
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
            .block(block(" Install · c to cancel ", false))
            .gauge_style(Style::new().fg(ACCENT).bg(Color::Black))
            .ratio(ratio.clamp(0.0, 1.0))
            .label(label);
        f.render_widget(gauge, bottom);
    }
}

fn draw_status(f: &mut Frame, app: &App, area: Rect) {
    let spinner = SPINNER[(app.tick as usize / 2) % SPINNER.len()];
    let mut spans = Vec::new();
    if app.lookup_running {
        spans.push(Span::raw(format!("{spinner} Steam lookup  ")).fg(ACCENT));
    }
    if !app.running.is_empty() {
        spans.push(Span::raw(format!("▶ {} running  ", app.running.len())).green());
    }
    if app.offline {
        spans.push(Span::raw("offline  ").dark_gray());
    }
    if let Some((msg, level, at)) = &app.status {
        // Info messages fade after a while; problems stay until replaced.
        let stale = *level == Level::Info && at.elapsed() > Duration::from_secs(8);
        if !stale && !msg.is_empty() {
            let style = match level {
                Level::Info => Style::new(),
                Level::Ok => Style::new().green(),
                Level::Warn => Style::new().yellow(),
                Level::Error => Style::new().red(),
            };
            spans.push(Span::styled(msg.clone(), style));
        }
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_keys(f: &mut Frame, app: &App, area: Rect) {
    let keys: &[(&str, &str)] = match (&app.modal, app.screen) {
        (Some(Modal::Input { .. }), _) => &[("Enter", "ok"), ("Esc", "cancel")],
        (Some(Modal::Select { .. }), _) => &[("↑↓", "move"), ("Enter", "choose"), ("Esc", "cancel")],
        (Some(Modal::Confirm { .. }), _) => &[("y", "yes"), ("n", "no")],
        (Some(Modal::Help), _) => &[("any key", "close")],
        (None, Screen::Games) => &[
            ("Enter", "play"),
            ("x", "stop"),
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
            ("Enter", "install/default"),
            ("d", "delete"),
            ("u", "refresh"),
            ("Esc", "back"),
        ],
    };
    let mut spans = Vec::new();
    for (k, v) in keys {
        spans.push(Span::raw(format!(" {k} ")).fg(ACCENT).bold());
        spans.push(Span::raw(format!("{v} ")).dark_gray());
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let w = width.min(area.width.saturating_sub(2));
    let h = height.min(area.height.saturating_sub(2));
    Rect { x: area.x + (area.width - w) / 2, y: area.y + (area.height - h) / 2, width: w, height: h }
}

fn draw_modal(f: &mut Frame, modal: &mut Modal) {
    let area = f.area();
    match modal {
        Modal::Input { kind, title, hint, input } => {
            let width = (area.width * 3 / 5).max(50);
            let rect = if matches!(kind, InputKind::Filter) {
                // Keep the list visible while filtering.
                Rect { x: 1, y: area.height.saturating_sub(6), width: width.min(area.width - 2), height: 4 }
            } else {
                centered(area, width, 4)
            };
            f.render_widget(Clear, rect);
            let b = block(Line::from(format!(" {title} ")).bold(), true)
                .title_bottom(Line::from(format!(" {hint} ")).dark_gray());
            let inner = b.inner(rect);
            f.render_widget(Paragraph::new(input.text().to_string()).block(b), rect);
            let x = inner.x + Line::from(input.before_cursor()).width() as u16;
            f.set_cursor_position((x.min(inner.right().saturating_sub(1)), inner.y));
        }
        Modal::Select { title, items, state, .. } => {
            let longest = items
                .iter()
                .map(|i| i.label.chars().count() + i.detail.chars().count() + 6)
                .max()
                .unwrap_or(20);
            let width = (longest as u16 + 5).clamp(40, area.width.saturating_sub(4));
            let rect = centered(area, width, items.len() as u16 + 2);
            f.render_widget(Clear, rect);
            let list_items: Vec<ListItem> = items
                .iter()
                .map(|i| {
                    let mut spans = vec![
                        Span::raw(if i.current { "• " } else { "  " }).green(),
                        Span::raw(i.label.clone()),
                    ];
                    if !i.detail.is_empty() {
                        spans.push(Span::raw(format!("  {}", i.detail)).dark_gray());
                    }
                    ListItem::new(Line::from(spans))
                })
                .collect();
            let list = List::new(list_items)
                .block(block(Line::from(format!(" {title} ")).bold(), true))
                .highlight_style(Style::new().bg(Color::DarkGray).add_modifier(Modifier::BOLD))
                .highlight_symbol("›");
            f.render_stateful_widget(list, rect, state);
        }
        Modal::Confirm { message, .. } => {
            let rect = centered(area, 56, message.lines().count() as u16 + 4);
            f.render_widget(Clear, rect);
            let text = format!("{message}\n\n[y]es / [n]o");
            f.render_widget(
                Paragraph::new(text).wrap(Wrap { trim: false }).block(block(" Confirm ", true)),
                rect,
            );
        }
        Modal::Help => {
            let rows: &[(&str, &str)] = &[
                ("Games", ""),
                ("Enter", "launch the selected game (detached; output goes to its log)"),
                ("x", "stop the selected game (kills it and its wine processes)"),
                ("j/k ↑/↓ g/G", "move"),
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
                ("Runtimes", ""),
                ("Tab", "switch between installed and available"),
                ("←/→", "switch download source"),
                ("Enter", "install (available) · toggle library default (installed)"),
                ("d", "delete an installed runtime"),
                ("c", "cancel the running download"),
                ("u", "refresh the release list"),
                ("", ""),
                ("q / Ctrl-C", "quit"),
            ];
            let lines: Vec<Line> = rows
                .iter()
                .map(|(k, v)| {
                    if v.is_empty() {
                        Line::from(Span::raw(k.to_string()).fg(ACCENT).bold())
                    } else {
                        Line::from(vec![Span::raw(format!("  {k:<14}")).bold(), Span::raw(v.to_string())])
                    }
                })
                .collect();
            let rect = centered(area, 82, lines.len() as u16 + 2);
            f.render_widget(Clear, rect);
            f.render_widget(Paragraph::new(lines).block(block(" Help ", true)), rect);
        }
    }
}
