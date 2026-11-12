# MisterCopy

A lightweight website copier for offline browsing, written in Rust. See [PLAN.md](PLAN.md) for the design and milestones.

**Status:** M3. MisterCopy copies sites through a browser-impersonating HTTP client and rewrites their links to work offline. It retries transient failures, respects robots.txt, can stop and resume, updates a copy by downloading only what changed, and writes a report. With the `browser` feature it also renders JavaScript-built pages in Chromium and can log in through a real browser. The desktop app (M4) does all of this from a window, and copies can also be saved as WARC archives or zip files (M5).

## Install

Releases are built by `.github/workflows/release.yml` when a `v*` tag is pushed. Each one has packages for Linux, macOS and Windows, with and without browser support, plus checksums. To build one for your own machine:

```bash
scripts/package.sh                   # dist/mistercopy-<version>-<target>.tar.gz
```

```bash
FEATURES=browser scripts/package.sh  # with Chromium rendering
```

Or run the command-line tool in Docker:

```bash
docker build -t mistercopy .
```

```bash
docker run --rm -v "$PWD/copies:/copies" mistercopy copy https://example.com -o /copies
```

## Usage

```bash
mistercopy copy https://example.com -o ./copies
```

The copy lands in `./copies/example.com/`, with a report in `./copies/mistercopy-report.html` (and `.json`). Progress is kept in `./copies/.mistercopy/`, so later commands can pick up where this one left off:

```bash
mistercopy resume ./copies   # continue after Ctrl-C, a crash or a limit
```

```bash
mistercopy update ./copies   # re-check every file; unchanged ones aren't downloaded again
```

```bash
mistercopy scan https://example.com --report links.html   # audit only, saves nothing else
```

Press Ctrl-C once to stop after the requests in flight; progress is saved. Press it twice to quit at once.

### Archives and serving

```bash
mistercopy copy https://example.com -o ./copies --warc example.warc.gz --zip
```

- `--warc FILE` also writes every response, as served, to a standard web archive: WARC 1.1, one gzip member per record. `resume` keeps appending to the same file.
- `--zip` packs the finished copy and its report into `<host>.zip`.

```bash
mistercopy serve ./copies   # http://127.0.0.1:8000/
```

`serve` is for copies that need http rather than `file://`, such as JavaScript apps that load data from absolute paths.

For those apps, `--requester playwright --replay` keeps their scripts and embeds the data they fetched (JSON, text, XML) in `mistercopy-replay.js`. The script answers the app's own `fetch` and `XMLHttpRequest` calls offline, even from `file://`.

### Desktop app

```bash
cargo run --release -p mc-gui
```

Enter a site's address and a folder, then click **Copy**. The window shows each file as it arrives, counts what was saved and what failed, and opens the report when the copy is done. **Stop** saves progress; copying the same site into the same folder again continues where it stopped.

- **Copies** (left): every copy you've made, with its status, remembered in `~/.config/mistercopy/projects.json`. Select one to work on it again; finished copies get an **Update** button that only downloads what changed.
- **Rules**: patterns of URLs to skip or to fetch from other sites, and a tester that says what the crawler would do with any URL before you copy.
- **Link map**: the pages of a copy as a tree, each under the page that first linked to it, with failures and skips marked. **Export SVG** saves it as a diagram (`mistercopy-linkmap.svg`, next to the report).
- **Command palette**: press **Ctrl+K** (or click **Commands**) and type to run anything: start, update, stop, switch tabs, export, open a saved copy.

Copies made in the app can also be resumed or updated with `mistercopy resume` and `mistercopy update`, which reuse the app's choices.

Build with `--features browser` to add a **Render JavaScript** option, which uses `--requester auto`.

On Linux the app needs the X11, Wayland and Vulkan development packages (`libxkbcommon-x11-dev libx11-xcb-dev libwayland-dev libvulkan-dev` on Debian/Ubuntu).

### JavaScript sites

Build with the `browser` feature to render pages in Chromium through Playwright. The first build downloads the Playwright driver (about 135 MB) into `~/.cache/playwright-rust/`.

```bash
cargo install --path crates/mc-cli --features browser
```

```bash
mistercopy browsers install   # once, unless Playwright's Chromium is already installed
```

```bash
mistercopy copy https://app.example.com --requester auto -o ./copies
```

Choose how pages are fetched with `--requester`:

- **`wreq`** (default): fast HTTP with a browser fingerprint. No JavaScript.
- **`playwright`**: renders every page in Chromium. It scrolls each page so lazy content loads, and saves every file the page loaded (scripts, styles, fonts, images, API data) without fetching them again.
- **`auto`**: uses `wreq` and switches to Chromium only for pages that turn out to be empty JavaScript shells, or that refuse plain HTTP with a 403.

