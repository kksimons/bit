import { getVersion } from "@tauri-apps/api/app";

// Settings window. Placeholder until the agent/voice/tools config lands (M4–M6).
const status = document.querySelector<HTMLElement>("#status");
getVersion()
  .then((v) => {
    if (status) status.textContent = `Bit v${v} · early build`;
  })
  .catch(() => {
    /* running outside Tauri (e.g. plain vite preview) */
  });
