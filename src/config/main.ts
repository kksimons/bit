import { invoke } from "@tauri-apps/api/core";

interface SettingsView {
  base_url: string;
  model: string;
  has_key: boolean;
}

const el = (id: string) => document.getElementById(id) as HTMLInputElement;
const setStatus = (t: string) => {
  const s = document.getElementById("status");
  if (s) s.textContent = t;
};

async function load() {
  try {
    const s = await invoke<SettingsView>("get_settings");
    el("base_url").value = s.base_url;
    el("model").value = s.model;
    el("api_key").value = "";
    el("api_key").placeholder = s.has_key
      ? "•••••••• saved — leave blank to keep"
      : "paste your Z.AI key";
    setStatus(s.has_key ? "Key saved in Keychain." : "No API key set yet.");
  } catch (e) {
    setStatus(`Error loading settings: ${e}`);
  }
}

document.getElementById("save")?.addEventListener("click", async () => {
  const key = el("api_key").value.trim();
  try {
    await invoke("save_settings", {
      baseUrl: el("base_url").value.trim(),
      model: el("model").value.trim(),
      apiKey: key.length > 0 ? key : null,
    });
    setStatus("Saved.");
    await load();
  } catch (e) {
    setStatus(`Error saving: ${e}`);
  }
});

void load();
