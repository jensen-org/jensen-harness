# Conventions

Rust 2024, one crate sliced like Jensen: each slice has `domain`, `application` and `adapters`.
The design and delivery phases live in `docs/plan.md`.

Failures are loud. A failure is a typed error the caller sees, never a quiet downgrade: no `let _ =`,
no `unwrap_or_default` standing in for a failed read. The lints deny `unwrap`, `expect`, `panic`,
indexing, lossy casts and printing.

Execution has no human in it. Do not add approval prompts. Safety comes from the sandbox, the
promotion gates, the budget caps and the stall detectors.

Classification and other snap decisions go through the `Judge` trait, never a generative model.

Commit messages are conventional commits, `type(scope): imperative subject`, lower case, with no
attribution trailers.
