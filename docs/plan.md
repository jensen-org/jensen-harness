# Jensen Harness, research and architecture

## Context

Jensen today wraps agent runtimes. It hosts OpenCode, Codex and others over ACP, gives them tools over MCP, and owns the plan lifecycle, hooks, worktrees, the cartograph and memory. It does not own the model loop, so it cannot control tokens, model routing, step semantics or how several agents work together. jensen-harness is the missing piece, a real harness that owns those things and is hosted by Jensen.

### Product principles, set by the user
1. **One human gate before the run.** The user approves the plan (Jensen's existing gate).
2. **Zero humans during execution.** There are no command prompts and no mid-run check-ins. The sandbox, the automatic gates, the budget caps and the stall detectors replace approvals.
3. **Strong human review of the code at the end.** Investment goes into review tooling, not into supervising the run. Rationale: people approve 93% of permission prompts, and attention collapses after 2 or 3 exposures.
4. **Every snap decision goes to a judgment model, not an LLM.** That covers classification, routing, risk and progress checks. The default backend is Laya (open, Apache 2.0, runs locally, so users need only an LLM key). Jev is an optional hosted backend for users who have a paid key.
5. **Triggers.** A run starts from an `@jensen` mention in a source comment, an editor comment thread, or a PR or issue comment.

## Verdict on the options studied

- **harness2 (google-research/harness2), rejected.** It is a batch research optimizer that rewrites OpenCode and Codex config files to score better on benchmarks. It has no agent loop, tools, sessions, daemon, MCP or LSP. It is v0.1.0 with 3 commits, Vertex only, and runs with sandboxes disabled. Two ideas are kept: completion markers fingerprinted with the harness version, and a per domain lesson library.
- **Fork of OpenCode V2, rejected.** It takes about 44 commits a day, depends on Effect v4, has experimental API groups, a trademark on its name, and no LSP. Its execution is owned by a single process, tool effects are at least once, and it has no DAG. OpenCode stays an alternative ACP runtime under Jensen, and its session durability spec (`specs/v2/session.md`) serves as a reference.
- **Chosen: a Rust harness core, launched by jensend as an ACP agent.** It matches Jensen's stack and SQLite store, and respects the process registry guard (`jensen/scripts/guards.sh`), because sub agents run as async tasks inside one harness process.
- **ACP is an outer shell only, never the engine.** jensend launches and counts the harness as one ACP process so the board can see and supervise it, the same mechanism OpenCode uses. Inside that shell, the harness never drives another tool's own agent loop over its ACP interface (not OpenCode's, not Codex's, not Gemini CLI's) as a black box sub agent. Every model call, across every vendor, goes through the harness's own agent loop and the Provider layer in section 6a. Multi model collaboration (section 7) means several vendors' APIs called directly by our own engine, never several wrapped CLIs talking to each other. For the shell itself, the harness depends on the published `agent-client-protocol` crate (`agentclientprotocol/rust-sdk`), not a hand rolled reimplementation: Jensen's own ACP client is about 991 lines of private, hand rolled NDJSON/JSON-RPC pinned to protocol version 1, which is not published or reusable across repos, and duplicating it by hand would only repeat that choice a second time instead of improving on it.

## Architecture

```
 @jensen source comment ─┐
 editor comment thread  ─┼─▶ Ingress ─▶ Event journal ─▶ Judge (Jev) ─▶ Workflow compiler ─▶ DAG executor
 PR / issue comment     ─┘    dedupe     (SQLite, append    intent,        YAML + plan →         │
                              trust      only, ULIDs)       difficulty,    typed DAG             │ nodes
                                                            risk                                 ▼
                    Review bundle ◀── Promotion gates ◀── Sandboxed writer nodes ◀── Agent loop + Budget ledger
                    (human, end)      (fail closed)       (Seatbelt / bwrap)          + Model router
                                                                                     Jensen MCP: plans, cartograph,
                                                                                     memory, forge, record_change
```

