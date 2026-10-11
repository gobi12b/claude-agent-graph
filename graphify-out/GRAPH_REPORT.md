# Graph Report - claude-agent-graph  (2026-10-11)

## Corpus Check
- 27 files · ~164,740 words
- Verdict: corpus is large enough that graph structure adds value.
- Unclassified: 4 file(s) not represented in the graph (top: (none) 3, .ico 1)

## Summary
- 755 nodes · 2345 edges · 25 communities (18 shown, 7 thin omitted)
- Extraction: 78% EXTRACTED · 22% INFERRED · 0% AMBIGUOUS · INFERRED: 519 edges (avg confidence: 0.85)
- Token cost: 0 input · 0 output

## Graph Freshness
- Built from commit: `e52c04c8`
- Run `git rev-parse HEAD` and compare to check if the graph is stale.
- Run `graphify update .` after code changes (no API cost).

## Community Hubs (Navigation)
- s
- wfstore.rs
- workflows.rs
- util.rs
- yamldoc.rs
- worktree.rs
- watcher.rs
- guard.rs
- cli.rs
- 5. Specs
- template.rs
- README.md
- compat.rs
- providers.rs
- UX review: Claude Agent Graph
- Notes for AI agents
- install.sh script
- main.rs
- lint-commits.sh
- next-version.sh
- claude-agent-graph

## God Nodes (most connected - your core abstractions)
1. `s()` - 105 edges
2. `Runner` - 81 edges
3. `b()` - 59 edges
4. `arr()` - 43 edges
5. `join()` - 40 edges
6. `validate()` - 28 edges
7. `truthy()` - 23 edges
8. `git()` - 20 edges
9. `migrate()` - 20 edges
10. `py_str()` - 19 edges

## Surprising Connections (you probably didn't know these)
- `Run` --calls--> `s()`  [INFERRED]
  src/cli.rs → src/util.rs
- `Resume` --calls--> `arr()`  [INFERRED]
  src/cli.rs → src/util.rs
- `Resume` --calls--> `s()`  [INFERRED]
  src/cli.rs → src/util.rs
- `Validate` --calls--> `abspath()`  [INFERRED]
  src/cli.rs → src/util.rs
- `Validate` --calls--> `arr()`  [INFERRED]
  src/cli.rs → src/util.rs

## Import Cycles
- None detected.

## Communities (25 total, 7 thin omitted)

### Community 0 - "s"
Cohesion: 0.07
Nodes (47): agent_instructions(), bash_log(), CHECK_TIMEOUT, dead_pid(), EDITABLE, gate_summary(), guardrails_end_to_end(), input_env() (+39 more)

### Community 1 - "wfstore.rs"
Cohesion: 0.10
Nodes (60): basename(), config_dir(), exists(), expanduser(), join(), mtime(), opt_truthy(), write_atomic() (+52 more)

### Community 2 - "workflows.rs"
Cohesion: 0.07
Nodes (60): claude_cmd(), char_len(), last_json_line(), py_int(), py_str(), s_or(), slug(), truthy() (+52 more)

### Community 3 - "util.rs"
Cohesion: 0.06
Nodes (32): API_POST, App, check_folder(), hdr(), HEARTBEAT, ICON_PNG, ICON_SVG, INDEX_HTML (+24 more)

### Community 4 - "yamldoc.rs"
Cohesion: 0.10
Nodes (36): bracket_depth(), child(), dump(), Emitter, errors_have_line_and_column(), flow_ok(), fmt_key(), Frame (+28 more)

### Community 5 - "worktree.rs"
Cohesion: 0.10
Nodes (43): version(), diff(), DIFF_LIMIT, evidence(), git(), GIT_TIMEOUT, is_repo(), judge() (+35 more)

### Community 6 - "watcher.rs"
Cohesion: 0.09
Nodes (25): claude_dir(), file_size(), list_dir(), local_offset(), read_json(), agent_re(), agent_status(), describe_tool() (+17 more)

### Community 7 - "guard.rs"
Cohesion: 0.09
Nodes (21): BUILT_IN, clean(), clean_keys(), fingerprint(), glob_regex(), globs(), Guard, items() (+13 more)

