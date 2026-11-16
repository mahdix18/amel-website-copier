# MisterCopy — Product & Architecture Plan

_Draft v2 · 2026-11-11_

MisterCopy is a lightweight website copier written in Rust. It runs on Windows, macOS and Linux, and copies sites for offline browsing and archiving. Pages are fetched through **pluggable requesters**. The first two are a browser-impersonating HTTP client (**wreq**) and a full browser (**Playwright**). The app ships as a CLI and as a fast native GUI built with **gpui-kit**.

---

## 1. Why build this

Existing tools (Cyotek WebCopy, HTTrack, `wget --mirror`) share the same gaps:

| Gap in existing tools | MisterCopy answer |
|---|---|
| No JavaScript execution, so SPAs and lazy-loaded content are missed | **Playwright requester** renders pages, captures every network response, and scrolls to trigger lazy loading |
| Plain HTTP clients get blocked or served degraded pages by sites that reject non-browser traffic | **wreq requester** speaks with real browser TLS/HTTP2 fingerprints (Chrome, Firefox, Safari profiles) |
| Hard-wired fetching | **`Requester` trait**: new transports (curl-impersonate, CDP-native, proxy pools, plain reqwest…) can be added without touching the crawler |
| Windows-only (WebCopy) / dated UX (HTTrack) | A single native binary for each OS. The GPUI desktop app is GPU-accelerated, and there is also a scriptable CLI |
| Crashes on bad URLs, illegal path characters, odd headers | Every input is validated up front. Errors are recorded per URL and never stop the whole crawl |
| No retries, fixed timeout, no TLS 1.3 | Retries with exponential backoff and jitter, per-request timeouts, and modern TLS via BoringSSL (wreq) and Chromium |
| Filenames change between runs | **Same paths every run**: the URL → path mapping is a pure function |
| Local filenames in rewritten links aren't URL-encoded | The rewriter percent-encodes every local reference. Golden tests cover spaces, Unicode and `#?%` |
| Can't resume or update a copy | Crawl state is kept in SQLite, so runs can resume. Updates use conditional requests (ETag / Last-Modified) |

**Non-goals (v1):** downloading server-side source code, solving CAPTCHAs or interactive bot challenges, distributed crawling, running as a hosted service.

## 2. Target users & core use cases

1. **Offline reading.** Copy a docs site or blog and browse it from `file://`.
2. **Archiving and migration.** Snapshot a site before redesigning or shutting it down, optionally as WARC.
3. **Asset extraction.** Download only images, PDFs or media that match rules.
4. **Auditing.** Get a link map, broken links, missing pages and a list of assets without saving anything (`scan`).

## 3. Technology choices

