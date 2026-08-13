//! The persistent two-pane control panel. Unlike the wizard's inline modals,
//! this owns the alternate screen for the whole `introdus menu` session: the
//! left column shows the live status + the grouped, filterable menu; the right
//! column is an output pane where each action's output streams in (instead of
//! clearing the screen and pausing). Prompts appear as centered popups.
//!
//! Action output reaches the pane two ways: the action's own [`Ui::log`] calls,
//! and — transparently — every external command it runs, because [`Ui::new`]
//! installs a [`process::capture_stdio`] guard that redirects `Cmd` output into
//! the same buffer instead of the screen.

use std::cell::RefCell;
use std::io::{self, Stdout};
use std::ops::Range;
use std::rc::Rc;
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::Terminal;

use introdus_core::process::{self, CaptureGuard, OutputBuffer};

pub(crate) use crate::panel_draw::{draw_frame, MenuView, Popup};
use crate::ui::{
    self, confirm_step, next_key, text_step, visible_items, Picker, Row, Status, Step,
};

type Backend = CrosstermBackend<Stdout>;

/// How long the menu waits for a keypress before returning [`Selection::Tick`]
/// so the caller can re-snapshot the (possibly just-started) container status.
const STATUS_POLL: Duration = Duration::from_secs(2);

/// The result of one turn at the menu.
pub enum Selection {
    /// The user chose the item at this index into `rows`.
    Item(usize),
    /// The user quit the menu (Esc / Ctrl-C).
    Quit,
    /// No input within [`STATUS_POLL`] — re-snapshot the status and redraw.
    Tick,
}

/// Braille spinner frames for the "action in progress" indicator.
const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// A block of output lines that erases itself once its TTL is up — for output
/// that shouldn't linger on a screen someone else may be looking at (the
/// direct-mode paseo URL, which embeds the daemon password).
struct Transient {
    /// Where the block sits in the output buffer, as a half-open index range.
    range: Range<usize>,
    /// When the block should be replaced by [`Transient::message`].
    deadline: Instant,
    /// The single line left behind in its place.
    message: String,
}

/// Replace `range` in `lines` with the single line `message`. Split out of
/// [`Ui`] so the erase itself is testable without a terminal.
fn erase(lines: &mut Vec<String>, range: Range<usize>, message: String) {
    if range.end > lines.len() {
        return; // Buffer was rewritten under us; nothing safe to erase.
    }
    lines.splice(range, [message]);
}

/// Owns the alternate screen + the output pane for a control-menu session.
pub struct Ui {
    term: Terminal<Backend>,
    out: OutputBuffer,
    status: Status,
    rows: Vec<Row>,
    /// Selection index into the *visible* (filtered) item list.
    sel: usize,
    query: String,
    /// Whether the menu is in filter mode (entered with `/`). When false, a
    /// letter runs its item's hotkey instead of narrowing the list.
    filtering: bool,
    /// When set, a long action is running: the footer shows a spinner and
    /// keystrokes are ignored (the menu is disabled until it finishes).
    busy: Option<String>,
    spin: usize,
    /// A pending self-erasing output block, if one is on screen.
    transient: Option<Transient>,
    _capture: CaptureGuard,
}

