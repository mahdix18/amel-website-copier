# MisterCopy

A lightweight website copier for offline browsing, written in Rust. See [PLAN.md](PLAN.md) for the design and milestones.

**Status:** M1. It copies static sites through a browser-impersonating HTTP client and rewrites their links to work offline. It doesn't render JavaScript yet (M3), and it can't resume an interrupted copy or retry failed requests yet (M2).

## Usage

```bash
mistercopy copy https://example.com -o ./copies
```

The copy lands in `./copies/example.com/`. Useful options:

| Option | Effect |
|---|---|
| `--profile chrome_149` | Browser fingerprint to present (`firefox_151`, `safari_26.4`, …) |
| `--max-depth N`, `--max-urls N` | Limit how far and how much to crawl |
| `--exclude REGEX`, `--download-only REGEX`, `--include REGEX` | Rules, matched against the full URL |
| `--alias HOST` | Treat another host as part of the same site |
| `--external-assets` | Also fetch images, CSS and scripts from other hosts |
| `--delay-ms 1000`, `--concurrency 4` | Politeness: gap per host, requests in flight |

Exit codes: 0 means everything copied, 1 means the copy finished but some URLs failed, and 2 means the command line or config was invalid.

## Layout

| Path | What it is |
|---|---|
| `crates/mc-requester` | The `Requester` trait every fetch backend implements, the registry, and a mock (`mock` feature) |
| `crates/mc-requester-wreq` | The default requester: wreq with browser TLS/HTTP2 fingerprints |
| `crates/mc-extract` | Finds and rewrites URLs in HTML and CSS |
| `crates/mc-core` | The crawler: URL normalization, rules, URL → file path mapping, link rewriting |
| `crates/mc-cli` | The `mistercopy` binary |
| `fixtures` | Local test websites and the server that hosts them (`mc-fixtures`) |

## Development

```bash
cargo test --workspace --all-features
cargo clippy --workspace --all-targets --all-features
cargo fmt --all
```

Serve a test site at http://127.0.0.1:8000/:

```bash
cargo run -p mc-fixtures -- static-blog
```

The wreq requester builds BoringSSL, which needs `cmake`, `clang`/`libclang` and, on Windows, `nasm`. If you have libclang but bindgen can't find `stddef.h`, either install your distro's `clang` package or point bindgen at GCC's headers:

```bash
export BINDGEN_EXTRA_CLANG_ARGS="-I/usr/lib/gcc/x86_64-linux-gnu/13/include"
```
