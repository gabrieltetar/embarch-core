# embarch-core

## Docs

**Four files, not one.** Current truth: [spec.md](../embarch-doc/embarch-core/spec.md). Why it is that way: [decisions.md](../embarch-doc/embarch-core/decisions.md) — an index over `decisions/`, and a decision number addresses this sub-project, not a file. Unresolved: [open.md](../embarch-doc/embarch-core/open.md). Reference: [interfaces.md](../embarch-doc/embarch-core/interfaces.md).

Update them proactively per [../embarch-doc/DOC-PROTOCOL.md](../embarch-doc/DOC-PROTOCOL.md) whenever a notable design decision, feature, or status change happens here — §4 says when, §5 says how, and history goes in a `changelog.d/` fragment rather than into a doc.

## Local dev safety

`install`/`start`/`stop`/`uninstall` install/control a real OS service and touch a real system-wide token file. **The repo owner has explicitly authorized running these live against their real daily-use machine directly — no need to ask first there** (2026-08-17). `embarch-umbrella`'s [dev-sandbox/](../embarch-umbrella/dev-sandbox/) still exists for anyone who'd rather stay fully isolated (a different machine, a different person, or a change risky enough to want a disposable environment regardless). Full detail: [../embarch-doc/embarch-dev-workflow.md](../embarch-doc/embarch-dev-workflow.md) §5. (The separate firmware build/flash rule this note originally distinguished itself from has since been removed too — see global `CLAUDE.md`.)

## Git

**Work directly on `main` — no feature branches, no PRs (2026-08-25).** Commit and push straight to `main` once the change builds and its tests and `clippy --all-targets -- -D warnings` are clean. This **overrides** the general "if you're on the default branch, branch first" default, for this suite only. It ends when the repo owner explicitly says it does, and on no other condition — not on an agent's read of whether the project has outgrown it. Reasoning, the sequencing rules that keep it safe, and the one case that still warrants a branch: [../embarch-doc/embarch-dev-workflow.md](../embarch-doc/embarch-dev-workflow.md) §6.
