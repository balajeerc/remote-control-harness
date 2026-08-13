//! Shared ratatui UI primitives: the status/menu row types, the key-reading
//! helpers, the prompt state machines (confirm / text / picker) and their
//! rendering, plus the *standalone inline modals* the one-shot setup wizard
//! uses. The persistent two-pane control panel lives in [`crate::panel`] and
//! reuses the same state machines and renderers, drawing them as popups over
//! its own frame instead of inline.
//!
//! Inline modals anchor at the cursor, which needs the terminal to answer a DSR
//! cursor-position query; where it won't (a bare test pty), they fall back to a
//! fixed top-of-screen region.

use std::io::{self, Stdout};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{bail, Result};
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState, Paragraph, Wrap};
use ratatui::{Frame, Terminal, TerminalOptions, Viewport};

pub(crate) type Backend = CrosstermBackend<Stdout>;

// ---- palette ---------------------------------------------------------------

pub(crate) const ACCENT: Color = Color::Cyan;
pub(crate) const DIM: Color = Color::DarkGray;

// ---- inline modal terminal (wizard) ----------------------------------------

/// Owns raw mode for an inline modal — a fixed-height viewport drawn at the
/// current cursor line. On drop it clears its region so the following
/// `println!` output flows from a clean line.
struct Inline {
    terminal: Terminal<Backend>,
}

/// Sticky once we learn the terminal won't report its cursor position, so only
/// the first modal pays the DSR timeout; the rest go straight to the fallback.
static INLINE_UNSUPPORTED: AtomicBool = AtomicBool::new(false);

impl Inline {
    fn enter(height: u16) -> Result<Self> {
        enable_raw_mode()?;
        Ok(Self {
            terminal: build_inline_terminal(height)?,
        })
    }
}

/// Build the modal's terminal. An inline viewport is anchored at the *current
/// cursor row*, which ratatui learns by asking the terminal (a DSR `ESC[6n`
/// query). Real terminals — xterm, tmux, an ssh pty — answer it; a bare test
/// pty doesn't, so the query times out. In that case we fall back to a fixed
/// region at the top of the screen (which needs only the ioctl size, never
/// DSR) and remember it, so later modals skip the doomed probe.
fn build_inline_terminal(height: u16) -> Result<Terminal<Backend>> {
    if !INLINE_UNSUPPORTED.load(Ordering::Relaxed) {
        match Terminal::with_options(
            CrosstermBackend::new(io::stdout()),
            TerminalOptions {
                viewport: Viewport::Inline(height),
            },
        ) {
            Ok(t) => return Ok(t),
            Err(_) => INLINE_UNSUPPORTED.store(true, Ordering::Relaxed),
        }
    }
    let (cols, rows) = crossterm_size();
    let area = Rect::new(0, 0, cols, height.min(rows));
    Ok(Terminal::with_options(
        CrosstermBackend::new(io::stdout()),
        TerminalOptions {
            viewport: Viewport::Fixed(area),
        },
    )?)
}

/// The terminal size via ioctl (never DSR), with an 80x24 fallback — including
/// when the pty reports a degenerate 0×0 (as bare test ptys do).
pub(crate) fn crossterm_size() -> (u16, u16) {
    match ratatui::crossterm::terminal::size() {
        Ok((c, r)) if c > 0 && r > 0 => (c, r),
        _ => (80, 24),
    }
}

impl Drop for Inline {
    fn drop(&mut self) {
        // Erase the viewport so the caller's summary line starts clean, then
        // hand the terminal back in cooked mode.
        let _ = self.terminal.clear();
        let _ = disable_raw_mode();
    }
}

// ---- key input -------------------------------------------------------------

/// Read the next key press, collapsing the key-repeat/release events some
/// backends emit into a single logical press. Returns `None` on a non-key event
/// (resize, focus) so the caller can just redraw.
pub(crate) fn next_key() -> Result<Option<(KeyCode, KeyModifiers)>> {
    match event::read()? {
        Event::Key(k) if k.kind == KeyEventKind::Press => Ok(Some((k.code, k.modifiers))),
        _ => Ok(None),
    }
}

pub(crate) fn is_ctrl_c(code: KeyCode, mods: KeyModifiers) -> bool {
    mods.contains(KeyModifiers::CONTROL) && matches!(code, KeyCode::Char('c'))
}

