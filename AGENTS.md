# Conventions

jensen-harness is a standalone agent harness. It ships only a backend; no runtime dependency on
Jensen or any other product. Rust 2024, a workspace of crates sliced like Jensen: each slice has
`domain`, `application` and `adapters`.

The design never lives in a committed file. It is a Jensen plan, registered with `plan_register`
and read back with `plan_status` or `plan_trace`; a stray `docs/plan.md` is a mistake, not a
convention, and is gitignored for exactly that reason.

Failures are loud. A failure is a typed error the caller sees, never a quiet downgrade: no `let _ =`,
no `unwrap_or_default` standing in for a failed read. The lints deny `unwrap`, `expect`, `panic`,
indexing, lossy casts and printing.

Execution has no human in it. Do not add approval prompts. Safety comes from the sandbox, the
promotion gates, the budget caps and the stall detectors.

Classification and other snap decisions go through the `Judge` trait, never a generative model.

Commit messages are conventional commits, `type(scope): imperative subject`, lower case, with no
attribution trailers.
