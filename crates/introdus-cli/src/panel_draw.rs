//! Pure rendering for the control panel: given a snapshot of the menu state
//! ([`MenuView`]), the output-pane lines, and an optional [`Popup`], lay out and
//! draw the two-pane frame. All functions here are side-effect-free views over
//! borrowed state — the interactive [`Ui`](crate::panel::Ui) loop that owns the
//! terminal and feeds them lives in [`crate::panel`].

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;

use introdus_core::ports;

use crate::ui::{
    self, confirm_options, question, text_render, visible_items, Picker, Row, Status, ACCENT, DIM,
};

/// Section-header palette, deliberately distinct from the items' ACCENT hotkey
/// column so a group header never reads as just another selectable row.
const HEADER_ICON: Color = Color::Magenta;
const HEADER_TEXT: Color = Color::Blue;

/// A centered popup prompt drawn over the two-pane frame.
pub(crate) enum Popup<'a> {
    Confirm {
        prompt: &'a str,
        answer: bool,
    },
    Text {
        prompt: &'a str,
        buf: &'a str,
        hidden: bool,
    },
    Pick {
        prompt: &'a str,
        items: &'a [String],
        picker: &'a Picker,
    },
}

/// The left-column state for one draw: the status header plus the menu rows and
/// their navigation state. Bundled so [`draw_frame`] stays a few arguments.
pub(crate) struct MenuView<'a> {
    pub status: &'a Status,
    pub rows: &'a [Row],
    pub query: &'a str,
    pub filtering: bool,
    pub sel: usize,
}

pub(crate) fn draw_frame(
    f: &mut Frame,
    m: &MenuView,
    out: &[String],
    popup: Option<&Popup>,
    busy: Option<(&str, char)>,
) {
    // A prompt (if any) is a full-width band at the bottom, above the footer —
    // it never covers the panes, and its full width keeps long prompt text on a
    // single line (so the output pane stays fully visible alongside it).
    let prompt_h = popup.map_or(0, |p| prompt_height(p, f.area().width));
    let root = Layout::vertical([
        Constraint::Min(3),
        Constraint::Length(prompt_h),
        Constraint::Length(1),
    ])
    .split(f.area());
    let cols =
        Layout::horizontal([Constraint::Percentage(48), Constraint::Percentage(52)]).split(root[0]);
    let left = Layout::vertical([Constraint::Length(7), Constraint::Min(3)]).split(cols[0]);

    // While a task runs, the menu is disabled — dim it and drop the highlight.
    // The status panel also reflects the in-progress op (e.g. "restarting the
    // container") in its state line, so the state — not just the footer — shows it.
    draw_status_panel(f, left[0], m.status, busy);
    draw_menu_list(f, left[1], m, popup.is_none() && busy.is_none());
    draw_output_pane(f, cols[1], out);
    if let Some(p) = popup {
        draw_prompt(f, root[1], p);
    }
    draw_footer(f, root[2], m.query, m.filtering, busy);
}

fn prompt_height(popup: &Popup, width: u16) -> u16 {
    // +1 for the top separator border line.
    1 + match popup {
        Popup::Pick { items, .. } => (items.len() as u16).min(12) + 1,
        // The (wrapped) question rows + the Yes/No option row below it. The
        // launch confirms name a long flag, so the question routinely wraps —
        // size the band to show all of it rather than clipping at the edge.
        Popup::Confirm { prompt, .. } => ui::confirm_question_rows(prompt, width) + 1,
        _ => 1,
    }
}

