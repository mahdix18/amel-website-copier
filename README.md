# Amel Website Copier

A lightweight website copier for offline browsing, written in Rust. See [PLAN.md](PLAN.md) for the design and milestones.

**Status:** M3. Amel copies sites through a browser-impersonating HTTP client and rewrites their links to work offline. It retries transient failures, respects robots.txt, can stop and resume, updates a copy by downloading only what changed, and writes a report. With the `browser` feature it also renders JavaScript-built pages in Chromium and can log in through a real browser. The desktop app (M4) does all of this from a window, and copies can also be saved as WARC archives or zip files (M5).

## Install

Releases are built by `.github/workflows/release.yml` when a `v*` tag is pushed. Each one has packages for Linux, macOS and Windows, with and without browser support, plus checksums. To build one for your own machine:

```bash
scripts/package.sh                   # dist/amel-<version>-<target>.tar.gz
```

```bash
FEATURES=browser scripts/package.sh  # with Chromium rendering
```

Or run the command-line tool in Docker:

```bash
docker build -t amel .
```

```bash
docker run --rm -v "$PWD/copies:/copies" amel copy https://example.com -o /copies
```

Add `--user "$(id -u):$(id -g)"` so the copied files belong to you rather than root. The image is about 100 MB and has no browser support.

## Usage

```bash
amel copy https://example.com -o ./copies
```

The copy lands in `./copies/example.com/`, with a report in `./copies/amel-report.html` (and `.json`). Progress is kept in `./copies/.amel/`, so later commands can pick up where this one left off:

```bash
amel resume ./copies   # continue after Ctrl-C, a crash or a limit
```

```bash
amel update ./copies   # re-check every file; unchanged ones aren't downloaded again
```

```bash
amel retry ./copies    # fetch again only what failed; the rest is kept
```

```bash
amel scan https://example.com --report links.html   # audit only, saves nothing else
```

Press Ctrl-C once to stop after the requests in flight; progress is saved. Press it twice to quit at once.

On a terminal a progress bar stays at the bottom: URLs done out of found so far, bytes, speed and how many workers are busy. Files download in parallel: `-j` sets the number of workers and `--delay-ms` the gap between two requests to the same site. The defaults (4 workers, 1 s gap) are polite. On a single site the gap, not the worker count, sets the pace. For a server you run or may load:

```bash
amel copy https://example.com -o ./copies -j 16 --delay-ms 0
```

Proxies: `--proxy socks5h://127.0.0.1:1080` or `--proxy http://user:pass@host:3128`. Repeat it, or list proxies one per line in `--proxy-file`, to rotate over several. A proxy that can't be reached sits out for 30 seconds while the others take its requests. Chromium (`--requester auto` or `playwright`) uses the first proxy, with the same bypass list.

`--sitemaps` also copies the pages the site's sitemaps list (named in robots.txt, else `/sitemap.xml`; indexes and gzipped sitemaps included), so pages nothing links to aren't missed. robots.txt and the site's scope still apply. `--limit-rate 500K` caps the average download speed across all workers.

### Archives and serving

```bash
amel copy https://example.com -o ./copies --warc example.warc.gz --zip
```

- `--warc FILE` also writes every response, as served, to a standard web archive: WARC 1.1, one gzip member per record. `resume` keeps appending to the same file.
- `--zip` packs the finished copy and its report into `<host>.zip`.

```bash
amel serve ./copies   # http://127.0.0.1:8000/
```

`serve` is for copies that need http rather than `file://`, such as JavaScript apps that load data from absolute paths.

For those apps, `--requester playwright --replay` keeps their scripts and embeds the data they fetched (JSON, text, XML) in `amel-replay.js`. The script answers the app's own `fetch` and `XMLHttpRequest` calls offline, even from `file://`.

### Desktop app

```bash
cargo run --release -p mc-gui
cargo run --release -p mc-gui -- https://example.com --output ~/copies --start
```

The window is laid out like an API client, in a dark theme (a light one is a click away):

