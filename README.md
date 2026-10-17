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

- `session.new {worktree, model?, max_turns?}` returns `{session_id}`. The worktree is canonicalized and
  must be a directory.
- `session.prompt {session_id, message}` runs the model until it ends its turn or `max_turns` model
  calls are spent, and returns `{stop_reason, model_calls, usage, savings}`.

Each session has its own lock, so a long run never blocks another session.

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
