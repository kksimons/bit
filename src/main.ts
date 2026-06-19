import { getCurrentWindow } from "@tauri-apps/api/window";
import { listen } from "@tauri-apps/api/event";
import { Bit } from "./bit/bit";
import type { BitState } from "./bit/shapes";

const canvas = document.querySelector<HTMLCanvasElement>("#bit-canvas")!;
const bit = new Bit(canvas);
const appWindow = getCurrentWindow();

// Backend drives the Bit's form (listening / thinking / yes / no / neutral).
void listen<string>("bit-state", (e) => {
  const s = e.payload as BitState;
  const revert = s === "yes" || s === "no" ? 1500 : 0;
  bit.setState(s, revert);
});

// Backend reports what it transcribed (no agent yet — just surface it).
void listen<string>("transcript", (e) => {
  if (e.payload) console.log("[bit] heard:", e.payload);
});

// Drag the frameless window by grabbing the Bit itself.
canvas.addEventListener("pointerdown", (e) => {
  if (e.button === 0) {
    void appWindow.startDragging();
  }
});

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
