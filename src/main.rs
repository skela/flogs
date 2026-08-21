use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Style},
    widgets::{List, ListItem, Paragraph},
    Frame, Terminal,
};
use regex::Regex;
use std::io::{self, BufRead, BufReader, Write};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

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
            '{' => {
                depth += 1;
                current.push(c);
            }
            '}' => {
                depth -= 1;
                current.push(c);
                if depth == 0 {
                    objects.push(current.clone());
                    current.clear();
                }
            }
            _ => {
                if depth > 0 {
                    current.push(c);
                }
            }
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

#[derive(Copy, Clone, PartialEq)]
enum Mode {
    Normal,
    Filtering,
}

struct App {
    lines: Vec<String>,
    filter: Option<String>,
    filter_input: String,
    mode: Mode,
    scroll: usize,
}

impl App {
    fn new() -> Self {
        Self {
            lines: Vec::new(),
            filter: None,
            filter_input: String::new(),
            mode: Mode::Normal,
            scroll: 0,
        }
    }

    fn filtered_lines(&self) -> Vec<&str> {
        match &self.filter {
            None => self.lines.iter().map(|s| s.as_str()).collect(),
            Some(filter_str) => {
                let tags: Vec<String> = filter_str
                    .split(',')
                    .map(|t| format!("[{}]", t.trim()).to_ascii_lowercase())
                    .filter(|t| t.len() > 2)
                    .collect();
                if tags.is_empty() {
                    return self.lines.iter().map(|s| s.as_str()).collect();
                }
                self.lines
                    .iter()
                    .filter(|l| {
                        let lower = l.to_ascii_lowercase();
                        tags.iter().any(|tag| lower.contains(tag))
                    })
                    .map(|s| s.as_str())
                    .collect()
            }
        }
    }
}

fn draw(frame: &mut Frame, app: &App) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(0), Constraint::Length(1)])
        .split(area);

    let filtered = app.filtered_lines();
    let visible = chunks[0].height as usize;
    let total = filtered.len();
    let scroll = app.scroll.min(total.saturating_sub(visible));
    let end = total.saturating_sub(scroll);
    let start = end.saturating_sub(visible);

    let items: Vec<ListItem> = filtered[start..end]
        .iter()
        .map(|l| ListItem::new(*l))
        .collect();
    frame.render_widget(List::new(items), chunks[0]);

    let status_text = match app.mode {
        Mode::Filtering => format!(
            "  Filter: {}█  [Enter] apply  [Esc] cancel",
            app.filter_input
        ),
        Mode::Normal => match &app.filter {
            None => "  [/] filter   [c] clear   [q] quit".to_string(),
            Some(filter_str) => {
                let pills: String = filter_str
                    .split(',')
                    .map(|t| t.trim())
                    .filter(|t| !t.is_empty())
                    .map(|t| format!("[{}]", t))
                    .collect::<Vec<_>>()
                    .join(" ");
                format!("  Filter: {}   [Esc] clear filter   [/] change   [c] clear logs   [q] quit", pills)
            }
        },
    };
    let bar_style = Style::default().bg(Color::DarkGray).fg(Color::White);
    frame.render_widget(
        Paragraph::new(status_text).style(bar_style),
        chunks[1],
    );
}

fn run_app(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    rx: mpsc::Receiver<String>,
) -> io::Result<()> {
    let mut app = App::new();
    loop {
        while let Ok(line) = rx.try_recv() {
            app.lines.push(line);
        }

        terminal.draw(|f| draw(f, &app))?;

        if event::poll(Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
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
                            app.scroll = 0;
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
                            app.lines.clear();
                            app.scroll = 0;
                        }
                        KeyCode::Char('/') => {
                            app.mode = Mode::Filtering;
                            app.filter_input = app.filter.clone().unwrap_or_default();
                        }
                        KeyCode::Esc => {
                            app.filter = None;
                            app.scroll = 0;
                        }
                        KeyCode::Up => app.scroll += 1,
                        KeyCode::Down => app.scroll = app.scroll.saturating_sub(1),
                        KeyCode::PageUp => app.scroll += 20,
                        KeyCode::PageDown => app.scroll = app.scroll.saturating_sub(20),
                        _ => {}
                    },
                }
            }
        }
    }
}

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
        let reader = BufReader::new(stdout);

        let prefix_re = Regex::new(r"^I/flutter\s*\(\s*\d+\):\s*").unwrap();
        let tag_re = Regex::new(r"^\[.+?\]").unwrap();
        const CHUNK_SIZE: usize = 800;
        let mut buffer: Option<String> = None;

        'outer: for line in reader.lines() {
            let line = match line {
                Ok(l) => l,
                Err(_) => break,
            };

            let stripped = if prefix_re.is_match(&line) {
                prefix_re.replace(&line, "").to_string()
            } else {
                line
            };

            if tag_re.is_match(&stripped) {
                if let Some(buf) = buffer.take() {
                    if tx.send(buf).is_err() {
                        break 'outer;
                    }
                }
                if stripped.len() >= CHUNK_SIZE - 20 {
                    buffer = Some(stripped);
                } else if tx.send(stripped).is_err() {
                    break 'outer;
                }
            } else if let Some(buf) = buffer.as_mut() {
                buf.push_str(&stripped);
                if stripped.len() < CHUNK_SIZE - 20 {
                    let buf = buffer.take().unwrap();
                    if tx.send(buf).is_err() {
                        break 'outer;
                    }
                }
            } else if tx.send(stripped).is_err() {
                break 'outer;
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
    execute!(stdout, EnterAlternateScreen).unwrap();
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).unwrap();

    let _ = run_app(&mut terminal, rx);

    disable_raw_mode().unwrap();
    execute!(terminal.backend_mut(), LeaveAlternateScreen).unwrap();
    terminal.show_cursor().unwrap();
}
