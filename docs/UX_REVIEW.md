# UX review: Claude Agent Graph

**Reviewer:** UX · **For:** engineering · **Date:** 2026-10-03
**Goal:** someone who doesn't write code should be able to open the app, see what Claude is doing, build a simple workflow and review its results without reading docs.

## Summary

Under the hood the app is strong: live graph, workflow builder, review checkpoints and step replay. The interface is still written for the people who built it. The main problems are:

1. **Jargon everywhere.** Terms like session, subagent, spawn, hand-back, artefacts, `acceptEdits`, `Bash(npm test:*)`, `exit 0`, `</>` and ⑂ all appear without explanation.
2. **Everything shows at once.** A step has 12 settings, and the workflow list shows 6 buttons per row in a 340 px column.
3. **No first-run guidance.** An empty canvas says "Click a node for details" when there are no nodes.
4. **Some actions lose data or can't be undone.** Esc or ← closes the builder and throws away unsaved work. Stop has no confirmation.

## Findings (by severity)

### P0: blocks or loses work
| # | Where | Problem | Fix |
|---|---|---|---|
| 1 | Builder | **Esc / ← silently discards unsaved changes.** | Ask "Discard unsaved changes?" when the draft differs from the saved version. |
| 2 | Run view | **Stop** in the header stops the run on one click. | Ask for a second click to confirm, as Kill session already does. |
| 3 | Main canvas | The connection status (`#conn`) and the zoom control are both at bottom-left, so the status is hidden under the zoom control. | Move the status above the zoom control. |
| 4 | First run | The canvas is blank and gives no hint of what the app is or what to do. | Add a welcome card that explains the app in one line and offers two actions: **Create a workflow** and **How it works**. |

### P1: confuses non-technical users
| # | Where | Problem | Fix |
|---|---|---|---|
| 5 | Global | No glossary. | Add a **Help** button with plain-language definitions: session, helper agent, workflow, step, review, permissions. |
| 6 | Time filter | "Show sessions active in · Live 1h 24h 7d All" is cryptic. | Change to "Show activity from" with **Running now · Last hour · Today · This week · All time**. |
| 7 | HUD | Four stat chips, including "interactions in the last hour" and "shown", are noise. | Keep the review call-to-action plus "N working now" and "N helpers running". |
| 8 | Details panel | Shows raw status (`busy`, `idle`, `ended`), PID, session id and "Connections". | Show friendly status words. Move the ids into a collapsed **Technical details** section. Rename "Kill session" to "Stop this session". |
| 9 | Activity feed | Kind badges read `spawn`, `result`, `prompt`. | Use **You asked**, **Started helper**, **Helper finished**, **Message**. |
| 10 | Legend | Always open and describes edge types in internal terms. | Make it a collapsible "What do the colours mean?" with everyday wording. |
| 11 | Workflow list | Six buttons per row, including an unlabeled `</>`. | Keep **▶ Run** and **Edit** visible and put the rest under **More ▾**. Make "Confirm delete" reset after 4 s. |
| 12 | Builder header | Three unlabeled inputs. The folder field accepts free-form paths, and the model field is a bare name. | Add visible labels: **Name**, **Project folder**, **AI model**. Describe each model ("Haiku: fastest, cheapest"). |
| 13 | Step inspector | All 12 settings are visible at once. | Show **Name**, **What should Claude do?** and **Pause for my review** by default. Put retries, model, success check, dependencies, loop-back and on-success/failure under **Advanced options**, and keep that section open across re-renders. |
| 14 | Workflow settings | The permission mode shows raw identifiers. Tool rule lists sit at top level. | Use plain mode labels such as "Edit files without asking (recommended)". Move tool lists and budget under **Advanced**. |
| 15 | Run view tabs | "Input · Log · Artefacts · Output · Errors" | Rename to **Instructions · Activity · Files · Result · Problems**. |
| 16 | Permissions modal | Mode names are raw (`dontAsk`, `bypassPermissions`). | Use the same plain labels as #14. Keep the rule syntax, but add a sentence that explains it. |
| 17 | Palette | "Subagents", "⑂", "roles" | Rename to "Helper agents" and explain them in one sentence. |