impl Ui {
    pub fn new() -> Result<Self> {
        enable_raw_mode()?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen)?;
        let mut term = Terminal::new(CrosstermBackend::new(stdout))?;
        term.hide_cursor()?;
        let out: OutputBuffer = Rc::new(RefCell::new(Vec::new()));
        let capture = process::capture_stdio(out.clone());
        Ok(Self {
            term,
            out,
            status: blank_status(),
            rows: Vec::new(),
            sel: 0,
            query: String::new(),
            filtering: false,
            busy: None,
            spin: 0,
            transient: None,
            _capture: capture,
        })
    }

    /// Refresh the left-column status + menu rows for the next turn.
    pub fn set_menu(&mut self, status: Status, rows: Vec<Row>) {
        self.status = status;
        self.rows = rows;
    }

    /// Append a progress line to the output pane and redraw.
    pub fn log(&mut self, line: impl Into<String>) {
        self.out.borrow_mut().push(line.into());
        let _ = self.draw(None);
    }

    /// Append a block of lines that is wiped from the pane after `ttl`, leaving
    /// `message` in its place — for output too sensitive to leave on screen (the
    /// direct-mode paseo URL carries the daemon password). The erase lands on
    /// the first redraw past the deadline; the menu redraws at least every
    /// [`STATUS_POLL`], so it is prompt without blocking the panel meanwhile.
    /// Only one block is pending at a time: starting a second erases the first.
    pub fn log_transient(&mut self, lines: Vec<String>, ttl: Duration, message: &str) {
        self.retire_transient(true);
        let range = {
            let mut out = self.out.borrow_mut();
            let start = out.len();
            out.extend(lines);
            start..out.len()
        };
        self.transient = Some(Transient {
            range,
            deadline: Instant::now() + ttl,
            message: message.to_owned(),
        });
        let _ = self.draw(None);
    }

    /// Erase the pending transient block if its deadline has passed (or `force`).
    /// Called before every redraw.
    fn retire_transient(&mut self, force: bool) {
        let due = self
            .transient
            .as_ref()
            .is_some_and(|t| force || Instant::now() >= t.deadline);
        if !due {
            return;
        }
        if let Some(t) = self.transient.take() {
            erase(&mut self.out.borrow_mut(), t.range, t.message);
        }
    }

    /// Announce the start of an action in the output pane (a visual separator).
    pub fn begin(&mut self, title: &str) {
        {
            let mut out = self.out.borrow_mut();
            if !out.is_empty() {
                out.push(String::new());
            }
            out.push(format!("▶ {title}"));
        }
        let _ = self.draw(None);
    }

    /// Run a long command as a foreground *task*: it streams its output into the
    /// pane line-by-line on a worker thread while the panel shows a spinner, and
    /// — crucially — all keystrokes are discarded until it finishes, so the menu
    /// is disabled and no other action can be started (or queued) meanwhile.
    /// Errors on a non-zero exit.
    pub fn run_task(&mut self, label: &str, cmd: introdus_core::process::Cmd) -> Result<()> {
        let (ok, _lines) = self.run_task_inner(label, cmd)?;
        if !ok {
            bail!("`{label}` exited non-zero");
        }
        Ok(())
    }

    /// Like [`run_task`](Self::run_task) but best-effort (the exit code is
    /// ignored) and returns the streamed lines — for a scan whose output we both
    /// show and need to inspect.
    pub fn run_task_lines(
        &mut self,
        label: &str,
        cmd: introdus_core::process::Cmd,
    ) -> Result<Vec<String>> {
        let (_ok, lines) = self.run_task_inner(label, cmd)?;
        Ok(lines)
    }

    /// Shared driver for the task variants: spin + stream + drain input, and
    /// return `(exited_zero, streamed_lines)`.
    fn run_task_inner(
        &mut self,
        label: &str,
        cmd: introdus_core::process::Cmd,
    ) -> Result<(bool, Vec<String>)> {
        let (tx, rx) = std::sync::mpsc::channel::<String>();
        let handle = std::thread::spawn(move || cmd.stream(tx));
        self.busy = Some(label.to_owned());
        let mut lines = Vec::new();
        let outcome = loop {
            let mut drained_any = false;
            while let Ok(line) = rx.try_recv() {
                lines.push(line.clone());
                self.out.borrow_mut().push(line);
                drained_any = true;
            }
            self.spin = self.spin.wrapping_add(1);
            let _ = self.draw(None);
            if handle.is_finished() && !drained_any {
                while let Ok(line) = rx.try_recv() {
                    lines.push(line.clone());
                    self.out.borrow_mut().push(line);
                }
                break handle.join();
            }
            // Wait briefly for input and throw it away — the task owns the UI.
            if ratatui::crossterm::event::poll(Duration::from_millis(120))? {
                let _ = ratatui::crossterm::event::read()?;
            }
        };
        self.busy = None;
        match outcome {
            Ok(Ok(ok)) => Ok((ok, lines)),
            Ok(Err(e)) => Err(e),
            Err(_) => bail!("`{label}` task thread panicked"),
        }
    }

    /// Discard any keystrokes buffered while an action was running, so keys
    /// mashed during a blocking step don't fire as a cascade of menu selections.
    pub fn drain_input(&self) {
        while ratatui::crossterm::event::poll(Duration::from_millis(0)).unwrap_or(false) {
            let _ = ratatui::crossterm::event::read();
        }
    }

    /// Run one turn at the menu. Two input modes: by default a letter runs its
    /// item's hotkey directly, `/` enters filter mode, and ↑/↓ + Enter still
    /// navigate; in filter mode (the send-files convention) typing narrows the
    /// list and Esc leaves it. Returns when the user picks an item or quits.
    pub fn run_menu(&mut self) -> Result<Selection> {
        loop {
            let visible = visible_items(&self.rows, &self.query);
            if self.sel >= visible.len() {
                self.sel = visible.len().saturating_sub(1);
            }
            self.draw(None)?;
            // Poll rather than block, so the status panel keeps up with a
            // container that starts (or stops) while we're sitting at the menu.
            if !ratatui::crossterm::event::poll(STATUS_POLL)? {
                return Ok(Selection::Tick);
            }
            let Some((code, mods)) = next_key()? else {
                continue;
            };
            if ui::is_ctrl_c(code, mods) {
                return Ok(Selection::Quit);
            }
            use ratatui::crossterm::event::KeyCode::*;
            match code {
                Up => self.sel = self.sel.saturating_sub(1),
                Down if self.sel + 1 < visible.len() => self.sel += 1,
                Enter => {
                    if let Some(&idx) = visible.get(self.sel) {
                        return Ok(self.chose(idx));
                    }
                }
                // --- filter mode: type to narrow, Esc/Backspace-empty to leave ---
                _ if self.filtering => match code {
                    Esc => self.end_filter(),
                    Backspace => {
                        self.query.pop();
                        self.sel = 0;
                        if self.query.is_empty() {
                            self.filtering = false;
                        }
                    }
                    Char(c) => {
                        self.query.push(c);
                        self.sel = 0;
                    }
                    _ => {}
                },
                // --- default mode: hotkeys + `/` filter ---
                Esc => return Ok(Selection::Quit),
                Char('/') => {
                    self.filtering = true;
                    self.sel = 0;
                }
                Char(c) => {
                    if let Some(idx) = self.rows.iter().position(|r| r.hotkey() == Some(c)) {
                        return Ok(self.chose(idx));
                    }
                }
                _ => {}
            }
        }
    }

    /// Commit a menu selection: reset the filter so the next turn starts from the
    /// full menu (the query/mode persist across turns otherwise).
    fn chose(&mut self, idx: usize) -> Selection {
        self.end_filter();
        Selection::Item(idx)
    }

    /// Leave filter mode and clear the query.
    fn end_filter(&mut self) {
        self.filtering = false;
        self.query.clear();
        self.sel = 0;
    }

    // ---- popup prompts (mirror the wizard's inline modals) -----------------

    pub fn confirm(&mut self, prompt: &str, default: bool) -> Result<bool> {
        let mut answer = default;
        loop {
            self.draw(Some(Popup::Confirm { prompt, answer }))?;
            let Some((code, mods)) = next_key()? else {
                continue;
            };
            match confirm_step(code, mods, &mut answer) {
                Step::Continue => {}
                Step::Accept => break,
                Step::Cancel => bail!("cancelled"),
            }
        }
        self.log(format!(
            "  ? {prompt} {}",
            if answer { "yes" } else { "no" }
        ));
        Ok(answer)
    }

    pub fn text(&mut self, prompt: &str, hidden: bool) -> Result<String> {
        let mut buf = String::new();
        loop {
            self.draw(Some(Popup::Text {
                prompt,
                buf: &buf,
                hidden,
            }))?;
            let Some((code, mods)) = next_key()? else {
                continue;
            };
            match text_step(code, mods, &mut buf) {
                Step::Continue => {}
                Step::Accept => break,
                Step::Cancel => bail!("cancelled"),
            }
        }
        let shown = if hidden {
            "••••••".to_owned()
        } else {
            buf.clone()
        };
        self.log(format!("  ? {prompt} {shown}"));
        Ok(buf)
    }

    pub fn select(&mut self, prompt: &str, items: Vec<String>) -> Result<String> {
        let picks = self.pick(prompt, &items, false, &[])?;
        picks
            .into_iter()
            .next()
            .and_then(|i| items.get(i).cloned())
            .ok_or_else(|| anyhow::anyhow!("cancelled"))
    }

    pub fn multiselect_indexed(
        &mut self,
        prompt: &str,
        items: &[String],
        default_checked: &[usize],
    ) -> Result<Vec<usize>> {
        if items.is_empty() {
            return Ok(Vec::new());
        }
        self.pick(prompt, items, true, default_checked)
    }

    fn pick(
        &mut self,
        prompt: &str,
        items: &[String],
        multi: bool,
        initial_checked: &[usize],
    ) -> Result<Vec<usize>> {
        if items.is_empty() {
            bail!("nothing to choose from");
        }
        let mut picker = Picker::new(items.len(), multi, initial_checked);
        loop {
            self.draw(Some(Popup::Pick {
                prompt,
                items,
                picker: &picker,
            }))?;
            let Some((code, mods)) = next_key()? else {
                continue;
            };
            match picker.step(code, mods) {
                Step::Continue => {}
                Step::Accept => break,
                Step::Cancel => bail!("cancelled"),
            }
        }
        Ok(picker.confirmed())
    }

    // ---- rendering ---------------------------------------------------------

    fn draw(&mut self, popup: Option<Popup>) -> Result<()> {
        // Every frame is a chance for a timed-out secret to leave the pane.
        self.retire_transient(false);
        // Pull each field out so `term.draw` (mut) and the read-only panel state
        // are disjoint borrows of `self`.
        let lines = self.out.clone();
        let lines = lines.borrow();
        let status = &self.status;
        let rows = &self.rows;
        let query = &self.query;
        let filtering = self.filtering;
        let sel = self.sel;
        let busy = self
            .busy
            .as_ref()
            .map(|label| (label.as_str(), SPINNER[self.spin % SPINNER.len()]));
        self.term.draw(|f| {
            let m = MenuView {
                status,
                rows,
                query,
                filtering,
                sel,
            };
            draw_frame(f, &m, &lines, popup.as_ref(), busy);
        })?;
        Ok(())
    }
}

