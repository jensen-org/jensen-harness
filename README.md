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

- `workflow.start {worktree, workflow, inputs?, budget?}` runs the workflow file (a path under the
  worktree) until it ends and returns `{run_id, status, channels, outcomes, failure?, super_steps}`.
  `status` is `running`, `succeeded`, `failed`, `cancelled`, `budget_exceeded` or `deadline_exceeded`.
- `workflow.status {run_id}`, `workflow.resume {run_id}` and `workflow.cancel {run_id}`. Resume rebuilds
  the graph from the spec journaled with the run and continues from its last committed wave. Cancel is
  sent on another connection than the one waiting for the run, so it works on `serve` and not on `run`,
  which serves one request at a time.

Unknown parameters are rejected, so a typo never silently drops a cap. Each session has its own lock, so
a long run never blocks another session.

## Workflows

A workflow is a YAML file (`workflows/examples/triage.yaml`): typed channels, nodes, and edges.

- Channels hold JSON validated by a JSON Schema, folded by a reducer (`overwrite`, `append`, `set_union`,
  `merge_map`, `max`). A node may read a channel only if every writer of it is its ancestor, so the shared
  state never depends on which branch finished first.
- Node kinds: `agent` (a model session that replies with one JSON object holding the channels it
  writes), `command` (a sandboxed shell command), `verifier` (a command plus a CEL check; only a
  verifier writes a `verified` channel), `router` (a CEL expression picks a label, edges with a `when`
  guard follow it) and `loop` (a body, a CEL `until` and a required `max_iterations`; it ends `ok`,
  `stalled` when an iteration repeats the last one, or `exhausted`).
- Every agent needs `max_turns` or a budget and every loop a bound, or the file does not load. Parallel
  branches may not hold a writer agent, and two parallel nodes may not write one `overwrite` channel.
- Validation runs on a petgraph graph, execution on dagrs, expressions in CEL.
- A role maps to a model in `providers.toml` under `[roles]`, for example `explore = "provider/model"`.

`jensen-harness workflow validate <file>` checks a file and `jensen-harness workflow dot <file>` prints
its graph.

Each agent node runs its own session, opened under a node budget scope that rolls up into the run. After
`kill -9`, `workflow.resume` restores finished waves from the checkpoint, resumes an agent node from its
own journal, and reports a command that had started but not finished as `interrupted` unless it is
marked `idempotent`. Run checkpoints are kept in `<state>.workflow-runs`, a directory only its owner can
open, next to the state file; like the state file it belongs to one daemon. A node result is also recorded by a hash of the node and its inputs, so a node that
finished inside a wave that did not commit is not run again. Cancelling a run releases its reservations.

## Isolation, promotion and publishing

A workflow with a `tools: writer` agent needs a git repository. Each writer works in its own linked
worktree under `<state>.workspaces/<run>/<node>` (mode 0700), on the branch `jensen/<run>/node/<node>`,
and nothing it does touches your branch or working tree. The host never reads git metadata from that
worktree: it checks the `.git` pointer, then builds the commit itself from the files with gitoxide, with
`Jensen-Run`, `Jensen-Node`, `Jensen-Session`, `Jensen-Model` and `Jensen-Spec` trailers. The whole crate
is gitoxide; the only `git` it runs is `git push`, because gitoxide has no push yet.

- A `promote` node (`from` a writer, `requires` verified channels) runs the gates on the diff and then
  puts the work on the run's integration branch `jensen/<run>/integration`: one squashed commit when the
  branch has not moved, otherwise a merge commit that is gated again. A conflict, a failed gate or an
  unmet `requires` ends the node as `conflict` or `blocked` with the findings in its output channel, and
  the branch is untouched.
- Gates: protected paths (`.git`, `.github/workflows`, `.gitlab-ci.yml`, `.gitmodules`, `.jensen`, plus
  `protected` globs), matched case insensitively and with NTFS and HFS normalization; added symlinks and
  submodules; a limit on changed files and added bytes; binary files unless allowed; and a secret screen
  (yara-x rules for private keys, cloud, VCS and model keys, credentialed URLs) over every blob. A report
  names the file and the rule, never the match. Risk flags (new `unsafe`, process spawning, `eval`,
  manifests and lockfiles) inform without blocking.
