---
name: vp-start-sp-design-implementation
description: >-
  Use to kick off implementing a design/architecture document at Virtuozzo (VHP / VSTOR
  projects). Trigger whenever the user points you at a design doc and wants to start building
  it — phrases like "here is the design we'll implement", "let's implement this design",
  "create a task and branch for this design", "start the implementation", or asks to bootstrap a
  design implementation even if they don't say "skill". Handles the full kickoff: stash a dirty
  tree, update the default branch, create the Jira Technical task under the parent, create and
  check out a branch named after the ticket, then hand off to brainstorming/writing-plans with
  this team's execution conventions (group-based batching, controller self-review, phase-end
  heavy gates, Opus for code generation).
---

# Start Design Implementation

Kick off implementing a design doc the way this team does it. The skill has two parts:

- **Part A — Bootstrap**: get the repo into a clean, ready-to-work state and create the Jira
  task + branch. Deterministic; follow it in order.
- **Part B — Execution conventions**: the team's overrides on top of the Superpowers
  plan-writing/execution skills. These change *how* the plan is built and executed. Apply them
  when you reach planning and implementation.

**Announce at start:** "I'm using the start-sp-design-implementation skill to kick this off."

Create a TodoWrite with the Part A steps so nothing gets skipped — a missed `git stash` can
lose the user's uncommitted work, and a missed branch step means you start implementing on
`master`.

## Inputs

- **Design doc path** — required. The user gives this.
- **Parent ticket** — look for it in the design doc first (grep for a `VHP-\d+` / `VSTOR-\d+`
  reference, an "Epic"/"Parent"/"Tracking" field, or a Jira link). If you can't find it, ask
  the user. Don't guess.

---

## Part A — Bootstrap

### A1. Make the working tree clean (stash, never discard)

Run `git status --porcelain`. If anything is uncommitted (tracked or untracked):

```bash
git stash push --include-untracked -m "start-sp-design-implementation: pre-kickoff stash <YYYY-MM-DD>"
```

**Why stash and not discard:** the user's uncommitted work is theirs. Stashing preserves it
and is reversible (`git stash pop`). After stashing, tell the user exactly what you stashed and
how to get it back, so a clean tree never comes as a surprise. If the tree is already clean,
say so and move on.

### A2. Update the default branch

Detect the default branch rather than assuming (`git symbolic-ref refs/remotes/origin/HEAD`
gives e.g. `refs/remotes/origin/master`). Then:

```bash
git checkout <default-branch>
git pull --ff-only
```

If `--ff-only` can't fast-forward (local diverged), stop and tell the user — don't force or
merge silently.

### A3. Resolve the parent ticket

Use the parent ticket from the inputs. Confirm it exists and read it so you have project
context for the new task — fetch it via the Jira MCP (`getJiraIssue`). The project key is the
prefix of the parent key (e.g. `VHP-1673` → project `VHP`); you'll need it in A4.

### A4. Create or reuse the Jira task

**First ask whether a task already exists** for this design — the user often creates one ahead
of time, and a duplicate is noise.

