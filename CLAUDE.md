# embarch-core

## Docs

Design doc: [../embarch-doc/embarch-core/design.md](../embarch-doc/embarch-core/design.md) — source of truth for this project's architecture/design.
Update it proactively per [../embarch-doc/DOC-PROTOCOL.md](../embarch-doc/DOC-PROTOCOL.md) whenever a notable design decision, feature, or status change happens here.

## Local dev safety

`install`/`start`/`stop`/`uninstall` install/control a real OS service and touch a real system-wide token file. Never run one of these live against a real machine unsupervised — ask first, same standing rule as firmware build/flash — unless it's running inside `embarch-umbrella`'s [dev-sandbox/](../embarch-umbrella/dev-sandbox/) or an equivalently disposable environment. Full detail: [../embarch-doc/embarch-dev-workflow.md](../embarch-doc/embarch-dev-workflow.md) §5. Unit tests remain the default, fully-autonomous way to verify this logic.
