//! Rendering for the TUI: channel table, status bar, and the group/help
//! popups. Pure drawing — all state changes happen in [`crate::app`].

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, List, ListItem, ListState, Paragraph, Row, Table};

use crate::app::{App, EpgState, Mode, View};
use crate::epg::{Programme, format_time};

type ProgrammePair<'a> = (Option<&'a Programme>, Option<&'a Programme>);

/// Draws one frame.
pub fn draw(frame: &mut Frame, app: &App) {
    draw_at(frame, app, chrono::Utc::now().timestamp());
}

/// Draws one frame as of `now` (Unix seconds — injected so tests render
/// against a fixed clock).
fn draw_at(frame: &mut Frame, app: &App, now: i64) {
    let selected_programmes = selected_programmes(app, now);
    if app.visible_guide().is_some() {
        let [list_area, epg_area, status_area] = Layout::vertical([
            Constraint::Fill(1),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas(frame.area());
        draw_channels(frame, list_area, app, now, selected_programmes);
        draw_epg_bar(frame, epg_area, selected_programmes);
        draw_status(frame, status_area, app);
    } else {
        let [list_area, status_area] =
            Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(frame.area());
        draw_channels(frame, list_area, app, now, selected_programmes);
        draw_status(frame, status_area, app);
    }
    match app.mode {
        Mode::Groups | Mode::GroupSearch => draw_group_popup(frame, app),
        Mode::Help => draw_help_popup(frame),
        Mode::Normal | Mode::Filter => {}
    }
}

/// The virtualized channel table: only the rows inside the viewport are
/// materialized, so list size does not affect frame time.
fn draw_channels(
    frame: &mut Frame,
    area: Rect,
    app: &App,
    now: i64,
    selected_programmes: Option<ProgrammePair<'_>>,
) {
    if app.filtered.is_empty() {
        let filtering = !app.filter.is_empty() || app.group_filter.is_some();
        let message = if app.channels.is_empty() {
            // Nothing loaded (yet).
            if app.loading {
                "loading…"
            } else {
                "playlist is empty"
            }
        } else if filtering {
            // Channels exist but the active filter/group excludes them all;
            // during load more may still match, so don't claim "none".
            if app.loading {
                "no matches yet — loading…"
            } else {
                "no channels match"
            }
        } else {
            // The whole view is empty on its own terms.
            match app.view {
                View::Favorites => "no favorites yet — f toggles the selected channel",
                View::Recents => "nothing played yet — Enter plays the selected channel",
                View::All => "no channels match",
            }
        };
        frame.render_widget(Paragraph::new(message).dim().centered(), area);
        return;
    }

    let guide = app.visible_guide();
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
            let star = if app.is_favorite(index) { "★ " } else { "  " };
            let row_style = if app.offset + row == app.selected {
                Style::new().add_modifier(Modifier::REVERSED)
            } else {
                Style::new()
            };
            let mut cells = vec![format!("{star}{}", channel.name)];
            if let Some(guide) = guide {
                let current = if app.offset + row == app.selected {
                    selected_programmes.and_then(|(current, _)| current)
                } else {
                    guide
                        .now_next(channel.tvg_id.as_deref(), &channel.name, now)
                        .0
                };
                let airing = current.map_or_else(String::new, |programme| programme.title.clone());
                cells.push(airing);
            }
            cells.push(group.to_owned());
            Row::new(cells).style(row_style)
        });
    // With a guide, the freed-up width goes to a "now playing" column.
    let widths: &[Constraint] = if guide.is_some() {
        &[
            Constraint::Fill(1),
            Constraint::Fill(1),
            Constraint::Length(24),
        ]
    } else {
        &[Constraint::Fill(1), Constraint::Length(30)]
    };
    let table = Table::new(rows, widths).column_spacing(2);
    frame.render_widget(table, area);
}

/// One-line now/next summary for the selected channel, shown between the
/// channel table and the status bar whenever a guide is visible.
fn selected_programmes(app: &App, now: i64) -> Option<ProgrammePair<'_>> {
    let guide = app.visible_guide()?;
    let channel = app
        .filtered
        .get(app.selected)
        .map(|&index| &app.channels[index])?;
    Some(guide.now_next(channel.tvg_id.as_deref(), &channel.name, now))
}

