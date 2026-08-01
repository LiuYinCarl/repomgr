//! All rendering. The app state lives in `main.rs`; this module only draws
//! it, so the UI can be tweaked without touching the logic.

use std::path::Path;

use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, Borders, Clear, HighlightSpacing, List, ListItem, ListState, Paragraph, Wrap,
    },
    Frame,
};

use crate::{git, App, Mode};

pub struct View;

const HELP_KEYS: &[(&str, &str)] = &[
    ("↑ ↓ / j k", "select repository"),
    ("g / G", "first / last repository"),
    ("u", "update selected repo (git pull --ff-only)"),
    ("s / Enter", "show git status"),
    ("b", "show recent commits"),
    ("l", "show local branches"),
    ("n", "clone a repository into the current dir"),
    ("o / O", "open folder / open remote in browser"),
    ("r / R", "rescan directory / reload info"),
    ("PgUp / PgDn", "scroll info panel or modal"),
    ("h / ?", "show help"),
    ("q / Esc / Ctrl-C", "quit"),
];

const BROWSE_HINTS: &str =
    "↑↓ j/k select · u update · s status · b log · l branches · n clone · o open · O remote · r rescan · R reload · h help · q quit";

impl View {
    pub fn draw(app: &App, f: &mut Frame) {
        let size = f.size();
        let chunks = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .split(size);

        Self::draw_title_bar(app, f, chunks[0]);
        Self::draw_main(app, f, chunks[1]);
        Self::draw_status_bar(app, f, chunks[2]);

        match app.mode {
            Mode::Browse => {}
            Mode::Help => Self::draw_help(f, size),
            Mode::InputClone => Self::draw_clone_input(app, f, size),
            Mode::Working
            | Mode::Status
            | Mode::Log
            | Mode::Branches
            | Mode::UpdateResult
            | Mode::Message => Self::draw_modal(app, f, size),
        }
    }

