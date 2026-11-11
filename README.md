# MisterCopy

A lightweight website copier for offline browsing, written in Rust. See [PLAN.md](PLAN.md) for the design and milestones.

**Status:** M3, partly done. MisterCopy copies sites through a browser-impersonating HTTP client and rewrites their links to work offline. It retries transient failures, respects robots.txt, can stop and resume, updates a copy by downloading only what changed, and writes a report. With the `browser` feature it can also render JavaScript-built pages in Chromium. Logging in through the browser (scripted or interactive) is the M3 work still to do.

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
| `--cookies cookies.txt` | Start with the cookies from a Netscape-format file, as exported by browser extensions |

Credentials are dropped when a redirect leaves the origin.

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

The wreq requester builds BoringSSL, which needs `cmake`, `clang`/`libclang` and, on Windows, `nasm`. If you have libclang but bindgen can't find `stddef.h`, either install your distro's `clang` package or point bindgen at GCC's headers:

```bash
export BINDGEN_EXTRA_CLANG_ARGS="-I/usr/lib/gcc/x86_64-linux-gnu/13/include"
```