### 1. Ingress and the event journal
- **Sources**
  - Source comments: Jensen's `FileSave` hook (`jensen/backend/src/workflow/domain/hook_event.rs`) feeds a tree sitter comment scanner, so only real comments count and string literals never do.
  - Editor threads: comment threads anchored with `surface/*` (`jensen/backend/src/graph/domain/surface.rs`).
  - PR and issue comments: forge comments through `forge/*` and `find_by_marker`. The OpenCode GitHub Action is not used, because it is still on the V1 SDK.
  - All three emit one `CommentMention` event.
- **Journal:** append only SQLite, ULID ids with parent pointers, the source of truth for replay and provenance. Attributing a failure after the fact with an LLM finds the guilty step only 14% of the time (Who&When), so provenance must be logged deterministically.
- **Dedupe key:** hash of source, location, comment text and content hash.
- **Trust**
  - Comment text is untrusted data.
  - Mentions arriving through `git pull` or a merge never start a run by themselves.
  - Jev answers yes or no tripwire questions: does the comment ask to reveal secrets, disable checks, or act outside the repo?

### 2. Judgment layer (System One)
- A `Judge` trait with two backends. Laya is the default: local, no extra API key, code never leaves the machine, and fine-tunable on the harness's own labeled outcomes. Jev is optional and hosted (paid), useful for questions with many options (Laya degrades above about 20) and for a zero-shot start. Each input gets one request that bundles all of its questions.
  - Laya runs as a `laya-serve` sidecar (its endpoint is Jev compatible) or as an ONNX model loaded from Rust. The sidecar must be checked against the process registry guard first.
  - Laya ships overconfident, so temperature is fitted per deployment on labeled examples. Its known weak spots (score questions, yes/no answers following option labels) are covered by deterministic fallbacks and confidence thresholds in code.
  - Before a backend is trusted for a question set, both backends run in shadow mode on the same labeled examples.
  - The answer types are Choice, Score and Noul (a yes or no that comes back as a probability).
  - Thresholds, weights and policy live in code.
  - Every question set is versioned and tested against labeled examples.
- **Uses**

  | Decision | Jev questions | Code acts on |
  |---|---|---|
  | Mention intent | Choice: question, change, review, fix CI, other | Picks the workflow template; low confidence replies to the commenter asking for detail |
  | Difficulty | Score, from features computed in code (files touched, cartograph fan out, predicted diff size) | Start rung of each role's model ladder, fan out width |
  | Stall | Noul: repeating itself, progress made | Feeds the stall detector together with the deterministic signals |
  | Hunk risk | Score per dimension (security sensitivity, public API, migration, tests touched) | Weighted in code to order the review queue |
  | Review finding in scope | Noul: is this finding within the plan's scope | Scope filter between reviewer and implementer |

### 3. Workflow compiler and DAG executor (orchestration graph)
- **Declarative YAML**, reusing Jensen's workflow schema (`jensen/backend/src/workflow/domain/default_workflows.yaml`, `spec.rs`, `run.rs`). The expression language is small and pure (JSONPath or CEL like), with no arbitrary code.
- **Node kinds:**
  - `Agent`: model, effort, tools, `max_turns`, budget, isolation, `writes: Exclusive | ReadOnly`
  - `Tool` / `Command`: deterministic, for build, test, lint and git
  - `Verifier`: executable checks first, then an objective check of whether the work meets the plan
  - `Router`: Jev or a pure function, producing enumerated labels
  - `Map`, `Join`, `Loop`, `Subgraph`
  - `Effect`: push, PR, comment, all idempotent
  - `ReviewBundle`: collects artifacts for the end of the run