// ---- menu row types (shared with the panel) --------------------------------

/// The live status shown in the panel's header.
pub struct Status {
    pub project: String,
    pub container: String,
    /// Human word for the container state: `running` / `stopped` / `not created`.
    pub state: &'static str,
    pub webapp_port: u16,
    /// Set only when the host side of the publish had to differ from
    /// `webapp_port` (see `Config::webapp_host_port`).
    pub webapp_host_port: Option<u16>,
    pub agents: String,
}

/// A row in the menu: an inert section header (with a group glyph) or a
/// selectable item (with a single-key hotkey that runs it directly).
pub enum Row {
    Header { icon: char, title: String },
    Item { key: char, label: String },
}

impl Row {
    pub(crate) fn label(&self) -> &str {
        match self {
            Row::Header { title, .. } => title,
            Row::Item { label, .. } => label,
        }
    }
    pub(crate) fn is_item(&self) -> bool {
        matches!(self, Row::Item { .. })
    }
    /// The item's direct-run hotkey, or `None` for a header.
    pub(crate) fn hotkey(&self) -> Option<char> {
        match self {
            Row::Item { key, .. } => Some(*key),
            Row::Header { .. } => None,
        }
    }
}

/// Indices into `rows` of the items visible under the current filter. With an
/// empty query every item shows; otherwise only items whose label contains the
/// query (case-insensitive).
pub(crate) fn visible_items(rows: &[Row], query: &str) -> Vec<usize> {
    let q = query.to_lowercase();
    rows.iter()
        .enumerate()
        .filter(|(_, r)| r.is_item() && (q.is_empty() || r.label().to_lowercase().contains(&q)))
        .map(|(i, _)| i)
        .collect()
}

// ============================================================================
// Prompt state machines + rendering (shared by inline modals and panel popups)
// ============================================================================

/// What feeding a key to a prompt did.
pub(crate) enum Step {
    /// Keep prompting.
    Continue,
    /// The user confirmed (Enter).
    Accept,
    /// The user cancelled (Esc / Ctrl-C).
    Cancel,
}

fn cancel_key(code: KeyCode, mods: KeyModifiers) -> bool {
    is_ctrl_c(code, mods) || code == KeyCode::Esc
}

/// A styled question glyph + text, the leading line of every prompt.
pub(crate) fn question(prompt: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            "? ",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            prompt.to_owned(),
            Style::default().add_modifier(Modifier::BOLD),
        ),
    ])
}

// -- confirm --

pub(crate) fn confirm_step(code: KeyCode, mods: KeyModifiers, answer: &mut bool) -> Step {
    match code {
        _ if cancel_key(code, mods) => Step::Cancel,
        // A single y/n submits immediately (the common expectation); Enter alone
        // accepts the currently-highlighted option.
        KeyCode::Char('y') | KeyCode::Char('Y') => {
            *answer = true;
            Step::Accept
        }
        KeyCode::Char('n') | KeyCode::Char('N') => {
            *answer = false;
            Step::Accept
        }
        // Move the highlight between Yes/No without committing, so the current
        // choice is always visible before Enter.
        KeyCode::Left | KeyCode::Right | KeyCode::Tab | KeyCode::BackTab => {
            *answer = !*answer;
            Step::Continue
        }
        KeyCode::Enter => Step::Accept,
        _ => Step::Continue,
    }
}

/// The Yes/No option row, rendered on its OWN line below the question so the
/// highlighted choice is always visible — never clipped off the right edge by a
/// long prompt (as a single-line `? prompt … Yes/No` would be).
pub(crate) fn confirm_options(answer: bool) -> Line<'static> {
    Line::from(vec![
        Span::raw("  "),
        // The current option is highlighted (reversed accent), so it's obvious
        // which way Enter will go — and which was just chosen.
        confirm_pill("Yes", answer),
        Span::styled(" / ", Style::default().fg(DIM)),
        confirm_pill("No", !answer),
        Span::styled("   (y/n · ←/→ · Enter)", Style::default().fg(DIM)),
    ])
}

/// Rows a single line of `text` occupies when wrapped to `width` columns
/// (char-count approximation — fine for the ASCII prompts here).
pub(crate) fn wrapped_line_count(text: &str, width: u16) -> u16 {
    let w = width.max(1) as usize;
    text.chars().count().div_ceil(w).max(1) as u16
}