fn draw_epg_bar(frame: &mut Frame, area: Rect, programmes: Option<ProgrammePair<'_>>) {
    let Some((current, next)) = programmes else {
        return;
    };
    if current.is_none() && next.is_none() {
        frame.render_widget(
            Paragraph::new(Span::styled(
                "no programme data for this channel",
                Style::new().dim(),
            )),
            area,
        );
        return;
    }
    let mut spans = Vec::new();
    if let Some(programme) = current {
        spans.push(Span::styled(
            format!(
                "▶ {}–{} ",
                format_time(programme.start),
                format_time(programme.stop)
            ),
            Style::new().dim(),
        ));
        spans.push(Span::raw(programme.title.clone()));
    }
    if let Some(programme) = next {
        if current.is_some() {
            spans.push(Span::styled("  ·  ", Style::new().dim()));
        }
        spans.push(Span::styled(
            format!("next {} ", format_time(programme.start)),
            Style::new().dim(),
        ));
        spans.push(Span::raw(programme.title.clone()));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// One-line status bar; doubles as the filter input line in filter mode.
fn draw_status(frame: &mut Frame, area: Rect, app: &App) {
    let line = if app.mode == Mode::Filter {
        let mut spans = vec![
            Span::raw("/"),
            Span::raw(app.filter.clone()),
            Span::styled("█", Style::new().dim()),
        ];
        if app.filter_regex_invalid() {
            spans.push(Span::styled(
                "  invalid regex, using substring",
                Style::new().yellow(),
            ));
        } else if app.filter_is_regex() {
            spans.push(Span::styled("  [regex]", Style::new().dim()));
        }
        spans.push(Span::raw("  (Enter apply · Esc clear)"));
        Line::from(spans)
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
        match app.view {
            View::All => {}
            View::Favorites => {
                spans.push(Span::styled("  ★ favorites", Style::new().magenta()));
            }
            View::Recents => spans.push(Span::styled("  ↻ recents", Style::new().magenta())),
        }
        if app.loading {
            let progress = app
                .percent
                .map_or_else(|| "  loading…".to_owned(), |p| format!("  loading {p}%"));
            spans.push(Span::styled(progress, Style::new().yellow()));
        }
        match &app.epg {
            EpgState::Loading => spans.push(Span::styled("  epg…", Style::new().dim())),
            EpgState::Failed(_) => {
                spans.push(Span::styled("  epg ✗ (see log)", Style::new().dim()));
            }
            EpgState::Absent | EpgState::Ready(_) => {}
        }
        if app.skipped > 0 {
            spans.push(Span::raw(format!("  {} skipped", app.skipped)));
        }
        if let Some(name) = app.group_filter.and_then(|id| app.groups.get(id)) {
            spans.push(Span::raw(format!("  group:{name}")));
        }
        if !app.filter.is_empty() {
            let tag = if app.filter_is_regex() {
                " [regex]"
            } else {
                ""
            };
            spans.push(Span::raw(format!("  filter:{}{tag}", app.filter)));
        }
        if let Some(message) = &app.message {
            spans.push(Span::styled(
                format!("  {message}"),
                Style::new().yellow().bold(),
            ));
        }
        spans.push(Span::styled(
            "  (/ filter · g groups · ? help · q quit)",
            Style::new().dim(),
        ));
        Line::from(spans)
    };
    frame.render_widget(Paragraph::new(line), area);
}

/// Centered popup listing "(all groups)" plus every interned group, in
/// alphabetical order.
fn draw_group_popup(frame: &mut Frame, app: &App) {
    let area = centered(frame.area(), 48, 17);
    let [list_area, search_area] =
        Layout::vertical([Constraint::Fill(1), Constraint::Length(1)]).areas(area);
    let items = if app.group_search.is_empty() {
        std::iter::once(ListItem::new("(all groups)"))
            .chain(
                app.visible_groups
                    .iter()
                    .map(|&id| ListItem::new(app.groups[id].clone())),
            )
            .collect::<Vec<_>>()
    } else if app.visible_groups.is_empty() {
        vec![ListItem::new("(no matching groups)").style(Style::new().dim())]
    } else {
        app.visible_groups
            .iter()
            .map(|&id| ListItem::new(app.groups[id].clone()))
            .collect()
    };
    let list = List::new(items)
        .block(Block::bordered().title(" group (Enter select · / search · PgUp/PgDn · Esc close) "))
        .highlight_style(Style::new().add_modifier(Modifier::REVERSED));
    let mut state = ListState::default().with_selected(Some(app.group_cursor));
    let cursor = if app.mode == Mode::GroupSearch {
        "█"
    } else {
        ""
    };
    let search = Paragraph::new(format!("/{}{cursor}", app.group_search));
    frame.render_widget(Clear, area);
    frame.render_stateful_widget(list, list_area, &mut state);
    frame.render_widget(search, search_area);
}

/// Centered static help overlay.
fn draw_help_popup(frame: &mut Frame) {
    let lines = [
        "↑/↓ PgUp/PgDn Home/End  navigate",
        "Enter                   play in VLC",
        "/                       filter channels (regex)",
        "g                       restrict to a group",
        "f                       toggle favorite",
        "F / R / Tab             favorites / recents / cycle views",
        "e                       toggle EPG display",
        "Esc                     clear filter and group",
        "?                       this help",
        "q / Ctrl+C              quit",
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
        let mut app = App::new("test.m3u".into(), None);
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
            percent: Some(100),
        });
        app.on_load_event(LoadEvent::Finished);
        app
    }

    fn render(app: &mut App) -> String {
        render_at(app, 0)
    }

    fn render_at(app: &mut App, now: i64) -> String {
        let mut terminal = Terminal::new(TestBackend::new(80, 12)).unwrap();
        app.update_viewports(12);
        terminal.draw(|frame| draw_at(frame, app, now)).unwrap();
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
    fn rendering_keeps_a_cursor_beyond_the_viewport_visible() {
        let mut app = app_with_channels(30);
        app.handle_key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));

        let screen = render(&mut app);

        assert_eq!(app.offset, 19);
        assert!(screen.contains("Channel 9"));
    }

    #[test]
    fn renders_group_popup() {
        let mut app = app_with_channels(3);
        app.handle_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE));
        let screen = render(&mut app);
        assert!(screen.contains("(all groups)"));
        assert!(screen.contains("/ search"));
    }

    #[test]
    fn renders_group_search_input_and_empty_result() {
        let mut app = app_with_channels(3);
        app.handle_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE));
        app.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
        for c in "sports".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        let screen = render(&mut app);
        assert!(screen.contains("/sports█"));
        assert!(screen.contains("(no matching groups)"));
    }

    #[test]
    fn group_popup_page_size_tracks_the_terminal_height() {
        let mut app = App::new("test.m3u".into(), None);
        app.on_load_event(LoadEvent::Batch {
            channels: Vec::new(),
            new_groups: (0..30).map(|index| format!("Group {index:02}")).collect(),
            skipped: 0,
            percent: Some(100),
        });
        app.handle_key(KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE));

        let _ = render(&mut app);
        app.handle_key(KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE));

        assert_eq!(app.group_page_rows, 9);
        assert_eq!(app.group_cursor, 9);
    }

    #[test]
    fn renders_help_popup() {
        let mut app = app_with_channels(1);
        app.handle_key(KeyEvent::new(KeyCode::Char('?'), KeyModifiers::NONE));
        let screen = render(&mut app);
        assert!(screen.contains("navigate"));
    }

    /// Fixed "now" for EPG tests: 2026-07-05 12:00 UTC.
    const NOW: i64 = 1_783_080_000;

    /// A guide where "Channel 0" (matched by display name; the test
    /// channels carry no tvg-id) airs "Current Show" then "Next Show".
    fn test_guide() -> crate::epg::Guide {
        let stamp = |offset_hours: i64| {
            chrono::DateTime::from_timestamp(NOW + offset_hours * 3600, 0)
                .unwrap()
                .format("%Y%m%d%H%M%S +0000")
                .to_string()
        };
        let xml = format!(
            r#"<tv>
<channel id="ch0.tv"><display-name>Channel 0</display-name></channel>
<programme start="{}" stop="{}" channel="ch0.tv"><title>Current Show</title></programme>
<programme start="{}" stop="{}" channel="ch0.tv"><title>Next Show</title></programme>
</tv>"#,
            stamp(-1),
            stamp(1),
            stamp(1),
            stamp(2),
        );
        crate::epg::parse_xmltv(xml.as_bytes(), NOW).unwrap()
    }

    #[test]
    fn guide_adds_now_playing_column_and_now_next_bar() {
        let mut app = app_with_channels(3);
        app.on_epg_event(crate::epg::EpgEvent::Loaded(test_guide()));
        let screen = render_at(&mut app, NOW);
        // Column: what's airing on Channel 0; bar: now + next for the
        // selection (also Channel 0).
        assert!(screen.contains("Current Show"), "screen: {screen}");
        assert!(screen.contains("Next Show"), "screen: {screen}");
        assert!(screen.contains("▶"), "screen: {screen}");
    }

    #[test]
    fn toggled_off_guide_renders_the_plain_table() {
        let mut app = app_with_channels(3);
        app.on_epg_event(crate::epg::EpgEvent::Loaded(test_guide()));
        app.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::NONE));
        let screen = render_at(&mut app, NOW);
        assert!(!screen.contains("Current Show"));
        assert!(screen.contains("Channel 0"));
    }

    #[test]
    fn selected_channel_without_programmes_says_so_in_the_bar() {
        let mut app = app_with_channels(3);
        app.on_epg_event(crate::epg::EpgEvent::Loaded(test_guide()));
        // Select "Channel 1", which the guide doesn't know.
        app.handle_key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        let screen = render_at(&mut app, NOW);
        assert!(screen.contains("no programme data"), "screen: {screen}");
    }

    #[test]
    fn epg_loading_shows_in_the_status_bar() {
        let mut app = app_with_channels(1);
        app.set_epg_loading();
        let screen = render(&mut app);
        assert!(screen.contains("epg…"));
    }

    #[test]
    fn empty_filter_result_says_no_match_not_loading() {
        // Regression: a non-matching filter on a loaded list showed the
        // wrong empty-state message.
        let mut app = app_with_channels(3);
        app.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
        for c in "zzz".chars() {
            app.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        let screen = render(&mut app);
        assert!(screen.contains("no channels match"));
        assert!(!screen.contains("loading"));
    }
}
