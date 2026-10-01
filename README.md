# dv

A fast terminal viewer for large JSON, NDJSON and YAML files, built in Rust with Ratatui.
It runs on macOS (Apple silicon and Intel) and Linux (x86_64 and aarch64), and is developed
and tuned on Apple silicon.

- **Opens big files at once.** 15 MB loads in about 20–40 ms. Files over the auto threshold
  stream instead. dv indexes them from disk in the background, you can browse after ~50 ms,
  and memory stays bounded (about 160 MB for a 10 GB file).
- **Tree browsing.** Keys, indices and inline values show in a tree. Large arrays split into
  buckets of 1024. A preview pane pretty-prints the selected value.
- **Search and jump.** Incremental substring or regex search over keys and/or values, jq-style
  path jumps (`.users[42].name`), and a fuzzy picker over the document's key paths. dv also
  keeps a jump history, marks, and your last position in each file.
- **Table and filter.** Any array of objects as a sortable table; filter records with
  expressions like `.age > 30 and has(.email)`.
- **Follow.** `--follow` tails a growing NDJSON log.
- **Strict parsing.** The parser validates everything. A malformed NDJSON line shows inline
  without breaking the lines around it, and errors point at the line and column. No `unsafe`
  code.

## Install

With Homebrew (macOS or Linux), add the tap, trust it, then install. Homebrew 6 and later
load third-party taps only after you trust them:

```sh
brew tap DwieDave/tap
brew trust --tap DwieDave/tap
brew install DwieDave/tap/dv
```

### Brewfile

For `brew bundle`, `trusted: true` on the tap does the `brew trust` step:

```ruby
tap "DwieDave/tap", trusted: true
brew "DwieDave/tap/dv"
```

Or download an archive for your platform from the
[releases](https://github.com/DwieDave/dv/releases) page. Each archive has a `.sha256`
beside it and a build provenance attestation:
`gh attestation verify dv-<version>-<target>.tar.gz -R DwieDave/dv`.

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
dv --follow app.log.ndjson     # keep indexing lines as they are appended
curl -s https://… | dv         # stdin (or pass `-`)
dv --format yaml config.txt    # override format detection
dv --config ./my-config.toml big.json
```

| Option | Meaning |
|---|---|
| `--format json\|ndjson\|yaml` | Skip format detection (by extension, then by content) |
| `--mode auto\|memory\|stream` | `auto` streams files larger than min(256 MB, 25% of RAM) |
| `--config PATH` | Config file (default `$XDG_CONFIG_HOME/dv/config.toml` or `~/.config/dv/config.toml`) |
| `--follow` | Follow a growing NDJSON file (implies streaming; not for stdin) |

Streaming mode keeps an on-disk index in private temp files. dv unlinks each one the moment it
creates it. It opens immediately, shows `n…` counts for containers still being indexed,
and fills them in as it goes. If a parse error turns up partway, everything before it stays
browsable and a banner shows the error. Piped input over the threshold goes to a temp file
first and streams from there. YAML can't be streamed; convert it first (`yq -o=json`).

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

### History and marks

| Key | Action |
|---|---|
| `Ctrl-o` / `Tab` | Back / forward through jumps (`:`, search hits, the picker, `gg`/`G`, marks, opening from the table or a filter) |
| `m{a-z}` / `'{a-z}` | Set a mark / go to it (for this session) |

On quit, the cursor is remembered per file (by path, size and modification time) in
`$XDG_STATE_HOME/dv/positions.tsv` (default `~/.local/state/dv/positions.tsv`, the last 500
files) and restored the next time the unchanged file opens.

### Table

`t` shows the array at the cursor (or the cursor's array) as a table when its elements are
objects. Columns are the keys of the first 1,000 rows. Missing keys show `—`, and nested
values show their size.

| Key | Action |
|---|---|
| `j` `k`, `gg` `G`, `Ctrl-d` `Ctrl-u`, `PgDn` `PgUp` | Rows |
| `h` `l` | Columns (scrolls sideways) |
| `s` | Sort by the column: ascending ▲, descending ▼, off. Numbers, then strings, booleans, null and nested values; missing last. Up to 1M rows, on a background thread |
| `x` / `X` | Hide the column / show all |
| `Enter` | Open the row in the tree |
| `t` / `Esc` / `q` | Close |

### Filter

`f` filters the array or object at the cursor. Only matching elements stay listed, with their
original indices, and browsing, search and the table all work on that view.
Matches arrive while the scan runs (`12 of 400,000 records (scanning 40%)`), and at most 1M
are kept. `o` opens the match under the cursor in the full tree, `Esc` clears the filter, and
`f` edits it.

```text
.status == "error"                     # compare a member
.age >= 30 and not has(.deleted)       # and, or, not, parentheses
.user.name ~ "^A"                      # regex on strings
."first name" != null or .tags[0] == "x"
```

Paths start at each element (`.` is the element itself). Comparisons are type-strict:
numbers with numbers, strings with strings, and `==`/`!=` also compare `true`, `false` and
`null`. A missing path makes every comparison false.

### Follow

`dv --follow FILE`, or `F` on an NDJSON file, keeps indexing lines as they're appended
(checked every 250 ms). With the cursor on the last record, it moves to each new one. The
status bar shows `following`. `F` again stops following. If the file shrinks, following
stops with a note.

### Preview and copying

| Key | Action |
|---|---|
| `p` | Show or hide the preview pane |
| `<` / `>` | Move the split |
| `J` / `K`, `Ctrl-e` / `Ctrl-y` | Scroll the preview (by screen rows when wrapped) |
| `}` / `{` | Scroll the preview down / up by half a pane |
| `w` | Wrap long lines in the preview; continuation rows align with the value |
| `yp` | Copy the jq path of the cursor |
| `yy` / `yY` | Copy the value, minified / pretty-printed |
| `?` | Help overlay with every key (`j`/`k` scroll, `?`/`Esc`/`q` close) |
| `q` / `Ctrl-c` | Quit |

Copying uses `pbcopy`. Without it, dv falls back to the OSC 52 terminal escape.

## Config

Lives at `~/.config/dv/config.toml`. Every setting is optional.

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

### How it works

The index is a *semi-index*, after Ottaviano & Grossi. Only containers of 64 bytes or more get
a node, with a checkpoint every 16 children. Everything else gets re-lexed from the raw bytes
when needed.

When streaming, a read-ahead thread feeds the parser and a separate thread builds the on-disk
index. NDJSON blocks parse on a worker pool and merge back in order.