/// Rows the wrapped confirm question needs, with a headroom row so word-wrap
/// (which can break earlier than the char-count estimate) is never clipped. The
/// Yes/No option row is added on top by the caller.
pub(crate) fn confirm_question_rows(prompt: &str, width: u16) -> u16 {
    let est = wrapped_line_count(&format!("? {prompt}"), width);
    if est > 1 {
        est + 1
    } else {
        1
    }
}

/// One Yes/No option; highlighted (reversed accent) when it's the active choice.
fn confirm_pill(text: &str, active: bool) -> Span<'static> {
    let style = if active {
        Style::default()
            .fg(ACCENT)
            .add_modifier(Modifier::BOLD | Modifier::REVERSED)
    } else {
        Style::default().fg(DIM)
    };
    Span::styled(format!(" {text} "), style)
}

// -- text --

pub(crate) fn text_step(code: KeyCode, mods: KeyModifiers, buf: &mut String) -> Step {
    match code {
        _ if cancel_key(code, mods) => Step::Cancel,
        KeyCode::Enter => Step::Accept,
        KeyCode::Backspace => {
            buf.pop();
            Step::Continue
        }
        KeyCode::Char(c) => {
            buf.push(c);
            Step::Continue
        }
        _ => Step::Continue,
    }
}

/// Render a text prompt into `area`, returning where to place the cursor.
pub(crate) fn text_render(
    f: &mut Frame,
    area: Rect,
    prompt: &str,
    buf: &str,
    hidden: bool,
) -> (u16, u16) {
    let shown = if hidden {
        "•".repeat(buf.chars().count())
    } else {
        buf.to_owned()
    };
    let mut spans = question(prompt).spans;
    spans.push(Span::raw(" "));
    let prefix: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    spans.push(Span::styled(shown.clone(), Style::default().fg(ACCENT)));
    f.render_widget(Paragraph::new(Line::from(spans)), area);
    let x = area.x + ((prefix + shown.chars().count()) as u16).min(area.width.saturating_sub(1));
    (x, area.y)
}

// -- picker (single- and multi-select) --

/// Cursor + checked state for a list picker.
pub(crate) struct Picker {
    pub cursor: usize,
    checked: Vec<bool>,
    multi: bool,
}

impl Picker {
    pub(crate) fn new(len: usize, multi: bool, initial_checked: &[usize]) -> Self {
        let mut checked = vec![false; len];
        for &i in initial_checked {
            if let Some(slot) = checked.get_mut(i) {
                *slot = true;
            }
        }
        Self {
            cursor: 0,
            checked,
            multi,
        }
    }

    pub(crate) fn step(&mut self, code: KeyCode, mods: KeyModifiers) -> Step {
        match code {
            _ if cancel_key(code, mods) => Step::Cancel,
            KeyCode::Up => {
                self.cursor = self.cursor.saturating_sub(1);
                Step::Continue
            }
            KeyCode::Down if self.cursor + 1 < self.checked.len() => {
                self.cursor += 1;
                Step::Continue
            }
            KeyCode::Char(' ') if self.multi => {
                self.checked[self.cursor] = !self.checked[self.cursor];
                Step::Continue
            }
            KeyCode::Enter => Step::Accept,
            _ => Step::Continue,
        }
    }

    pub(crate) fn confirmed(&self) -> Vec<usize> {
        if self.multi {
            self.checked
                .iter()
                .enumerate()
                .filter(|(_, &c)| c)
                .map(|(i, _)| i)
                .collect()
        } else {
            vec![self.cursor]
        }
    }

    /// The list widget + its state for the current cursor/checks.
    pub(crate) fn list<'a>(&self, items: &'a [String]) -> (List<'a>, ListState) {
        let rows: Vec<ListItem> = items
            .iter()
            .enumerate()
            .map(|(i, label)| {
                let mark = match (self.multi, self.checked[i]) {
                    (true, true) => "[x] ",
                    (true, false) => "[ ] ",
                    (false, _) => "",
                };
                ListItem::new(Line::from(format!("  {mark}{label}")))
            })
            .collect();
        let mut state = ListState::default();
        state.select(Some(self.cursor));
        // Pure black on the ACCENT bar (no bold — bold-black renders as muddy
        // grey on many terminals; see the panel's menu highlight).
        let widget = List::new(rows).highlight_style(
            Style::default()
                .bg(ACCENT)
                .fg(Color::Black)
                .remove_modifier(Modifier::BOLD),
        );
        (widget, state)
    }
}

