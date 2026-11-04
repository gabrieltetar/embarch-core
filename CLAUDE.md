# embarch-core

## Docs

**Four files, not one.** Current truth: [spec.md](../embarch-doc/embarch-core/spec.md). Why it is that way: [decisions.md](../embarch-doc/embarch-core/decisions.md) — an index over `decisions/`, and a decision number addresses this sub-project, not a file. Unresolved: [open.md](../embarch-doc/embarch-core/open.md). Reference: [interfaces.md](../embarch-doc/embarch-core/interfaces.md).

Update them proactively per [../embarch-doc/DOC-PROTOCOL.md](../embarch-doc/DOC-PROTOCOL.md) whenever a notable design decision, feature, or status change happens here — §4 says when, §5 says how, and history goes in a `changelog.d/` fragment rather than into a doc.

## Local dev safety

`install`/`start`/`stop`/`uninstall` install/control a real OS service and touch a real system-wide token file. **The repo owner has explicitly authorized running these live against their real daily-use machine directly — no need to ask first there** (2026-08-17). `embarch-umbrella`'s [dev-sandbox/](../embarch-umbrella/dev-sandbox/) still exists for anyone who'd rather stay fully isolated (a different machine, a different person, or a change risky enough to want a disposable environment regardless). Full detail: [../embarch-doc/embarch-dev-workflow.md](../embarch-doc/embarch-dev-workflow.md) §5. (The separate firmware build/flash rule this note originally distinguished itself from has since been removed too — see global `CLAUDE.md`.)

## Git

**Work on a branch, land when the owner asks — no PRs (2026-11-03).** Work in a worktree, with one branch name across every repo the change touches, and push the branch. Land on `main` only when the repo owner asks: rebase on `origin/main`, confirm it builds and its tests and `clippy --all-targets -- -D warnings` are clean, fast-forward, push, delete the branch. The owner's explicit call ended the earlier commit-straight-to-`main` rule, and changing this one is the owner's call too, not an agent's read of what the project needs. The fleet lands under its own protocol. Reasoning, setup and the landing order across repos: [../embarch-doc/embarch-dev-workflow.md](../embarch-doc/embarch-dev-workflow.md) §6.
