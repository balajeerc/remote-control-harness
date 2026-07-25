//! Test-only helper that renders a real ratatui frame into an off-screen
//! [`TestBackend`] buffer and serializes it to a self-contained, colour SVG —
//! the "screenshots" embedded in the docs. It is compiled only under `cfg(test)`
//! and driven by the `#[ignore]`d `shot_*` generator tests scattered through the
//! UI modules (`panel`, `send_files::browser`, `ui`); run them on demand with:
//!
//! ```text
//! cargo test -p introdus-cli -- --ignored shot_
//! ```
//!
//! The generators own the fixtures (what state to draw); this module owns the
//! backend plumbing and the buffer→SVG conversion, so the two concerns stay
//! separate and every screen is captured the same way.

use std::path::PathBuf;

use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::style::{Color, Modifier};
use ratatui::Terminal;

/// Monospace cell geometry, in SVG user units. `textLength` pins each run to an
/// exact multiple of `CW`, so glyph alignment is independent of the viewer's
/// installed font metrics.
const CW: f32 = 8.5;
const CH: f32 = 18.0;
const FS: f32 = 14.0;
/// Padding inside the terminal area, plus the height of the window title bar.
const PAD: f32 = 14.0;
const BAR: f32 = 30.0;

const FONT: &str = "'DejaVu Sans Mono','JetBrains Mono',Menlo,Consolas,'Liberation Mono',monospace";

/// Page background (behind `Color::Reset` bg) and default foreground.
const BG: &str = "#181825";
const FG: &str = "#cdd6f4";

/// Render `draw` into a `width`×`height` off-screen buffer and return it.
pub(crate) fn render<F>(width: u16, height: u16, draw: F) -> Buffer
where
    F: FnOnce(&mut ratatui::Frame),
{
    let mut term = Terminal::new(TestBackend::new(width, height)).expect("test backend");
    term.draw(|f| draw(f)).expect("draw");
    term.backend().buffer().clone()
}

/// Render a frame and write it out as `docs/img/<name>.svg` with `caption` in
/// the window title bar.
pub(crate) fn capture<F>(name: &str, caption: &str, width: u16, height: u16, draw: F)
where
    F: FnOnce(&mut ratatui::Frame),
{
    let buf = render(width, height, draw);
    let svg = to_svg(&buf, caption);
    let path = out_dir().join(format!("{name}.svg"));
    std::fs::create_dir_all(path.parent().unwrap()).expect("mkdir docs/img");
    std::fs::write(&path, svg).expect("write svg");
    // Visible when the generator runs with --nocapture.
    println!("wrote {}", path.display());
}

/// `<repo>/docs/img`, resolved from this crate's manifest dir (stable no matter
/// what CWD the test runner uses).
fn out_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/img")
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/img"))
}

/// Serialize a buffer to a standalone SVG styled as a terminal window.
fn to_svg(buf: &Buffer, caption: &str) -> String {
    let area = *buf.area();
    let (cols, rows) = (area.width, area.height);
    let term_w = cols as f32 * CW;
    let term_h = rows as f32 * CH;
    let w = term_w + 2.0 * PAD;
    let h = term_h + 2.0 * PAD + BAR;

    let mut s = String::new();
    s.push_str(&format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{w:.0}\" height=\"{h:.0}\" \
         viewBox=\"0 0 {w:.0} {h:.0}\" font-family=\"{FONT}\">\n"
    ));
    // Window chrome: rounded card + title bar + traffic-light dots + caption.
    s.push_str(&format!(
        "<rect x=\"0\" y=\"0\" width=\"{w:.0}\" height=\"{h:.0}\" rx=\"10\" fill=\"{BG}\"/>\n"
    ));
    s.push_str(&format!(
        "<path d=\"M0 10 Q0 0 10 0 H{x} Q{w:.0} 0 {w:.0} 10 V{BAR} H0 Z\" fill=\"#313244\"/>\n",
        x = w - 10.0,
    ));
    for (i, c) in ["#f38ba8", "#f9e2af", "#a6e3a1"].iter().enumerate() {
        s.push_str(&format!(
            "<circle cx=\"{cx:.0}\" cy=\"15\" r=\"5.5\" fill=\"{c}\"/>\n",
            cx = 18.0 + i as f32 * 20.0
        ));
    }
    s.push_str(&format!(
        "<text x=\"{cx:.0}\" y=\"20\" fill=\"#9399b2\" font-size=\"13\" \
         text-anchor=\"middle\">{}</text>\n",
        xml_escape(caption),
        cx = w / 2.0
    ));

    // Terminal cells live below the bar.
    let top = BAR + PAD;
    s.push_str(&format!("<g transform=\"translate({PAD},{top})\">\n"));
    push_backgrounds(&mut s, buf, cols, rows);
    push_text(&mut s, buf, cols, rows);
    s.push_str("</g>\n</svg>\n");
    s
}