- **State:** named channels, each with a declared reducer (`Replace`, `Append`, `MergeMap`, `SetUnion`, `Max`). Nodes declare `reads` and `writes`. The loader rejects parallel branches that would write the same `Replace` channel. Writes within one step are applied in a deterministic order (node id, then branch index). Artifacts (plan, tasks, diff, test report, findings) are content addressed, have a schema, and are referenced from channels.
- **Durability:** journal per step in the DBOS and Restate style, with no global barrier.
  - Every LLM response, tool result and effect is journaled, so a crash never pays twice for a completed call.
  - A git commit is made at each verified edit, and the best snapshot so far is kept, so later regressions can be rolled back.
  - The workflow definition hash is pinned for each run.
- **Dynamic fan out:** the planner emits typed tasks (write set, interfaces it depends on, acceptance tests). The engine validates them and checks that write sets are disjoint, then expands them. Child ids are `hash(parent_id, item_key)`, so resume stays deterministic. The plan is data, not control flow written by the LLM.
- **Fan out policy:**
  - Parallelize only three kinds of work:
    - read only work (localization, research, review lenses);
    - best of N candidates in isolated worktrees, with exactly one selected;
    - tasks whose write sets are provably disjoint, checked with cartograph `impact` and `work_overlap`.
  - Everything else runs single threaded. Google measured multi agent setups losing 39 to 70% on sequential tasks.
  - Merges go through one integrator node.
  - Effort tiers follow Anthropic's research system: 1 agent, then 2 to 4, then 10 or more, with hard caps on width.
- **Join policies:**
  - `All`, `AllSettled`, `Quorum(k)`, `FirstSuccess` (cancellation is journaled), `Vote`.
  - `BestOfN`, selected in three passes:
    1. hard filters: it builds, causes no regressions, and passes the reproduction and acceptance tests;
    2. dedup of normalized diffs, then a vote;
    3. a pairwise judge from a different model family, run in both orders, only to break ties.
- **Loops and stalls:**
  - Every loop and agent has max iterations, max turns, a budget and a wall clock limit, and the remaining budget is visible to the node.
  - A stall is detected deterministically: no new passing test, an unchanged diff hash, or a repeated tool call signature 3 times, combined with Jev's stall Noul.
  - After 2 stalls the node replans, then ends in a terminal `needs_human` state that appears in the review. Execution never pauses for a human.
  - Fix loops are capped at 3 retries (MetaGPT).
- **Plan ledger:**
  - The plan carries Magentic One style facts, guesses and steps, plus a JSON acceptance list where only verifiers may flip `passes`.
  - Replanning creates a new plan version, and completed outputs are kept when their input hashes match.
- **Lifecycle authority stays in Jensen.** The harness reports `record_change`, `implemented` and `blocked` over MCP, and never approves or lands work. The single writer remains `jensen/backend/src/workflow/application/submit.rs`.

### 4. Multi agent context rules
- Writes are single threaded per write set. Other agents contribute findings, not actions (Cognition 2026).
- Agents hand off typed artifacts, never chat transcripts. Each edge declares a context filter: summary, named artifacts, or a fork of the full trace (only for advisor nodes).
- Reviewers start from a clean context. They see the diff, the plan and the acceptance list, never the implementer's trace.
- Writes count once per verified edit commit, which recovers from coherence collapse, where a correct patch is later overwritten.

### 5. Agent loop
- **Transcript:** append only, a requirement on Claude 5 models, where edited history invalidates thinking blocks. Streaming, with `max_tokens` around 64K on agentic nodes. Uses server side compaction, and clears tool results rarely and in large chunks, because each clear rewrites the cache.
- **Context builder:** per node it pulls a cartograph slice, recalled memory, matching skills and the plan. Retrieval is just in time, and explore subagents return summaries of 2K tokens at most.
- **Tools:**
  - built in: read, edit, patch, glob, grep, shell, all inside the sandbox;
  - Jensen tools over the MCP bridge (`jensen/backend/src/bootstrap/bridge/`);
  - registry MCP servers;
  - `get_diagnostics` for LSP.
  - Tool output is formatted for the agent, in the SWE agent ACI style: terse, with one `ERROR` line per failure.