### Community 8 - "cli.rs"
Cohesion: 0.11
Nodes (29): ask_review(), c(), Cli, Cmd, Clean, List, Resume, Run (+21 more)

### Community 9 - "5. Specs"
Cohesion: 0.05
Nodes (36): 1. Why this document, 2.1 Who we compared against, 2.2 Feature matrix, 2. Competitive landscape, 3. Where we win (keep and lean on), 4.1 Value ranking, 4. Gaps, ranked, 5. Specs (+28 more)

### Community 10 - "template.rs"
Cohesion: 0.14
Nodes (29): as_text(), check(), check_reports_unknown_references(), clean_inputs(), coerce(), ctx(), Expr, Lit (+21 more)

### Community 11 - "README.md"
Cohesion: 0.06
Nodes (30): AI judge, AI judge, step diffs and rewind, AI models: Claude, Gemini, GPT and local models, Also, Build from source, Building a release, Check workflows: `validate`, Development (+22 more)

### Community 12 - "compat.rs"
Cohesion: 0.17
Nodes (18): bash(), detached(), file_url(), interrupt(), kill_tree(), looks_like_claude(), MAC, notify() (+10 more)

### Community 13 - "providers.rs"
Cohesion: 0.19
Nodes (10): check_ready(), CLAUDE_MODELS, command(), extra_models(), http_get(), Info, ollama_models(), ollama_url() (+2 more)

### Community 14 - "UX review: Claude Agent Graph"
Cohesion: 0.20
Nodes (9): Findings (by severity), Implementation status (2026-10-03), Out of scope for this pass (follow-ups), P0: blocks or loses work, P1: confuses non-technical users, P2: polish and accessibility, Phase 2: full UI revamp (2026-10-03), Summary (+1 more)

### Community 15 - "Notes for AI agents"
Cohesion: 0.33
Nodes (5): Code graph, Layout, Notes for AI agents, Pull requests and commits, Releases

### Community 16 - "install.sh script"
Cohesion: 0.67
Nodes (5): die(), fetch(), install.sh script, say(), sha256()

### Community 17 - "main.rs"
Cohesion: 0.47
Nodes (4): Args, decode_icon(), main(), open_window()

## Knowledge Gaps
- **3 isolated node(s):** `claude-agent-graph`, `Path`, `Seq`
  These have ≤1 connection - possible missing edges or undocumented components. (Counts symbols only; 172 node(s) total have ≤1 connection when file, concept and rationale nodes are included.)
- **7 thin communities (<3 nodes) omitted from report** — run `graphify query` to explore isolated nodes.

## Suggested Questions
_Questions this graph is uniquely positioned to answer:_

- **Why does `s()` connect `s` to `wfstore.rs`, `workflows.rs`, `util.rs`, `worktree.rs`, `watcher.rs`, `cli.rs`, `template.rs`, `providers.rs`?**
  _High betweenness centrality (0.095) - this node is a cross-community bridge._
- **Are the 102 inferred relationships involving `s()` (e.g. with `ask_review()` and `Resume`) actually correct?**
  _`s()` has 102 INFERRED edges - model-reasoned connections that need verification._
- **What connects `claude-agent-graph`, `Path`, `Seq` to the rest of the system?**
  _3 weakly-connected nodes found - possible documentation gaps or missing edges._
- **Should `s` be split into smaller, more focused modules?**
  _Cohesion score 0.07203339296362553 - nodes in this community are weakly interconnected._
- **Why does `Runner` connect `s` to `cli.rs`, `util.rs`, `watcher.rs`?**
  _High betweenness centrality (0.057) - this node is a cross-community bridge._
- **Are the 56 inferred relationships involving `b()` (e.g. with `Validate` and `follow()`) actually correct?**
  _`b()` has 56 INFERRED edges - model-reasoned connections that need verification._
- **Should `wfstore.rs` be split into smaller, more focused modules?**
  _Cohesion score 0.09931798806479114 - nodes in this community are weakly interconnected._