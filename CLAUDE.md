# DropboxSync

Stack: desktop-tauri

Cross-platform Dropbox sync desktop client (Tauri v2 + Rust backend + React/TS frontend),
organized as an npm workspace monorepo (`apps/desktop`, `packages/*`).

## Build / test

```bash
npm install
npm run dev              # dev:mac on macOS, dev:win on Windows
npm run build
npm run test
cargo test --manifest-path apps/desktop/src-tauri/Cargo.toml
cargo clippy --manifest-path apps/desktop/src-tauri/Cargo.toml
```

## Issue tracker: Jira (Atlassian Rovo MCP)

This project is tracked in **Jira**, not in local files. Never create `.md-issues/` here.

| Setting | Value |
|---|---|
| Site | `https://manuelbsan.atlassian.net` |
| Cloud ID | `6a6a8471-2468-4625-a803-cc0250181149` |
| Project key | `DBSYNC` (DropboxSync) |
| Access | `mcp__claude_ai_Atlassian_Rovo__*` tools (read + write Jira scopes) |
| Plugin | `cowork-workflow@workflow-marketplace` (enabled in `.claude/settings.json`) |

Activate the golden rules for a session with `/cowork-workflow:activate`.

### Pipeline

```
/cowork-workflow:jira-ticket-start      DBSYNC-NN   architecture validation
/cowork-workflow:jira-ticket-improve    DBSYNC-NN   PRD, user stories, vertical slices
/cowork-workflow:jira-ticket-plan       DBSYNC-NN   implementation plan per slice
/cowork-workflow:jira-ticket-implement  DBSYNC-NN   code
/cowork-workflow:jira-ticket-commit     DBSYNC-NN   commit + PR
/cowork-workflow:jira-ticket-cto-review DBSYNC-NN   PR review
/cowork-workflow:jira-ticket-qa         DBSYNC-NN   manual validation
/cowork-workflow:jira-ticket-po-approve DBSYNC-NN   final gate
```

One step per command — never skip steps without explicit confirmation.

### Status mapping (IMPORTANT — verify before every transition)

The DBSYNC workflow has five statuses, all reachable through global transitions:

| Status | Status id | Transition id |
|---|---|---|
| To Do | `10072` | `11` |
| In Progress | `10073` | `21` |
| In Review | `10074` | `31` |
| QA | `10076` | `2` |
| Done | `10075` | `41` |

`In Review` plays the role the default pipeline calls `PR Review`, and there is no separate
`PO Approval` / `To be validated by PO` status — PO approval closes the ticket to `Done`.

| Step | From → To | Transition id |
|---|---|---|
| `jira-ticket-start` | To Do → In Progress | `21` |
| `jira-ticket-commit` (PR opened) | In Progress → In Review | `31` |
| `jira-ticket-cto-review` approved | In Review → QA | `2` |
| `jira-ticket-qa` PASS | stays QA (verdict on the PR) | — |
| `jira-ticket-po-approve` approved | QA → Done | `41` |
| any step rejected | → In Progress | `21` |

**Rule: always call `getTransitionsForJiraIssue` first and only use ids it returns.** The QA
transition was added on 2026-08-18 and is not part of the original workflow, so never hardcode
`PR Review`, `PO Approval` or `To be validated by PO` — those statuses do not exist here.

### Conventions

- Branch: `feature/DBSYNC-NN-short-slug`.
- Commit subject carries `(#DBSYNC-NN)`; the body ends with the bare `#DBSYNC-NN` reference
  (matches the existing history).
- Jira comments only on status change / block / rejection, ≤5 lines, prefixed
  `(Auth by Claude - <Agent Name>)`. Technical detail goes on the GitHub PR, not Jira.
- QA on this project is **manual desktop validation** (Windows CfAPI / macOS Finder Sync),
  not Chrome — the `qa-engineer` browser flow does not apply.

## Language

Per `.cursor/rules/english-repo.mdc`: commit messages, code comments and doc comments are
English only. The maintainer may communicate in Spanish; repository artifacts stay English.
