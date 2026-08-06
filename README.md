# embarch-core

The OS-level service that owns the debug probe (flash / reset) and the
serial console log. This is *Core* — it has no idea the EmbArch API or
Claude Code exist. It just exposes five bearer-token-authed HTTP endpoints
and holds the hardware connection so nothing else has to fight over the
USB port.

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

## Layout

```
src/
├── main.rs        — CLI (clap): `run`, `install`, `uninstall`,
│                    `detect-dev-bench`; resolves the token via token_store,
│                    builds AppState, starts Axum
├── api.rs         — Axum router, handlers, bearer-token auth middleware
├── hardware.rs    — probe-rs: list probes, flash, reset
├── serial.rs      — serialport: read the UART console log
├── dev_bench.rs   — serialport: find embarch-dev-bench's port (SEGGER VID +
│                    product/serial/interface match); enumeration only
├── service.rs     — service-manager: register/remove as a background service
└── token_store.rs — resolves/generates/persists the machine-wide EMBARCH_TOKEN file
```

## Endpoints

All routes require `Authorization: Bearer <token>` (see **Auth** below).

| Method | Path          | Body / Query                                             |
|--------|---------------|-----------------------------------------------------------|
| GET    | `/status`     | —                                                          |
| POST   | `/flash`      | `{"chip": "...", "firmware_path": "...", "format": "elf"}` |
| POST   | `/reset`      | `{"chip": "..."}`                                          |
| GET    | `/serial-log` | `?port=...&baud=115200&duration_ms=2000`                   |
| GET    | `/dev-bench/port` | —                                                      |

`/dev-bench/port` answers which serial port
[`embarch-dev-bench`](https://github.com/gabrieltetar/embarch-dev-bench) is on,
by matching SEGGER's USB VID `0x1366` (the DK's on-board J-Link — dev-bench's
own SoC has no USB peripheral) plus a product-string / serial-number /
interface-index heuristic. `404` means no port matched, which is just "the
bench isn't plugged in"; `500` means detection itself failed or was ambiguous.
Env overrides, in precedence order:

| Variable | Effect |
|---|---|
| `EMBARCH_DEV_BENCH_PORT` | Skip detection, use this port name |
| `EMBARCH_DEV_BENCH_SERIAL` | Require this J-Link serial number |
| `EMBARCH_DEV_BENCH_PRODUCT` | Product-string needle (default `jlink`; empty = VID only) |
| `EMBARCH_DEV_BENCH_INTERFACE` | Require this USB interface number |

`embarch-core detect-dev-bench` runs the same detection from the CLI.

`chip` must be a probe-rs target name (e.g. `STM32F407VG`, `nRF52840_xxAA`,
`esp32c3`) — Core doesn't validate it beyond whatever `probe.attach()` itself
rejects. `format` is one of `elf` / `bin` / `hex` / `uf2` / `idf`.
`firmware_path` is read from Core's own local disk — the caller is
responsible for getting the file onto whatever machine Core runs on.

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

Binds to `0.0.0.0:4884` by default — deliberately not `127.0.0.1`, since
the point of this service is to be reachable from WSL2 (if Core runs
native on Windows) or the LAN (if Core moves to a Pi). Override with
`--bind` / `--port`.

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
- **No multi-probe selection.** `open_first_probe()` in `hardware.rs` takes
  the first probe-rs finds. Fine at single-board scope; the moment you add
  a second probe, that's the one function that needs a serial-number
  selector.
- **`EMBARCH_TOKEN` is a single shared static token**, not per-caller
  credentials. Adequate for one engineer; revisit if this ever needs to
  distinguish *who* is calling, not just *whether* they're allowed to.

## License

MIT — see [LICENSE](LICENSE).
