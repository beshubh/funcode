# Repository Guidelines

## Project Structure & Module Organization

`funcode` is a Rust 2024 terminal application built with Ratatui and Crossterm. Source lives in `src/`: `main.rs` is the binary entry point, `lib.rs` exposes the library, `runtime.rs` owns the terminal event loop, `app.rs` contains application state and input transitions, and `ui.rs` renders the TUI. Background agent coordination is in `agent.rs`; ChatGPT OAuth and credential persistence are in `auth.rs`; shared styling belongs in `theme.rs`.

Tests are colocated with their modules in `#[cfg(test)]` blocks. There is currently no separate `tests/` directory or asset pipeline. Build output under `target/` is generated and must not be committed.

## Build, Test, and Development Commands

- `cargo run` — start the interactive TUI.
- `cargo run -- auth` — open the authentication picker directly.
- `cargo build` — compile a development binary.
- `cargo test` — run all unit and UI rendering tests.
- `cargo fmt --check` — verify Rust formatting.
- `cargo clippy --all-targets --all-features -- -D warnings` — lint every target and reject warnings.

Run formatting, tests, and Clippy before submitting changes.

## Coding Style & Naming Conventions

Use standard `rustfmt` output and four-space indentation. Name modules, functions, and variables with `snake_case`; types and enum variants with `PascalCase`; constants with `SCREAMING_SNAKE_CASE`. Keep terminal input handling deterministic and move blocking work to background runners. Prefer small public interfaces and keep OAuth, persistence, and transport details inside `auth.rs`.

## Testing Guidelines

Write behavior-focused `#[test]` functions named as readable outcomes, for example `auth_command_opens_the_provider_picker_without_submitting_a_prompt`. Add tests beside the changed module. Use Ratatui’s `TestBackend` for visual state and hit-region assertions. Cover success, cancellation, failure, and late-event behavior where asynchronous state is involved. No coverage threshold is enforced, but regressions require a test.

## Commit & Pull Request Guidelines

Recent history favors concise Conventional Commit-style subjects such as `feat: add ChatGPT browser authentication` and `fix: add transcript breathing room`. Use `<type>: <imperative summary>` where practical.

Pull requests should explain user-visible behavior, list verification commands, and link the relevant issue or specification. Include a terminal screenshot or capture for TUI changes. Call out authentication, credential-storage, or dependency changes explicitly.

## Security & Configuration Tips

Never commit or log OAuth tokens. Credentials belong in `~/.funcode/auth.json` with restrictive permissions. Preserve PKCE, state validation, loopback-only callbacks, redacted debug output, and atomic credential replacement when changing authentication code.