// ============================================================================
// Standalone inline modals — used only by the one-shot wizard
// ============================================================================

/// An inline modal: how tall, how to draw it, how a key advances it.
trait Modal {
    fn height(&self) -> u16;
    fn draw(&self, f: &mut Frame);
    fn step(&mut self, code: KeyCode, mods: KeyModifiers) -> Step;
}

/// Drive an inline modal to completion. Returns whether it was cancelled.
fn run_inline<M: Modal>(m: &mut M) -> Result<bool> {
    let mut inline = Inline::enter(m.height())?;
    loop {
        inline.terminal.draw(|f| m.draw(f))?;
        let Some((code, mods)) = next_key()? else {
            continue;
        };
        match m.step(code, mods) {
            Step::Continue => {}
            Step::Accept => return Ok(false),
            Step::Cancel => return Ok(true),
        }
    }
}

struct ConfirmModal<'a> {
    prompt: &'a str,
    answer: bool,
}
impl Modal for ConfirmModal<'_> {
    fn height(&self) -> u16 {
        // Wrapped question rows + the Yes/No option row — so a long prompt
        // (e.g. a reuse-key path) is fully shown rather than clipped.
        confirm_question_rows(self.prompt, crossterm_size().0) + 1
    }
    fn draw(&self, f: &mut Frame) {
        let rows = Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(f.area());
        f.render_widget(
            Paragraph::new(question(self.prompt)).wrap(Wrap { trim: false }),
            rows[0],
        );
        f.render_widget(Paragraph::new(confirm_options(self.answer)), rows[1]);
    }
    fn step(&mut self, code: KeyCode, mods: KeyModifiers) -> Step {
        confirm_step(code, mods, &mut self.answer)
    }
}

struct TextModal<'a> {
    prompt: &'a str,
    buf: String,
    hidden: bool,
}
impl Modal for TextModal<'_> {
    fn height(&self) -> u16 {
        1
    }
    fn draw(&self, f: &mut Frame) {
        let area = f.area();
        let pos = text_render(f, area, self.prompt, &self.buf, self.hidden);
        f.set_cursor_position(pos);
    }
    fn step(&mut self, code: KeyCode, mods: KeyModifiers) -> Step {
        text_step(code, mods, &mut self.buf)
    }
}

struct PickerModal<'a> {
    prompt: &'a str,
    items: &'a [String],
    picker: Picker,
}
impl Modal for PickerModal<'_> {
    fn height(&self) -> u16 {
        (self.items.len() as u16 + 2).min(14)
    }
    fn draw(&self, f: &mut Frame) {
        let rows = Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).split(f.area());
        f.render_widget(Paragraph::new(question(self.prompt)), rows[0]);
        let (list, mut state) = self.picker.list(self.items);
        f.render_stateful_widget(list, rows[1], &mut state);
    }
    fn step(&mut self, code: KeyCode, mods: KeyModifiers) -> Step {
        self.picker.step(code, mods)
    }
}

/// Yes/no confirmation. `y`/`n` set the answer, Enter accepts the current one
/// (from `default`), Esc/Ctrl-C cancels.
pub fn confirm(prompt: &str, default: bool) -> Result<bool> {
    let mut m = ConfirmModal {
        prompt,
        answer: default,
    };
    if run_inline(&mut m)? {
        bail!("cancelled");
    }
    println!("  ? {prompt} {}", if m.answer { "yes" } else { "no" });
    Ok(m.answer)
}

/// Free-text entry. `default` pre-fills the buffer; `hidden` masks it.
pub fn text(prompt: &str, default: Option<&str>, hidden: bool) -> Result<String> {
    let mut m = TextModal {
        prompt,
        buf: default.unwrap_or("").to_owned(),
        hidden,
    };
    if run_inline(&mut m)? {
        bail!("cancelled");
    }
    let shown = if hidden {
        "••••••".to_owned()
    } else {
        m.buf.clone()
    };
    println!("  ? {prompt} {shown}");
    Ok(m.buf)
}

