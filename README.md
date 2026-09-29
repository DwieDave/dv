# dv

A fast terminal viewer for large JSON, NDJSON and YAML files, built in Rust with Ratatui.
It targets macOS on Apple silicon.

- **Opens big files at once.** 15 MB loads in about 20–40 ms. Files larger than the auto
  threshold are *streamed*: indexed from disk in the background, browsable within ~50 ms, with
  memory bounded (about 160 MB for a 10 GB file).
- **Hierarchical browsing.** Keys, indices and inline values show in a tree. Large arrays are
  split into buckets of 1024. A preview pane pretty-prints the selected value.
- **Search and jump.** Incremental substring or regex search over keys and/or values, jq-style
  path jumps (`.users[42].name`), and a fuzzy picker over the document's key paths.
- **Robust.** A strict validating parser. Malformed NDJSON lines are isolated and shown inline.
  Errors show the line and column in context. No `unsafe` code.

## Build

The toolchain comes from `flake.nix` (stable Rust, aarch64-darwin):

```sh
direnv allow            # or: nix develop
cargo build --release   # binary: target/release/dv
```

## Usage

```sh
dv data.json                   # a file
dv logs.ndjson --mode stream   # force streaming mode
curl -s https://… | dv         # stdin (or pass `-`)
dv --format yaml config.txt    # override format detection
dv --config ./my-config.toml big.json
```

| Option | Meaning |
|---|---|
| `--format json\|ndjson\|yaml` | Skip format detection (by extension, then by content) |
| `--mode auto\|memory\|stream` | `auto` streams files larger than min(256 MB, 25% of RAM) |
| `--config PATH` | Config file (default `$XDG_CONFIG_HOME/dv/config.toml` or `~/.config/dv/config.toml`) |

**Streaming mode** keeps an on-disk index in private temp files, which are unlinked the moment
they're created. It opens immediately, shows `n…` counts for containers still being indexed,
and fills them in as it goes. If a parse error turns up partway, everything before it stays
browsable and a banner shows the error. Piped input larger than the threshold is spooled to a
temp file and streamed. YAML can't be streamed; convert it first (`yq -o=json`).

## Keys

The footer shows the keys that matter right now, lazygit-style. It changes in the search
prompt, the path prompt, the picker and the help overlay, and drops the least important hints
on narrow terminals (`? more`). `?` opens an overlay listing every key.

### Navigation

| Key | Action |
|---|---|
| `j` `k` / `↓` `↑` | Down / up |
| `l` / `→` | Expand (or move into) |
| `h` / `←` | Collapse (or move to the parent) |
| `Space` / `Enter` | Toggle expand |
| `gg` / `Home`, `G` / `End` | Top / bottom |
| `Ctrl-d` / `Ctrl-u` | Half page down / up |
| `PageDown` / `PageUp` | Page down / up |
| `zo` | Expand all children of the cursor |
| `zc` | Collapse the cursor's subtree |
| `zM` | Collapse everything |
| Mouse | Click selects (and toggles on the marker); the wheel scrolls |

### Finding

| Key | Action |
|---|---|
| `/` | Search as you type; `Enter` counts matches, `Esc` returns to where you were |
| in the search prompt: `Tab` | Cycle the scope: keys and values → keys → values |
| in the search prompt: `Ctrl-r` / `Ctrl-e` | Toggle regex / case sensitivity |
| `n` / `N` | Next / previous match (or the next picked key path) |
| `:` | Jump to a jq-style path, e.g. `.items[-1].id` or `.[10:20]` |
| `Ctrl-p` | Fuzzy picker over key paths; `↑` `↓` (or `Ctrl-p`/`Ctrl-n`) select, `Enter` jumps, `Esc` closes |

In streaming mode the picker covers the container at the cursor, and its title says which one.
Collecting paths stops after 2M values ("partial list").

### Preview and copying

| Key | Action |
|---|---|
| `p` | Show or hide the preview pane |
| `<` / `>` | Move the split |
| `J` / `K` | Scroll the preview (by screen rows when wrapped) |
| `w` | Wrap long lines in the preview; continuation rows align with the value |
| `yp` | Copy the jq path of the cursor |
| `yy` / `yY` | Copy the value, minified / pretty-printed |
| `?` | Help overlay with every key (`j`/`k` scroll, `?`/`Esc`/`q` close) |
| `q` / `Ctrl-c` | Quit |

Copying uses `pbcopy`, falling back to the OSC 52 terminal escape.

## Config

`~/.config/dv/config.toml`; everything is optional:

```toml
theme = "dusk"

[themes.dusk]            # tokens you leave out keep the default
key         = "#e0a060 bold"
string      = "lightgreen"
number      = "yellow"
bool        = "magenta"
null        = "darkgray"
punctuation = "gray"
badge       = "darkgray"      # counts, borders, indent guides
marker      = "blue"          # markers, cursor bar, the cursor's container guide
selection   = "bg:#303848"    # tint of the cursor row (keys there are bold)
error       = "lightred"

[ui]
footer = true             # the key-hint row and rule under the tree

[mode]
threshold     = "256MB"   # files above this stream (auto mode)
memory_budget = "512MB"   # caches and buffers in streaming mode
```

A style is space-separated words: an ANSI color name or `#rrggbb` (foreground), `bg:<color>`,
and `bold`, `dim`, `italic`, `underline`, `reverse`. An invalid config falls back to the
defaults and shows a one-line warning in the status bar.

## Performance

Measured on an M-series Mac (34 GB RAM), release build, 2026-09-29. Details are in
`.docs/workflows/large-file-tui-viewer/verification.md`.

| | Result | Target |
|---|---|---|
| 15 MB JSON / NDJSON to an interactive tree | 17–42 ms (`--index-only`), ~66 ms on screen | < 1 s |
| 15 MB YAML | 370 ms (`--index-only`), ~430 ms on screen | < 2 s |
| 256 MB in memory: render / navigation step | 77–111 µs / 54–123 ns | < 16 ms |
| Peak RSS in memory mode (realistic shapes) | 1.12–1.32× the file | ≤ 3× |
| 10 GB NDJSON streamed | 4.6 s (2.2 GB/s), 168 MB RSS | ≥ 1 GB/s, < 512 MB |
| 10 GB JSON streamed | 10.2 s (980 MB/s), 157 MB RSS | ≥ 950 MB/s, < 512 MB |
| First screen on 10 GB / key latency after indexing | 46–58 ms / 13–16 ms | < 1 s / < 100 ms |

## Development

```sh
just check          # fmt, clippy -D warnings, nextest, cargo deny (all must pass before commits)
just data           # generate the benchmark fixtures in target/bench-data
just bench-load     # hyperfine load times
just fuzz-long      # a million property cases per parser
just suite DIR FILE # interactive streaming suite through tmux (needs a large FILE)
nix develop .#fuzz  # then: just fuzz json|ndjson|yaml [secs]
```

How it works:
- **The index:** a *semi-index* (after Ottaviano & Grossi). Only containers of 64 bytes or more
  get a node, with a checkpoint every 16 children; everything else is re-lexed on demand from
  the raw bytes.
- **Streaming:** a read-ahead thread feeds the parser. The on-disk index is built on its own
  thread. NDJSON blocks are parsed on a worker pool and merged in order.
