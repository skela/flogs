# flogs

A TUI log viewer for `flutter logs`. Reassembles chunked log lines and lets you filter by tag interactively.

## Install

```sh
make build
# binary at target/release/flogs
```

Copy it somewhere on your `$PATH`, e.g. `cp target/release/flogs ~/.local/bin/`.

## Usage

```sh
flogs
```

If multiple devices are connected you'll be prompted to pick one first. After that the TUI opens and logs stream in.

## Keys

| Key | Action |
|-----|--------|
| `/` | Enter filter mode |
| `Enter` | Apply typed filter |
| `Esc` (filtering) | Cancel, keep previous filter |
| `Esc` (normal) | Clear filter, show all |
| `↑ / ↓` | Scroll up / down |
| `PgUp / PgDn` | Scroll by 20 lines |
| `q` / `Ctrl+C` | Quit |

## Filtering

Press `/`, type one or more comma-separated tag names, then press `Enter`:

```
Signals
Signals,Http
```

Matching is case-insensitive. The status bar shows the active filter and how to clear it.