fn draw_status_panel(f: &mut Frame, area: Rect, status: &Status, busy: Option<(&str, char)>) {
    // While a lifecycle op runs, surface it in the state line itself (spinner +
    // label, e.g. "⠹ tearing down the container") so the status — not just the
    // footer — reflects that something is in progress.
    let (color, glyph, state_text) = match busy {
        Some((label, spin)) => (Color::Yellow, spin.to_string(), label.to_owned()),
        None => {
            let (c, g) = match status.state {
                "running" => (Color::Green, "●"),
                "starting container…" => (Color::Cyan, "◌"),
                "stopped" => (Color::Yellow, "◐"),
                _ => (Color::Red, "○"),
            };
            (c, g.to_owned(), status.state.to_owned())
        }
    };
    let label = Style::default().fg(DIM);
    let bold = Style::default().add_modifier(Modifier::BOLD);
    // The left column is narrow, so state gets its own line — otherwise a long
    // container name pushes the state word off the right edge.
    let body = vec![
        Line::from(vec![
            Span::styled(" project    ", label),
            Span::styled(&status.project, bold),
        ]),
        Line::from(vec![
            Span::styled(" container  ", label),
            Span::raw(&status.container),
        ]),
        Line::from(vec![
            Span::styled(" state      ", label),
            Span::styled(
                format!("{glyph} {state_text}"),
                Style::default().fg(color).add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(vec![
            Span::styled(" webapp     ", label),
            Span::raw(ports::publish_desc(
                status.webapp_port,
                status.webapp_host_port,
            )),
        ]),
        Line::from(vec![
            Span::styled(" agents     ", label),
            Span::raw(if status.agents.is_empty() {
                "(none)".to_owned()
            } else {
                status.agents.clone()
            }),
        ]),
    ];
    let title = Line::from(vec![
        Span::styled(" introdus ", Style::default().fg(Color::Black).bg(ACCENT)),
        Span::styled(
            " control ",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
    ]);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(ACCENT))
        .title(title);
    f.render_widget(Paragraph::new(body).block(block), area);
}

fn draw_menu_list(f: &mut Frame, area: Rect, m: &MenuView, focused: bool) {
    let visible = visible_items(m.rows, m.query);
    let selected_row = visible.get(m.sel).copied();
    // Group headers + dividers only make sense on the full list; a filtered list
    // is a flat set of matches, so drop the section scaffolding while filtering.
    let filtered = !m.query.is_empty();
    let divider = Span::styled(
        format!("  {}", "─".repeat((area.width as usize).saturating_sub(4))),
        Style::default().fg(DIM),
    );
    let mut items: Vec<ListItem> = Vec::new();
    let mut selected_line = None;
    let mut first_header = true;
    for (i, row) in m.rows.iter().enumerate() {
        if filtered && !visible.contains(&i) {
            continue;
        }
        if Some(i) == selected_row {
            selected_line = Some(items.len());
        }
        items.push(match row {
            Row::Header { icon, title } => {
                // Headers get their own palette — a distinct glyph colour and a
                // distinct text colour — so a section reads as a section and not
                // as another item (whose hotkey column is ACCENT).
                let head = Line::from(vec![
                    Span::styled(
                        format!(" {icon} "),
                        Style::default()
                            .fg(HEADER_ICON)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        title.clone(),
                        Style::default()
                            .fg(HEADER_TEXT)
                            .add_modifier(Modifier::BOLD),
                    ),
                ]);
                // A divider rule above every group but the first sets the sections
                // apart; the first group sits directly under the panel top.
                if first_header {
                    first_header = false;
                    ListItem::new(head)
                } else {
                    ListItem::new(vec![Line::from(divider.clone()), head])
                }
            }
            // `  [k] label` — the hotkey sits in a bracketed accent chip right up
            // against its label, so the eye jumps to the key and reads across.
            Row::Item { key, label } => ListItem::new(Line::from(vec![
                Span::styled("  [", Style::default().fg(DIM)),
                Span::styled(
                    key.to_string(),
                    Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
                ),
                Span::styled("] ", Style::default().fg(DIM)),
                Span::raw(label.clone()),
            ])),
        });
    }
    let mut state = ListState::default();
    if focused {
        state.select(selected_line);
    }
    // Pure black on the ACCENT bar. NB: *remove* bold rather than leave the
    // items' own bold on — many terminals render bold-black as bright-black
    // (grey), which is muddy and hard to read against the bright bar.
    let hl = Style::default()
        .bg(ACCENT)
        .fg(Color::Black)
        .remove_modifier(Modifier::BOLD);
    f.render_stateful_widget(List::new(items).highlight_style(hl), area, &mut state);
}

fn draw_output_pane(f: &mut Frame, area: Rect, out: &[String]) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(DIM))
        .title(Span::styled(" output ", Style::default().fg(DIM)));
    let inner_w = area.width.saturating_sub(2);
    let inner_h = area.height.saturating_sub(2);
    let text: Vec<Line> = if out.is_empty() {
        vec![Line::from(Span::styled(
            "  (select an action — its output appears here)",
            Style::default().fg(DIM),
        ))]
    } else {
        out.iter().map(|l| Line::from(l.as_str())).collect()
    };
    // Pin to the bottom: scroll past the wrapped rows that overflow the pane.
    let total = wrapped_rows(out, inner_w);
    let scroll = total.saturating_sub(inner_h as usize) as u16;
    f.render_widget(
        Paragraph::new(text)
            .block(block)
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0)),
        area,
    );
}

