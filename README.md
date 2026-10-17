# jensen-harness

A standalone agent harness for software engineering. It calls model providers directly, runs its own
tool calling loop inside a sandboxed worktree, and keeps tool input and output small. It has no
runtime dependency on any other product.

## Run it

```sh
cp providers.example.toml providers.toml
jensen-harness --config providers.toml auth set anthropic   # key from one line of stdin
jensen-harness --config providers.toml run                  # JSON-RPC over stdio
jensen-harness --config providers.toml serve --socket /tmp/jh.sock
```

Build the binary with `cargo build`. Without `--config` the file is `providers.toml` in the user config
directory, under `jensen-harness`. `--model provider_id/model_id` overrides `default_model`.

## Providers

Every provider is a table in `providers.toml`: a transport, a base URL, and where its key lives.
Anthropic, OpenAI compatible endpoints and gateways (OpenRouter, LiteLLM, a self hosted router) and
the other vendors genai supports are all config, never code. A model is addressed as
`provider_id/model_id`, split at the first slash, so a gateway's own `vendor/model` ids stay whole.

A key comes from exactly one of:

- `api_key_env`, the name of an environment variable.
- `api_key_keychain`, the OS keychain (macOS Keychain, Windows Credential Manager, Linux Secret
  Service). `service` defaults to `jensen-harness` and `user` to the provider id. Store a key with
  `jensen-harness auth set <provider>`.

A provider whose key is missing fails loudly at startup and when a session asks for it.

Anthropic extended thinking is not supported yet: the genai crate drops thinking signatures, so the
blocks cannot be sent back unchanged.

## Protocol

Newline delimited JSON-RPC 2.0, identical on stdio and the Unix socket (owner only, peer checked).

- `run.new {budget?}` returns `{run_id}`: a budget scope that sessions can join and share.
- `session.new {worktree, model?, max_turns?, run_id?, budget?}` returns `{session_id}`. The worktree is
  canonicalized and must be a directory. `budget` is `{max_tokens?, max_cost_micro_usd?}`.
- `session.prompt {session_id, message}` runs the model until it ends its turn, `max_turns` model calls
  are spent, or a budget cap refuses the next call. It returns `{stop_reason, model_calls, cost_micro_usd,
  usage, savings}`. `stop_reason` is `end_turn`, `max_tokens`, `stop_sequence`, `refusal`, `turn_limit` or
  `budget_exhausted`.
- `session.resume {session_id}` continues a prompt that was cut short by a provider error or a restart,
  and returns what `session.prompt` would have.

Unknown parameters are rejected, so a typo never silently drops a cap. Each session has its own lock, so
a long run never blocks another session.

## Budget

Every model call is reserved before it is made and settled with the provider's real usage after. Scopes
form a tree (a run holds sessions) and a reservation is checked against the session and every ancestor in
one transaction, so two sessions can never both take the last of a shared cap. Caps count all four token
buckets and, when the model has a price, micro dollars. At the soft cap (80% by default) the model is told
once to wrap up; at the hard cap the next call is not made and the prompt ends with `budget_exhausted`.

A session with no cap from the request or from `[budget]` in the config gets a built in cap of 5,000,000
tokens. Cache reads count in full, so a session with a large context reaches it sooner than the bill would
suggest; set `max_tokens` to suit. Prices are integer micro dollars per million tokens in `[pricing]`, with a version that is journaled
with every call. A cost cap on a model with no price is refused.

## Journal and recovery

State lives in one SQLite file, by default `state.sqlite` in the user data directory (`--state` overrides
it), readable by its owner only. The journal and the ledger are append only: the database refuses any
`UPDATE` or `DELETE`, and a call can be settled or released once.

Each model response and the settlement of what it cost are written in one transaction, then each tool
start, tool result and committed results turn. After `kill -9` the daemon rebuilds a session from the
journal on first use. `session.resume` then continues from exactly where it stopped: a completed model
call is not repeated, a finished tool is not run again, and a model call that was in flight is released and
not charged. A tool that only reads is simply run again; `edit` and `shell` that had started but not
finished are reported to the model as interrupted rather than run twice.

Every call is journaled with its model, tokens, cost, price version, latency and cache hit ratio, and is
also a `tracing` event. For a provider that reports cache tokens, a hit ratio under 80% after the first
call raises one `alert.cache_low` event per session.

Only one daemon may use a state file at a time: a second one is refused at startup, so two processes
can never both continue the same prompt. The lock is released by the operating system when a process
dies, including by `kill -9`. Run a second daemon with its own `--state`.

The journal holds transcripts and tool output, which can include whatever the model read. It is not
redacted.

## Tools and token savings

The model gets `read`, `edit`, `glob`, `grep`, `shell` and `expand`. File tools open the worktree as a
`cap-std` capability, so `..`, absolute paths and symlinks that lead out are refused at the syscall.
`shell` runs inside Seatbelt (macOS) or bubblewrap (Linux) with a cleared environment and a timeout.

Every result is compacted before it reaches the model:

- A large source file reads as an outline of signatures with line ranges. The model then asks for one
  `symbol` or a line range.
- `grep` groups by file and caps matches. `glob` groups long listings by directory.
- `shell` output is filtered by command (cargo, git, pytest, go, npm and others), keeping failures and
  dropping passes. Anything compound gets the generic filter. The command is never rewritten.
- Whatever a filter cuts is saved, and the result ends with an `expand` handle for the full text.
- `session.prompt` reports the estimated tokens saved.

## Develop

`make check` runs `cargo fmt --check`, `cargo clippy -D warnings` and `cargo test`. Tests live in
`tests/integration`, none in production code. Two ignored tests call a real model: set
`JENSEN_HARNESS_LIVE_CONFIG` and `JENSEN_HARNESS_LIVE_MODEL`, then run with `--ignored`.