- **Request bar**: the browser being impersonated (click it to change), the site's address and **Copy**. While copying there are **Pause**/**Resume** and **Stop** buttons. Pause lets the requests in flight finish and starts nothing new until you resume; Stop ends the copy, which you can continue later.
- **Mini window**: the shrink button next to Copy minimizes the window and opens a small strip that stays on top. It shows the status, the site, the progress bar, files done out of found, size and speed. Drag it anywhere; its buttons pause or resume the copy and bring the full window back. `amel-gui URL --start --mini` starts a copy straight in the mini window. On X11 it opens at the top-left of the screen; on Wayland the desktop decides where it goes.
- **Title bar**: the app draws its own, with minimize, maximize and close buttons, because GNOME on Wayland leaves that to apps. Drag it to move the window; double-click it to maximize. Enter starts too. **Stop** saves progress; copying the same site into the same folder again continues where it stopped.
- **Options**: the folder (`~/Amel Copies` unless you pick another), **Parallel downloads** (1–32 workers) and **Speed**: *Polite* (1 request per second per site), *Balanced* (4 per second) or *Fast* (no gap, all workers at once). There are also switches for robots.txt and JavaScript.
- **Browser**: which browser to impersonate (Chrome, Edge, Opera, Firefox, Safari or OkHttp), its version (‹ › steps through about 150 profiles), and the platform it claims (Auto, Windows, macOS, Linux, Android, iOS). TLS fingerprint, HTTP/2 settings and headers all match the choice. With browser support, a switch also renders JavaScript-built pages in Chromium.
- **Options** also has **Check for missed files after downloading** (on by default). When a copy finishes, it checks every saved file is on disk and downloads again anything missing or that failed for a passing reason (timeouts, dropped connections, 5xx answers). It doesn't retry 404s. The summary says how many were recovered and how many are still missing.
- **Retrying**: each file is retried on timeouts, dropped connections and 429/5xx answers. **Retries** and **First wait (s)** are in **Options**.
- **Types**: untick a file type to leave it out, or give it a size limit in MB; skipped files show as SKIP rows in Activity.
- **Activity rows** have buttons to open the saved file, open the page in your browser, copy its address and, for failed or skipped files, fetch that one again.
- **Notify me when a copy finishes** (Options, on by default) shows a desktop notification.
- **Smart** (Options, on by default): when a site answers 429 (too many requests) or 503 (busy), the gap between requests to it doubles, from 0.5 s up to 60 s. Each page counts once, so one broken page can't slow a whole site. After 10 good answers in a row the gap shrinks by a quarter, back down to your chosen speed. The Activity log shows each change. When a copy ends with failures, **Retry N failed** in the request bar fetches only those again, for example after fixing a proxy, a login or a limit.
- **Limits**: max depth, max pages, stop after a size or a time, skip files over a size, a bandwidth limit, other names of the same site, sitemaps (on by default here), and assets from other sites.
- **Auth**: Basic auth, extra headers (one per line) and a cookies file. These are used for the run only and never saved. In builds with browser support, **Log in with browser** opens Chromium at the site. You log in there and press **Done**; the session's cookies are then saved in the copy's state folder (owner-only, left out of zips) and used for the copy.
- **Proxy**: a list of proxies (one per line; http, https, socks4, socks5 or socks5h, credentials as `user:pass@`), *Round-robin* or *Sticky per site* rotation, hosts that go direct, and a switch for the system proxy. **Test** fetches the site through each proxy and shows the status and time, or why it failed. The status bar shows whether copies go direct or through proxies.
- **Output**: also write a WARC archive, a zip of the finished copy, or (with JavaScript rendering) the offline replay shim.
- **Progress**: a progress bar (URLs done out of found so far), elapsed time, size and live speed, and counters for saved, unchanged, failed and retried files. **Workers** shows what each parallel worker is doing: downloading (GET), holding a URL until the site's turn comes (WAIT), or idle. **Activity** lists every file with its result, size or error, and can show errors only. When the copy ends, **Open report** opens the HTML report.
- **Log panel**: **Log** in the status bar (or **Ctrl+`**) opens a timestamped log of everything the copy does: each file, retry, slow-down, script line and error, and the app's own notes. Filter it by level or search it, copy it, or open the log folder. Each day's log is also written to `~/.config/amel/logs/amel-YYYY-MM-DD.log` and kept for 14 days. The status bar counts warnings and errors.
- **Script**: a code editor with line numbers, JavaScript syntax colors, folding of `{ }` blocks, and autocomplete for the hooks (inserted as templates), `log`, the fields of `info`, `file` and `summary` after a dot, and JavaScript keywords. Suggestions appear as you type; **Ctrl+Space** shows them at any point, even on a blank line.
- The right side scrolls as a whole, so long tabs such as Script or Types never hide the progress panel in a short window.
- Workers, speed, switches, the script and the theme are remembered in `~/.config/amel/prefs.json`.

- **Copies** (left): every copy you've made, with its status, remembered in `~/.config/amel/projects.json`. Select one to work on it again; finished copies get an **Update** button that only downloads what changed.
- **Rules**: patterns of URLs to skip or to fetch from other sites, and a tester that says what the crawler would do with any URL before you copy.
- **Link map**: the pages of a copy as a tree, each under the page that first linked to it, with failures and skips marked. **Export SVG** saves it as a diagram (`amel-linkmap.svg`, next to the report).
- **Command palette**: press **Ctrl+K** (or click **Search commands**) and type to run anything: start, update, stop, switch tabs, export, open a saved copy.

Copies made in the app can also be resumed or updated with `amel resume` and `amel update`, which reuse the app's choices.

Build with `--features browser` to add a **Render JavaScript** option, which uses `--requester auto`.

On Linux the app needs the X11, Wayland and Vulkan development packages (`libxkbcommon-x11-dev libx11-xcb-dev libwayland-dev libvulkan-dev` on Debian/Ubuntu).

### JavaScript sites

Build with the `browser` feature to render pages in Chromium through Playwright. The first build downloads the Playwright driver (about 135 MB) into `~/.cache/playwright-rust/`.

```bash
cargo install --path crates/mc-cli --features browser
```

```bash
amel browsers install   # once, unless Playwright's Chromium is already installed
```

```bash
amel copy https://app.example.com --requester auto -o ./copies
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
| `-j/--workers/--concurrency 4`, `--delay-ms 1000` | Parallel downloads, and the gap between two requests to one site |
| `--adaptive-delay` | Smart slow-down on 429/503, speeding up again after good answers |
| `--retries 3`, `--retry-delay-ms 1000`, `--timeout-secs 30` | Patience: retries per URL for timeouts, dropped connections and 429/5xx, the wait before the first one (each later one waits about twice as long; a server's Retry-After wins) |
| `--sitemaps` | Also copy the pages listed in the site's sitemaps |
| `--skip-types video,audio` | File types not to download (page, style, script, image, font, video, audio, document, archive, data, other) |
| `--type-limit video=200` | The largest file of a type to keep, in MiB; repeat for several types |
| `--script hooks.js` | JavaScript hooks run with the copy (see Scripts below) |
| `--log-file FILE` | Also write a timestamped log of every event to FILE (`copy`, `dir` and `scan`) |
| `--verify` | When done, check for missed files and download them again |
| `--profile NAME`, `--platform OS` | Browser to impersonate (`amel requesters profiles` lists them) and the platform it claims |
| `--proxy URL` (repeatable), `--proxy-file FILE` | Proxies to use; several rotate |
| `--proxy-rotation round-robin\|sticky` | Each request the next proxy, or one proxy per site |
| `--no-proxy HOSTS`, `--system-proxy` | Hosts that go direct (names, `.suffix`, IPs, CIDR); use `HTTPS_PROXY`/`NO_PROXY` |
| `--limit-rate RATE` | Cap the average download speed, e.g. `500K` or `2M` |
| `--ignore-robots` | Fetch what robots.txt disallows; only for sites you may copy |

These options are saved with the copy and reused by `resume` and `update`.

Options that carry credentials are never saved, so pass them again to `resume` and `update`:

| Option | Effect |
|---|---|
| `--auth HOST=USER:PASSWORD` | HTTP Basic login for one host. `--auth HOST=USER` reads the password from `$AMEL_PASSWORD`, which keeps it out of your shell history |
| `--header "Name: value"` | Extra header, sent only to the site's own hosts |
| `--cookies cookies.txt` | Start with the cookies from a Netscape-format file, as exported by browser extensions or by `amel login` |
| `--login-script login.json` | Log in through Chromium before the crawl (needs `--requester playwright` or `auto`) |

Credentials are dropped when a redirect leaves the origin. Links that look like logout (`/logout`, `/sign-out`, `?action=logout`…) are never followed unless an `--include` rule names them, so a crawl doesn't end its own session.

A login script is a JSON list of steps, each with exactly one action. Use `value_env` to keep the password out of the file:

```json
[
  { "goto": "https://example.com/login" },
  { "fill": "#username", "value": "ann" },
  { "fill": "#password", "value_env": "AMEL_PASSWORD" },
  { "click": "button[type=submit]" },
  { "wait_for": ".account-menu" }
]
```

The browser logs in once before the first request. With `--requester auto`, plain pages are then fetched over HTTP using the same session.

To log in by hand instead (for example past two-factor prompts), open a browser window and save the session:

```bash
amel login https://example.com/login --save cookies.txt
```

Then pass `--cookies cookies.txt`. The file holds a live session and is created readable only by you.

Exit codes:

| Code | Meaning |
|---|---|
| 0 | Everything copied |
| 1 | The copy finished, but some URLs failed (listed in the report) |
| 2 | Invalid command line, configuration or saved state |
| 3 | Stopped before finishing; run `resume` |

### Large files

Files over 4 MiB are written to disk as they arrive, not kept in memory, under the copy's `.amel/partial/` folder. If the connection drops, the retry asks the server only for the rest (`Range`, guarded by `If-Range` so a changed file starts over), and the pieces are joined. In the app, each worker's row shows a progress bar for its file, such as "11.1 MiB / 20.0 MiB", and Size and Speed count what has arrived so far.

### Scripts

A small JavaScript engine (QuickJS) runs hooks with a copy: `--script hooks.js` on the command line, or the **Script** tab in the app, which has an editable example. Define any of these:

```js
function onStart(info)          { log("Copying " + info.url) }
function shouldFetch(url, kind) { return !url.includes("/tag/") }  // false skips it
function onFile(file)           { if (file.bytes > 1e7) log("big: " + file.url) }  // url, path, bytes, mime, type
function onError(url, reason)   { log(url + " failed: " + reason) }
function onFinish(summary)      { log(summary.saved + " files in " + summary.seconds + " s") }
```

`log(text)` prints a line (a JS row in the app's Activity). Each call may run for 2 seconds and use 64 MiB. A script that throws is reported and the copy goes on; one that runs too long is turned off.

### Renamed from MisterCopy

The commands are now `amel` and `amel-gui`, and the settings live in `~/.config/amel`. Copies keep their state in `.amel/` inside the copy folder. Settings in `~/.config/mistercopy` and copies with a `.mistercopy/` folder are moved over the first time they're opened, and `MISTERCOPY_PASSWORD` is still read.

## Layout

| Path | What it is |
|---|---|
| `crates/mc-requester` | The `Requester` trait every fetch backend implements, the hybrid `auto` requester, the shared cookie jar, the registry, and a mock (`mock` feature) |
| `crates/mc-requester-wreq` | The default requester: wreq with browser TLS/HTTP2 fingerprints |
| `crates/mc-requester-playwright` | The browser requester: Chromium via Playwright (`browser` feature) |
| `crates/mc-extract` | Finds and rewrites URLs in HTML and CSS |
| `crates/mc-core` | The crawler: rules, URL → file path mapping, retries, robots.txt, limits, link rewriting, reports |
| `crates/mc-store` | Crawl state in SQLite, for resume and update |
| `crates/mc-cli` | The `amel` binary |
| `crates/mc-gui` | The `amel-gui` desktop app (gpui-kit) |
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