    fn draw_title_bar(app: &App, f: &mut Frame, area: Rect) {
        let text = Line::from(vec![
            Span::styled(
                " repomgr ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            if app.scanned {
                Span::styled(
                    format!(
                        "{} repos in {}",
                        app.repos.len(),
                        truncate(&git::sanitize(&app.root.display().to_string()), 60)
                    ),
                    Style::default().fg(Color::DarkGray),
                )
            } else {
                Span::styled(
                    "scanning…",
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::ITALIC),
                )
            },
        ]);
        f.render_widget(Paragraph::new(text), area);
    }

    fn draw_main(app: &App, f: &mut Frame, area: Rect) {
        let columns = Layout::horizontal([Constraint::Percentage(38), Constraint::Percentage(62)])
            .split(area);

        Self::draw_repo_list(app, f, columns[0]);
        Self::draw_info_panel(app, f, columns[1]);
    }

    fn draw_repo_list(app: &App, f: &mut Frame, area: Rect) {
        let items: Vec<ListItem> = app
            .repos
            .iter()
            .map(|path| {
                let name = git::sanitize(&path.file_name().unwrap_or_default().to_string_lossy());
                ListItem::new(name)
            })
            .collect();

        let list = List::new(items)
            .highlight_style(
                Style::default()
                    .add_modifier(Modifier::REVERSED)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("> ")
            .highlight_spacing(HighlightSpacing::Always)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(format!(" Repos ({}) ", app.repos.len())),
            );

        let mut state = ListState::default();
        state.select((!app.repos.is_empty()).then_some(app.selected));
        f.render_stateful_widget(list, area, &mut state);
    }

    fn draw_info_panel(app: &App, f: &mut Frame, area: Rect) {
        let title = match app.current_name() {
            Some(name) => format!(" {name} "),
            None => " Info ".to_string(),
        };
        let block = Block::default().borders(Borders::ALL).title(title);

        let Some(path) = app.current_path() else {
            let empty = if !app.scanned {
                "scanning for git repositories…".to_string()
            } else if app.repos.is_empty() {
                format!(
                    "no git repositories found in {}",
                    git::sanitize(&app.root.display().to_string())
                )
            } else {
                "select a repository".to_string()
            };
            f.render_widget(
                Paragraph::new(Line::raw(empty))
                    .block(block)
                    .wrap(Wrap { trim: true }),
                area,
            );
            return;
        };

        let Some(info) = app.current_info() else {
            f.render_widget(
                Paragraph::new(Line::raw("loading repository info…"))
                    .block(block)
                    .wrap(Wrap { trim: true }),
                area,
            );
            return;
        };

        let lines = info_lines(path, &info);
        let max_scroll = lines.len().saturating_sub(2) as u16;
        let paragraph = Paragraph::new(lines)
            .block(block)
            .scroll((app.scroll.min(max_scroll), 0))
            .wrap(Wrap { trim: false });
        f.render_widget(paragraph, area);
    }

    fn draw_status_bar(app: &App, f: &mut Frame, area: Rect) {
        let text: String = if app.mode == Mode::Browse {
            app.status_msg
                .clone()
                .unwrap_or_else(|| BROWSE_HINTS.to_string())
        } else {
            match app.mode {
                Mode::Help => "h / ? close help · Esc / q back".to_string(),
                Mode::InputClone => "Enter clone · Esc cancel".to_string(),
                Mode::Working => "working…".to_string(),
                _ => "↑/↓ j/k scroll · h help · Esc / q close".to_string(),
            }
        };
        f.render_widget(
            Paragraph::new(text).style(Style::default().fg(Color::DarkGray)),
            area,
        );
    }

    fn draw_help(f: &mut Frame, area: Rect) {
        let rect = centered_rect(58, 62, area);
        let mut lines: Vec<Line> = HELP_KEYS
            .iter()
            .map(|(key, desc)| {
                Line::from(vec![
                    Span::styled(format!("  {key:<16}"), Style::default().fg(Color::Cyan)),
                    Span::styled(*desc, Style::default().fg(Color::Gray)),
                ])
            })
            .collect();
        lines.insert(
            0,
            Line::from(Span::styled(
                " Keybindings ",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            )),
        );
        lines.push(Line::raw(""));
        lines.push(Line::from(Span::styled(
            " h / ? or Esc / q to close ",
            Style::default().fg(Color::DarkGray),
        )));

        let widget =
            Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" Help "));
        f.render_widget(Clear, rect);
        f.render_widget(widget, rect);
    }

    fn draw_clone_input(app: &App, f: &mut Frame, area: Rect) {
        let rect = centered_rect(64, 3, area);
        let placeholder = app.input.is_empty();
        let value = if placeholder {
            "git@github.com:user/repo.git"
        } else {
            app.input.as_str()
        };
        let inner_width = rect.width.saturating_sub(2) as usize;
        let scroll = app.input.len().saturating_sub(inner_width);

        let widget = Paragraph::new(Line::from(Span::styled(
            value,
            Style::default().fg(if placeholder {
                Color::DarkGray
            } else {
                Color::White
            }),
        )))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Clone repository "),
        )
        .scroll((0, scroll.min(usize::from(u16::MAX)) as u16));

        f.render_widget(Clear, rect);
        f.render_widget(widget, rect);

        let offset = (app.input.len() - scroll).min(usize::from(u16::MAX));
        let cursor_x = (rect.x + 1 + offset as u16).min(rect.right().saturating_sub(1));
        f.set_cursor(cursor_x, rect.y + 1);
    }

    fn draw_modal(app: &App, f: &mut Frame, area: Rect) {
        let rect = centered_rect(72, 60, area);
        let lines: Vec<Line> = if app.modal_text.is_empty() {
            vec![Line::raw("working…")]
        } else {
            app.modal_text
                .iter()
                .map(|line| Line::raw(line.as_str()))
                .collect()
        };
        let max_scroll = lines.len().saturating_sub(2) as u16;

        let widget = Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(app.modal_title.as_str()),
            )
            .scroll((app.scroll.min(max_scroll), 0))
            .wrap(Wrap { trim: true });

        f.render_widget(Clear, rect);
        f.render_widget(widget, rect);
    }
}

