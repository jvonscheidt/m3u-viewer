//! Rendering for the TUI: channel table, status bar, and the group/help
//! popups. Pure drawing — all state changes happen in [`crate::app`].

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, List, ListItem, ListState, Paragraph, Row, Table};

use crate::app::{App, Mode};

/// Draws one frame.
pub fn draw(frame: &mut Frame, app: &mut App) {
    let [list_area, status_area] =
        Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(frame.area());
    draw_channels(frame, list_area, app);
    draw_status(frame, status_area, app);
    match app.mode {
        Mode::Groups => draw_group_popup(frame, app),
        Mode::Help => draw_help_popup(frame),
        Mode::Normal | Mode::Filter => {}
    }
}

/// The virtualized channel table: only the rows inside the viewport are
/// materialized, so list size does not affect frame time.
fn draw_channels(frame: &mut Frame, area: Rect, app: &mut App) {
    if app.filtered.is_empty() {
        let message = if app.loading {
            "loading…"
        } else if app.channels.is_empty() {
            "playlist is empty"
        } else {
            "no channels match"
        };
        frame.render_widget(Paragraph::new(message).dim().centered(), area);
        return;
    }

    app.ensure_visible(usize::from(area.height));
    let end = (app.offset + usize::from(area.height)).min(app.filtered.len());
    let rows = app.filtered[app.offset..end]
        .iter()
        .enumerate()
        .map(|(row, &index)| {
            let channel = &app.channels[index];
            let group = channel
                .group
                .and_then(|id| app.groups.get(id))
                .map_or("", String::as_str);
            let row_style = if app.offset + row == app.selected {
                Style::new().add_modifier(Modifier::REVERSED)
            } else {
                Style::new()
            };
            Row::new([channel.name.clone(), group.to_owned()]).style(row_style)
        });
    let table = Table::new(rows, [Constraint::Fill(1), Constraint::Length(30)]).column_spacing(2);
    frame.render_widget(table, area);
}

/// One-line status bar; doubles as the filter input line in filter mode.
fn draw_status(frame: &mut Frame, area: Rect, app: &App) {
    let line = if app.mode == Mode::Filter {
        Line::from(vec![
            Span::raw("/"),
            Span::raw(app.filter.clone()),
            Span::styled("█", Style::new().dim()),
            Span::raw("  (Enter apply · Esc clear)"),
        ])
    } else if let Some(error) = &app.error {
        Line::from(Span::styled(format!("error: {error}"), Style::new().red()))
    } else {
        let mut spans = vec![Span::styled(
            format!(
                "{} — {}/{} channels",
                app.file_name,
                app.filtered.len(),
                app.channels.len()
            ),
            Style::new().bold(),
        )];
        if app.loading {
            spans.push(Span::styled(
                format!("  loading {}%", app.percent),
                Style::new().yellow(),
            ));
        }
        if app.skipped > 0 {
            spans.push(Span::raw(format!("  {} skipped", app.skipped)));
        }
        if let Some(name) = app.group_filter.and_then(|id| app.groups.get(id)) {
            spans.push(Span::raw(format!("  group:{name}")));
        }
        if !app.filter.is_empty() {
            spans.push(Span::raw(format!("  filter:{}", app.filter)));
        }
        spans.push(Span::styled(
            "  (/ filter · g groups · ? help · q quit)",
            Style::new().dim(),
        ));
        Line::from(spans)
    };
    frame.render_widget(Paragraph::new(line), area);
}

/// Centered popup listing "(all groups)" plus every interned group.
fn draw_group_popup(frame: &mut Frame, app: &App) {
    let area = centered(frame.area(), 44, 16);
    let items = std::iter::once(ListItem::new("(all groups)"))
        .chain(app.groups.iter().map(|name| ListItem::new(name.clone())))
        .collect::<Vec<_>>();
    let list = List::new(items)
        .block(Block::bordered().title(" group (Enter select · Esc close) "))
        .highlight_style(Style::new().add_modifier(Modifier::REVERSED));
    let mut state = ListState::default().with_selected(Some(app.group_cursor));
    frame.render_widget(Clear, area);
    frame.render_stateful_widget(list, area, &mut state);
}

/// Centered static help overlay.
fn draw_help_popup(frame: &mut Frame) {
    let lines = [
        "↑/↓ PgUp/PgDn Home/End  navigate",
        "/                       filter channels",
        "g                       restrict to a group",
        "Esc                     clear filter and group",
        "?                       this help",
        "q / Ctrl+C              quit",
        "",
        "Enter-to-play, favorites and recents are coming soon.",
    ];
    let area = centered(
        frame.area(),
        58,
        u16::try_from(lines.len()).unwrap_or(u16::MAX) + 2,
    );
    let text = lines.iter().copied().map(Line::from).collect::<Vec<_>>();
    let help = Paragraph::new(text).block(Block::bordered().title(" help (any key closes) "));
    frame.render_widget(Clear, area);
    frame.render_widget(help, area);
}

/// A `width` × `height` rectangle centered in `outer` (clamped to fit).
fn centered(outer: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(outer.width);
    let height = height.min(outer.height);
    Rect {
        x: outer.x + (outer.width - width) / 2,
        y: outer.y + (outer.height - height) / 2,
        width,
        height,
    }
}

#[cfg(test)]
// unwrap is fine in tests (see CLAUDE.md).
#[allow(clippy::unwrap_used)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::*;
    use crate::loader::LoadEvent;
    use crate::playlist::Channel;

    fn app_with_channels(count: usize) -> App {
        let mut app = App::new("test.m3u".into());
        let channels = (0..count)
            .map(|i| Channel {
                name: format!("Channel {i}"),
                url: format!("http://example.com/{i}"),
                tvg_id: None,
                group: Some(0),
            })
            .collect();
        app.on_load_event(LoadEvent::Batch {
            channels,
            new_groups: vec!["News".into()],
            skipped: 0,
            percent: 100,
        });
        app.on_load_event(LoadEvent::Finished);
        app
    }

    fn render(app: &mut App) -> String {
        let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
        terminal.draw(|frame| draw(frame, app)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        buffer
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect()
    }

    #[test]
    fn renders_visible_channels_and_status() {
        let mut app = app_with_channels(3);
        let screen = render(&mut app);
        assert!(screen.contains("Channel 0"));
        assert!(screen.contains("News"));
        assert!(screen.contains("3/3 channels"));
    }

    #[test]
    fn windows_rows_beyond_the_viewport() {
        let mut app = app_with_channels(500);
        let screen = render(&mut app);
        assert!(screen.contains("Channel 0"));
        // Rows past the 11-row viewport must not be materialized.
        assert!(!screen.contains("Channel 20"));
    }

    #[test]
    fn renders_group_popup() {
        let mut app = app_with_channels(3);
        app.handle_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE));
        let screen = render(&mut app);
        assert!(screen.contains("(all groups)"));
    }

    #[test]
    fn renders_help_popup() {
        let mut app = app_with_channels(1);
        app.handle_key(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE));
        let screen = render(&mut app);
        assert!(screen.contains("navigate"));
    }
}