/// The panel drives the reusable action cores through the shared [`Frontend`]
/// surface (delegating to its own inherent methods), so a core can run under
/// either the panel or the headless CLI. The panel keeps its richer inherent
/// prompt methods (`confirm`/`text`/`select`/…) for the interactive wrappers.
impl crate::frontend::Frontend for Ui {
    fn log(&mut self, line: impl Into<String>) {
        Ui::log(self, line);
    }
    fn run_task(&mut self, label: &str, cmd: introdus_core::process::Cmd) -> Result<()> {
        Ui::run_task(self, label, cmd)
    }
}

impl Drop for Ui {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(self.term.backend_mut(), LeaveAlternateScreen);
        let _ = self.term.show_cursor();
    }
}

fn blank_status() -> Status {
    Status {
        project: String::new(),
        container: String::new(),
        state: "…",
        webapp_port: 0,
        webapp_host_port: None,
        agents: String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ta170_transient_erase_replaces_only_its_own_lines() {
        let mut lines: Vec<String> = ["▶ connect", "tcp://x?password=p", "host x", "  ! later"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect();
        erase(&mut lines, 1..3, "(hidden)".to_owned());
        assert_eq!(lines, vec!["▶ connect", "(hidden)", "  ! later"]);
        // The secret is gone from the buffer entirely, not just off-screen.
        assert!(!lines.iter().any(|l| l.contains("password")));
        // A stale range (buffer rewritten under us) is a no-op, never a panic.
        let before = lines.clone();
        erase(&mut lines, 2..9, "(hidden)".to_owned());
        assert_eq!(lines, before);
    }
}
