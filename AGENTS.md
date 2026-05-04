# Repository Guidelines

`json-repair-rs` is a Rust library and CLI for repairing malformed JSON. The crate is in early scaffolding (edition 2024, no dependencies yet); follow the conventions below as code lands.

## Project Structure & Module Organization
- `Cargo.toml` — package manifest. Keep it the single source of truth for crate metadata and dependencies.
- `src/main.rs` — current binary entry point. As the crate grows, move shared logic into `src/lib.rs` and submodules (`src/parser.rs`, `src/repair/…`); leave `main.rs` as a thin CLI shell.
- `tests/` — integration tests, one `*.rs` file per scenario. Place malformed-JSON fixtures under `tests/fixtures/`.
- `benches/` — Criterion-style benchmarks (added when needed).
- `examples/` — runnable usage samples surfaced via `cargo run --example <name>`.
- `.omc/` is local tooling state — do not commit files there.

## Build, Test, and Development Commands
- `cargo build` / `cargo build --release` — compile debug or optimized artifacts.
- `cargo run -- <args>` — run the CLI against ad-hoc input.
- `cargo test` — execute unit + integration tests.
- `cargo clippy --all-targets -- -D warnings` — lint; treat all warnings as errors.
- `cargo fmt --check` — verify formatting (run `cargo fmt` to fix).
- `cargo doc --open` — build and view rustdoc.

## Coding Style & Naming Conventions
- 4-space indentation; 100-char line limit (rustfmt default).
- `snake_case` for functions/modules, `PascalCase` for types/traits, `SCREAMING_SNAKE_CASE` for constants.
- No wildcard imports outside preludes and `mod tests`. Order imports: std → external crates → local.
- No `.unwrap()` in library paths — return `Result<T, E>` with `thiserror`-derived errors and propagate via `?`. Use `anyhow` only at the binary boundary.
- All public items require doc comments with `# Errors`, `# Examples` when meaningful.
- No emoji, no commented-out code, no leftover `dbg!`/`println!` from debugging.

## Testing Guidelines
- Built-in harness: `#[test]` in `#[cfg(test)] mod tests` for unit tests; `tests/<feature>.rs` for integration.
- Name tests after observable behavior, e.g. `repairs_trailing_comma`, `errors_on_unterminated_string`.
- Every reported repair bug lands with a failing regression test before the fix.
- Treat fixture files as read-only inputs; assertions go in code, not in the JSON.

## Commit & Pull Request Guidelines
- History so far is a single `Initial commit`. Going forward, use imperative subjects ≤72 chars (`Add trailing-comma repair`); add a body when the motivation isn't obvious from the diff.
- Sign commits (`git commit --gpg-sign`).
- PRs: state motivation, link issues, list verification (`cargo test`, `cargo clippy`, `cargo fmt --check`), and include before/after JSON snippets for any repair-behavior change. Keep PRs focused — split refactors from feature work.

## Toolchain & Dependencies
- Rust edition 2024 — requires a recent stable toolchain. Pin via `rust-toolchain.toml` if you need reproducibility across contributors.
- Add dependencies sparingly; prefer `std` and small focused crates, and justify each addition in the PR description.