- **If a task already exists**: ask the user for its key and use that. Don't create a new one.
- **If not**: create a new issue under the parent.
  - Issue type: **Technical task** (this team's default for implementation work).
  - Parent: the ticket from A3.
  - Project: derived from the parent key.
  - Summary: derive from the design doc title; keep it short and specific.
  - Description: a one/two-line pointer to the design doc path + its goal.

Use the Atlassian/Jira MCP tools. If you don't yet have a cloud id, resolve it with
`getAccessibleAtlassianResources` / `getVisibleJiraProjects` before creating. Confirm the new
key back to the user.

### A5. Create and check out the branch

The branch name is **just the ticket key**, e.g. `VHP-1234` (not `pr/...`, no slug).

```bash
git checkout -b <TICKET-KEY>
```

If that branch name is already taken, **ask the user for a new name** rather than picking one
yourself — they may already have work under a variant name.

### A6. Confirm state, then move to Part B

Report a short summary: stash status, default branch updated, ticket key (created or reused),
branch checked out. Then proceed to planning under Part B.

---

## Part B — Execution conventions

The Superpowers skills (`brainstorming` → `writing-plans` → `subagent-driven-development`) drive
the actual work. This team overrides four of their defaults. **These overrides take precedence
over the corresponding Superpowers defaults** — apply them whenever they conflict.

### B0. Analyze the design, then brainstorm and plan

Read the design doc fully. Then use **`superpowers:brainstorming`** to pin down intent and
open questions, and **`superpowers:writing-plans`** to produce the plan. The conventions below
shape both the plan's structure and how it's executed.

### B1. Structure the plan as Phase → Group → Task

The default plan is a flat list of tasks. This team works at three levels:

- **Phase** — the largest unit; a milestone that ends with a heavy verification gate.
- **Group** — a set of related tasks within a phase, labelled `A`, `B`, `C`, … Tasks are
  `A1`, `A2`, `B1`, … A group is the unit of review and progress.
- **Task** — a single bite-sized change (as in `writing-plans`).

Example shape (Phase 1):

```
A1–A5   token-issuer-sdk crate
B1–B3   signing proto + parse + SigningService
B4–B6   signing gRPC server/client + gateway
C1–C7   token-issuer crate (config/canonicalization/claims/cache/JWT/JWKS/error)
C8–C10  gear init/serve/REST + composition + metrics
D1      vhp-installer OpenBao Transit (vault-config.yaml)
E1      mint→JWKS→verify integration test
Phase-1 end gate: dylint --all + cargo shear --expand + workspace clippy
```

Write the plan so groups are explicit and each group's tasks are self-contained.

### B2. Batch by group, not by task

Drive execution **a group at a time**, not one isolated subagent per task with a full review
cycle after every single task. Within a group, the tasks are closely related and share context;
reviewing the group as a coherent unit is faster and gives better signal than nitpicking each
task in isolation. Mark progress per group (the `controller-reviewed✓` marker in the example
plan is a completed group).

### B3. Do the review yourself; agents gather facts, never run cold builds

Default `subagent-driven-development` dispatches a **spec-reviewer subagent** and a
**code-quality-reviewer subagent** after each task. Override that: **you (the controller) do the
review yourself**, in the main session, at the end of each group.

Subagents are still useful for **gathering facts** — collecting diffs, running a linter, pulling
test output — but the **verdict is yours**. The reason to keep review in your hands: you hold the
cross-group context and the design intent; a fresh reviewer subagent re-derives less of it and
tends to nitpick locally.

**Never run cold builds inside a subagent.** A subagent starts with a cold build cache, so a
`cargo build`/`cargo test` there pays the full compile cost from scratch — slow and wasteful.
The main session has the warm cache; run builds and tests there. Fact-gathering agents may run
cheap, cache-independent things (read files, run an already-built linter), not full builds.

### B4. Heavy gates run once at phase end, not per step

Lightweight checks (compilation, unit tests for the code just touched) run continuously as you
go — that's normal. But the **heavy gates run once, at the end of the phase**, after all its
groups are done:

```
dylint --all
cargo shear --expand
cargo clippy --workspace   # workspace-wide clippy
```

**Why batch them:** these are slow and operate on the whole workspace, so running them after
every step wastes large amounts of time re-checking unchanged code. Running them once at the
phase boundary catches the same issues at a fraction of the cost. If the gate fails, fix and
re-run before declaring the phase done.

### B5. Use Opus for code generation

When you dispatch a subagent to **generate code**, set its model to **Opus** explicitly
(`model: "opus"` on the Agent call). This overrides the default "cheapest model that can do the
job" guidance in `subagent-driven-development` — this team prioritizes generation quality.

This applies to **code-generation** subagents specifically. Pure research/exploration or
fact-gathering dispatches don't need Opus; use your judgement there.

---

## Quick reference

| Step | Action | Note |
|------|--------|------|
| A1 | `git stash push --include-untracked` if dirty | Never discard; tell the user |
| A2 | Checkout + `git pull --ff-only` default branch | Stop if diverged |
| A3 | Fetch parent ticket | Project = key prefix |
| A4 | Create **Technical task** under parent, or reuse | Ask if one exists first |
| A5 | `git checkout -b <TICKET-KEY>` | Ask for new name if taken |
| B1 | Plan as Phase → Group → Task | Groups labelled A, B, C… |
| B2 | Execute group-by-group | Group is the review unit |
| B3 | Review yourself; agents gather facts | No cold builds in agents |
| B4 | Heavy gates (`dylint`/`shear`/`clippy`) at phase end | Not per step |
| B5 | Opus for code-generation subagents | Not for research dispatches |