| Concern | Choice | Notes |
|---|---|---|
| Language | **Rust** (stable, edition 2024) | One binary per OS, low memory use, no runtime |
| Async runtime | `tokio` | Required by wreq and playwright-rs |
| Impersonating HTTP | [`wreq`](https://docs.rs/wreq) + [`wreq-util`](https://docs.rs/wreq-util) | Browser TLS/JA3/JA4 + HTTP/2 fingerprints. `wreq-util::Emulation` provides 100+ profiles. Versions have changed often (5.x → 6.0-rc → 0.1x), so **pin exact versions** and isolate the crate behind our trait |
| Browser automation | [`playwright-rs`](https://docs.rs/crate/playwright-rs) | Unofficial bindings, pre-1.0. It drives the Playwright driver plus Chromium, which are downloaded on first use, not bundled |
| GUI | [`gpui-kit`](https://docs.rs/gpui-kit) (Longbridge: GPUI + gpui-component + assets) | GPU-rendered and lightweight, the stack Zed is built on. One dependency that pins a matching GPUI |
| HTML | `html5ever`/`scraper` to extract, `lol_html` to rewrite | lol_html streams its rewriting with low memory use |
| CSS | `cssparser` (or `lightningcss`) | To find `url()`, `@import` and `image-set()` |
| State | `rusqlite` (bundled SQLite, WAL mode) | Frontier, resume, incremental updates |
| CLI | `clap` v4 + `indicatif` | |
| Config | `serde` + YAML, schema via `schemars` | The CLI and GUI share the same config structs |
| Cookies | `cookie_store` | One jar shared by all requesters |
| Logging | `tracing` + `tracing-subscriber` (JSON option) | |

**Lightweight by default:** Playwright is an optional cargo feature (`browser`), and its driver and browsers are installed only when first needed (`mistercopy browsers install`). Without it, the CLI binary should stay well under 20 MB.

**Build dependencies:** wreq builds BoringSSL, so contributors and CI need `cmake`, `clang`/`libclang` and `perl`/`nasm` (Windows). GPUI on Linux needs the Vulkan, Wayland/X11 and `xkbcommon` development packages. Both lists get documented in `CONTRIBUTING.md`.

### Workspace layout

```
crates/
  mc-core/                 frontier, scheduler, rules, path mapper, rewriter, events
  mc-requester/            Requester trait, request/response types, registry
  mc-requester-wreq/       wreq + wreq-util impersonating client
  mc-requester-playwright/ playwright-rs browser requester   (feature = "browser")
  mc-extract/              HTML / CSS / sitemap link extraction
  mc-store/                SQLite crawl state + output writers (dir, WARC, zip)
  mc-config/               project.yaml model, validation, JSON Schema
  mc-cli/                  `mistercopy` binary
  mc-gui/                  `mistercopy-gui` binary (gpui-kit)
fixtures/                  local test websites + axum fixture server
```

`mc-core` depends only on the `mc-requester` **trait**, never on wreq or Playwright directly. Requester crates register themselves with the registry from the binaries.

## 4. Architecture

```
          ┌──────────────── project.yaml ─────────────────┐
          ▼                                               │
 Seeds → Frontier/Scheduler ──► Requester Registry ──► Extractors
         (SQLite queue,          ├─ wreq  (impersonate)    HTML/CSS/srcset/
          per-host limits)       ├─ playwright (render)    network log
               ▲                 └─ …future requesters        │
               │                                              ▼
               └──────────── Rules engine ◄──── discovered URLs
                                   │
                  Path mapper ──► Rewriter ──► Output (dir | WARC | zip)
                                   │
                   Event bus (tokio broadcast) → CLI progress / GUI / report
```

### 4.1 Requester abstraction (the extension point)

```rust
#[async_trait::async_trait]
pub trait Requester: Send + Sync + 'static {
    /// Stable id used in config and rules, e.g. "wreq", "playwright".
    fn id(&self) -> &'static str;
    fn capabilities(&self) -> Capabilities;
    async fn fetch(&self, req: FetchRequest, ctx: &FetchContext) -> Result<FetchResponse, FetchError>;
    async fn shutdown(&self) {}
}

pub struct Capabilities {
    pub executes_js: bool,
    pub captures_subresources: bool, // page load yields extra responses
    pub impersonation: bool,
    pub http2: bool,
    pub streaming_body: bool,
}

pub struct FetchResponse {
    pub final_url: Url,
    pub redirects: Vec<Redirect>,
    pub status: u16,
    pub headers: HeaderMap,
    pub body: Body,                    // streamed to a temp file over a size threshold
    pub rendered_dom: Option<String>,  // browser requesters
    pub subresources: Vec<CapturedResponse>,
    pub discovered_links: Vec<Url>,    // links found at runtime (JS-built anchors, pushState)
}

pub trait RequesterFactory: Send + Sync {
    fn id(&self) -> &'static str;
    fn capabilities(&self) -> Capabilities;
    fn build(&self, cfg: &serde_json::Value, shared: &SharedResources) -> Result<Arc<dyn Requester>, BuildError>;
}
```

- `SharedResources` holds the cookie jar, proxy settings, global timeouts and the event sender, so every requester sees the same session.
- Any HTTP response, whatever its status, comes back as `Ok(FetchResponse)`. `FetchError` (`Timeout`, `Connect`, `Tls`, `Decode`, `TooLarge`, `Blocked`, `UnsupportedScheme`, `Other`) is only for fetches with no usable response. The retry policy is decided in `mc-core` from statuses and these types, not inside each requester.
- The project file is YAML, but factories receive their block as a `serde_json::Value`, so the trait doesn't depend on any YAML crate.
- **Adding a requester later** means a new crate that implements `Requester` + `RequesterFactory` and one `registry.register(...)` line in the binaries. Nothing in core, config schema plumbing or the GUI changes, because the requester's own config block is passed through untyped and validated by its factory.
- Candidate future requesters: `reqwest` (plain and honest, no impersonation), `chromiumoxide` (a pure-Rust CDP browser with no Node driver), `curl-impersonate` via FFI, and a `proxy-pool` wrapper that decorates another requester.

### 4.2 wreq requester (default)
- Uses `wreq::Client` with `wreq_util::Emulation::<Profile>`. The profile is configurable (`chrome`, `firefox`, `safari`, `edge`, specific versions) and defaults to a recent Chrome.
- Headers, header order, the User-Agent and client hints all come from the profile, so they stay consistent with the TLS fingerprint.
- Supports HTTP/2, gzip/br/zstd/deflate, proxies (HTTP/SOCKS5), conditional GETs (`If-None-Match` / `If-Modified-Since`) and redirect tracking (the policy is ours; each hop is recorded).
- One client per profile and proxy combination. The connection pool is shared across workers.
- HTTP/3 is out of scope (wreq doesn't emulate it).

### 4.3 Playwright requester (feature `browser`)
- A pool of Chromium browser contexts through playwright-rs. The default pool size is 2, and contexts are recycled every N pages.
- Wait strategy: `networkidle` plus a configurable settle delay, plus an optional `wait_for_selector`.
- **Network capture:** every response the page triggers (XHR/fetch JSON, fonts, lazy images, JS chunks) is returned in `subresources` and saved without fetching it again.
- **Auto-scroll** and optional "click to expand" selectors trigger lazy content.
- `rendered_dom` (the serialized DOM after rendering) is saved by default. Raw server HTML is an option.
- Cookies are synced both ways with the shared jar, so a login done in the browser carries over to wreq.
- The driver and browsers are managed by `mistercopy browsers install|status|remove`.

### 4.4 Choosing a requester
- `requester:` is set globally and can be overridden per rule (for example `^/app/` → `playwright`).
- **Hybrid mode (`requester: auto`):** fetch with wreq first, and escalate the page to Playwright when a heuristic says the HTML is a JS shell (tiny body plus `<div id="root|app">`, `<noscript>` warnings, or a script-only body).
- **Fallback chain:** `fallback: [wreq, playwright]` retries a URL with the next requester on failures like `Blocked`, 403 or a JS challenge page. This is configurable and off by default.

### 4.5 Frontier / Scheduler
- Persistent queue in SQLite (`urls` table: normalized URL, state, depth, referrer, requester used, attempts, ETag/Last-Modified, content hash, local path).
- **URL normalization:** lowercase scheme and host, drop default ports, resolve dot segments, strip fragments, optionally sort the query, and drop configurable tracking parameters (`utm_*`, `fbclid`).
- Concurrency limits apply globally and per host. Each host gets a token-bucket rate limit (`governor`) and honors `Crawl-delay`.
- Limits on depth, page count, total bytes, per-file size and run time.
- `robots.txt` is respected by default (`texting_robots`). Turning it off needs an explicit `ignore_robots: true` and logs a warning.
- **Retries:** network errors, 408/425/429/5xx and timeouts are retried with exponential backoff and jitter, honoring `Retry-After`. Request timeouts are configurable.

### 4.6 Extractors
- **HTML:** `a/link/script/img/source/video/audio/iframe/object/embed`, `srcset`, `poster`, `<meta http-equiv=refresh>`, `<base href>`, inline `style=""` and `<style>` blocks, configurable lazy attributes (`data-src`, `data-srcset`…), and icon/manifest links.
- **CSS:** `url()`, `@import`, `image-set()`.
- **Other:** `sitemap.xml`, RSS/Atom and the `Link:` header become extra seeds.
- JS is not parsed in the wreq path (Playwright covers it). An optional "string sniff" picks up quoted URLs ending in known asset extensions.

### 4.7 Rules engine
Rules are evaluated in order and the first match wins:

```yaml
rules:
  - match: { path: "^/admin" }
    action: exclude
  - match: { mime: "image/*" }
    action: include
  - match: { host: "cdn.example.com" }
    action: download-only        # save, don't crawl links from it
  - match: { path: "^/app/" }
    requester: playwright        # per-section requester
```

Match fields: `url`, `host`, `path`, `query`, `ext`, `mime`, `depth`, `size`. Actions: `include`, `exclude`, `download-only`, `crawl-only`. Per-rule overrides: `requester`, `headers`, `rate_limit`.

The default scope is the seed host plus **domain aliases**. External links are recorded but not fetched unless `include_external: assets` is set.

### 4.8 Path mapper (same output every run, portable)
This is a pure function `(normalized_url, content_type) → relative_path`, with identical output on every OS and every run.

1. `host/` prefix (configurable: `flat`, `host`, or `host+port`).
2. Directory URLs (`/docs/`) → `docs/index.html`.
3. **Extensionless pages → `about.html`** (decided). Mismatched or missing extensions are derived from `Content-Type`.
4. Query strings → `name.<hash8(query)>.ext`.
5. **Windows-safe names everywhere** (decided): reserved names (`CON`, `NUL`, `COM1`…), `<>:"/\|?*`, trailing dots and spaces, and control characters are replaced, even on Linux and macOS, so a copy moves between systems.
6. Unicode NFC normalization. Case collisions resolved with a stable hash suffix.
7. Path-length cap (200 characters per segment, 240 total). Longer paths are truncated with a hash suffix.

Tests: golden tables plus `proptest` properties (always valid on Windows, same output every run, no collisions across inputs).

### 4.9 Rewriter
- `lol_html` streaming rewrite of every extracted reference to a **relative, percent-encoded** local path. CSS is rewritten through the CSS extractor's spans.
- Rewriting always starts from a pristine copy of each document kept in `.mistercopy/originals/`. That makes it safe to run again after a resume, and lets `update` crawl onward from pages the server reports unchanged (304).
- References that are out of scope or excluded stay absolute (or point to a local "not copied" stub page).
- Removes `<base>`, `integrity=` on modified resources and CSP meta tags that would block local files.
- **Offline replay shim (experimental, M5):** for pages fetched with Playwright, an injected script serves recorded `fetch`/XHR responses from a local manifest, so client-side data loads work offline.

### 4.10 Authentication
- Basic and digest credentials per host, custom headers and bearer tokens.
- Import cookies from a Netscape cookies.txt file or browser-export JSON.
- **Scripted login** (Playwright): steps like `goto`, `fill`, `click`, `wait_for`. The cookies land in the shared jar.
- **Interactive login:** `mistercopy login <url>` (or a GUI button) opens a headed browser, the user logs in by hand, and the session is saved to the project.
- Logout and destructive URLs (`/logout`, `/delete`, non-GET forms) are excluded by default.

### 4.11 Output & reporting
- Output formats: **directory** (default), **WARC**, **zip**.
- A typed `CrawlEvent` stream (`Queued`, `Fetched`, `Skipped`, `Failed`, `Retrying`, `Saved`, `Stats`) on a `tokio::sync::broadcast` channel. The CLI, GUI and report writer all subscribe to it.
- `report.json` + `report.html`: totals, errors by type, broken links with referrers, redirects, external links, assets by MIME type, largest files, requester usage and a link graph.

## 5. Interfaces

### CLI (`mistercopy`)
```bash
mistercopy copy https://example.com -o ./out
mistercopy copy https://example.com --impersonate firefox --concurrency 8
mistercopy copy https://app.example.com --requester playwright --scroll --max-pages 500
mistercopy copy https://example.com --requester auto      # wreq, escalate to browser when needed
mistercopy copy -c project.yaml
mistercopy resume ./out
mistercopy update ./out                                   # conditional GETs only
mistercopy scan https://example.com                       # audit, no files written
mistercopy login https://example.com -c project.yaml
mistercopy requesters list                                # registered requesters + capabilities
mistercopy browsers install
```
Exit codes: 0 = done, 1 = finished with some URL errors, 2 = config or input error, 3 = aborted.

### GUI (`mistercopy-gui`, gpui-kit)
A modern, lightweight native app sharing `mc-core` in-process.

- **Projects sidebar:** recent projects, a "New copy" button and a quick URL bar on top ("paste a URL → Copy").
- **Project editor** (tabs): Target and scope · Requester (picker driven by `requesters list`, profile dropdown for wreq, render options for Playwright) · Rules (table editor with a **live rule tester**: paste a URL to see which rule matches and which requester runs) · Limits and politeness · Auth (credentials, cookie import, "Log in with browser") · Output.
- **Live crawl view:** throughput and progress header, a virtualized URL list with status, requester and size columns, a filter bar, and pause/resume/stop.
- **Results:** report summary, errors, link map (graph view, export to SVG/PNG), and "Open copy" in the system browser.
- Light and dark themes from gpui-component. Keyboard-first (command palette). Target cold start under 300 ms and idle memory under 80 MB, without the browser feature in use.

## 6. Robustness principles
- **Validate at the boundary:** URLs, output paths and config are checked before the crawl starts, with clear messages and no panics on user input (`#![deny(clippy::unwrap_used)]` in library crates).
- **Contain errors per URL:** one bad response marks that URL as failed with a reason and the crawl continues. Requester panics are caught at the task boundary.
- **Atomic writes:** write to a temp file, then rename. SQLite in WAL mode. Ctrl-C or closing the GUI saves state cleanly.
- **Bounded resources:** bodies stream to disk above a threshold, in-memory parse size is capped and browser contexts are recycled.

## 7. Responsible use
Impersonation is there for **compatibility**, so sites that refuse non-browser clients still copy correctly. It is not there to break access controls. Polite defaults stay on whatever the requester:
- robots.txt respected
- 1 request/s per host
- low concurrency

MisterCopy does not solve CAPTCHAs or interactive challenges, and the docs say users should only copy sites they're allowed to.

## 8. Testing strategy
- **Unit:** normalizer, path mapper (golden + `proptest`), extractors, rules engine, retry policy, requester registry. Snapshot tests with `insta`.
- **Mock requester:** `mc-core` is tested against an in-memory `Requester`, which also proves the trait is enough for a third-party requester.
- **Fixture sites** (served by an `axum` test server):
  - `static-blog`: srcset, query-string assets, Unicode filenames.
  - `spa-react`: client routing, lazy images, fetch-loaded data.
  - `auth`: basic auth, form login with a CSRF token, logout trap.
  - `ua-gate`: serves the full page only to browser-like TLS/HTTP2 clients, to check that wreq impersonation is wired correctly.
  - `hostile`: redirect loops, infinite calendars, very long paths, bad encodings, malformed headers, 429/503 with `Retry-After`, huge files, `CON.html`.
- **End-to-end:** copy each fixture, then open the output from `file://` with Playwright and check that pages render, there are no network requests in strict offline mode and every link resolves.
- **Fuzzing:** `cargo-fuzz` targets for the URL normalizer, path mapper and HTML/CSS extractors.
- CI runs on Linux, macOS and Windows (GitHub Actions matrix), plus a job with `--no-default-features` to keep the lean build honest.

## 9. Milestones

| # | Scope | Done when |
|---|---|---|
| **M0** Skeleton | Cargo workspace, CI matrix (incl. BoringSSL and GPUI build deps), clippy/rustfmt, axum fixture server, `Requester` trait + mock | CI green on all 3 OSes |
| **M1** Static copier | wreq requester, normalizer, in-memory frontier, HTML/CSS extractors, path mapper, rewriter, basic rules, `copy` CLI | `static-blog` and `ua-gate` copy and browse offline with 0 broken links on all OSes |
| **M2** Reliable crawler | SQLite state, resume/update, retries/backoff, rate limits, robots.txt, limits, auth (basic, headers, cookies), reports, `scan`, `hostile` fixture | `hostile` completes without a panic; interrupting and resuming produces an output tree identical to an uninterrupted run |
| **M3** Browser requester | playwright-rs requester, network capture, auto-scroll, `auto` hybrid + fallback chain, scripted and interactive login, `browsers` command | `spa-react` routes and lazy images present offline; `auth` login works |
| **M4** GUI | gpui-kit app: projects, editor with rule tester, live crawl view, results and link map | Full copy of every fixture driven from the GUI; startup and memory targets met |
| **M5** Archival & SPA replay | WARC/zip output, offline replay shim, link-map export | SPA fixture data views work offline; WARC validates with `warcio` |
| **M6** Distribution | Release binaries (cargo-dist), signed installers (MSI, DMG, AppImage/Flatpak), Homebrew/Scoop/winget | One-command install on each OS |

**Progress (2026-11-11):**

- **M0–M2 done.**
- **M3 done:**
  - the Playwright requester, behind the `browser` feature, using an already installed Chromium when there is one
  - network capture and auto-scroll
  - the `auto` hybrid, which also falls back to the browser on a 403
  - scripted login (`--login-script`, run before the first request) and interactive login (`mistercopy login`, saved as `cookies.txt`)
  - logout links skipped by default
  - `browsers status|install`
  - the `spa` fixture (vanilla JavaScript rather than React) and the `auth` fixture
- **Deferred from M3:** choosing a requester per rule (one requester per crawl for now).
- **M4 done, except the performance targets.** `mistercopy-gui` (gpui-kit 0.9.1) has:
  - a copies sidebar, remembered across launches
  - the copy form with Copy, Update, Stop and Open report, plus live counters and a log
  - a Rules tab with a live tester that uses the crawler's own rule engine
  - a Link map tab with SVG export (PNG export isn't done)
  - a command palette (Ctrl+K)
  - shared state with the CLI
  - headless UI tests covering each feature
- **Performance targets missed:** cold start and idle memory both come out about twice the targets. Measured 2026-11-11 with a release build on this Linux/Wayland machine, using the built-in `MISTERCOPY_PROBE=1`:
  - cold start: 550–670 ms to the first frame, against under 300 ms
  - memory: about 160 MB resident, against under 80 MB
  - binary: 66.7 MB
  - Next steps: profile how much of the memory is the Vulkan driver's mappings, try gpui-kit without its bundled assets, and initialize components lazily.

- **M5 done:**
  - WARC 1.1 output (`--warc`, gzip per record, appended to on resume)
  - zip output (`--zip`)
  - `mistercopy serve`
  - the offline replay shim (`--replay`): captured JSON/text/XML embedded per host and answered to `fetch` and XHR, even from `file://`
  - Link map SVG export came with M4.
- **Caveats:**
  - WARC files are checked by the project's own reader (gzip framing, record lengths, ids), not yet by `warcio`.
  - Client-side routes still need `serve`: under `file://`, a path-routed app sees the file path as its route.

- **M6 partly done:**
  - release profile (thin LTO, stripped): the app went from 71.2 MB to 43.1 MB, though the 71.2 MB build included browser support and the new one doesn't, so it isn't a like-for-like comparison; the CLI stayed about 13 MB
  - `scripts/package.sh` (archive plus SHA-256), checked locally: a 23.6 MB Linux tarball whose binaries run
  - a tag-triggered release workflow for all three OSes, not run yet because there's no remote
  - a Dockerfile for the CLI, built and run locally: a 97.6 MB image that copied the static-blog fixture with exit code 0
- **Still open in M6:**
  - signed installers (MSI, DMG, AppImage/Flatpak), which need signing identities
  - Homebrew, Scoop and winget manifests, which need published release URLs
  - choosing a license: the manifests declare MIT OR Apache-2.0, but the repo has no LICENSE files

- **Design and parallelism pass (2026-11-15):**
  - The crawler numbers its workers and reports them: `Waiting` (holding a URL until the host's slot), `Fetching` and `WorkerIdle`. It also sends `Progress {done, total, active}` from exact counters, so there's no scan per event. Fetches already ran in parallel on the multi-thread runtime; the 1 s per-host gap was what made a single-site copy look serial.
  - The desktop app was redone in an API-client layout: header with command search, icon rail, filterable copies list, request bar (requester chip, address, Copy/Stop, Update), and Options/Rules/Link map tabs. Below is a result panel with a progress bar, time/size/speed, counters, and **Workers** and **Activity** tabs. It has dark and light themes.
  - Options for workers (1–32), speed presets (Polite/Balanced/Fast) and robots.txt. These, and the theme, persist in `prefs.json`. `mistercopy-gui URL --output DIR --start` launches straight into a copy.
  - Copy statuses in the sidebar are read when the list changes, no longer from SQLite on every frame.
  - The CLI draws an indicatif progress bar on a terminal; `-j`/`--workers` alias `--concurrency`.
  - Checked: 122 tests pass and clippy is clean. Screenshots of the running app show Polite mode with all four workers in WAIT, and then the finished copy. The CLI bar was checked in a pseudo-terminal.

- **Competitor gaps, top group (2026-11-15):**
  - Sitemap seeding (`--sitemaps`; on by default in the app):
    - Reads the `Sitemap:` lines of robots.txt, else `/sitemap.xml`. Handles sitemap indexes, gzipped XML and plain-text lists.
    - Capped at 200 files and 200,000 pages.
    - The pages it finds are queued one level below the seed. Scope and robots.txt still apply, and the robots.txt it reads is reused by the crawl.
  - Bandwidth limit (`--limit-rate 500K`): a shared limiter makes each worker wait its share of time after a body arrives. Measured on the static-blog fixture: 3.3 KiB took 3.7 s at 1K, against 0.19 s unlimited.
  - App parity with the CLI:
    - `mc-cli` is now also a library (options, session, requesters, zip), and the app builds its copies from the same `CrawlOptions`, so `resume` and `update` repeat every choice made in the app.
    - New Limits, Auth and Output tabs, plus "save without following links" in Rules. Form errors are explained before anything starts.
  - Browser login from the app: Chromium opens at the site, and **Done** saves the cookies into the copy's state folder. Writing the cookie file now creates missing folders, which also fixes `mistercopy login --save` into a new directory.
  - Checked:
    - 129 tests pass, including a headless UI test that sets max pages and zip output and checks both reach the crawl.
    - Clippy is clean with and without `browser`.
    - The real window was driven over X: Limits, Auth and Output tabs, a 1K-capped copy, and the browser login opening and saving.
    - Not checked: typing real credentials into the opened Chromium. It opened as a Wayland window, which the X test driver can't type into.

- **Proxy configuration (2026-11-15):**
  - `ProxyConfig` and `ProxyPicker` in `mc-requester`:
    - a list of servers (http, https, socks4, socks5, socks5h; credentials in the URL)
    - round-robin or sticky-per-host rotation
    - bypass rules (names with subdomains, `.suffix`, IPs, CIDR, `*`)
    - optional system proxy from `HTTPS_PROXY`/`HTTP_PROXY`/`ALL_PROXY`/`NO_PROXY`
  - The HTTP requester picks a proxy for each hop. If a proxy can't be reached, it sits out for 30 s and the request moves on.
  - Chromium previously ignored the proxy and went direct. It now launches with the first proxy, the bypass list and the credentials.
  - CLI: `--proxy` (repeatable), `--proxy-file`, `--proxy-rotation`, `--no-proxy`, `--system-proxy`. Older saved copies with a single `proxy` string still load.
  - App: a Proxy tab with a list, rotation, bypass, a system switch and **Test**, which tries each proxy against the site. The status bar shows the route.
  - Checked:
    - 140 tests pass, including copies through real local forward proxies: rotation over two, credentials, bypass, a 407 refusal, a bad scheme, and a dead proxy whose requests the live one takes over.
    - In the real window, Test showed a live proxy at 200 and a dead one with a plain error. With the dead proxy listed first, the copy finished with 0 failures, against 6 before failover.
    - Not checked: HTTPS through a proxy (CONNECT), and Chromium's proxy, which is only unit-tested.

- **Missed-files check and impersonation picker (2026-11-15):**
  - Check for missed files (`--verify`; a checkbox in the app, on by default):
    - When the crawl finishes, every saved file is checked on disk. URLs whose files are gone, or that failed for a passing reason (not a 4xx other than 408/429, not a soft 404, not a size limit), are queued and crawled again.
    - `Verifying` and `Verified` events report it, and the summary carries a `Verification` (files checked, retried, recovered, still missing with reasons).
  - Impersonation:
    - `mistercopy requesters profiles` lists the about 150 wreq-util profiles by browser. `--profile` and `--platform` are checked before anything is written.
    - The app has a Browser tab (browser, version stepper, platform, JavaScript switch), and the requester chip shows the profile in use and opens that tab. The choice is remembered.
  - Checked:
    - Engine tests: a dropped connection recovered while a 404 isn't asked again, and a deleted file restored on the next run.
    - A UI test drives the Browser tab and checks the saved preferences.
    - An echo server saw each profile's own User-Agent (Chrome, Firefox, Safari), and `--platform windows` switched Chrome's UA and `sec-ch-ua-platform`.

- **Pause, mini window, own title bar (2026-11-15):**
  - Pause and resume:
    - `StopHandle::pause`/`resume` hold back new requests while those in flight finish. A paused crawl with work left waits instead of ending, and sitemap discovery waits too.
    - `Paused`/`Resumed` events report it. The app has a Pause/Resume button, a Paused phase and palette commands.
  - Mini window:
    - A borderless pop-up window (360×74) that stays on top, shows the main window's progress and can be dragged.
    - Opening it minimizes the main window; its buttons pause/resume and bring the main window back.
    - It opens after the current update, because its first frame reads the main window (opening it inline panicked in the test).
  - Title bar: the header is gpui-kit's `TitleBar` with client-side decorations, which on GNOME/Wayland gives minimize, maximize and close buttons, dragging and double-click to maximize.
  - Checked:
    - an engine test that pausing holds new requests and resuming finishes the copy
    - a UI test that pauses a real copy, opens the mini window (two windows), waits for the engine to confirm the pause, resumes and finishes
  - Checked in real windows:
    - The title bar shows minimize, maximize and close on GNOME, seen through an XComposite capture of the X11 window. The startup probe reports client decorations with both controls on Wayland and on X11.
    - In a nested X server I paused and resumed from both windows, opened and expanded the mini window, and watched the copy finish.
    - That run found a bug the first engine test missed: under Polite, workers already holding a URL kept sending after a pause. They now hold too, and the test covers it (it fails without the fix).
  - The mini window first opened clipped to a 30 px slice on GNOME, because gpui-kit's client-side frame keeps 21 px per side for its shadow and border. It now measures that frame on its first frame and grows to fit (402×116 for 360×74 of content); checked by capture on GNOME. `--mini` opens straight into it.
  - Not checked: dragging the mini window (the nested server has no window manager to honour the move), and where it opens and whether it stays on top under Wayland, which GNOME decides.

- **Retrying failures (2026-11-16):**
  - `CrawlConfig::retry_failed` re-queues a stored copy's failed URLs (whatever the reason) when it is continued.
  - `mistercopy retry DIR` uses it, and so does a **Retry N failed** button in the app's request bar after a copy with failures. Only the failures are fetched; pages are rewritten so recovered files link locally.
  - Per-request retries can now be set: `--retry-delay-ms` in the CLI, and **Retries per file** and **Wait before retrying** in the app's Limits tab.
  - Checked:
    - an engine test (only the failures are requested again; the 404 stays failed; the start page links the recovered file)
    - CLI tests against the hostile fixture's `/rate-limited` (recovered on the second `retry`) and a clean copy ("nothing to retry")
    - a UI test that fails a whole copy with a 0 MB file limit, lifts it and retries
  - The UI test also moved the button: in the progress panel's tab row it could be pushed out of view.

## 10. Risks & open questions
- **Dependency churn:** wreq's versioning has jumped around (5.x → 6.0-rc → 0.1x), playwright-rs is pre-1.0 and GPUI is pre-1.0 too. Mitigations: exact version pins, each kept behind our own crate boundary, and Renovate PRs gated by the fixture suite.
- **Playwright from Rust** still needs the Playwright driver plus Chromium (about 150 MB+ downloaded on demand). If that proves painful, `chromiumoxide` (pure-Rust CDP) is the drop-in alternative requester. The trait makes the swap cheap.
- **SPA offline fidelity** is best-effort, because of auth'd APIs, websockets and hydration mismatches. The replay shim ships experimental.
- **GPUI on Linux and Windows** is younger than on macOS. Rendering on Linux and Windows needs checking early in M4, with a CLI-only fallback.
- **License:** pick one (MIT or Apache-2.0 suggested). Check the licenses of BoringSSL (wreq) and GPUI (Apache-2.0) before release.
- **Open question:** should the GUI also offer a headless/"server" mode for remote machines, or is the CLI enough for that?