### 6. Token control and budget ledger
- **Hierarchical scopes:** Run, Node, Agent, Call.
- **Accounting:**
  - The ledger is append only, with `Reserve`, `Settle`, `Release` and `Adjust` entries.
  - A check and reserve walks all ancestor scopes atomically.
  - A reservation is the `count_tokens` estimate times the expected cache mix, plus the role's p90 output. It is never sized against `max_tokens`.
  - Reservations expire, so leaked holds are reclaimed.
- **Caps:**
  - Soft cap at 80%: inject a budget message, lower the effort, no escalation.
  - Hard cap: let the in flight call finish, checkpoint, end the node as `blocked` with a recorded decision.
- **In model pacing:** Anthropic task budgets (beta, advisory), with the ledger as the actual enforcer.
- **Prompt cache layering:**
  - L0: the same tool superset for every role on a model, restricted by harness permissions rather than by changing the tool list.
  - L1: constitution, conventions and repo map, with a 1h breakpoint.
  - L2: the role.
  - L3: the task.
  - Then the automatic tail.
  - Rendering is deterministic: sorted JSON, no timestamps above L3.
  - The shared prefix is pre warmed once before a fan out.
  - Every distinct model is a separate cache namespace, so fewer models means more sharing.
  - An alert fires when cache reads fall below 80% of input.
- **Price tables** are versioned config, never hard coded. Sources disagree, for example the Sonnet 5.5 cache read price.
- **Haiku 5.5:** input is capped below 100K tokens, because larger prompts cost 5x for the whole request.
- **Batch API** for every non interactive node: evals, log summaries, router training.
- **Errors:**
  - A spend cap 429 without `retry-after` is terminal and never retried.
  - Other 429s honor `retry-after`.
  - Fallback goes across providers within the same tier first, then down a tier, and the budget is checked again before each fallback.

### 6a. Provider layer, every vendor plus gateways

Added after the user flagged that the harness must support every model provider, including gateways that merge several providers behind one endpoint, with a configurable base URL and API key for each, never a hardcoded vendor list.

- **No provider is named in code.** A `Provider` is config, not a Rust type per vendor: `{id, transport, base_url, api_key_env, models: [...]}`. Users add, remove or point a provider at a different URL by editing config, never by changing the harness. This mirrors Jensen's own `no-provider-clients` / `no-hardcoded-integrations` rule, a good discipline to borrow even though it is not binding on this repo.
- **Two transports cover every vendor:**
  - **Anthropic Messages**, native. Kept as its own transport because the token control design in section 6 depends on Anthropic-specific mechanics: prompt cache breakpoints and TTLs, `output_config.effort`, task budgets, and thinking-block interop. A provider entry with `transport: anthropic` speaks this shape against its own `base_url`, so Anthropic direct, Bedrock, Vertex and any Anthropic-compatible gateway all fit here.
  - **OpenAI-compatible Chat Completions**, generic. Covers everything else behind one implementation: OpenAI itself, Gemini's OpenAI-compatible endpoint, Mistral, and the Chinese providers (DeepSeek, Qwen/Alibaba, Zhipu GLM, Moonshot Kimi, MiniMax, Baichuan, and others), each of which exposes this shape. A provider entry with `transport: openai_compatible` only needs its `base_url`, `api_key_env` and a model list; no per-vendor code.
  - A provider can instead be a **gateway** (OpenRouter, a LiteLLM proxy, Portkey, or a self-hosted router that merges several upstream vendors behind one endpoint): it is configured exactly like any other `openai_compatible` (or `anthropic`-shaped, for a gateway that proxies that shape) entry, since a gateway's whole purpose is presenting one URL and key for many underlying models. The model ladder in section 7 references `provider_id/model_id`, so a gateway's merged models are addressable the same way a direct vendor's are, and switching a role from a direct provider to a gateway is a config change, not a code change.