/// Single-choice picker. Up/Down move, Enter selects.
pub fn select(prompt: &str, items: Vec<String>) -> Result<String> {
    if items.is_empty() {
        bail!("nothing to choose from");
    }
    let mut m = PickerModal {
        prompt,
        items: &items,
        picker: Picker::new(items.len(), false, &[]),
    };
    if run_inline(&mut m)? {
        bail!("cancelled");
    }
    m.picker
        .confirmed()
        .into_iter()
        .next()
        .and_then(|i| items.get(i).cloned())
        .ok_or_else(|| anyhow::anyhow!("cancelled"))
}

/// Multi-choice picker returning the selected *indices* into `items`, with
/// `default_checked` pre-toggled. Used where the caller maps picks back to a
/// parallel array (e.g. the agent registry).
pub fn multiselect_indexed(
    prompt: &str,
    items: &[String],
    default_checked: &[usize],
) -> Result<Vec<usize>> {
    if items.is_empty() {
        return Ok(Vec::new());
    }
    let mut m = PickerModal {
        prompt,
        items,
        picker: Picker::new(items.len(), true, default_checked),
    };
    if run_inline(&mut m)? {
        bail!("cancelled");
    }
    Ok(m.picker.confirmed())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ta81_filter_matches_labels_case_insensitively() {
        let item = |key: char, label: &str| Row::Item {
            key,
            label: label.into(),
        };
        let rows = vec![
            Row::Header {
                icon: '↻',
                title: "Container lifecycle".into(),
            },
            item('s', "Restart the container"),
            item('k', "Stop the container"),
            item('f', "Refresh status"),
        ];
        // Header is never selectable.
        assert_eq!(visible_items(&rows, ""), vec![1, 2, 3]);
        // Substring, case-insensitive; the harness sends label fragments.
        assert_eq!(visible_items(&rows, "refresh"), vec![3]);
        assert_eq!(visible_items(&rows, "the container"), vec![1, 2]);
        assert!(visible_items(&rows, "nope").is_empty());
    }

    #[test]
    fn ta81_confirm_single_key_submits_enter_takes_default() {
        let none = KeyModifiers::empty();
        // A single y/n submits immediately with that answer.
        let mut a = false;
        assert!(matches!(
            confirm_step(KeyCode::Char('y'), none, &mut a),
            Step::Accept
        ));
        assert!(a);
        let mut b = true;
        assert!(matches!(
            confirm_step(KeyCode::Char('n'), none, &mut b),
            Step::Accept
        ));
        assert!(!b);
        // Enter accepts whatever the current default is (unchanged).
        let mut d = true;
        assert!(matches!(
            confirm_step(KeyCode::Enter, none, &mut d),
            Step::Accept
        ));
        assert!(d);
        // ←/→/Tab move the highlight without committing, so the choice is visible
        // before Enter.
        let mut e = false;
        for key in [KeyCode::Right, KeyCode::Left, KeyCode::Tab] {
            let before = e;
            assert!(matches!(confirm_step(key, none, &mut e), Step::Continue));
            assert_eq!(e, !before, "{key:?} should toggle the highlight");
        }
    }

    #[test]
    fn ta81_confirm_question_grows_for_long_prompts() {
        // A short prompt fits one row (the options row is added by the caller).
        assert_eq!(confirm_question_rows("Delete it?", 80), 1);
        // A long launch-style prompt wraps and gets a headroom row.
        let long = "Launch Codex (OpenAI) with --dangerously-bypass-approvals-and-sandbox \
                    — skips ALL permission prompts (unattended)?";
        assert!(
            confirm_question_rows(long, 80) >= 2,
            "should wrap at 80 cols"
        );
        // A narrower terminal needs at least as many rows.
        assert!(confirm_question_rows(long, 40) >= confirm_question_rows(long, 80));
    }

    #[test]
    fn ta81_picker_multi_toggles_and_confirms_indices() {
        let mut p = Picker::new(3, true, &[0]);
        assert_eq!(p.confirmed(), vec![0]); // seeded
        let none = KeyModifiers::empty();
        p.step(KeyCode::Down, none); // cursor -> 1
        p.step(KeyCode::Char(' '), none); // check 1
        assert_eq!(p.confirmed(), vec![0, 1]);
    }
}
