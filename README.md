# embarch-core

The OS-level service that owns the debug probe (flash / reset) and the
serial console log. This is *Core* — it has no idea the EmbArch API or
Claude Code exist. It just exposes a bearer-token-authed HTTP interface
(owned by the interface docs, see **Interfaces** below) and holds the
hardware connection so nothing else has to fight over the USB port.

Built with **Rust + probe-rs (as a library) + Axum + service-manager**.

## Architecture

```
embarch-api --HTTP+Bearer--> embarch-core --probe-rs/serialport--> hardware
                                    ^
                                    |
                         embarch-core CLI (same machine)
```

Core is reached two ways: over HTTP by [`embarch-api`](https://github.com/gabrieltetar/embarch-api),
and directly via its own CLI (`run`/`install`/`uninstall`/`detect-dev-bench`)
for local operation and service management. Both paths converge on the same
hardware/serial modules — there's no separate code path for "CLI mode."

## Interfaces

Endpoints, request shapes, the module layout and the dev-bench port
detection are owned by the interface docs in
[`embarch-doc/embarch-core/interfaces/`](https://github.com/gabrieltetar/embarch-doc/tree/main/embarch-core/interfaces)
and the sub-project's
[`spec.md`](https://github.com/gabrieltetar/embarch-doc/blob/main/embarch-core/spec.md),
not repeated here, so this page does not go stale. Every route requires
`Authorization: Bearer <token>` (see **Auth** below). The router is
`src/api.rs`; `embarch-core --help` lists the CLI.

## Auth

A single shared secret, `EMBARCH_TOKEN`, checked via exact-string comparison
against every request's `Authorization` header. The token is resolved with
this precedence:

1. An explicit `EMBARCH_TOKEN` environment variable, if set.
2. Otherwise, the machine-wide token file is reused if present
   (`/var/lib/embarch/token` on Linux/macOS, `%ProgramData%\embarch\token`
   on Windows), or generated and persisted there on first startup.

The generated file is restricted to the owning account (`chmod 600` on
Unix, an `icacls`-restricted ACL on Windows). This matches the
single-engineer, single-shared-secret scope of the whole EmbArch suite —
there's no per-caller identity, only "does the caller know the token."

## Building

**This repo does not build on its own.** Two sibling repos must be cloned into the
same parent directory, because `Cargo.toml` depends on them by relative path:

| Sibling | Why |
|---|---|
| [`embarch-study-designer`](https://github.com/gabrieltetar/embarch-study-designer) | the shared study/registry type model |
| [`embarch-topology`](https://github.com/gabrieltetar/embarch-topology) | the board registry, with its `hardware` feature |

So the layout cargo expects is `<parent>/embarch-core`, `<parent>/embarch-study-designer`,
`<parent>/embarch-topology`. Clone this repo on its own and `cargo build` fails with
`failed to read ../embarch-topology/Cargo.toml` — an error naming a path outside this
repo, which means nothing more than "the sibling is not there".

Path dependencies rather than git or registry ones is a deliberate choice, not an
oversight: `embarch-study-designer` decision 8 and `embarch-topology` decision 13.

```
cargo build --release
```

You'll need a **current stable Rust toolchain (installed via
[rustup](https://rustup.rs), not your Linux distro's package manager)**.
`probe-rs` and several of its dependencies require Rust 1.85+ (the
edition2024 baseline) — Ubuntu's `apt` package is usually much older than
that. On the Raspberry Pi side this matters too: use `rustup`, not
`apt install cargo`.

On Linux (WSL2 or a Pi), `serialport`'s USB enumeration needs `libudev`:

```
sudo apt install libudev-dev pkg-config
```

## Running

```
export EMBARCH_TOKEN=some-long-random-string
cargo run -- run
```

Binds to `127.0.0.1:4884` by default — loopback-only (decision 6's
amendment, 2026-08-15). The original default was `0.0.0.0`; it was
reversed because that plus no TLS plus a static bearer token plus
`/flash` reading an arbitrary local path, in a process that may run as
`LocalSystem`, is a posture nobody had assessed as a whole. Override with
`--bind` / `--port` for a one-off `run`; to widen a permanently
*installed* service (the WSL2-guest-reaches-Windows-native-Core topology
needs this), reinstall elevated with `embarch-core install --bind
0.0.0.0` — the same command `suite/user-guide.md` prescribes for
`doctor`'s `bound-narrow`. Plain `embarch setup` only widens the bind
correctly when run from the WSL2 guest side; run natively on Windows it
infers `local` and reinstalls the narrow bind instead.

## Installing as a background service

```
cargo build --release
sudo ./target/release/embarch-core install     # Linux: systemd unit
# or, on Windows (as Administrator):
.\target\release\embarch-core.exe install      # Windows: registers via sc.exe
```

`service-manager` detects the right backend automatically — same command,
same code, either OS. `uninstall` reverses it.

## What's deliberately not here yet

- **No ESP-IDF UART-bootloader flashing.** probe-rs's `Format::Idf` variant
  covers some ESP flashing via USB-JTAG, but the classic UART bootloader
  path most ESP-IDF workflows use isn't covered. The planned escape hatch
  is an `esptool` subprocess fallback in `hardware.rs`, not yet implemented.
- **`EMBARCH_TOKEN` is a single shared static token**, not per-caller
  credentials. Adequate for one engineer; revisit if this ever needs to
  distinguish *who* is calling, not just *whether* they're allowed to.

## License

MIT — see [LICENSE](LICENSE).
