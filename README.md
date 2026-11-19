# Genesis

**Genesis is SevynOS's official compositor, built for SevynOS only,
open-source.** It is the one and only shipping compositor for SevynOS —
Weston is development infrastructure only (the Stage 2 nested compositor
runs inside it during development), never a shipping configuration and
never a product fallback. If Genesis fails, SevynOS fails safe to a
terminal/recovery environment, not to Weston.

Genesis is a custom Wayland compositor built on
[wlroots](https://gitlab.freedesktop.org/wlroots/wlroots) (pinned 0.17.1).
It owns display outputs, surfaces, input routing, focus, window
placement, and presentation for SevynOS — and it is what makes true
browser embedding possible: Genesis places Chromium's surface inside the
Browser app's content rect in its own scene graph, at 60 fps, zero-copy.
See [COMPOSITOR_PLAN.md](COMPOSITOR_PLAN.md) for the full plan: the staged
approach, the adoption mechanism, the browser embedding contract, and the
recovery guarantee.

## Layout

```text
.
├── compositor/          # the C wlroots compositor (the product)
│   ├── include/sevyn/   # SevynOS-owned public headers (Stage 2)
│   ├── src/             # server (Stage 2)
│   ├── tests/           # test clients (sevyn-test-client, the M1 client)
│   ├── protocols/       # SevynOS-owned protocol XML (Stage 4)
│   └── meson.build
├── model/               # the TypeScript policy model (spec-as-code + tests)
│   ├── src/             # scene, display, surface, window, renderer
│   └── package.json     # @sevynos/graphics (name is historical; see below)
├── COMPOSITOR_PLAN.md   # the plan (stages, adoption, embedding, recovery)
└── LICENSE              # GPL-3.0-or-later
```

## Status

Seed (2026-11-19): the boundary is set, the model and the test client are
in, the plan is here. Stage 2 (minimal nested wlroots compositor) is next.
The compositor is C; the model is TypeScript. Nothing here is a Weston
fork, libweston, CEF, or a frame transport.

## The model (`model/`)

`model/` is the pure-TypeScript, host-independent scene/display/surface/
window/renderer model: `GenesisCompositor`, `GenesisScene` + scene nodes,
`GenesisDisplay` + display manager/registry/render plans, surface and
window registries, frame executor/render loop — with its vitest suite.
It is the executable spec for exactly the concepts the C server must
implement. The C server is the implementation; the model is the spec and
the tests. It is not shipped code in the SevynOS image.

The package keeps its established name, `@sevynos/graphics`: the SevynOS
v1 tree imports it by that name, and SevynOS consumes this repo's model
by a pinned-commit fetch that materializes it at `graphics/core/`, which
is lockfile-identical only under that name. The name is historical; the
home is here now.

```sh
pnpm install
pnpm --filter @sevynos/graphics typecheck
pnpm --filter @sevynos/graphics test
pnpm --filter @sevynos/graphics build
```

## The compositor (`compositor/`)

```sh
meson setup build compositor
meson compile -C build
```

Needs: gcc, pkg-config, libwayland-dev, wayland-protocols, meson, ninja.
At seed time the only target is `sevyn-test-client` (the M1 test client);
Stage 2 adds the server. See `compositor/README.md`.

## How SevynOS consumes Genesis

SevynOS consumes this repo by **pinned-commit fetch at image-build
time** — not a submodule, not a vendored copy. The pin lives in
`v2/image/genesis-pin.txt` in sevynos (commit sha + tarball sha256); the
image builder fetches
`https://codeload.github.com/realkevonporter/genesis/tar.gz/<sha>`,
verifies the sha256, and builds the compositor + test client from the
fetched tree. A builder env override (`SEVYN_GENESIS_SRC`) lets a
developer build against a local checkout without bumping the pin. The
TypeScript model is consumed the same way: a fetch script materializes
`model/` at `graphics/core/` in sevynos (gitignored, generated) before
`pnpm install`, so the workspace lockfile stays valid.

## Provenance

Seeded 2026-11-19 from `realkevonporter/sevynos` (main `f3ce6739`):

- `compositor/tests/test-client.c` ← `v2/compositor/test-client.c`
  (last changed in `5fef893`, "v2 M1 follow-up" #86).
- `model/` ← `graphics/core/` (the whole package; the genesis-*
  compositor/display core's import closure spans `window/` and
  `errors/`, so the package moves whole, not sliced).
- `COMPOSITOR_PLAN.md` ← sevynos `COMPOSITOR_PLAN.md` (sevynos PR #135).

Clean move, no history preserved: the push path (git-database API) cannot
transport local git history, so the seed is a single initial commit with
this attribution. The moved files carry
`SPDX-License-Identifier: GPL-3.0-or-later` headers.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