/// Emit one `<rect>` per horizontal run of cells sharing a non-default bg.
fn push_backgrounds(s: &mut String, buf: &Buffer, cols: u16, rows: u16) {
    for y in 0..rows {
        let mut x = 0u16;
        while x < cols {
            let bg = eff_bg(cell(buf, x, y));
            if bg == BG {
                x += 1;
                continue;
            }
            let start = x;
            while x < cols && eff_bg(cell(buf, x, y)) == bg {
                x += 1;
            }
            s.push_str(&format!(
                "<rect x=\"{px:.1}\" y=\"{py:.1}\" width=\"{rw:.1}\" height=\"{CH:.1}\" \
                 fill=\"{bg}\"/>\n",
                px = start as f32 * CW,
                py = y as f32 * CH,
                rw = (x - start) as f32 * CW,
            ));
        }
    }
}

/// Emit one `<text>` per horizontal run of cells sharing a foreground style,
/// pinned to an exact width so alignment survives any font.
fn push_text(s: &mut String, buf: &Buffer, cols: u16, rows: u16) {
    for y in 0..rows {
        let baseline = y as f32 * CH + FS;
        let mut x = 0u16;
        while x < cols {
            let key = fg_key(cell(buf, x, y));
            let start = x;
            let mut run = String::new();
            while x < cols && fg_key(cell(buf, x, y)) == key {
                run.push_str(cell(buf, x, y).map_or(" ", |c| c.symbol()));
                x += 1;
            }
            if run.trim().is_empty() {
                continue;
            }
            let (fill, bold) = key;
            let weight = if bold { " font-weight=\"bold\"" } else { "" };
            s.push_str(&format!(
                "<text xml:space=\"preserve\" x=\"{px:.1}\" y=\"{baseline:.1}\" \
                 fill=\"{fill}\" font-size=\"{FS}\" textLength=\"{tl:.1}\" \
                 lengthAdjust=\"spacingAndGlyphs\"{weight}>{}</text>\n",
                xml_escape(&run),
                px = start as f32 * CW,
                tl = (x - start) as f32 * CW,
            ));
        }
    }
}

fn cell(buf: &Buffer, x: u16, y: u16) -> Option<&ratatui::buffer::Cell> {
    buf.cell((x, y))
}

/// The effective background hex for a cell, honouring the REVERSED modifier.
fn eff_bg(c: Option<&ratatui::buffer::Cell>) -> &'static str {
    match c {
        None => BG,
        Some(c) if c.modifier.contains(Modifier::REVERSED) => hex(c.fg, FG),
        Some(c) => hex(c.bg, BG),
    }
}

/// Foreground run key: (hex colour, bold?) — cells break into a new `<text>`
/// run when either changes.
fn fg_key(c: Option<&ratatui::buffer::Cell>) -> (&'static str, bool) {
    match c {
        None => (FG, false),
        Some(c) => {
            let bold = c.modifier.contains(Modifier::BOLD);
            if c.modifier.contains(Modifier::REVERSED) {
                (hex(c.bg, BG), bold)
            } else {
                (hex(c.fg, FG), bold)
            }
        }
    }
}

