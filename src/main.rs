use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers, MouseEventKind, EnableMouseCapture, DisableMouseCapture},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{List, ListItem, Paragraph},
    Frame, Terminal,
};
use regex::Regex;
use std::io::{self, BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

// ── Device detection ─────────────────────────────────────────────────────────

fn get_devices() -> Vec<(String, String)> {
    let output = Command::new("flutter")
        .args(["devices", "--machine"])
        .output()
        .expect("failed to run 'flutter devices'");
    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_devices_json(&stdout).unwrap_or_default()
}

fn parse_devices_json(json: &str) -> Result<Vec<(String, String)>, ()> {
    let mut devices = Vec::new();
    let content = json.trim().strip_prefix('[').ok_or(())?;
    let content = content.trim_end_matches(']');
    let mut depth = 0;
    let mut current = String::new();
    let mut objects = Vec::new();
    for c in content.chars() {
        match c {
            '{' => { depth += 1; current.push(c); }
            '}' => {
                depth -= 1;
                current.push(c);
                if depth == 0 {
                    objects.push(current.clone());
                    current.clear();
                }
            }
            _ => { if depth > 0 { current.push(c); } }
        }
    }
    for obj in objects {
        let id = extract_json_string(&obj, "id");
        let name = extract_json_string(&obj, "name");
        if let (Some(id), Some(name)) = (id, name) {
            devices.push((id, name));
        }
    }
    Ok(devices)
}

fn extract_json_string(json: &str, key: &str) -> Option<String> {
    let pattern = format!("\"{}\"", key);
    let idx = json.find(&pattern)?;
    let rest = &json[idx + pattern.len()..];
    let rest = rest.trim_start().strip_prefix(':')?.trim_start().strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

fn select_device(devices: &[(String, String)]) -> &str {
    eprintln!("Multiple devices found. Select one:");
    for (i, (id, name)) in devices.iter().enumerate() {
        eprintln!("  [{}] {} ({})", i + 1, name, id);
    }
    eprint!("Enter number: ");
    std::io::stderr().flush().unwrap();
    let mut input = String::new();
    std::io::stdin().read_line(&mut input).unwrap();
    let choice: usize = input.trim().parse().unwrap_or(1);
    let idx = choice.saturating_sub(1).min(devices.len() - 1);
    &devices[idx].0
}

// ── Clipboard ─────────────────────────────────────────────────────────────────

fn copy_to_clipboard(text: &str) {
    // Try Wayland first, then X11 tools
    let candidates: &[(&str, &[&str])] = &[
        ("wl-copy", &[]),
        ("xclip", &["-selection", "clipboard"]),
        ("xsel", &["--clipboard", "--input"]),
    ];
    for (cmd, args) in candidates {
        if let Ok(mut child) = Command::new(cmd)
            .args(*args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            if let Some(stdin) = child.stdin.as_mut() {
                let _ = stdin.write_all(text.as_bytes());
            }
            let _ = child.wait();
            return;
        }
    }
}

/// Open content in vim via a temp file.
/// If the text contains a JSON body (anything from the first `{` or `[` after
/// the log header), it is pretty-printed with `python3 -m json.tool` first.
/// Returns the formatted text so the caller can restore the TUI after vim exits.
fn open_in_vim(text: &str) {
    let (header, json_body) = find_json_split(text);

    let display_text = if let Some(json) = json_body {
        let pretty = pretty_print_json(json).unwrap_or_else(|| json.to_string());
        // Comment out every header line with // so the file is valid JSONC
        let commented_header: String = header
            .lines()
            .map(|l| format!("// {}\n", l))
            .collect();
        format!("{}{}", commented_header, pretty)
    } else {
        // No JSON — comment out all lines so it at least opens as jsonc cleanly
        text.lines()
            .map(|l| format!("// {}\n", l))
            .collect()
    };

    // Write to a temp file with .jsonc extension for vim filetype detection
    let tmp_path = "/tmp/flogs_view.jsonc";
    if let Ok(mut f) = std::fs::File::create(tmp_path) {
        let _ = f.write_all(display_text.as_bytes());
    } else {
        return;
    }

    // Suspend the TUI
    let _ = disable_raw_mode();
    let mut stdout = io::stdout();
    let _ = execute!(stdout, LeaveAlternateScreen, DisableMouseCapture);

    // Open vim read-only; set ft=jsonc explicitly in case vim doesn't auto-detect
    let _ = Command::new("vim")
        .args(["-R", "+set ft=jsonc", "+set wrap", tmp_path])
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status();

    // Restore the TUI
    let _ = enable_raw_mode();
    let _ = execute!(stdout, EnterAlternateScreen, EnableMouseCapture);
}

/// Find the split point between a log header and a JSON body.
/// Returns (header, Some(json_str)) or (full_text, None).
fn find_json_split(text: &str) -> (&str, Option<&str>) {
    // Skip past the leading [TAG] prefix so we don't match the opening bracket
    // of the tag itself (e.g. "[Http]" starts with '[').
    let search_from = if text.starts_with('[') {
        text.find(']').map(|i| i + 1).unwrap_or(0)
    } else {
        0
    };

    for (i, c) in text[search_from..].char_indices() {
        if c == '{' || c == '[' {
            let abs = search_from + i;
            let before = &text[..abs];
            let trimmed = before.trim_end();
            // Valid JSON start: preceded by `: ` or a newline, or start of string
            if trimmed.ends_with(':') || trimmed.ends_with('\n') || abs == 0 {
                let header = text[..abs]
                    .trim_end_matches(|c: char| c == ':' || c == ' ');
                return (header, Some(&text[abs..]));
            }
        }
    }
    (text, None)
}

/// Pretty-print JSON using python3. Returns None if python3 isn't available or
/// the input isn't valid JSON.
fn pretty_print_json(json: &str) -> Option<String> {
    let mut child = Command::new("python3")
        .args(["-m", "json.tool", "--no-ensure-ascii"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    if let Some(stdin) = child.stdin.as_mut() {
        let _ = stdin.write_all(json.as_bytes());
    }
    let out = child.wait_with_output().ok()?;
    if out.status.success() {
        String::from_utf8(out.stdout).ok()
    } else {
        None
    }
}

// ── App state ─────────────────────────────────────────────────────────────────

#[derive(Copy, Clone, PartialEq)]
enum Mode {
    Normal,
    Filtering,
}

/// One reassembled log entry.
struct LogEntry {
    text: String,
    expanded: bool,
}

impl LogEntry {
    fn new(text: String) -> Self {
        Self { text, expanded: true }
    }

    /// True if this entry spans more than one rendered row when expanded.
    /// Entries that would fit on a single row have nothing to expand/collapse.
    fn is_expandable(&self, width: usize) -> bool {
        let usable = width.saturating_sub(2).max(1);
        let rows: usize = self.text
            .lines()
            .map(|l| {
                if l.is_empty() { 1 } else { (l.len() + usable - 1) / usable }
            })
            .sum();
        rows > 1
    }

    /// Number of rendered rows this entry occupies given a terminal width.
    /// Expanded: wraps; collapsed: always 1 row.
    fn row_count(&self, width: usize) -> usize {
        if !self.expanded {
            return 1;
        }
        // Account for the 2-char "▼ " prefix
        let usable = width.saturating_sub(2).max(1);
        self.text
            .lines()
            .map(|l| {
                if l.is_empty() {
                    1
                } else {
                    (l.len() + usable - 1) / usable
                }
            })
            .sum::<usize>()
            .max(1)
    }
}

struct App {
    entries: Vec<LogEntry>,
    filter: Option<String>,
    filter_input: String,
    mode: Mode,
    /// Index into filtered entries that is currently selected (cursor).
    selected: usize,
    /// Clipboard feedback message + countdown ticks.
    copied_msg: Option<(String, u8)>,
}

impl App {
    fn new() -> Self {
        Self {
            entries: Vec::new(),
            filter: None,
            filter_input: String::new(),
            mode: Mode::Normal,
            selected: 0,
            copied_msg: None,
        }
    }

    fn push_entry(&mut self, text: String) {
        self.entries.push(LogEntry::new(text));
        // If selected was pointing at the last item, keep tracking the tail
        let filtered_len = self.filtered_indices().len();
        if self.selected + 1 >= filtered_len.saturating_sub(1) {
            self.selected = filtered_len.saturating_sub(1);
        }
    }

    /// Returns indices into `self.entries` that pass the current filter.
    fn filtered_indices(&self) -> Vec<usize> {
        match &self.filter {
            None => (0..self.entries.len()).collect(),
            Some(filter_str) => {
                let filters: Vec<String> = filter_str
                    .split(',')
                    .map(|t| t.trim().to_ascii_lowercase())
                    .filter(|t| !t.is_empty())
                    .collect();
                if filters.is_empty() {
                    return (0..self.entries.len()).collect();
                }

                let tag_re = Regex::new(r"^\[[A-Za-z][A-Za-z0-9 _\-]{0,40}\]").unwrap();
                let mut matching = std::collections::HashSet::new();
                for (i, entry) in self.entries.iter().enumerate() {
                    let lower = entry.text.to_ascii_lowercase();
                    if filters.iter().any(|f| lower.contains(f)) {
                        matching.insert(i);
                    }
                }

                let mut include = std::collections::HashSet::new();
                for &idx in &matching {
                    include.insert(idx);
                    // Walk backwards to include context lines
                    let mut i = idx;
                    while i > 0 {
                        i -= 1;
                        if tag_re.is_match(&self.entries[i].text) {
                            if matching.contains(&i) { include.insert(i); }
                            break;
                        }
                        include.insert(i);
                    }
                    // Walk forwards to include context lines
                    let mut i = idx;
                    while i < self.entries.len() - 1 {
                        i += 1;
                        if tag_re.is_match(&self.entries[i].text) {
                            if matching.contains(&i) { include.insert(i); }
                            break;
                        }
                        include.insert(i);
                    }
                }

                let mut indices: Vec<_> = include.into_iter().collect();
                indices.sort_unstable();
                indices
            }
        }
    }

    fn clamp_selected(&mut self) {
        let len = self.filtered_indices().len();
        if len == 0 {
            self.selected = 0;
        } else if self.selected >= len {
            self.selected = len - 1;
        }
    }

    fn move_up(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
        }
    }

    fn move_down(&mut self) {
        let len = self.filtered_indices().len();
        if len > 0 && self.selected < len - 1 {
            self.selected += 1;
        }
    }

    fn toggle_selected(&mut self) {
        let indices = self.filtered_indices();
        if let Some(&real_idx) = indices.get(self.selected) {
            self.entries[real_idx].expanded = !self.entries[real_idx].expanded;
        }
    }

    fn copy_selected(&mut self) {
        let indices = self.filtered_indices();
        if let Some(&real_idx) = indices.get(self.selected) {
            let text = self.entries[real_idx].text.clone();
            copy_to_clipboard(&text);
            self.copied_msg = Some(("Copied!".to_string(), 12)); // ~0.6s at 50ms tick
        }
    }

    fn open_selected(&self) {
        let indices = self.filtered_indices();
        if let Some(&real_idx) = indices.get(self.selected) {
            open_in_vim(&self.entries[real_idx].text);
        }
    }
}

// ── Rendering ─────────────────────────────────────────────────────────────────

fn draw(frame: &mut Frame, app: &App) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(area);

    let log_height = chunks[0].height as usize;
    let log_width = chunks[0].width as usize;

    let indices = app.filtered_indices();
    let total = indices.len();

    // Build rendered rows for each visible entry.
    // We need to figure out which entries are visible given the selected cursor
    // and view_offset. Strategy: ensure selected entry is always visible.
    //
    // We render from the bottom upwards, placing the selected item in view.

    // Compute total rows needed for all entries (expensive for large logs,
    // but we only need to walk from selected outward to fill log_height rows).
    //
    // Find the row range [first_visible_entry .. last_visible_entry].

    // Phase 1: collect row counts for all entries (only compute near selection)
    // We walk from selected upward until we've accumulated log_height rows.
    let selected = app.selected.min(total.saturating_sub(1));

    // Build a window of entries that fits on screen, keeping `selected` visible.
    // We'll try to keep the selected entry near the bottom (tail-following feel).
    let mut window_entries: Vec<(usize, &LogEntry)> = Vec::new(); // (filtered_idx, entry)

    if total > 0 {
        // Walk backwards from selected accumulating rows until full
        let mut start_idx = selected;

        // First, include selected and entries below it up to the screen
        let mut below_rows = 0usize;
        for fi in selected..total {
            let entry = &app.entries[indices[fi]];
            let r = entry.row_count(log_width);
            if below_rows + r > log_height {
                break;
            }
            below_rows += r;
        }

        // Now fill the rest of the screen going upward from selected
        let mut rows_accumulated = below_rows;
        loop {
            if start_idx == 0 || rows_accumulated >= log_height {
                break;
            }
            start_idx -= 1;
            let r = app.entries[indices[start_idx]].row_count(log_width);
            if rows_accumulated + r > log_height {
                start_idx += 1;
                break;
            }
            rows_accumulated += r;
        }

        // Collect entries from start_idx until we fill the screen
        let mut used_rows = 0usize;
        for fi in start_idx..total {
            if used_rows >= log_height {
                break;
            }
            window_entries.push((fi, &app.entries[indices[fi]]));
            used_rows += app.entries[indices[fi]].row_count(log_width);
        }
    }

    // Convert entries in the window into ListItems
    let mut items: Vec<ListItem> = Vec::new();
    let usable_width = log_width.saturating_sub(2).max(1); // subtract prefix "▶ " or "▼ "

    for (fi, entry) in &window_entries {
        let is_selected = *fi == selected;
        let expandable = entry.is_expandable(log_width);
        let prefix = if !expandable {
            "· "
        } else if entry.expanded {
            "▼ "
        } else {
            "▶ "
        };

        let base_style = if is_selected {
            Style::default().bg(Color::Blue).fg(Color::White).add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };

        if !entry.expanded {
            // Single row, truncate
            let first_line = entry.text.lines().next().unwrap_or("");
            let truncated = if first_line.len() > usable_width {
                format!("{}{}…", prefix, &first_line[..usable_width.saturating_sub(1)])
            } else {
                format!("{}{}", prefix, first_line)
            };
            items.push(ListItem::new(Line::from(Span::styled(truncated, base_style))));
        } else {
            // Expanded: word-wrap each logical line
            let mut first_row = true;
            for logical_line in entry.text.lines() {
                if logical_line.is_empty() {
                    let pfx = if first_row { prefix } else { "  " };
                    items.push(ListItem::new(Line::from(Span::styled(pfx.to_string(), base_style))));
                    first_row = false;
                    continue;
                }
                // Wrap the logical line into chunks of usable_width
                let mut offset = 0;
                while offset < logical_line.len() {
                    // Find a safe char boundary
                    let mut end = (offset + usable_width).min(logical_line.len());
                    while !logical_line.is_char_boundary(end) {
                        end -= 1;
                    }
                    let chunk = &logical_line[offset..end];
                    let pfx = if first_row { prefix } else { "  " };
                    items.push(ListItem::new(Line::from(Span::styled(
                        format!("{}{}", pfx, chunk),
                        base_style,
                    ))));
                    first_row = false;
                    offset = end;
                }
            }
            if first_row {
                // Entry text was empty
                items.push(ListItem::new(Line::from(Span::styled(
                    prefix.to_string(),
                    base_style,
                ))));
            }
        }
    }

    frame.render_widget(List::new(items), chunks[0]);

    // ── Status bar ────────────────────────────────────────────────────────────
    let bar_style = Style::default().bg(Color::DarkGray).fg(Color::White);

    let status_text = match app.mode {
        Mode::Filtering => format!(
            "  Filter: {}█  [Enter] apply  [Esc] cancel",
            app.filter_input
        ),
        Mode::Normal => {
            // Show clipboard feedback if active
            if let Some((msg, _)) = &app.copied_msg {
                format!("  {} ({}/{})", msg, selected + 1, total)
            } else {
                let pos = if total > 0 {
                    format!(" ({}/{})", selected + 1, total)
                } else {
                    String::new()
                };
                match &app.filter {
                    None => format!(
                        "  [↑↓] navigate  [Space] expand  [y] copy  [o] open  [/] filter  [c] clear  [q] quit{}",
                        pos
                    ),
                    Some(filter_str) => {
                        let pills: String = filter_str
                            .split(',')
                            .map(|t| t.trim())
                            .filter(|t| !t.is_empty())
                            .map(|t| format!("[{}]", t))
                            .collect::<Vec<_>>()
                            .join(" ");
                        format!(
                            "  Filter: {}  [Esc] clear  [/] change  [Space] expand  [y] copy  [o] open  [c] clear logs  [q] quit{}",
                            pills, pos
                        )
                    }
                }
            }
        }
    };

    frame.render_widget(
        Paragraph::new(status_text).style(bar_style),
        chunks[1],
    );
}

// ── Event loop ────────────────────────────────────────────────────────────────

fn run_app(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    rx: mpsc::Receiver<String>,
) -> io::Result<()> {
    let mut app = App::new();
    loop {
        // Drain incoming log lines
        let mut got_new = false;
        while let Ok(line) = rx.try_recv() {
            app.push_entry(line);
            got_new = true;
        }
        if got_new {
            app.clamp_selected();
        }

        // Tick down the clipboard feedback message
        if let Some((_, ref mut ticks)) = app.copied_msg {
            if *ticks == 0 {
                app.copied_msg = None;
            } else {
                *ticks -= 1;
            }
        }

        terminal.draw(|f| draw(f, &app))?;

        if event::poll(Duration::from_millis(50))? {
            match event::read()? {
                Event::Key(key) => {
                    match app.mode {
                        Mode::Filtering => match key.code {
                            KeyCode::Esc => {
                                app.mode = Mode::Normal;
                                app.filter_input.clear();
                            }
                            KeyCode::Enter => {
                                let input = app.filter_input.trim().to_string();
                                app.filter = if input.is_empty() { None } else { Some(input) };
                                app.filter_input.clear();
                                app.mode = Mode::Normal;
                                app.selected = app.filtered_indices().len().saturating_sub(1);
                            }
                            KeyCode::Backspace => {
                                app.filter_input.pop();
                            }
                            KeyCode::Char(c) => {
                                app.filter_input.push(c);
                            }
                            _ => {}
                        },
                        Mode::Normal => match key.code {
                            KeyCode::Char('q') => return Ok(()),
                            KeyCode::Char('c')
                                if key.modifiers.contains(KeyModifiers::CONTROL) =>
                            {
                                return Ok(())
                            }
                            KeyCode::Char('c') => {
                                app.entries.clear();
                                app.selected = 0;
                            }
                            KeyCode::Char('/') => {
                                app.mode = Mode::Filtering;
                                app.filter_input = app.filter.clone().unwrap_or_default();
                            }
                            KeyCode::Esc => {
                                app.filter = None;
                                app.selected = app.filtered_indices().len().saturating_sub(1);
                            }
                            KeyCode::Up | KeyCode::Char('k') => app.move_up(),
                            KeyCode::Down | KeyCode::Char('j') => app.move_down(),
                            KeyCode::PageUp => {
                                for _ in 0..10 { app.move_up(); }
                            }
                            KeyCode::PageDown => {
                                for _ in 0..10 { app.move_down(); }
                            }
                            KeyCode::Enter | KeyCode::Char(' ') => app.toggle_selected(),
                            KeyCode::Char('y') => app.copy_selected(),
                            KeyCode::Char('o') => {
                                // Suspend TUI, open in vim, then redraw fully on return
                                app.open_selected();
                                // Force a full redraw after vim exits
                                terminal.clear()?;
                            }
                            _ => {}
                        },
                    }
                }
                Event::Mouse(mouse) => {
                    if app.mode == Mode::Normal {
                        match mouse.kind {
                            MouseEventKind::ScrollUp => {
                                for _ in 0..3 { app.move_up(); }
                            }
                            MouseEventKind::ScrollDown => {
                                for _ in 0..3 { app.move_down(); }
                            }
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

// ── Entry point ───────────────────────────────────────────────────────────────

fn main() {
    let devices = get_devices();
    let mut flutter_args = vec!["logs".to_string()];
    if devices.len() > 1 {
        let device_id = select_device(&devices).to_string();
        flutter_args.push("-d".to_string());
        flutter_args.push(device_id);
    }

    let (tx, rx) = mpsc::channel::<String>();

    thread::spawn(move || {
        let mut child = Command::new("flutter")
            .args(&flutter_args)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to run 'flutter logs'");

        let stdout = child.stdout.take().unwrap();

        // Pipe raw lines from the flutter process through an internal channel
        // so we can do timed reads (needed to flush a buffered entry that has
        // no following [TAG] line yet — e.g. the last entry in a burst).
        let (line_tx, line_rx) = mpsc::channel::<Option<String>>(); // None = EOF
        thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines() {
                match line {
                    Ok(l) => { if line_tx.send(Some(l)).is_err() { break; } }
                    Err(_) => break,
                }
            }
            let _ = line_tx.send(None);
        });

        // Strip Android logcat prefix: "I/flutter ( 1234): "
        let prefix_re = Regex::new(r"^I/flutter\s*\(\s*\d+\):\s*").unwrap();
        // An entry boundary: line starting with a real log tag like [Http], [Signals], etc.
        // Deliberately excludes JSON-like content: [{"id":...}] or ["ToggleRFIDAccess"] etc.
        // Tags are short alphanumeric identifiers, never starting with { " [ or digits.
        let tag_re = Regex::new(r"^\[[A-Za-z][A-Za-z0-9 _\-]{0,40}\]").unwrap();
        // Continuation chunk marker emitted by Dart's _printMsg for chunks 2+
        // "··" = U+00B7 U+00B7 = 4 bytes in UTF-8
        let cont_prefix = "··";
        let cont_prefix_len = cont_prefix.len(); // 4 bytes
        // Strip bare ANSI color-only lines (e.g. "\x1b[33;20m" emitted by printLogWarning/Critical)
        let ansi_only_re = Regex::new(r"^\x1b\[[0-9;]*m$").unwrap();

        // How long to wait for the next line before flushing a buffered entry.
        // Must be long enough to let all ·· continuation chunks arrive after
        // the final chunk. Dart prints synchronously but logcat/pipe buffering
        // can introduce delays between lines. 500ms is safe.
        let idle_timeout = Duration::from_millis(500);

        // buffer accumulates the current log entry.
        // last_chunk_was_cont: true if the most recent line was a ·· chunk,
        // meaning more continuations may still be in flight — don't flush yet.
        let mut buffer: Option<String> = None;
        let mut last_chunk_was_cont = false;

        loop {
            // Wait for the next line, with a timeout so we can flush on idle
            let recv_result = if buffer.is_some() {
                line_rx.recv_timeout(idle_timeout)
            } else {
                // No buffer — block indefinitely to avoid busy-spinning
                match line_rx.recv() {
                    Ok(v) => Ok(v),
                    Err(_) => break,
                }
            };

            match recv_result {
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    // Idle flush — but only if the last chunk was NOT a ··
                    // continuation (which would mean more chunks are still coming)
                    if !last_chunk_was_cont {
                        if let Some(buf) = buffer.take() {
                            if tx.send(buf).is_err() { break; }
                        }
                    }
                    continue;
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Ok(None) => {
                    // EOF from the flutter process
                    break;
                }
                Ok(Some(line)) => {
                    // DEBUG: dump raw lines to /tmp/flogs_raw.log
                    if std::env::var("FLOGS_DEBUG").is_ok() {
                        use std::io::Write;
                        let mut f = std::fs::OpenOptions::new()
                            .create(true).append(true)
                            .open("/tmp/flogs_raw.log").unwrap();
                        writeln!(f, "{:?}", line).unwrap();
                    }

                    // Strip Android prefix
                    let stripped = if prefix_re.is_match(&line) {
                        prefix_re.replace(&line, "").to_string()
                    } else {
                        line
                    };

                    // Skip bare ANSI color sentinels
                    if ansi_only_re.is_match(&stripped) {
                        continue;
                    }

                    // Strip inline ANSI sequences
                    let stripped = strip_ansi(&stripped);

                    if tag_re.is_match(&stripped) {
                        // New entry boundary — flush whatever was buffered
                        if let Some(buf) = buffer.take() {
                            if tx.send(buf).is_err() { break; }
                        }
                        // Always buffer: the entry may have continuation lines
                        buffer = Some(stripped);
                        last_chunk_was_cont = false;
                    } else if stripped.starts_with(cont_prefix) {
                        // Explicit continuation chunk from Dart's _printMsg
                        let rest = &stripped[cont_prefix_len..];
                        if let Some(buf) = buffer.as_mut() {
                            buf.push_str(rest);
                        } else {
                            // No buffer (was flushed by timeout mid-entry) —
                            // start a new buffer with this continuation content
                            buffer = Some(rest.to_string());
                        }
                        last_chunk_was_cont = true;
                    } else if let Some(buf) = buffer.as_mut() {
                        // Untagged continuation (stack trace, response body line, etc.)
                        buf.push('\n');
                        buf.push_str(&stripped);
                        last_chunk_was_cont = false;
                    } else {
                        // Pre-tag noise (before the first [TAG] ever seen)
                        if tx.send(stripped).is_err() { break; }
                        last_chunk_was_cont = false;
                    }
                }
            }
        }

        if let Some(buf) = buffer.take() {
            let _ = tx.send(buf);
        }

        let _ = child.kill();
        let _ = child.wait();
    });

    enable_raw_mode().unwrap();
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture).unwrap();
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).unwrap();

    let _ = run_app(&mut terminal, rx);

    disable_raw_mode().unwrap();
    execute!(terminal.backend_mut(), LeaveAlternateScreen, DisableMouseCapture).unwrap();
    terminal.show_cursor().unwrap();
}

// ── Utilities ─────────────────────────────────────────────────────────────────

/// Remove ANSI escape sequences from a string.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if chars.peek() == Some(&'[') {
                chars.next(); // consume '['
                // consume until a letter (the SGR terminator)
                for ch in chars.by_ref() {
                    if ch.is_ascii_alphabetic() { break; }
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}