fn info_lines(path: &Path, info: &git::RepoInfo) -> Vec<Line<'static>> {
    let name = git::sanitize(&path.file_name().unwrap_or_default().to_string_lossy());
    let mut lines = vec![
        Line::from(Span::styled(
            name,
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            truncate(&git::sanitize(&path.display().to_string()), 90),
            Style::default().fg(Color::DarkGray),
        )),
        Line::raw(""),
    ];

    lines.push(Line::from(vec![
        Span::styled("branch    ", Style::default().fg(Color::DarkGray)),
        Span::styled(info.branch.clone(), Style::default().fg(Color::Yellow)),
    ]));

    match &info.remote {
        Some((remote_name, remote_url)) => lines.push(Line::from(vec![
            Span::styled("remote    ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{} → {}", remote_name, truncate(remote_url, 60)),
                Style::default().fg(Color::Gray),
            ),
        ])),
        None => lines.push(Line::from(vec![
            Span::styled("remote    ", Style::default().fg(Color::DarkGray)),
            Span::styled("(none)", Style::default().fg(Color::Red)),
        ])),
    }

    let sync_line = match (info.ahead, info.behind) {
        (Some(ahead), Some(behind)) if ahead == 0 && behind == 0 => Line::from(vec![
            Span::styled("sync      ", Style::default().fg(Color::DarkGray)),
            Span::styled("up to date", Style::default().fg(Color::Green)),
        ]),
        (Some(ahead), Some(behind)) => {
            let mut parts = Vec::new();
            if ahead > 0 {
                parts.push(format!("ahead {ahead}"));
            }
            if behind > 0 {
                parts.push(format!("behind {behind}"));
            }
            let color = if behind > 0 {
                Color::Red
            } else {
                Color::Yellow
            };
            Line::from(vec![
                Span::styled("sync      ", Style::default().fg(Color::DarkGray)),
                Span::styled(parts.join(" · "), Style::default().fg(color)),
            ])
        }
        _ => Line::from(vec![
            Span::styled("sync      ", Style::default().fg(Color::DarkGray)),
            Span::styled("no upstream", Style::default().fg(Color::DarkGray)),
        ]),
    };
    lines.push(sync_line);

    let worktree_line = if info.dirty == 0 {
        Line::from(vec![
            Span::styled("worktree  ", Style::default().fg(Color::DarkGray)),
            Span::styled("clean", Style::default().fg(Color::Green)),
        ])
    } else {
        Line::from(vec![
            Span::styled("worktree  ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!("{} changed", info.dirty),
                Style::default().fg(Color::Red),
            ),
            Span::styled(
                format!(" ({} untracked)", info.untracked),
                Style::default().fg(Color::DarkGray),
            ),
        ])
    };
    lines.push(worktree_line);

    lines.push(Line::from(vec![
        Span::styled("stashes   ", Style::default().fg(Color::DarkGray)),
        Span::styled(info.stashes.to_string(), Style::default().fg(Color::Gray)),
    ]));
    lines.push(Line::from(vec![
        Span::styled("branches  ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            info.local_branches.to_string(),
            Style::default().fg(Color::Gray),
        ),
    ]));

    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(
        "last commit",
        Style::default().fg(Color::DarkGray),
    )));
    lines.push(Line::from(Span::styled(
        info.last_commit.clone(),
        Style::default().fg(Color::Gray),
    )));
    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(
        "s status · b log · l branches · u update",
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::ITALIC),
    )));
    lines
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::vertical([
        Constraint::Percentage((100 - percent_y) / 2),
        Constraint::Percentage(percent_y),
        Constraint::Percentage((100 - percent_y) / 2),
    ])
    .split(area);
    Layout::horizontal([
        Constraint::Percentage((100 - percent_x) / 2),
        Constraint::Percentage(percent_x),
        Constraint::Percentage((100 - percent_x) / 2),
    ])
    .split(vertical[1])[1]
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}