- **Secrets:** every provider holds `api_key_env`, the name of an environment variable, never the key itself, so config files commit safely. The harness reads the variable at startup and fails loudly if it is unset for a provider any role references.
- **Capabilities are declared per transport, not assumed:** prompt caching, extended thinking, task budgets, structured outputs and batch APIs exist only on `transport: anthropic`. An `openai_compatible` provider gets the budget ledger, effort-as-prompt-hint, and escalation ladder from section 7, but not cache-layer accounting, since most OpenAI-compatible endpoints do not expose cache token counts the way Anthropic does. Where a specific OpenAI-compatible vendor does expose them (some do, in nonstandard response fields), the ledger reads them opportunistically and never assumes they exist.
- **Routing and fallback (section 6, Errors) extends across transports, not just within one:** the per-tier, cross-provider fallback chain can mix an `anthropic` provider and several `openai_compatible` ones, including gateways, as long as each rung names a provider id and a model id the config declares.
- **Config shape (illustrative, not final):**
  ```yaml
  providers:
    anthropic: {transport: anthropic, base_url: https://api.anthropic.com, api_key_env: ANTHROPIC_API_KEY}
    openai: {transport: openai_compatible, base_url: https://api.openai.com/v1, api_key_env: OPENAI_API_KEY}
    mistral: {transport: openai_compatible, base_url: https://api.mistral.ai/v1, api_key_env: MISTRAL_API_KEY}
    gemini_compat: {transport: openai_compatible, base_url: https://generativelanguage.googleapis.com/v1beta/openai, api_key_env: GEMINI_API_KEY}
    deepseek: {transport: openai_compatible, base_url: https://api.deepseek.com, api_key_env: DEEPSEEK_API_KEY}
    openrouter_gateway: {transport: openai_compatible, base_url: https://openrouter.ai/api/v1, api_key_env: OPENROUTER_API_KEY}
  ```

### 7. Assigning models to agents
- **Declarative per role ladder** with `escalate_on` (tests failed, CI failed, reviewer rejected, max tokens, loop detected) and `no_escalate_on` (soft cap, refusal).
- **Escalation order:** raise effort first, then move to a stronger model, always gated on a verifier. Anthropic measured low effort followed by a retry at high reaching about 97% at $0.17 per task, against 95.3% at $0.29 for running everything at high. A model switch starts a fresh context from a handoff summary.
- **Default mapping:**

  | Role | Default | Escalation |
  |---|---|---|
  | classify, route, risk, stall | Laya (Jev optional) | none, low confidence goes to a deterministic fallback |
  | explore | Haiku 5.5, medium effort, returns a summary of 2K tokens at most | Sonnet 5.5 on large or unfamiliar repos |
  | plan | Opus 5.5, high effort | Fable 5.1 on high risk or failed plans |
  | implement | Sonnet 5.5, high effort | Opus 5.5 high, then Fable 5.1. A/B tested against Opus 5.5 low with a retry at high |
  | review | a different model from the implementer, Opus 5.5 high | a second opinion from another family on high risk diffs |
  | fix CI | Sonnet 5.5 high with a 1h cache TTL | Opus 5.5 after 2 red runs |

- **Telemetry per call:** ids (run, node, role, attempt, rung), model, effort, tokens (input, cache write 5m and 1h, cache read, output), cost, price version, a prefix hash per cache layer, latency, retry and fallback reasons, stop reason, the verifier outcome joined back from the node, and difficulty features. Exported over OpenTelemetry.
- **Calibration:**
  - Use a frozen internal eval of 20 to 50 tasks from our own repos with 5 trials each, plus SWE-bench Pro. SWE-bench Verified is contaminated and is not used.
  - Measure cost per solved task and the tail of the hardest decile.
  - Train a RouteLLM style router only after enough labeled outcomes exist. Offline AFlow style search over YAML variants comes later.

