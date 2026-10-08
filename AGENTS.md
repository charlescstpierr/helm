# Project agent memory

This file is the project's committed home for project-intrinsic agent knowledge: build, test, release, architecture, and sharp-edge notes that should travel with the code.

- Language: docs and UI strings in French; code, identifiers and commit messages in English.
- Architecture, data model and the planned orchestrator design: `docs/architecture.md`. Run/config/checks: `README.md`.
- Before committing: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test` (same as `.github/workflows/ci.yml`).
- Every visual value lives in `assets/tokens.css`; `assets/app.css` only reads `var(--…)` (a test rejects hard-coded colours). Never add raw colours or a second token file.
- No Node, no front-end build step, no CDN: all web assets are embedded and served under `default-src 'self'` (no inline scripts or styles).
- Agent runs are Unix-only and tested with `tests/fixtures/fake-claude.sh` (recorded `claude -p` streams, scenario named in the prompt), never with a real `claude`; a change to the stream parsing starts from a fresh capture of the real CLI.
- Migrations in `migrations/` are append-only: add a new numbered file and register it in `src/db.rs`; never edit a released one.
- The app must stay usable without JavaScript (plain form posts); `assets/app.js` is progressive enhancement.

## Maintaining this file

Keep this file for knowledge useful to almost every future agent session in this project.
Do not repeat what the codebase already shows; point to the authoritative file or command instead.
Prefer rewriting or pruning existing entries over appending new ones.
When updating this file, preserve this bar for all agents and keep entries concise.
