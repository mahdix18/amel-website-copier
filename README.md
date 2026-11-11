# MisterCopy

A lightweight website copier for offline browsing, written in Rust. See [PLAN.md](PLAN.md) for the design and milestones.

**Status:** M0, the workspace skeleton. It can't copy sites yet.

## Layout

| Path | What it is |
|---|---|
| `crates/mc-requester` | The `Requester` trait every fetch backend implements, the registry, and a mock (`mock` feature) |
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

From M1 on, the wreq requester builds BoringSSL, which needs `cmake`, `clang`/`libclang` and, on Windows, `nasm`.
