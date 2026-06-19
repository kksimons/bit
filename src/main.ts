import { getCurrentWindow } from "@tauri-apps/api/window";
import { Bit } from "./bit/bit";

const canvas = document.querySelector<HTMLCanvasElement>("#bit-canvas")!;
const bit = new Bit(canvas);
const appWindow = getCurrentWindow();

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