/// Approximate the number of display rows `lines` occupy when wrapped to
/// `width` columns (char-count approximation — fine for the ASCII output here).
fn wrapped_rows(lines: &[String], width: u16) -> usize {
    let w = (width.max(1)) as usize;
    lines
        .iter()
        .map(|l| {
            let n = l.chars().count();
            if n == 0 {
                1
            } else {
                n.div_ceil(w)
            }
        })
        .sum()
}

fn draw_footer(
    f: &mut Frame,
    area: Rect,
    query: &str,
    filtering: bool,
    busy: Option<(&str, char)>,
) {
    let hint =
        "press a key to run · / filter · ↑/↓ move · Enter select · Esc detach · Ctrl-a ⟨n⟩ windows";
    let line = if let Some((label, spin)) = busy {
        Line::from(vec![
            Span::styled(
                format!(" {spin} working: {label}… "),
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("  menu paused until it finishes", Style::default().fg(DIM)),
        ])
    } else if filtering {
        Line::from(vec![
            Span::styled("filter: ", Style::default().fg(DIM)),
            Span::styled(query, Style::default().fg(ACCENT)),
            Span::styled("▏", Style::default().fg(ACCENT)),
            Span::styled("   Esc clears", Style::default().fg(DIM)),
        ])
    } else {
        Line::from(Span::styled(hint, Style::default().fg(DIM)))
    };
    f.render_widget(Paragraph::new(line), area);
}

fn draw_prompt(f: &mut Frame, area: Rect, popup: &Popup) {
    // A top separator line sets the band off from the panes; the content spans
    // the full width below it (no side borders), so long prompts stay on one row.
    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::default().fg(ACCENT))
        .title(Span::styled(" prompt ", Style::default().fg(ACCENT)));
    let inner = block.inner(area);
    f.render_widget(block, area);
    match *popup {
        Popup::Confirm { prompt, answer } => {
            // Question wraps in the top area; the Yes/No options are pinned to the
            // last row so they're always visible even when the question wraps.
            let rows = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(inner);
            f.render_widget(
                Paragraph::new(question(prompt)).wrap(Wrap { trim: false }),
                rows[0],
            );
            f.render_widget(Paragraph::new(confirm_options(answer)), rows[1]);
        }
        Popup::Text {
            prompt,
            buf,
            hidden,
        } => {
            let pos = text_render(f, inner, prompt, buf, hidden);
            f.set_cursor_position(pos);
        }
        Popup::Pick {
            prompt,
            items,
            picker,
        } => {
            let rows = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).split(inner);
            f.render_widget(Paragraph::new(question(prompt)), rows[0]);
            let (list, mut state) = picker.list(items);
            f.render_stateful_widget(list, rows[1], &mut state);
        }
    }
}
