# bit

A desktop "pet" homage to the **Bit** from TRON (1982): a floating, transparent,
always-on-top geometric companion you mostly *talk* to. It transcribes your speech
locally, hands it to an LLM agent that carries out what you asked using a growing
toolset, and answers out loud with **only "yes" or "no"** — snapping its geometry
between the canonical Bit forms.

## Status

Early WIP. Working today:

- Transparent, frameless, always-on-top, draggable overlay (Tauri v2).
- The three canonical Bit polyhedra, built procedurally:
  - **neutral** — stellated icosahedron (cyan)
  - **yes** — octahedron (amber)
  - **no** — stellated dodecahedron (red)
- Symmetric "contract → swap → expand" form-change animation, per-mood spin, gentle hover bob.
- Yes/No voice playback synced to the shape snap.

Dev keys (click the Bit to focus first): `y` = yes, `n` = no, `t` = thinking,
`l` = listening, `space` = neutral.

## Roadmap

- **M2** pet polish: accessory mode (no Dock icon), tray to quit/open settings, click-through on empty pixels.
- **M3** finish: visual tuning.
- **M4** local speech-to-text (Parakeet v2 via `transcribe-rs`, `cpal` + Silero VAD).
- **M5** agent harness over an Anthropic-compatible API (configurable base URL + key), reduce final answer to yes/no.
- **M6** toolset: built-in macOS actions (Focus/DND, etc.) + MCP client for extensibility.

## Develop

```bash
bun install
bun run tauri dev
```

## Tech

Tauri v2 (Rust) · TypeScript · Three.js
