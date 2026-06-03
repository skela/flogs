use regex::Regex;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

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
    let rest = rest.trim_start();
    let rest = rest.strip_prefix(':')?;
    let rest = rest.trim_start();
    let rest = rest.strip_prefix('"')?;
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

fn main() {
    let devices = get_devices();

    let mut args = vec!["logs".to_string()];

    if devices.len() > 1 {
        let device_id = select_device(&devices);
        args.push("-d".to_string());
        args.push(device_id.to_string());
    }

    let mut child = Command::new("flutter")
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("failed to run 'flutter logs'");

    let stdout = child.stdout.take().unwrap();
    let reader = BufReader::new(stdout);

    let prefix_re = Regex::new(r"^I/flutter\s*\(\s*\d+\):\s*").unwrap();
    let tag_re = Regex::new(r"^\[.+?\]").unwrap();

    const CHUNK_SIZE: usize = 800;

    let mut buffer: Option<String> = None;

    for line in reader.lines() {
        let line = match line {
            Ok(l) => l,
            Err(_) => break,
        };

        let stripped = if prefix_re.is_match(&line) {
            prefix_re.replace(&line, "").to_string()
        } else {
            line
        };

        // A new tag starts a new log statement — flush any pending buffer first
        if tag_re.is_match(&stripped) {
            if let Some(buf) = buffer.take() {
                println!("{}", buf);
            }
            // If this line is near the chunk boundary, it's likely truncated — buffer it
            if stripped.len() >= CHUNK_SIZE - 20 {
                buffer = Some(stripped);
            } else {
                println!("{}", stripped);
            }
        } else {
            // Continuation line (no tag) — append to buffer
            if let Some(buf) = buffer.as_mut() {
                buf.push_str(&stripped);
            } else {
                // Orphan continuation with no buffer — just print it
                println!("{}", stripped);
            }

            // After appending, check if the last chunk was short (end of statement)
            if let Some(ref buf) = buffer {
                if stripped.len() < CHUNK_SIZE - 20 {
                    println!("{}", buf);
                    buffer = None;
                }
            }
        }
    }

    if let Some(buf) = buffer.take() {
        println!("{}", buf);
    }

    let _ = child.wait();
}