### 8. Command execution without approvals (sandbox, verify, promote)
- **The boundary is the sandbox, not a command classifier.** Anthropic's own auto mode classifier misses 17% of overeager actions, and allowlists matched by command name have been bypassed (the Codex `git show` escape).
- **Sandbox per writer node**, each with its own worktree:
  - **macOS:** Seatbelt with a deny first profile, in the sandbox runtime (srt) style.
    - Writable: the worktree and a tmp directory.
    - Denied: `.git*`, the gitdir pointer, hooks, git config, `.gitmodules`, rc files, `~/.ssh`, `~/.aws`, the Docker and Lima sockets, IDE task files.
    - Network: only a localhost proxy port.
  - **Linux:** bubblewrap with `--unshare-net`, a proxy over a Unix socket, seccomp, and Landlock as defense in depth (the `landlock` crate, BestEffort mode).
  - **Strong tier,** for untrusted repos or Linux native workloads: reuse Jensen's existing `jensen` Lima VM and nerdctl (`jensen/backend/src/agents/adapters/session_lifecycle/containers.rs`), with gVisor or Firecracker on Linux CI hosts.
  - Never mount the Docker socket, the main checkout or `$HOME`.
  - Resource limits: CPU, memory, PIDs, disk, wall clock, output size.
- **Network:** deny by default, split in two phases.
  - Setup phase: an allowlist proxy to package registries, checked again after DNS resolution, with private and metadata addresses blocked.
  - Agent phase: deny, or a per project allowlist.
  - Every connection is logged.
- **Secrets:** none inside the sandbox. Credentials are injected at the proxy, git goes through a proxy pinned to the node's branch, and the environment is stripped.
- **Commands:** no prompts. There is a static tripwire denylist, an append only audit log (argv, cwd, exit code, sandbox violations), and circuit breakers: 3 consecutive or 20 total violations fail the node and are recorded for review.
- **Promotion gates**, host side, fail closed, in this order:
  1. Paths. The scope, then forbidden paths: `.git*`, CI configs, hooks, build scripts, lockfiles, `package.json` scripts, `.envrc`, IDE tasks. A touched forbidden path parks the change for review instead of merging it.
  2. Host git hardening. Every host git command runs with `-c core.fsmonitor= -c core.hooksPath=/dev/null` against a verified gitdir, and the diff is computed from the object store.
  3. Screen. Secrets, entropy and size caps, reusing `jensen/backend/src/governance/domain/screen.rs`.
  4. Integration branch. The change merges into a temporary integration branch.
  5. Fresh sandbox checks. Build, test and lint run in a fresh sandbox, never the writer's, so tampered caches cannot fake a pass.
  6. Fast forward. The task branch moves only when everything passes. Otherwise the work stays on the node branch with its gate report.
- **Never automatic:** push outside the harness's branch prefix, deploys, and widening the allowlist. These are deferred to the end of run review, not prompted mid run.

### 9. Review tooling at the end of the run
- **One reviewable unit per plan,** with stacked commits per writer node and provenance trailers (node, session, model) on every commit.
- **Per node card:** the objective, scope, files, gate results with logs, budget spent, escalations and stalls.
- **Risk ordered queue:**
  - Jev hunk risk, weighted in code, plus hard flags: executable or config paths changed, new dependencies, network or credential code, deleted or skipped tests, size.
  - High risk hunks come first.
  - Forbidden path changes are labeled as never merged without a human.
- **Evidence attached:** the command audit log, the egress log, test reports, the plan with its acceptance list, and the `record_decision` deviations.
- **Grouping:** changes are grouped by meaning, with moved and copied code detected (Devin Review style), not just listed file by file.
- **Rejecting is cheap.** Nothing lands until the human accepts, and a rejection discards the integration branch.
- **The reason this matters:** AI assisted authors write less secure code with more confidence (Perry et al.). Reviewers are also more lenient toward AI written PRs.