- A `publish` node (`from` a promote node) pushes the branch as `jensen/<run>` and opens a GitHub pull
  request or a GitLab merge request, reusing one that is already open for the branch. The push runs from a
  throwaway repository that borrows the object store, so no repository config can redirect it, and the
  token travels as an HTTP header bound to the forge's host, never in an argument.
- Verifiers and commands after a writer run in the writer's worktree with a read write sandbox, since a
  verifier such as `make check` writes build output. An agent without `tools: writer` gets a read only
  sandbox and a tool set that refuses `edit` and `shell`.

Forges are configured in `providers.toml`, never in a workflow: `[forges.github]` and `[forges.gitlab]`
take `base_url` (GitHub Enterprise, self hosted GitLab) and `api_key_env` or `api_key_keychain`
(`jensen-harness auth set github`). The forge is read from the host of `origin`; anything else is
`UnknownForge`. See `providers.example.toml` and `workflows/examples/implement.yaml`.

A run whose promotion was blocked still reports status `succeeded`, because the workflow itself ran to
its end; read `outcomes` (`promote` is `ok`, `blocked` or `conflict`) before assuming work landed. Nodes
that run outside a writer's scope get a detached worktree at the integration head, so they see promoted
work and never write into your checkout.

Not covered yet: a denylist of dangerous commands, a command audit log, circuit breakers on repeated
failures, and waiting for CI results from the forge.

## Extending agent nodes

An agent node is a template plus overrides, so a design agent, an architect or a reviewer is a file, not
code.

- **Templates.** `uses: figma-designer` on an agent node pulls `figma-designer.yaml` from an `agents/`
  directory next to the workflow, from `[library] paths` in `providers.toml`, or from the user config
  directory. Lists append, maps merge and scalars override. A template's `inputs` become `reads` and its
  `outputs` become `writes` (their schemas must equal the channel's). A missing, twice found or cyclic template
  is a load error naming the node, and `jensen-harness workflow validate --resolved` prints the merged
  result. The resolved spec is what is hashed and stored, so a run never reads the library again. See
  `agents/` and `workflows/examples/design.yaml`.
- **Skills.** `skills: [name]` attaches [Agent Skills](https://agentskills.io/specification). Only names and
  descriptions go in the system prompt; the `skill` tool loads the body or a bundled `references/` file.
  Skills come from the user config directory and each library path's `skills/`; the repository's own
  `skills/` is loaded only with `[skills] allow_project = true`, and a name found twice is an error.
- **MCP.** `mcp: [figma]` attaches a server declared in `providers.toml` under `[mcp.servers.<name>]`, never in a
  workflow. A stdio server runs inside the node's OS sandbox with only its `env_allow` variables and the
  network closed unless the entry opens it; `access = "read_only"` hides every tool that is not marked
  read only. Tools appear as `mcp__server__tool`, results are compacted like shell output, and after a
  `kill -9` an unfinished read only call is run again while any other is reported to the model as
  interrupted and never repeated. A remote server takes its token from the environment or keychain.
- **Context.** An agent's `context` block can `inherit` channels (full or summarized), take the final answer of
  upstream nodes (`from_nodes`), and `fetch` from `files`, `treesitter` outlines, `skill` bodies, `mcp`
  resources or read only tools, and `lsp` (diagnostics, symbols, references from a server under
  `[lsp.servers.<name>]`). `eager` sources are gathered once into the first prompt, which is journaled, so a
  resume replays the same text; `lazy` sources become a `fetch_context` tool. A total budget drops what
  does not fit and says so.

An MCP or language server is code you chose to run. It is confined by the OS sandbox, but whatever it is
allowed to reach (its token, the network you open, the worktree you make writable) it can use.

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
