# Agent Instructions

This repository contains `sjmb_matrix`, a Rust Matrix bot that watches joined Matrix rooms for URLs and stores them in PostgreSQL.

## Working Rules

- Preserve the current bot behavior unless the user asks for a functional change.
- Do not revert unrelated user changes. Check `git status --short` before and after edits.
- Prefer small, direct patches. Use `cargo fmt` after touching Rust code.
- Keep dependencies explicit in `Cargo.toml`; avoid broad `0` version requirements.
- The Matrix SDK currently uses the upstream git dependency with `bundled-sqlite`. Do not switch back to system SQLite unless the environment has `libsqlite3` available and the user requests it.
- There are no checked-in migrations. Database table expectations are documented in `README.md`.

## Useful Commands

```sh
cargo update
cargo fmt --check
cargo test
cargo clippy --all-targets -- -D warnings
```

Use `cargo fmt` to apply formatting when `cargo fmt --check` reports diffs.

## Project Layout

- `src/bin/sjmb_matrix.rs`: executable entrypoint.
- `src/config.rs`: CLI flags, config path expansion, tracing setup.
- `src/matrixbot.rs`: Matrix login, sync, message handling, URL extraction.
- `src/db_util.rs`: PostgreSQL connection helpers and URL insert logic.
- `src/str_util.rs`: whitespace normalization helper for room/user labels.
- `config/sjmb_matrix.json`: example runtime config.
- `build.rs`: injects build metadata environment variables.

## Verification Expectations

For dependency or code changes, run:

```sh
cargo fmt --check
cargo test
cargo clippy --all-targets -- -D warnings
```

If a command cannot be run because of local tooling, permissions, or network access, report that explicitly.
