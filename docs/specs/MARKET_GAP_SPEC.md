# Market comparison and feature-gap spec

**Author:** market research · **For:** product and engineering · **Date:** 2026-10-04
**Branch:** `spec/market-gap-analysis` · **Status:** proposal; G1, G2 and G5 implemented in the Rust version (see [section 9](#9-implementation-status))

## 1. Why this document

Claude Agent Graph is a local app for watching Claude Code and running multi-step agent workflows. In 2026 that space is crowded: tools that run coding agents in parallel, YAML workflow engines for coding agents, general agent frameworks with visual builders, and eval/observability platforms all cover part of it. This document:

1. Places our product against the main alternatives (section 2).
2. Lists what we do that others don't (section 3).
3. Lists the gaps that come up most often, ranked (section 4).
4. Specifies each gap so it can be built (section 5) and proposes an order (section 6).

All specs apply to the app in `src/`. Every YAML key added here must also be accepted by `agent-graph validate`.

## 2. Competitive landscape

### 2.1 Who we compared against

| Category | Products | What users get from them |
|---|---|---|
| **Parallel coding-agent managers** | Conductor (macOS app), Claude Squad (open-source TUI), Vibe Kanban (open source, now community-maintained), Sculptor (container-based), Superset, Nimbalyst (successor to Crystal) | Many Claude Code or Codex agents at once, each in its own git worktree or container, a diff review screen, and one-click PR creation. |
| **Workflow engines for coding agents** | Archon (open source, YAML DAGs), GitHub Agentic Workflows (public preview since June 2026), Kiro (spec-driven IDE with agent hooks) | Repeatable development processes as files in the repo, run on triggers (issues, PRs, schedules, chat commands), with isolation and safe write permissions. |
| **General agent frameworks and builders** | LangGraph + LangGraph Studio, CrewAI, OpenAI Agents SDK / Agent Builder, n8n, Dify, Langflow, Flowise | Graphs with typed state, conditional edges and checkpoints; human-in-the-loop; hundreds of integrations; triggers; workflows generated from a description; publishing a workflow as an API. |
| **Eval and observability** | Langfuse (now part of ClickHouse), LangSmith, Braintrust | Traces, datasets, offline experiments, baseline comparison to catch regressions, LLM-as-judge scoring online and offline. |
| **Cloud coding agents** | GitHub Copilot coding agent, OpenAI Codex cloud, Google Jules, Devin | Assign an issue, get a PR back. Runs in a cloud sandbox, so your machine doesn't need to stay on. |

### 2.2 Feature matrix

✅ = has it · ◐ = partial · — = missing. Competitor cells are a summary of public docs and reviews as of October 2026, not a test of each product.

| Capability | **Agent Graph** | Conductor / Claude Squad / Vibe Kanban | Archon | GitHub Agentic Workflows | LangGraph (+Studio) | n8n / Dify | Langfuse / LangSmith |
|---|---|---|---|---|---|---|---|
| Live view of every local Claude Code session and subagent | ✅ | ◐ (only sessions it started) | — | — | — | — | ◐ (traces, not live) |
| Visual workflow builder | ✅ | — | ◐ | — (Markdown) | ✅ (Studio) | ✅ | — |
| Workflows as files in the repo | ✅ YAML | — | ✅ YAML | ✅ Markdown | code | ◐ (export JSON) | — |
| Human review pause, steering mid-step | ✅ | ◐ (chat with agent) | ◐ | — | ✅ (interrupts) | ✅ | — |
| LLM judge on real evidence (diff) | ✅ | — | ◐ (validation step) | — | DIY | DIY | ✅ (on traces) |
| Step checkpoints with diff + rewind | ✅ | ◐ (per-agent diff) | — | — | ✅ (state time-travel) | — | — |
| Multi-provider (Claude, Gemini, GPT, local) | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | n/a |
| **Isolated workspace per run (worktree / container)** | — | ✅ | ✅ | ✅ (Actions runner) | n/a | n/a | n/a |
| **Triggers: schedule, webhook, GitHub event, file change** | — (external cron only) | — | ✅ | ✅ | ◐ (Platform cron) | ✅ | n/a |
| **Typed run inputs / parameters** | — | n/a | ✅ | ✅ | ✅ (state schema) | ✅ | n/a |
| **Structured step outputs passed by field** | — (free text only) | — | ◐ | ✅ (safe outputs) | ✅ | ✅ | n/a |
| **Conditional branching on output** | ◐ (success / failure only) | — | ✅ | ◐ | ✅ | ✅ | n/a |
| **Fan-out over a list (map)** | — | ◐ (manual parallel agents) | ◐ | — | ✅ (Send API) | ✅ (loop node) | n/a |
| **Branch / commit / PR delivery** | — | ✅ | ✅ | ✅ | n/a | ◐ (GitHub node) | n/a |
| **Chat / remote notifications and approvals** | ◐ (desktop notification only) | ◐ | ✅ (Slack, Telegram, Discord) | ✅ (comments) | ◐ | ✅ | ◐ |
| **Eval datasets and regression comparison** | — | — | — | — | ◐ (via LangSmith) | ◐ | ✅ |
| **Trace export (OpenTelemetry)** | — | — | — | ◐ | ✅ | ◐ | ✅ (ingest) |
| **Auto-resume after crash / reboot** | — (run marked stopped; manual replay) | ◐ | ◐ | ✅ | ✅ (durable checkpointer) | ✅ | n/a |
| **Sub-workflows** | — | — | ✅ | ◐ | ✅ (subgraphs) | ✅ | n/a |
| **Generate a workflow from a description** | ◐ (rewrite one prompt) | — | ◐ | ✅ (natural-language Markdown) | — | ✅ | n/a |
| **MCP: configure per workflow / expose as server** | — | — | ◐ | ✅ (tools config) | ✅ | ✅ | ◐ |
| **Shared workflow library / marketplace** | ◐ (built-in samples) | — | ✅ | ◐ (shared via repos) | — | ✅ (templates) | — |
| **Cost / quality analytics over time** | ◐ (per run) | ◐ | — | ◐ | ◐ | ◐ | ✅ |

## 3. Where we win (keep and lean on)

1. **Watching everything Claude Code is already doing.** No competitor shows sessions it didn't start. This is our entry point: people install it to see, then stay to build.
2. **Evidence-based AI judge.** It grades the real diff against plain-language criteria and feeds its reasons into the retry. Others need custom code or a separate eval platform for this.
3. **Checkpoints with rewind and undo**, using a private git index that never touches the user's branch.
4. **Steering a running step** without restarting it (for Claude steps).
5. **Local-first, no account, MIT.** Free shell steps and Ollama support make a $0 run possible.
6. **One file format for the app, the CLI and CI** (`agent-graph run/validate`).

New features must not weaken these. In particular, isolation (G1) has to keep diffs, rewind and the live graph working.

## 4. Gaps, ranked

Scored on how often the gap comes up in reviews and competitor comparisons (demand), how much it limits current users (pain), and effort (S ≤ 1 week, M 1–3 weeks, L > 3 weeks, for one engineer on both implementations).

| ID | Gap | Demand | Pain | Effort | Value rank | Priority | Status |
|---|---|---|---|---|---|---|---|
| G1 | Isolated workspace per run (git worktree, optional container) | High | High: parallel steps and runs share one folder | M | 2 | **P0** | ✅ Rust (worktree; containers not yet) |
| G2 | Workflow inputs and templating | High | High: every run of "Fix a bug" means editing the prompt | S | 1 | **P0** | ✅ Rust |
| G3 | Structured outputs and conditional routing | High | Medium | M | 8 | **P2** | Not started |
| G4 | Triggers: schedule, file watch, webhook, GitHub | High | Medium | M | 6 | **P1** | Not started |
| G5 | Git delivery: branch, commit, pull request | High | Medium | S | 3 | **P0** | ✅ Rust |
| G6 | Durable runs: auto-resume after crash or reboot | Medium | High for long or overnight runs | M | 5 | **P1** | Not started |
| G7 | Guardrails: step budgets, limits, protected paths, secret redaction | Medium | Medium | M | 4 | **P1** | Not started |
| G8 | Fan-out over a list (`forEach`) | Medium | Medium | M | 12 | **P3** | Not started |
| G9 | Outbound notifications and remote approvals (Slack, Discord, webhook) | Medium | Medium | M | 9 | **P2** | Not started |
| G10 | Eval suites: run a workflow over a dataset, compare to a baseline | Medium | Low today, high for teams | L | 7 | **P1** | Not started |
| G11 | MCP: per-workflow servers, and Agent Graph as an MCP server | Medium | Low | M | 11 | **P2** | Not started |
| G12 | Sub-workflows (call a workflow as a step) | Medium | Low | S | 14 | **P3** | Not started |
| G13 | Generate a workflow from a description | Medium | Low (adoption) | S | 10 | **P2** | Not started |
| G14 | Trace export via OpenTelemetry | Low–Medium | Low | S | 15 | **P3** | Not started |
| G15 | Analytics: cost, pass rate and flaky steps over time | Low–Medium | Low | S | 13 | **P3** | Not started |
| G16 | Import / share workflows from a URL or git repo | Low–Medium | Low | S | 16 | **P3** | Not started |

Priority follows the value ranking in 4.1: ranks 1–3 are P0, 4–7 P1, 8–11 P2 and 12–16 P3.

**Not planned:** hosted cloud execution, multi-user accounts, and a hosted marketplace. They would conflict with "local-first, no account" and with the security model (localhost only, random token). G4's webhook listener and G9's remote approvals are opt-in and specified so the default stays local.

### 4.1 Value ranking

"Value" is user value for the effort: how many runs the feature improves, whether it builds the trust needed to run agents unattended, whether other features depend on it, and how much it sets us apart from competitors (not just catches up). This ranking sets the priorities above and the roadmap in section 6.

| Rank | Feature | Value | Effort | Why it ranks here |
|---|---|---|---|---|
| 1 | **G2: Run-time inputs** | ★★★★★ | S | Every run of every workflow benefits right away: no more editing prompts before running. The cheapest big win, and triggers, evals and PR titles need it. |
| 2 | **G1: Separate worktree per run** | ★★★★★ | M | The foundation. Without it, unattended, parallel or scheduled runs aren't safe. It also makes our step diffs exact, which strengthens the judge and rewind. |
| 3 | **G5: Branch, commit, PR** | ★★★★★ | S (after G1) | Finishes the job: users get a PR, not a changed folder. Competitors put this front and centre, and it's small once G1 exists. |
| 4 | **G7: Guardrails** | ★★★★☆ | M | Builds trust. People won't leave agents running overnight without spend caps and "don't touch `.env`". |
| 5 | **G6: Resume after a crash** | ★★★★☆ | M | Long runs lost to a closed laptop or a crash are a sharp pain point, and unattended runs need this. |
| 6 | **G4: Triggers** | ★★★★☆ | M | Turns the app from a tool you drive into automation that works without you, level with Archon and GitHub Agentic Workflows. It ranks below 1–5 because it's only safe once those exist. |
| 7 | **G10: Eval suites and baseline comparison** | ★★★★☆ | L | Our strongest possible differentiator: the AI judge plus worktrees answers "is Haiku good enough for this workflow?" with real numbers. High value for teams, but expensive. |
| 8 | **G3: Structured outputs and branching** | ★★★☆☆ | M | Powerful, but mostly for advanced users. Most coding workflows are straight lines with checks, which we already handle. |
| 9 | **G9: Slack and webhook notifications** | ★★★☆☆ | M | Becomes important once triggers exist. On its own, desktop notifications cover a local user. |
| 10 | **G13: Describe a workflow, get it built** | ★★★☆☆ | S | Cheap, and it helps new users get started (n8n and Agent Builder market this heavily). It doesn't help existing users much. |
| 11 | **G11: Agent Graph as an MCP server** | ★★★☆☆ | M | "Claude, run my release workflow" from any chat, with the run showing in the live graph. A neat fit with our live-view strength. |
| 12 | **G8: Run a step once per item in a list** | ★★☆☆☆ | M | Useful for batch work (tests for each file, triage each issue), but a niche. |
| 13 | **G15: Cost and quality trends** | ★★☆☆☆ | S | Nice to have. The per-run numbers already cover most needs. |
| 14 | **G12: Workflows that call other workflows** | ★★☆☆☆ | S | Matters only once users have many workflows. |
| 15 | **G14: Trace export (OpenTelemetry)** | ★☆☆☆☆ | S | Only for teams that already run Langfuse, LangSmith or Grafana. |
| 16 | **G16: Import workflows from a URL** | ★☆☆☆☆ | S | Copying a YAML file already works. |

How this differs from ranking by demand alone:

- **Guardrails (G7) and resume (G6) move up.** Trust and reliability decide whether anyone runs agents unattended, so they matter more than routing.
- **Evals (G10) move up to #7.** No competitor combines an evidence-based judge with isolated runs, so this is where we can stand out instead of catching up.
- **Branching (G3) moves down.** It's impressive in a demo, but fewer real coding workflows need it.

## 5. Specs

Conventions for every spec:

- New YAML keys are optional, and a workflow without them behaves exactly as it does today.
- `validate` gains errors and warnings for each new key. Unknown-key warnings keep their "did you mean" hint.
- New keys appear in the builder under **Advanced options** unless stated otherwise, following the UX review (plain labels, no raw identifiers).
- Every feature has CLI parity and a `--json` form where it produces output.

---

### G1. Isolated workspace per run

**Status:** ✅ Rust, worktree only. Not yet: containers, per-step worktrees, Python. Details in section 9.

**Problem.** Steps and runs share the project folder. Two runs on the same project collide, parallel steps see each other's half-finished edits, and the README has to warn that a step's diff can include another step's changes. Conductor, Claude Squad, Vibe Kanban and Archon all default to a git worktree per agent or run. Sculptor uses containers.

**YAML**

```yaml
isolation: worktree        # none (default today) | worktree | container
worktree:
  base: HEAD               # branch or commit to start from
  keep: onFailure          # always | onFailure | never — when to delete the worktree after the run
  setup: npm ci            # optional shell command run once in the new worktree
container:                 # only for isolation: container
  image: node:22
  mounts: []               # extra read-only mounts
  network: true
```

**Behavior**

- `worktree`: at run start, `git worktree add <runs>/<run-id>/wt <base> --detach`. All steps run there. Untracked files the user lists in `worktree.copy:` (for example `.env`) are copied in.
- If the project isn't a git repo, `worktree` fails validation with a clear message ("This folder isn't a git repository, so it can't run in a separate copy").
- Parallel steps inside a run still share the run's worktree. Optional `step.isolation: worktree` gives one step its own child worktree, merged back with `git merge --no-ff` after it succeeds. A merge conflict fails the step, and the reason lists the conflicting files.
- Checkpoints (`quality.rs`) take snapshots from the worktree. Rewind acts on the worktree. Per-step diffs become exact, because nothing else writes there.
- The watcher maps a worktree path back to its project so sessions stay grouped under the right project in the live graph.
- At the end of the run, the run view shows **Apply to my folder** (cherry-pick the run's net diff onto the user's working tree, which must be clean or the action refuses), **Create branch** (see G5) and **Discard**.
- `container`: steps run as `docker run` (or `podman`) with the worktree mounted at `/work`, and the Claude CLI and auth are mounted read-only. This is a phase-2 option; ship worktree first.

**CLI:** `agent-graph run flow.yaml --isolation worktree`, and `agent-graph clean` to remove worktrees from finished runs.

**Acceptance**

- Two runs of the same workflow on the same project at the same time finish without touching each other's files or the user's checkout.
- A step's diff never contains files changed by a parallel step that has `isolation: worktree`.
- With `keep: never`, `git worktree list` shows no leftovers after the run.

---

### G2. Workflow inputs and templating

**Status:** ✅ Rust. Not yet: `env.<NAME>` and `steps.<id>.json` (that one needs G3), Python. Details in section 9.

**Problem.** Samples contain `<describe …>` and `{task}` placeholders that the user must edit before each run, and `validate` warns about them. Archon, n8n, LangGraph and GitHub Agentic Workflows all take typed inputs at run time.

**YAML**

```yaml
inputs:
  bug:
    description: What's going wrong?
    type: text             # string | text (multi-line) | number | boolean | choice | file
    required: true
  severity:
    type: choice
    options: [low, high]
    default: low
steps:
  - id: find
    prompt: |
      Find the cause of this bug: {{ inputs.bug }}
      Severity: {{ inputs.severity }}
```

**Templating.** Use `{{ … }}` with a deliberately small grammar: `inputs.<name>`, `steps.<id>.output` (text), `steps.<id>.json.<path>` (see G3), `run.id`, `run.folder`, `env.<NAME>` (only variables listed in `envAllow:`), and the filters `| default("x")` and `| json`. Implement it as our own short parser rather than Jinja, so Python and Rust render exactly the same. An unknown reference is a validation error.

**UI.** **▶ Run** opens a short form generated from `inputs` (plain labels, with each `description` as help text). The last values are remembered per workflow. The run header shows the values used.

**CLI:** `agent-graph run "Fix a bug" --input bug="Login fails on Safari" --input severity=high`, or `--inputs inputs.json`. A missing required input in a non-interactive run exits with code `1` and lists what's missing.

**Migration.** The samples move to `inputs`. The `{task}` placeholder warning remains for old files.

**Acceptance:** the "Fix a bug" sample runs from the CLI without editing YAML, and replaying a run reuses its original input values.

---

### G3. Structured outputs and conditional routing

**Problem.** Steps pass free text, and routing only knows success and failure. You can't say "if the triage step says it's a docs issue, go to the docs step". LangGraph conditional edges, n8n IF/Switch nodes and Archon all support this.

**YAML**

```yaml
- id: triage
  prompt: Classify this issue: {{ inputs.issue }}
  output:
    schema:                # JSON Schema; the step must produce a matching object
      type: object
      required: [kind, confidence]
      properties:
        kind: { enum: [bug, docs, feature] }
        confidence: { type: number }
- id: route
  switch:                  # a routing step: no AI, no cost
    - when: steps.triage.json.kind == "docs"
      goto: write-docs
    - when: steps.triage.json.confidence < 0.6
      goto: ask-human
    - default: fix
```

**Behavior**

- Claude steps with `output.schema` run with `--output-format json` plus a schema instruction added to the prompt. The result is validated, and a mismatch counts as a failure, so it feeds the retry (the same loop the judge uses).
- Gemini and GPT steps get the schema as an instruction, and the last JSON block in their output is parsed.
- Shell steps can set `output: { from: stdout, format: json }`.
- Expressions: `==, !=, <, <=, >, >=, and, or, not, in`, literals, and references from G2. Same small parser in both implementations, no `eval`.
- A step that isn't taken is shown as **Skipped (route)** in the run view. Its dependents are skipped too, unless they also depend on a step that ran (any-of joins: `dependsOn: { any: [a, b] }`).
- Builder: the `switch` step draws one labeled edge per branch.

**Acceptance:** a three-way triage sample routes correctly in tests that use stubbed step outputs. `validate` rejects a `goto` to a missing step and a `when` that references an unknown field in the schema.

---

### G4. Triggers

**Problem.** Workflows only start by hand or from external cron (the README even documents a crontab line). Archon, GitHub Agentic Workflows, n8n and Kiro hooks start work on events.

**YAML**

```yaml
on:
  schedule: "0 2 * * 1-5"          # cron, local time
  files:                           # Kiro-style hook
    paths: ["src/**/*.ts"]
    debounce: 30s
  webhook: true                    # POST /hooks/<workflow-id> with a per-workflow secret
  github:
    repo: owner/name
    events: [issues.labeled]       # issues.opened, issues.labeled, pull_request.opened, issue_comment.created
    filter: label == "agent"       # G3 expression over the event payload
    inputs:                        # map event fields to G2 inputs
      bug: "{{ event.issue.title }}\n\n{{ event.issue.body }}"
```

**Behavior**

- A **trigger daemon** runs inside the app, and also headless as `agent-graph serve --triggers` for machines without a window. It is registered with systemd (user unit), launchd or Windows Task Scheduler by `agent-graph triggers install`.
- `schedule` uses the cron grammar of the `croniter` / `cron` crates. Missed runs (machine asleep) run once on wake unless `catchUp: false`.
- `files` watches with `watchdog` / `notify`. It ignores changes made by our own runs, which are tracked by worktree path or by a run lock, so a run can't trigger itself in a loop.
- `webhook` stays off by default. When on, it listens on 127.0.0.1 only and needs an `X-Agent-Graph-Signature` HMAC. Exposing it to the internet is the user's choice (tunnel), and the docs say so.
- `github` polls the REST API with the user's `gh` auth (default every 60 s, ETag-cached), so no public endpoint is needed. Webhook mode is optional.
- A per-workflow concurrency limit (`concurrency: 1` by default) queues or drops extra events (`onBusy: queue | skip`).
- New **Triggers** page: every trigger, its next fire time and its last 20 firings, with a pause switch for each.

**Acceptance:** a schedule fires within 5 s of its time, a file trigger doesn't re-fire from its own run's edits, and a labeled issue in a test repo starts a run whose inputs hold the issue text.

---

### G5. Git delivery: branch, commit, PR

**Status:** ✅ Rust. Not yet: delivery without a worktree, the "Open pull request" step template, GitLab, Python. Details in section 9.

**Problem.** The last mile in every competitor is "here is a PR". Today the user commits by hand.

**YAML**

```yaml
deliver:
  branch: "agent/{{ run.id }}-{{ inputs.bug | slug }}"
  commit: perStep            # perStep | squash
  pr:
    draft: true
    title: "{{ steps.fix.json.title | default(inputs.bug) }}"
    body: summary            # summary = auto: steps, judge verdicts, cost, run link
  when: success              # success | always
```

**Behavior.** Works with G1: the branch is created from the run's worktree. Without isolation, the action refuses if the user's working tree has unrelated changes. `perStep` makes one commit per step, with the step name as the subject, which lines up with checkpoints. The PR is opened through `gh pr create`, and the run links to it. A built-in step template, **Open pull request**, does the same thing for people who don't want to write YAML.

**Acceptance:** the "Fix a bug" sample with `deliver` opens a draft PR whose body lists each step and the judge score, and the branch holds exactly the run's diff.

---

### G6. Durable runs

**Problem.** If the app or terminal process exits mid-run, `sync_disk` marks the run **stopped**, and the user has to replay steps by hand. LangGraph checkpointers, GitHub Actions and n8n resume on their own.

**Behavior**

- The run file already stores per-step attempts. Add a `resumeFrom` record: the completed steps with their outputs, loop counters and pending reviews.
- On startup, a run whose process is gone and whose status is `running` becomes **Interrupted**, not stopped. The run view and the home page show **Resume**, which continues from the first step that hadn't finished. A step that was mid-flight restarts. For Claude steps it uses `--resume <session>` with a short "you were interrupted, continue" message, reusing the steering prompt in `workflows.py:943`.
- A workflow-level `autoResume: true` resumes without asking, for trigger-started runs (G4).
- CLI: `agent-graph resume <run-id>`.

**Acceptance:** killing the process during step 2 of 4 and then running `resume` finishes the run with step 1 not re-executed and its cost not counted twice.

---

### G7. Guardrails

**Problem.** There is only a workflow-wide `maxBudgetUsd`. Competitors offer per-step limits (LangGraph recursion limits, the Agents SDK's guardrails, GitHub's safe outputs and read-only default).

**YAML (workflow or step level; the step overrides the workflow)**

```yaml
limits:
  budgetUsd: 0.50
  maxTurns: 40
  timeout: 20m
protect:                       # paths the step must not change
  - ".github/**"
  - "**/*.lock"
  - ".env*"
redact: true                   # mask secrets in logs, run files and judge input
```

**Behavior.** `maxTurns` and the budget map to `claude -p` flags where they exist, and are otherwise enforced by the runner, which stops the process and fails the step with "went over its limit". `protect` is checked against the step's checkpoint diff after the step. A violation fails the step and offers **Rewind this step**. For Claude steps it's also added to `disallowedTools` as `Edit(<glob>)` / `Write(<glob>)` rules, so it's enforced up front too. `redact` uses a small built-in pattern list (API keys, tokens, private keys), plus `redactPatterns:`. The builder shows a cost forecast per step from the median of that step's past runs.

**Acceptance:** a step that edits `.env` fails even if Claude claims success, and a step with `maxTurns: 3` stops at 3 turns.

---

### G8. Fan-out over a list

**YAML**

```yaml
- id: fix-each
  forEach: "{{ steps.find.json.files }}"   # or a literal list, or a shell command's stdout lines
  as: file
  concurrency: 3
  prompt: Add missing tests for {{ item.file }}
  collect: list                            # list | concat
```

**Behavior.** Each item is a child attempt shown as a stacked node in the graph and listed in the run view. Failures obey `onFailure` per item, and `continueOnItemFailure: true` lets the rest finish. The output is the list of child outputs (`steps.fix-each.json` is an array). It combines with `step.isolation: worktree` so items don't collide. There's a hard cap of 100 items unless `maxItems` is raised, so a bad list can't spawn hundreds of agents.

---

### G9. Outbound notifications and remote approvals

**YAML (workflow level)**

```yaml
notify:
  - on: [review, failure, success]
    slack: { webhook: "{{ secret.SLACK_HOOK }}" }
  - on: [review]
    webhook: { url: "https://…", secret: "{{ secret.HOOK_KEY }}" }
```

**Behavior.** Secrets are read from the OS keychain (`keyring` / `keyring-rs`), never from YAML. Messages carry the step name, a short result and the judge verdict. **Remote approval** is opt-in: a review message includes approve and request-changes links to a short-lived, single-use signed URL. The links only work if the user has deliberately exposed the app (tunnel) or uses the Slack interactive-button relay. The default stays local, and the docs explain the trade-off. Discord and Telegram use the generic webhook format.

---

### G10. Eval suites and regression comparison

**Problem.** The judge grades a single run. Teams want to know whether a prompt change, a model swap (Haiku → Sonnet → Ollama) or a new step makes the workflow better across many tasks. That is the core of Langfuse and LangSmith experiments.

**Files**

```yaml
# .claude/workflows/evals/fix-a-bug.eval.yaml
workflow: fix-a-bug
fixtures:                      # each case runs in its own worktree from this commit
  - name: safari-login
    ref: eval/safari-login     # branch or commit with the planted bug
    inputs: { bug: "Login fails on Safari" }
    expect:
      check: npm test -- login # must exit 0
      judge: The fix touches only auth code
variants:                      # optional: compare configurations
  - { name: haiku, model: haiku }
  - { name: sonnet, model: sonnet }
repeat: 3                      # runs per case and variant, to measure flakiness
```

**Behavior.** `agent-graph eval fix-a-bug.eval.yaml` runs the matrix (G1 isolation is required), then reports pass rate, mean judge score, cost and duration per variant, plus per-case flakiness. Results are saved to `runs/evals/`. `--baseline <eval-run-id>` marks regressions in red, and `--fail-on-regression` makes it usable in CI. An **Evals** page shows the comparison table. **Save as eval case** on any finished run captures its start commit and inputs as a new fixture, which is how LangSmith turns real failures into tests.

---

### G11. MCP

1. **Per-workflow MCP servers:** an `mcpServers:` block in the same shape as Claude Code's `.mcp.json`, passed to `claude -p --mcp-config`, with an allow-list per step (`step.mcp: [github]`).
2. **Agent Graph as an MCP server** (`agent-graph mcp`, stdio): the tools `list_workflows`, `run_workflow(name, inputs)`, `run_status(id)` and `approve_review(id, feedback)`. Claude Code (or any MCP client) can then start and supervise workflows from a chat, and the run shows up in the live graph as a child of the calling session.

---

### G12. Sub-workflows

`- id: release`, `workflow: release-checklist`, `inputs: {…}`. The step runs the other workflow as a child run, linked in the run view, with its final output (or JSON) as the step output. `validate` detects cycles across workflows. Budgets add up into the parent's limits (G7).

---

### G13. Generate a workflow from a description

The builder gets a **Describe it** box ("Every night, update dependencies, run the tests, and open a PR if they pass"). One Claude call is given the workflow JSON schema, the step templates and the agent library, and returns YAML. The result goes through `validate` before it's shown, any validation errors are sent back for a single repair attempt, and the draft opens in the builder unsaved. This reuses the existing `rewrite_text` call path.

---

### G14. Trace export

Optional `otel:` app setting (OTLP endpoint and headers). Each run is a trace, each step attempt a span, and each Claude turn a child span, using the OpenTelemetry GenAI semantic conventions (model, input/output/cache tokens, cost). The judge verdict is attached as a span event, so a run can be viewed in Langfuse, LangSmith, Jaeger or Grafana. Off by default.

---

### G15. Analytics

An **Insights** page built from run files only (no new storage): cost per workflow per week, success rate, mean judge score, the steps that fail or retry most, and the cache hit rate trend. It links each step to its worst recent runs, and is also available as `agent-graph stats --json`.

---

### G16. Import and share workflows

`agent-graph add https://github.com/org/team-flows/blob/main/review.yaml` (or a git repo and path). The workflow is copied into the project with an `origin:` field, and **Check for updates** shows a diff before replacing it. Imported workflows are validated and shown with a "from <origin>" badge, and they never run on import. No hosted marketplace (see section 4).

## 6. Proposed roadmap

| Release | Contents | Theme |
|---|---|---|
| **0.next (6 weeks)** | G2 inputs · G1 worktree isolation · G5 delivery · G7 guardrails | Start it with your inputs, walk away, come back to a safe, budget-capped PR |
| **+1 (6 weeks)** | G6 durable runs · G4 triggers · G9 notifications · G13 describe-to-workflow | Runs by itself, overnight, on events |
| **+2 (8 weeks)** | G10 evals · G3 outputs and routing · G11 MCP | Measure quality, handle advanced flows |
| **Later** | G8 forEach · G15 insights · G12 sub-workflows · G14 OTel · G16 import · G1 containers | Scale and fit into the wider tool chain |

Ordering rationale: the releases follow the value ranking in 4.1. G2 is small and removes the most friction. G1 is a precondition for G4, G5, G8 and G10, because unattended and parallel runs aren't safe in a shared folder. G5 and G7 make the first release story whole. G6 ships before or with G4, because triggered runs happen when nobody is watching.

Each release adds to a fixture set of workflow YAML files with expected `validate` output and stubbed-run traces that the test suite loads.

## 7. Success metrics

- Share of runs started without editing YAML (G2) above 80%.
- Share of runs with `isolation: worktree` above 50% within two releases of G1.
- Runs started by triggers make up over 30% of all runs among users who define one.
- Runs that end in a PR or **Apply to my folder**, as a share of successful runs (the "useful output" rate).
- Judge pass rate per workflow version, tracked by G10, used to stop prompt or model changes that make results worse.

## 8. Open questions

1. Container isolation: Docker only, or Podman as well? How do we pass Claude auth into the container safely on macOS (keychain)?
2. GitHub triggers: polling with `gh` auth is simple and local. Is the delay of up to 60 s acceptable, or do we ship an optional webhook relay?
3. Should `deliver` support GitLab and Bitbucket (`glab`) from the start?
4. Remote approval (G9) adds attack surface. Is a Slack-only relay enough, so we never expose the app's own HTTP server?
5. Do eval fixtures (G10) live as branches in the user's repo, or as patch files under `.claude/workflows/evals/`?

## 9. Implementation status

Updated 2026-10-04. Branch `rust-inputs-worktree-delivery` (based on `spec/market-gap-analysis`).

| ID | Status | Where |
|---|---|---|
| G2 Inputs and templating | ✅ Done in Rust | `src/template.rs`, validation in `src/workflows.rs`, runner, CLI, run form in `index.html` |
| G1 Separate worktree per run | ✅ Done in Rust (worktree) | `src/worktree.rs`, `src/runner.rs`, run view in `index.html` |
| G5 Branch, commit, PR | ✅ Done in Rust | `src/worktree.rs` (commits, branch, push, `gh pr create`), `src/runner.rs` |
| Everything else (G3, G4, G6–G16) | Not started | |
| Python version of G1, G2, G5 | Not started | The Python app warns about the new keys as unknown settings and ignores them |

### G2: inputs and templating

Done:
- `inputs:` with the types string, text, number, boolean and choice, plus `required`, `default` and `options`. The shorthand `name: description` makes a required input.
- `{{ … }}` templates in `prompt`, `run`, `check`, `judge` and every `deliver` text. They can use `inputs.*`, `steps.<id>.output`, `run.id`, `run.folder`, `run.project` and `run.workflow`, with the filters `default("…")`, `slug`, `trim` and `json`. Quoted literals work, so `{{ "{{" }}` writes a literal `{{`.
- Validation errors for references to unknown inputs or steps, unknown filters and unclosed `{{`. Lint warnings for unused inputs and for unknown keys inside `inputs`, `worktree` and `deliver`.
- **Beyond the spec:** in `run:` and `check:`, every value is single-quoted for the shell, so an input can't inject commands. Shell steps also get each input as `$INPUT_<NAME>`.
- Run form in the app, prefilled with the last values used (stored per workflow in the browser). **Run again** reuses the run's values. The run view lists the inputs.
- CLI: `--input NAME=VALUE` (repeatable) and `--inputs file.json`. In a terminal, missing required inputs are asked for. Without a terminal, the run exits with code `1` and names what's missing.
- Replaying a step reuses the run's input values.

Not done: `env.<NAME>` with `envAllow:`, `steps.<id>.json.*` (needs G3's structured outputs), changing the samples to use `inputs` (they keep their `{task}` question), Python.

### G1: separate worktree per run

Done:
- `isolation: worktree` with `worktree: {base, keep, setup, copy}`. Worktrees are created in `~/.config/claude-agent-graph/worktrees/<run id>`, outside the project, so they never appear as untracked files there. A workflow in a subfolder of its repository runs in the same subfolder of the worktree.
- Validation fails if the folder isn't a git repository or the base doesn't exist (for example, a repository with no commits).
- `copy` files and the `setup` command are excluded from the run's changes: the base snapshot is taken after both.
- Step checkpoints, diffs, rewind and the judge all work in the worktree, so each step's diff is exact. Rewind, undoing a rewind and replay bring back a removed worktree with the run's files.
- `keep: always | onFailure | never`. The final files are kept as a git tree, so **Apply**, **Create branch** and **Open pull request** still work after the worktree is gone.
- Run view: **Apply to my folder**, **↶ Undo apply**, **Create / Update branch**, **Open pull request** and **Discard copy**. API: `/api/runs/apply`, `/api/runs/unapply`, `/api/runs/deliver` and `/api/runs/discard`.
- **Different from the spec:** Apply doesn't require a clean working tree. It applies the run's patch atomically (all or nothing, with a clear message if it doesn't apply) and can be undone.
- Worktree folders are never counted as projects, so their copy of `.claude/workflows` doesn't create duplicate workflows.
- CLI: `--isolation worktree|none` for one run, `agent-graph clean`, and a note in the output when a copy is kept.

Not done: `isolation: container` (validation says it's not supported yet), per-step `isolation: worktree` with merge back, showing worktree sessions under their project in the live graph (they show under the worktree path), an automated test of two runs at once, Python.

### G5: branch, commit, PR

Done:
- `deliver: true`, or a mapping with `{branch, commit: perStep | squash, message, push, pr: {draft, title, body, base}, when: success | always}`.
- Commits are built from the step checkpoints with a private index and `commit-tree`, on top of the base commit. The user's checkout, HEAD and index are never touched.
- `perStep` makes one commit per successful step, in the order the steps finished, skipping steps that changed nothing, plus a final commit for anything changed after the last step. `squash` makes a single commit.
- A branch name that's taken gets `-2`, `-3` and so on. Delivering again after a replay moves the same branch (pushed with `--force-with-lease`) and keeps the existing PR.
- `push` (implied by `pr`) and `gh pr create`. The `summary` body lists the inputs, each step's result, its judge score and its cost.
- Without a git identity, commits are made as "Claude Agent Graph <agent-graph@localhost>".
- Delivery errors are recorded on the run and shown in the run view and the CLI (exit code `1`). The run itself still counts as succeeded.

Not done:
- **Different from the spec:** `deliver` requires `isolation: worktree`. Delivering from the user's own folder isn't supported.
- The **Open pull request** step template, GitLab or Bitbucket, Python.

### Testing

- Unit tests: `cargo test` (9 pass). They cover template rendering, shell quoting, reference checks, input coercion, and a worktree test against a real git repository: copy and setup are excluded, the commit chain skips no-op steps, branch names don't collide, apply and undo work, and the worktree is removed and recreated.
- End-to-end, using shell steps only (no AI cost), with an isolated `HOME`:
  - CLI runs with inputs.
  - An input containing a shell injection attempt, which was quoted safely.
  - A branch with per-step commits that doesn't contain `.env`.
  - A failed run with `when: always` and `squash`, where the worktree was kept and `clean` removed it.
  - `--isolation none`.
  - Pushing to a bare `origin`.
  - The app API: start without a required input (refused), start with inputs, apply, apply twice (refused), unapply, deliver with a PR and no `origin` (clear error), discard.
- Not tested: opening a real GitHub PR (no GitHub remote in the test environment), and the new UI in a browser. The page's JavaScript passes a syntax check.

### Also shipped (not in this spec)

- **Map filters** in a bar at the top-right of the live map: **Live sessions** (open Claude sessions and their helpers) and **Running workflows** (running or review-waiting workflows with their steps and helpers) are on by default, and **Older** adds closed sessions and finished workflows from a time range (last hour, today, this week, any time) that appears while it's on. Any mix works, and the choice is remembered. This replaces the "Show activity from" box in the side panel. Checked in a headless browser with real data.

## Sources

- [The Best Tools to Run Multiple Coding Agents in 2026 (AgentsRoom)](https://agentsroom.dev/blog/best-multi-agent-coding-tools)
- [Conductor alternative (Superset)](https://superset.sh/compare/conductor-alternative)
- [Archon on GitHub](https://github.com/coleam00/archon) · [Archon guide (Better Stack)](https://betterstack.com/community/guides/ai/archon-ai/) · [Archon Workflow Marketplace](https://www.contextstudios.ai/blog/archon-workflow-marketplace-deterministisches-ki-coding-im-grossen-massstab)
- [GitHub Agentic Workflows technical preview (GitHub changelog)](https://github.blog/changelog/2026-02-13-github-agentic-workflows-are-now-in-technical-preview/) · [Getting started (KDnuggets)](https://www.kdnuggets.com/getting-started-with-github-agentic-workflows)
- [Top 9 AI Agent Builders in 2026 (Bannerbear)](https://www.bannerbear.com/blog/top-9-ai-agent-builders-in-2026/) · [AI agent frameworks comparison 2026 (Noqta)](https://noqta.tn/blog/ai-agent-frameworks-langgraph-crewai-openai-sdk-comparison-2026) · [LangGraph vs n8n (Peliqan)](https://peliqan.io/blog/langgraph-vs-n8n/)
- [Langfuse vs LangSmith (DataCamp)](https://www.datacamp.com/blog/langfuse-vs-langsmith) · [LangSmith vs Langfuse (LangChain)](https://www.langchain.com/resources/langsmith-vs-langfuse)