## Fixes needed in Jensen now, found during research
1. **Probable host code execution through the `.git` pointer** (not yet confirmed).
   - A writer may edit its worktree's `.git` file and point it at a fake config with `core.fsmonitor=<cmd>`.
   - Host side `git add -A` (`jensen/backend/src/bootstrap/writer_git.rs`, `delegation.rs:667-708`) would then run the command before the scope check voids the node.
   - This is the same class as Cursor's July 2026 escape.
   - Fix:
     - deny writes to `.git*` in writer permissions (`jensen/backend/src/agents/domain/specialist/permission.rs`);
     - run host git with fsmonitor and hooks disabled;
     - verify the gitdir.
   - Prove it with a regression test first.
2. **No build or test gate between a writer commit and `merge_writers`** (`delegation.rs:389-467`). The gate pipeline from section 8 should be added.
3. **jensen-git-guard error paths let the operation through** (`return 0`, "was not checked"). They must fail closed in auto mode.

## Proposed layout of jensen-harness
One Rust crate sliced like Jensen (`domain`, `application`, `adapters`): `events`, `ingress`, `judge`, `workflow` (compiler, executor, joins, loops), `agent` (loop, context builder), `tools`, `sandbox` (seatbelt, bwrap, lima), `promote` (gates), `budget` (ledger, cache layering, prices), `router` (model ladders), `providers`, `review` (bundle builder), `acp` (the binary jensend launches).

## Delivery phases
0. **Jensen security fixes 1 to 3** above, each with a regression test.
1. **Spike.** An ACP shell (the `agent-client-protocol` crate, not hand rolled) wraps our own agent loop: the Anthropic provider, an append only loop and 5 tools in a Seatbelt sandbox. jensend launches it from a manifest (`jensen/backend/src/agents/adapters/assistant/manifest.rs`), and it completes one approved plan through the Jensen MCP tools.
2. **Durability and budget.** Step journal, resume after `kill -9` with no double charge, the hierarchical ledger, cache layering and telemetry.
3. **DAG.** The YAML compiler, channels and reducers, and the node kinds. Jensen's `feature` workflow runs end to end with no human after approval, including a clean context reviewer, the scope filter and the promotion gates.
4. **Judge and ingress.** The `Judge` trait with the Laya backend first, labeled question sets, temperature fitting and shadow comparison, then the optional Jev backend; source comment, PR and issue, then editor ingress; dedupe and trust.
5. **Fan out.** Map and join with disjoint write sets, best of N with the hybrid verifier, the integrator node.
6. **Review bundle.** Provenance, risk queue and evidence, shown in Jensen's board.
7. **Calibration.** The internal eval set, then routing ladders tuned on cost per solved task.

## Verification
- Phase 0: the `.git` pointer attack test fails before the fix and passes after it, and a hook error blocks the operation.
- Phase 1: approve a small plan in Jensen, then confirm the `record_change` entries, the `implemented` status, the commit in the task worktree, and that a write outside the worktree is denied by the sandbox.
- Phase 2: `kill -9` in the middle of a node, then restart. Completed steps are not re-run, the ledger has no double settle, and a hard cap stops the node cleanly.
- Phase 3: a fixture repo goes from an approved plan to a merge request with no prompts. A planted test deletion is flagged high risk, and a change to a forbidden path is parked.
- Phase 4: a mention arriving through `git pull` does not start a run, a double save produces one event, and the question sets meet their thresholds on labeled data with Laya, with no Jev key configured.
- Phase 5: overlapping write sets are rejected at load time, and resume after a crash during a fan out recreates the same child ids.
- Throughout: `cargo test` and `cargo clippy` in jensen-harness, and Jensen's `make check` for the Jensen side changes.

## After approval
1. Publish the full research report, covering all six studies with sources, as a shareable page.
2. Save memories:
   - execution runs with no human, with strong review at the end;
   - judgment models rather than LLMs for classification;
   - plan approval stays as the human gate.
3. Register this as a Jensen plan with `plan_template` and `plan_register`, starting with phase 0, then work in the repository the user creates.
