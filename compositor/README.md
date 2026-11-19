# Genesis compositor (Rust, Smithay)

The SevynOS compositor. See `../COMPOSITOR_PLAN.md` for the full plan.

## Language (locked)

Rust end to end, on Smithay 0.7.0 (pinned) — "Rust all the way" (Kevon
2026-11-19). No C core, no wlroots, no meson. The only C in the repo is
the test client, which is a client, not the compositor. See the plan's
"Language: Rust" section for the Smithay-vs-wlroots evaluation and the
live-source coverage check (foreign-toplevel management is the one gap,
with a workaround: Genesis implements the manager global from the
maintained `wayland-protocols-wlr` generated types).

## Layout

```text
compositor/
├── Cargo.toml      # genesis-compositor, binary sevyn-compositor
├── Cargo.lock      # committed; builds are --locked
├── src/            # server: main.rs + modules (Stage 2)
│                   #   server, backend, output, seat, input, xdg_shell,
│                   #   surface_registry, runtime_bridge, logging
├── tests/          # test clients (sevyn-test-client, the M1 client, C)
└── protocols/      # SevynOS-owned protocol XML (Stage 4 adoption)
```

At seed time only `src/main.rs` (a stub: Stage 2 not yet implemented)
and `tests/test-client.c` exist; Stage 2 lands the server in `src/`.
The Shell stays TypeScript/React Native and talks to it over the
Runtime IPC socket, never over library internals.

## Build

```sh
cargo build --locked
cargo clippy --locked -- -D warnings
cargo fmt --check
```

Needs: Rust stable (pinned by `../rust-toolchain.toml`), gcc,
pkg-config, libwayland-dev, libxkbcommon-dev, libudev-dev,
libinput-dev, libgbm-dev, libseat-dev, wayland-protocols. The build
produces `target/debug/sevyn-compositor` (`target/release/` with
`--release`). The test client builds with gcc + wayland-scanner (see
the `compositor` CI job).