### P2: polish and accessibility
| # | Problem | Fix |
|---|---|---|
| 18 | 13 px base font with many 10–11 px labels. | Raise the base font to 14 px. |
| 19 | Buttons have no visible keyboard focus. | Add a `:focus-visible` outline. |
| 20 | Icon-only buttons (reload, 📁, ⭐, `</>`). | Add `aria-label` and a visible text label where there's room. |
| 21 | Status is shown by colour only on the canvas. | The tooltip and details panel now show the status word. Longer term, add a shape or icon. |
| 22 | Tabs lack ARIA roles. | Add `role="tablist"`, `role="tab"` and `aria-selected`. |

## Implementation status (2026-10-03)

**Fixed in `index.html`** (the original is saved as `index.html.bak-before-ux`): items 1–20, 22, plus extras:
- Builder: Esc / ← asks before discarding changes. The builder header has labeled fields and model descriptions. Step and workflow settings put the expert controls under a remembered **Advanced options** section, which shows how many settings are changed ("· 3 set"). Step cards use plain wording ("if it fails: stop", "runs after Plan").
- Main view: a welcome card when nothing is showing, a **Help** modal with a glossary, plain time filter and tab names (Now · History · Workflows), simpler HUD, collapsible legend, friendly status words, and a collapsed **Technical details** section in the details panel.
- Workflow list: Run, Edit and See last run stay visible. Duplicate, Open as text file and Delete are under **More ▾**, which closes on an outside click. Delete asks for a second click, which resets after 4 s.
- Run view: renamed tabs, labeled **← Back**, plain status chip, and Stop asks for a second click.
- Permissions: plain mode names, clearer group names (Always allow / Always ask first / Never allow) and an explanation of the rule syntax.

**Not done:** #21 (canvas status relies on colour alone), plus the follow-ups below.

## Phase 2: full UI revamp (2026-10-03)

The app is now organised around its entities. Each one has its own page, a one-line definition that is reused in Help, a clear **＋ New** action and cards with Edit, Duplicate and Delete.

| Page | Entity | What you can do |
|---|---|---|
| Home | – | Things waiting for your review, what's running now, a tile per entity (count, definition, Open / + New), recent runs |
| Live map | – | The original graph and side panel (Now / History) |
| Sessions | Claude session | List filtered by Working now / Open / This week; show a session on the map; stop it (asks to confirm) |
| Workflows | Workflow | Cards showing the step flow (Plan → Build → …), review points, helpers and last-run status. Actions: ▶ Run (opens the run view), Edit, Last run, and More ▾ (Duplicate, Open as text file, Delete) |
| Runs | Run | Filter by All / Running / Needs review / Finished / Failed or stopped; Review now; Stop (asks to confirm) |
| Helpers | Helper (subagent role) | Search; filter by Made by you / Built-in; grouped by category. **New / Edit / Duplicate / Delete**. Built-in helpers can be customized as your own version and reset later. Tools are plain-language checkboxes; ✨ Improve rewrites the instructions |
| Step templates | Step template | **New / Edit / Duplicate / Delete**, customize built-ins and reset them. The editor has Ask Claude / Run a command, review toggle and an Advanced section |
| Permissions | Permission rules | A full page instead of a modal. Each rule shows a plain-language label ("Run “npm test …” commands") next to its raw syntax. The page tracks unsaved changes and offers Undo or Save |

**Visual refresh:** a left navigation rail, an accent colour and icon for each entity, new design tokens for light and dark, softer cards and shadows, an Inter or system font stack, and plain-language text areas (only commands use monospace). The rail collapses to icons below 900 px.

**Backend** (`wfstore.py`, `agent_graph.py`): added `/api/agents/delete`, `/api/steps/save` and `/api/steps/delete`, plus renaming via `oldName` on `/api/agents/save`. Helpers and templates now carry `source` (builtin/user) and `overrides`. Built-ins are never modified; your versions go to `~/.config/claude-agent-graph/{agents,steps}.yaml`. Originals are saved as `*.bak-before-ux`.

**Verified:** every page and editor was rendered with live data and an error banner injected, with no JS errors. Creating a helper, customizing a built-in template and creating a template were tested end to end against a sandboxed home folder.

## Out of scope for this pass (follow-ups)
- Replace the free-text folder input with a native folder picker. This needs a backend endpoint.
- Make the canvas graph navigable by keyboard.
- Add "Simple vs Expert" mode as a persistent preference.
