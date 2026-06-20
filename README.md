# bit

A desktop "pet": a floating, transparent, always-on-top geometric
companion you mostly *talk* to. It transcribes your speech locally, hands it
to an LLM agent that carries out what you asked using a growing toolset, and
answers out loud with **only "yes" or "no"** — snapping its geometry between
forms for yes and no.

## Status

Early WIP. Working today:

- Transparent, frameless, always-on-top, draggable overlay (Tauri v2).
- Three procedural polyhedra it morphs between:
  - **neutral** — stellated icosahedron (cyan)
  - **yes** — octahedron (amber)
  - **no** — stellated dodecahedron (red)
- Symmetric "contract → swap → expand" form-change animation, per-mood spin, gentle hover bob.
- Yes/No voice playback synced to the shape snap.

Dev keys (click the Bit to focus first): `y` = yes, `n` = no, `t` = thinking,
`l` = listening, `space` = neutral.

## Develop

### Prerequisites

- macOS (it's a Tauri desktop app built for the Mac)
- [Bun](https://bun.sh) (JS runtime + package manager)
- A Rust toolchain (`rustup`) — Tauri compiles the native core
- Xcode Command Line Tools (`xcode-select --install`)

### Run the dev build

```bash
bun install            # install JS deps (also runs once after pulling)
bun run tauri dev      # compiles the Rust core + starts the Vite dev server
```

The first launch downloads the speech-to-text model (Parakeet) on demand, so it
needs network. The Bit appears centered on screen; the tray icon (top-right)
opens Settings and quits.

### Where things live

- `src/` — the overlay frontend (Three.js Bit renderer) and the Settings UI
- `src-tauri/src/` — the Rust core: agent loop, tools, MCP client, STT, motion
- `config.html` / `src/config/` — the Settings window
- App state on disk lives under `~/Library/Application Support/ca.magsolar.bit/`
  (settings, API key, workflows, MCP servers)

### Lint, typecheck, tests

```bash
bun run check          # everything: biome (TS/HTML/CSS) + cargo clippy/fmt + tsc
bun run lint           # biome only (frontend)
bun run lint:fix       # biome with --write (auto-fix)
bun run lint:rust      # cargo fmt --check + clippy with -D warnings
bun run typecheck      # tsc --noEmit
```

Rust unit tests (including a live MCP handshake test, ignored by default):

```bash
cd src-tauri
cargo test --lib                                   # unit tests
cargo test --lib -- --ignored gmail_handshake      # live test (needs npx + network)
```

### A note on dev builds

The "Launch at sign in" setting is **intentionally disabled in dev builds** —
the dev binary loads its UI from the Vite dev server, which isn't running after a
reboot, so autostarting it would boot to a blank window. Build and install the
release app (below) to use launch-on-login.

## Build & install (production)

```bash
bun run tauri build
```

Produces, under `src-tauri/target/release/bundle/`:

- `macos/bit.app` — the app bundle
- `dmg/bit_<version>_aarch64.dmg` — a redistributable installer

### Install it

```bash
# replace any previous install
rm -rf /Applications/bit.app
cp -R src-tauri/target/release/bundle/macos/bit.app /Applications/

# clear Gatekeeper quarantine (the app is ad-hoc signed, not notarized)
xattr -cr /Applications/bit.app
```

Then launch it from Spotlight or `/Applications/bit.app`. The **Launch at sign
in** toggle (Settings → General) works in this build — it registers the
installed app as a login item, so Bit appears each time you sign in.

## Tech

Tauri v2 (Rust) · TypeScript · Three.js · local speech-to-text (Parakeet via
ONNX Runtime) · MCP (Model Context Protocol) client
