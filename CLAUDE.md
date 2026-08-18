# embarch-core

## Docs

Design doc: [../embarch-doc/embarch-core/design.md](../embarch-doc/embarch-core/design.md) — source of truth for this project's architecture/design.
Update it proactively per [../embarch-doc/DOC-PROTOCOL.md](../embarch-doc/DOC-PROTOCOL.md) whenever a notable design decision, feature, or status change happens here.

## Local dev safety

`install`/`start`/`stop`/`uninstall` install/control a real OS service and touch a real system-wide token file. **The repo owner has explicitly authorized running these live against their real daily-use machine directly — no need to ask first there** (2026-08-17). `embarch-umbrella`'s [dev-sandbox/](../embarch-umbrella/dev-sandbox/) still exists for anyone who'd rather stay fully isolated (a different machine, a different person, or a change risky enough to want a disposable environment regardless). Full detail: [../embarch-doc/embarch-dev-workflow.md](../embarch-doc/embarch-dev-workflow.md) §5. (The separate firmware build/flash rule this note originally distinguished itself from has since been removed too — see global `CLAUDE.md`.)