Rendered pages are saved without their `<script>` tags, since a snapshot that re-runs its own scripts offline tends to blank itself. Pass `--keep-scripts` to keep them. `--no-scroll` skips the scrolling, and `--browser-path` picks a specific Chromium.

Useful `copy` and `scan` options:

| Option | Effect |
|---|---|
| `--profile chrome_149` | Browser fingerprint to present (`firefox_151`, `safari_26.4`, …) |
| `--max-depth N`, `--max-urls N` | Limit how far and how much to crawl |
| `--max-size-mb N`, `--max-time-secs N` | Stop (resumably) after downloading or running this much |
| `--max-file-mb N` | Skip responses larger than this (default 512) |
| `--exclude REGEX`, `--download-only REGEX`, `--include REGEX` | Rules, matched against the full URL |
| `--alias HOST` | Treat another host as part of the same site |
| `--external-assets` | Also fetch images, CSS and scripts from other hosts |
| `--delay-ms 1000`, `--concurrency 4`, `--retries 3`, `--timeout-secs 30` | Politeness and patience |
| `--ignore-robots` | Fetch what robots.txt disallows; only for sites you may copy |

These options are saved with the copy and reused by `resume` and `update`.

Options that carry credentials are never saved, so pass them again to `resume` and `update`:

| Option | Effect |
|---|---|
| `--auth HOST=USER:PASSWORD` | HTTP Basic login for one host. `--auth HOST=USER` reads the password from `$MISTERCOPY_PASSWORD`, which keeps it out of your shell history |
| `--header "Name: value"` | Extra header, sent only to the site's own hosts |
| `--cookies cookies.txt` | Start with the cookies from a Netscape-format file, as exported by browser extensions or by `mistercopy login` |
| `--login-script login.json` | Log in through Chromium before the crawl (needs `--requester playwright` or `auto`) |

Credentials are dropped when a redirect leaves the origin. Links that look like logout (`/logout`, `/sign-out`, `?action=logout`…) are never followed unless an `--include` rule names them, so a crawl doesn't end its own session.

A login script is a JSON list of steps, each with exactly one action. Use `value_env` to keep the password out of the file:

```json
[
  { "goto": "https://example.com/login" },
  { "fill": "#username", "value": "ann" },
  { "fill": "#password", "value_env": "MISTERCOPY_PASSWORD" },
  { "click": "button[type=submit]" },
  { "wait_for": ".account-menu" }
]
```

The browser logs in once before the first request. With `--requester auto`, plain pages are then fetched over HTTP using the same session.

To log in by hand instead (for example past two-factor prompts), open a browser window and save the session:

```bash
mistercopy login https://example.com/login --save cookies.txt
```

Then pass `--cookies cookies.txt`. The file holds a live session and is created readable only by you.

Exit codes:

| Code | Meaning |
|---|---|
| 0 | Everything copied |
| 1 | The copy finished, but some URLs failed (listed in the report) |
| 2 | Invalid command line, configuration or saved state |
| 3 | Stopped before finishing; run `resume` |

## Layout

| Path | What it is |
|---|---|
| `crates/mc-requester` | The `Requester` trait every fetch backend implements, the hybrid `auto` requester, the shared cookie jar, the registry, and a mock (`mock` feature) |
| `crates/mc-requester-wreq` | The default requester: wreq with browser TLS/HTTP2 fingerprints |
| `crates/mc-requester-playwright` | The browser requester: Chromium via Playwright (`browser` feature) |
| `crates/mc-extract` | Finds and rewrites URLs in HTML and CSS |
| `crates/mc-core` | The crawler: rules, URL → file path mapping, retries, robots.txt, limits, link rewriting, reports |
| `crates/mc-store` | Crawl state in SQLite, for resume and update |
| `crates/mc-cli` | The `mistercopy` binary |
| `crates/mc-gui` | The `mistercopy-gui` desktop app (gpui-kit) |
| `fixtures` | Local test websites and the server that hosts them (`mc-fixtures`), including `hostile`, a site full of traps, and `spa`, a JavaScript app |

## Development

```bash
cargo test --workspace --all-features
```

```bash
cargo clippy --workspace --all-targets --all-features
```

```bash
cargo fmt --all
```

Serve a test site at http://127.0.0.1:8000/:

```bash
cargo run -p mc-fixtures -- hostile
```

The toolchain is pinned in `rust-toolchain.toml` (rustup installs it on first use; GPUI needs Rust 1.95 or newer). The wreq requester builds BoringSSL, which needs `cmake`, `clang`/`libclang` and, on Windows, `nasm`. If you have libclang but bindgen can't find `stddef.h`, either install your distro's `clang` package or point bindgen at GCC's headers:

```bash
export BINDGEN_EXTRA_CLANG_ARGS="-I/usr/lib/gcc/x86_64-linux-gnu/13/include"
```
