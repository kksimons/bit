import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { Bit } from "./bit/bit";
import type { BitState } from "./bit/shapes";

const canvas = document.querySelector<HTMLCanvasElement>("#bit-canvas");
if (!canvas) throw new Error("#bit-canvas not found");
const bit = new Bit(canvas);

// Backend drives the Bit's form (listening / thinking / neutral).
void listen<string>("bit-state", (e) => {
  bit.setState(e.payload as BitState, 0);
});

// Final verdict: say yes/no 1–3 times for personality.
void listen<{ kind: "yes" | "no"; times: number }>("bit-verdict", (e) => {
  bit.react(e.payload.kind, e.payload.times);
});

// Backend reports what it transcribed (no agent yet — just surface it).
void listen<string>("transcript", (e) => {
  if (e.payload) console.log("[bit] heard:", e.payload);
});

// Custom drag with fling physics: the Rust side follows the cursor while
// dragging, tracks release velocity, and throws the Bit with momentum on let-go.
let dragging = false;
canvas.addEventListener("pointerdown", (e) => {
  if (e.button !== 0) return;
  dragging = true;
  try {
    canvas.setPointerCapture(e.pointerId);
  } catch {
    /* ignore */
  }
  void invoke("start_drag");
});
const endDrag = (e: PointerEvent) => {
  if (!dragging) return;
  dragging = false;
  try {
    canvas.releasePointerCapture(e.pointerId);
  } catch {
    /* ignore */
  }
  void invoke("end_drag");
};
canvas.addEventListener("pointerup", endDrag);
canvas.addEventListener("pointercancel", endDrag);

// Dev harness: exercise states from the keyboard until the agent drives them.
window.addEventListener("keydown", (e) => {
  switch (e.key.toLowerCase()) {
    case "y":
      bit.setState("yes");
      break;
    case "n":
      bit.setState("no");
      break;
    case "t":
      bit.setState("thinking", 0);
      break;
    case "l":
      bit.setState("listening", 0);
      break;
    case " ":
      bit.setState("neutral", 0);
      break;
  }
});