/// Map a ratatui colour to a hex string (Catppuccin Mocha palette), falling back
/// to `default` for `Reset`.
fn hex(c: Color, default: &'static str) -> &'static str {
    match c {
        Color::Reset => default,
        Color::Black => "#11111b",
        Color::Red | Color::LightRed => "#f38ba8",
        Color::Green | Color::LightGreen => "#a6e3a1",
        Color::Yellow | Color::LightYellow => "#f9e2af",
        Color::Blue | Color::LightBlue => "#89b4fa",
        Color::Magenta | Color::LightMagenta => "#f5c2e7",
        Color::Cyan => "#89dceb",
        Color::LightCyan => "#94e2d5",
        Color::Gray => "#bac2de",
        Color::DarkGray => "#6c7086",
        Color::White => "#cdd6f4",
        // Rgb/Indexed are not used by this UI; approximate to the default fg.
        _ => default,
    }
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

// ============================================================================
// Doc-screenshot generators — `#[ignore]`d so they never run in the normal
// suite; invoke on demand with `cargo test -p introdus-cli -- --ignored shot_`.
// Fixtures live here; the drawn state mirrors what production renders.
// ============================================================================

use ratatui::layout::{Constraint, Layout};
use ratatui::widgets::Paragraph;

use crate::panel::{draw_frame, MenuView, Popup};
use crate::send_files::browser::{render as render_browser, Pane, PaneSource};
use crate::ui::{question, Picker, Row, Status};
use introdus_core::containers::{DirEntry, SortMode};

fn demo_status() -> Status {
    Status {
        project: "acme-web".into(),
        container: "introdus-brave-swift-otter".into(),
        state: "running",
        webapp_port: 5173,
        agents: "claude, codex".into(),
    }
}

/// The full control menu, mirroring `menu::MENU` (icons + hotkeys included).
fn demo_menu_rows() -> Vec<Row> {
    let section = |icon: char, title: &str, items: &[(char, &str)], rows: &mut Vec<Row>| {
        rows.push(Row::Header {
            icon,
            title: title.to_owned(),
        });
        rows.extend(items.iter().map(|(key, label)| Row::Item {
            key: *key,
            label: (*label).to_owned(),
        }));
    };
    let mut rows = Vec::new();
    section(
        '$',
        "Access container",
        &[
            ('t', "Open a dev terminal (tmux window)"),
            ('T', "Open a root terminal (tmux window)"),
            ('c', "Copy a host file/folder into the container"),
        ],
        &mut rows,
    );
    section(
        '✦',
        "Agents",
        &[
            ('a', "Launch an installed agent (tmux window)"),
            ('i', "Install a coding agent"),
            ('p', "(Re)Install paseo (control agents from your phone)"),
            ('P', "Show paseo pairing QR code (connect your phone)"),
            (
                'o',
                "Add a paseo client origin (direct-mode browser access)",
            ),
        ],
        &mut rows,
    );
    section(
        '⇅',
        "Networking & egress security",
        &[
            ('b', "List recently blocked egress URLs"),
            ('w', "Add hostnames to the egress allowlist"),
            ('e', "(Re)Expose app via Cloudflare Tunnel"),
            ('u', "Show tunnel URL"),
            ('n', "Enable ntfy.sh mobile notifications"),
        ],
        &mut rows,
    );
    section(
        '?',
        "Troubleshooting",
        &[
            ('f', "Refresh container status"),
            ('N', "Send a test notification to host"),
            ('l', "Show the notification log"),
            ('v', "Restart the notification service"),
        ],
        &mut rows,
    );
    section(
        '↻',
        "Container lifecycle",
        &[
            ('s', "Restart the container"),
            ('x', "Recreate the container (apply config changes)"),
            (
                'h',
                "Detach from this tmux session (keep container running)",
            ),
            ('d', "Destroy/Reset the container (wipe the volume)"),
            ('q', "Quit introdus (stops the container)"),
        ],
        &mut rows,
    );
    rows
}

#[test]
#[ignore = "doc screenshot generator"]
fn shot_control_panel() {
    let status = demo_status();
    let rows = demo_menu_rows();
    let out = vec![
        "▶ Show tunnel URL".to_owned(),
        "  webapp tunnel is live:".to_owned(),
        "  https://brave-swift-otter.trycloudflare.com".to_owned(),
        String::new(),
        "  (also written to the dev-container log)".to_owned(),
    ];
    // sel 11 = "Show tunnel URL" (the 12th selectable item), matching the output.
    // Height fits the full five-section menu + status header + footer.
    capture(
        "control-panel",
        "introdus — control panel",
        112,
        40,
        |f| {
            let m = MenuView {
                status: &status,
                rows: &rows,
                query: "",
                filtering: false,
                sel: 11,
            };
            draw_frame(f, &m, &out, None, None);
        },
    );
}

#[test]
#[ignore = "doc screenshot generator"]
fn shot_control_panel_confirm() {
    let status = demo_status();
    let rows = demo_menu_rows();
    let out = vec!["▶ Destroy/Reset the container (wipe the volume)".to_owned()];
    let popup = Popup::Confirm {
        prompt: "Destroy/Reset introdus-brave-swift-otter and permanently wipe its \
                 /home/dev volume (repo, node_modules, toolchains)?",
        answer: false,
    };
    // sel 20 = "Destroy/Reset the container", matching the prompt.
    capture(
        "control-panel-confirm",
        "introdus — confirmation prompt",
        112,
        40,
        |f| {
            let m = MenuView {
                status: &status,
                rows: &rows,
                query: "",
                filtering: false,
                sel: 20,
            };
            draw_frame(f, &m, &out, Some(&popup), None);
        },
    );
}

#[test]
#[ignore = "doc screenshot generator"]
fn shot_wizard_agents() {
    let items: Vec<String> = [
        "Claude (Anthropic)  [runs its own npm postinstall]",
        "Codex (OpenAI)",
        "Antigravity (Google)  [vendor installer — runs remote code]",
        "Opencode (Open source)",
        "Pi agent (Open source)",
        "Kilocode CLI (kilo.sh)",
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect();
    // Claude + Codex pre-checked, cursor resting on Antigravity.
    let mut picker = Picker::new(items.len(), true, &[0, 1]);
    picker.cursor = 2;
    capture(
        "wizard-agents",
        "introdus setup wizard — pick coding agents",
        88,
        8,
        |f| {
            let rows =
                Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).split(f.area());
            f.render_widget(
                Paragraph::new(question("Which coding agents should introdus install?")),
                rows[0],
            );
            let (list, mut state) = picker.list(&items);
            f.render_stateful_widget(list, rows[1], &mut state);
        },
    );
}

fn demo_pane(title: &'static str, cwd: &str, all: Vec<DirEntry>, selected: usize) -> Pane {
    let view: Vec<usize> = (0..all.len()).collect();
    let mut state = ratatui::widgets::ListState::default();
    state.select(Some(selected));
    Pane {
        title,
        cwd: cwd.to_owned(),
        source: PaneSource::Local,
        state,
        all,
        view,
        cursor: selected,
        sort: SortMode::Name,
        filter: String::new(),
        show_hidden: false,
    }
}

#[test]
#[ignore = "doc screenshot generator"]
fn shot_send_files() {
    let d = |name: &str| DirEntry {
        name: name.to_owned(),
        is_dir: true,
        modified: None,
        created: None,
    };
    let file = |name: &str| DirEntry {
        name: name.to_owned(),
        is_dir: false,
        modified: None,
        created: None,
    };
    let mut left = demo_pane(
        "laptop",
        "/home/you/datasets",
        vec![
            d(".."),
            d("imagenet-mini"),
            d("raw"),
            file("train.csv"),
            file("labels.parquet"),
            file("NOTES.md"),
        ],
        3,
    );
    let mut right = demo_pane(
        "container",
        "/home/dev/work/acme-web",
        vec![
            d(".."),
            d("src"),
            d("node_modules"),
            file("package.json"),
            file("pnpm-lock.yaml"),
            file("tsconfig.json"),
        ],
        1,
    );
    capture(
        "send-files",
        "introdus send-files — dual-pane transfer",
        118,
        24,
        |f| {
            render_browser(
                f,
                "acme-web  (introdus-brave-swift-otter · local)",
                &mut left,
                &mut right,
                true,
                Some("/home/you/datasets/train.csv"),
                "",
                false,
            );
        },
    );
}
