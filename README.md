# cctop

Live, htop-style terminal monitor for Claude Code token usage.

![Rust](https://img.shields.io/badge/rust-stable-orange)
![License](https://img.shields.io/badge/license-MIT-blue)

cctop watches Claude Code's session JSONL files in real-time and displays
per-project token rates, costs, and activity trends in an interactive TUI --
like htop, but for your Claude Code spend.

```
 cctop ─ Claude Code Token Monitor                   Window: [5m]
  Rate: 45.2K tok/min   $2.31/min
  Loaded: $87.42 across 12 sessions
────────────────────────────────────────────────────────────────────
 PROJECT              TREND    SESS MODEL       IN/min OUT/min $/min
 ▸/home/user/myproj   ▁▃▅▇█▆▃▁   2 opus-4.6    38.2K   14.1K $1.23
 ▸/home/user/other    ▁▁▁▂▃▅▇█   1 sonnet-4.6   8.4K    3.7K $0.34
────────────────────────────────────────────────────────────────────
 ■in ■out ■cache                              -5m            now
 ▁▁▂▃▃▅▅▆▇▇█▇▆▅▃▂▁▁▁▁▂▃▄▅▆▇█▇▇▆▅▄▃▂▁▁▁▁▁▁▁▂▃▄▅▅▆▇▇█▇▆▅▄▃▂▁
────────────────────────────────────────────────────────────────────
 ↑↓ navigate  ←→ window  Enter expand  s sort  c collapse  q quit
```

## Features

- **Live monitoring** via inotify filesystem notifications -- no polling
- **Hierarchical tree view**: projects > sessions > subagents, expandable with Enter
- **Per-row sparkline** showing each project's activity trend over the window
- **Stacked bar histogram** on the lower half with color-coded token types
  (green = input, blue = output, magenta = cache)
- **Configurable sliding window**: 1m, 5m, 15m, 30m, 1h, 2h, 4h, 8h, 24h
- **Wall-clock-quantized bucketing** so the chart slides smoothly instead of jittering
- **Fast startup**: tail-reads the last 512KB of each JSONL file, even for
  sessions with hundreds of megabytes of history
- **Full pricing table**: 70+ Claude model variants with tiered pricing and
  fast-mode multipliers
- **Keyboard-driven**: vim-style navigation (hjkl), sort cycling, collapse all

## Installation

```sh
cargo install --path .
```

Or build from source:

```sh
git clone https://github.com/olofj/cctop
cd cctop
cargo build --release
# Binary at ./target/release/cctop
```

## Usage

```sh
# Default: 5-minute window
cctop

# Start with a 1-hour window
cctop -w 1h

# Filter to a specific project
cctop -p myproject
```

### Keyboard shortcuts

| Key | Action |
|-----|--------|
| `q` / `Esc` | Quit |
| `↑` `↓` / `j` `k` | Navigate rows |
| `Enter` / `Space` | Expand/collapse project or session |
| `←` `→` / `h` `l` | Shrink/grow time window |
| `s` | Cycle sort column ($/min, IN/min, OUT/min, Last, Project) |
| `S` | Reverse sort direction |
| `d` | Hide selected project |
| `u` | Unhide all hidden projects |
| `c` | Collapse all expanded rows |
| `g` / `G` | Jump to top/bottom |
| `PgUp` / `PgDn` | Scroll by page |

## How it works

Claude Code stores per-session token usage in JSONL files under
`~/.claude/projects/`. cctop discovers these files, tail-reads recent entries
on startup, then uses inotify to watch for new data in real-time. Each JSONL
line containing `input_tokens` is parsed, costed against the pricing table,
and fed into a time-windowed in-memory store. The TUI renders at 4 Hz,
recomputing rates and histograms from the windowed data.

## Acknowledgments

This project would not exist without
[ccusage](https://github.com/ryoppippi/ccusage) by
[@ryoppippi](https://github.com/ryoppippi) -- the original Node.js CLI tool
for analyzing Claude Code token usage. cctop's data format understanding,
pricing table, file discovery logic, and cost calculation are all derived from
a Rust port of ccusage. Thank you for building the tool that made this possible.

## License

[MIT](LICENSE)
