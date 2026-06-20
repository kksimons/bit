import { invoke } from "@tauri-apps/api/core";
import { open as openDialog } from "@tauri-apps/plugin-dialog";

// ---- types (mirror the Rust serde model) ----
type Step =
  | { type: "shell"; command: string }
  | { type: "open_app"; name: string }
  | { type: "open_url"; url: string }
  | { type: "applescript"; script: string }
  | { type: "ghostty"; tabs: GhosttyTab[] }
  | { type: "focus"; enabled: boolean }
  | { type: "delay"; ms: number };
interface GhosttyTab {
  dir: string;
  command?: string;
  title?: string;
}
interface Workflow {
  id: string;
  name: string;
  trigger_phrases: string[];
  enabled: boolean;
  steps: Step[];
}

const $ = (id: string) => document.getElementById(id)!;
const elInput = (id: string) => $(id) as HTMLInputElement;

const STEP_LABELS: Record<Step["type"], string> = {
  ghostty: "Open terminal tabs",
  shell: "Run a command",
  open_app: "Open an app",
  open_url: "Open a website",
  focus: "Do Not Disturb",
  delay: "Wait",
  applescript: "Run AppleScript",
};

// ================= Agent settings =================
interface SettingsView {
  provider: string;
  base_url: string;
  model: string;
  has_key: boolean;
  developer_mode: boolean;
}

const elSel = (id: string) => document.getElementById(id) as HTMLSelectElement;

const PROVIDER_DEFAULTS: Record<string, { base_url: string; model: string; label: string }> = {
  zai: { base_url: "https://api.z.ai/api/anthropic", model: "glm-5.2", label: "Z.AI" },
  anthropic: { base_url: "https://api.anthropic.com", model: "claude-sonnet-4-6", label: "Anthropic" },
  openai: { base_url: "https://api.openai.com/v1", model: "gpt-4o", label: "OpenAI" },
  openrouter: { base_url: "https://openrouter.ai/api/v1", model: "openai/gpt-4o", label: "OpenRouter" },
};

async function loadSettings() {
  const s = await invoke<SettingsView>("get_settings");
  elSel("provider").value = s.provider;
  elInput("base_url").value = s.base_url;
  elInput("model").value = s.model;
  elInput("api_key").value = "";
  const label = PROVIDER_DEFAULTS[s.provider]?.label ?? "provider";
  elInput("api_key").placeholder = s.has_key
    ? "•••••••• saved — leave blank to keep"
    : `paste your ${label} key`;
  elInput("dev_mode").checked = s.developer_mode;
  $("status").textContent = s.has_key ? "Key saved." : "No API key set yet.";
}

// Switching provider prefills its default endpoint + model (and reminds you the
// key is per-provider).
elSel("provider").addEventListener("change", () => {
  const d = PROVIDER_DEFAULTS[elSel("provider").value];
  if (d) {
    elInput("base_url").value = d.base_url;
    elInput("model").value = d.model;
    elInput("api_key").value = "";
    elInput("api_key").placeholder = `paste your ${d.label} key`;
    $("status").textContent = `Switched to ${d.label} — enter that provider's key and Save.`;
  }
});

async function persistSettings(): Promise<boolean> {
  const key = elInput("api_key").value.trim();
  try {
    await invoke("save_settings", {
      provider: elSel("provider").value,
      baseUrl: elInput("base_url").value.trim(),
      model: elInput("model").value.trim(),
      apiKey: key.length > 0 ? key : null,
      developerMode: elInput("dev_mode").checked,
    });
    return true;
  } catch (e) {
    $("status").textContent = `Error saving: ${e}`;
    return false;
  }
}

$("save").addEventListener("click", async () => {
  if (await persistSettings()) {
    $("status").textContent = "Saved.";
    await loadSettings();
  }
});

// Developer mode toggles immediately (with a confirm, since it grants raw power).
elInput("dev_mode").addEventListener("change", async () => {
  if (
    elInput("dev_mode").checked &&
    !confirm(
      "Developer mode lets Bit run arbitrary shell/AppleScript commands by voice. Only enable if you understand the risk. Continue?",
    )
  ) {
    elInput("dev_mode").checked = false;
    return;
  }
  if (await persistSettings()) {
    $("status").textContent = elInput("dev_mode").checked
      ? "Developer mode ON."
      : "Developer mode off.";
  }
});

// ================= Do Not Disturb =================
async function refreshDnd() {
  const ready = await invoke<boolean>("dnd_status").catch(() => false);
  $("dnd_status").textContent = ready
    ? "Ready."
    : "Not set up yet — one-time setup needed.";
  $("dnd_setup").classList.toggle("hidden", ready);
  if (ready) $("dnd_help").classList.add("hidden");
}

async function dnd(enabled: boolean) {
  try {
    await invoke("set_dnd", { enabled });
    $("dnd_status").textContent = enabled ? "Do Not Disturb on." : "Do Not Disturb off.";
  } catch (e) {
    $("dnd_status").textContent = `${e}`;
    void refreshDnd();
  }
}
$("dnd_on").addEventListener("click", () => dnd(true));
$("dnd_off").addEventListener("click", () => dnd(false));
$("dnd_setup").addEventListener("click", async () => {
  $("dnd_help").classList.remove("hidden");
  await invoke("setup_dnd").catch(() => {});
});
$("dnd_recheck").addEventListener("click", refreshDnd);

// ================= Workflows list =================
async function loadWorkflows() {
  const list = await invoke<Workflow[]>("get_workflows").catch(() => []);
  const root = $("workflow_list");
  root.innerHTML = "";
  if (list.length === 0) {
    root.innerHTML = `<p class="muted">No workflows yet.</p>`;
    return;
  }
  for (const wf of list) {
    const card = document.createElement("div");
    card.className = "wf-card";
    const summary = wf.steps.map(stepSummary).join(" · ") || "no steps";
    const draft = wf.enabled ? "" : ` <span class="tag">disabled</span>`;
    card.innerHTML = `
      <div class="row spread">
        <b>${escapeHtml(wf.name)}${draft}</b>
        <label class="switch"><input type="checkbox" ${wf.enabled ? "checked" : ""}/> on</label>
      </div>
      <div class="muted small">${escapeHtml(summary)}</div>
      ${wf.enabled ? "" : `<div class="muted small">Disabled — review the steps, then switch on to allow it to run.</div>`}
      <div class="row">
        <button class="run">Run</button>
        <button class="edit ghost">Edit</button>
        <button class="del ghost danger">Delete</button>
      </div>`;
    card.querySelector<HTMLInputElement>(".switch input")!.addEventListener("change", async (e) => {
      wf.enabled = (e.target as HTMLInputElement).checked;
      await invoke("save_workflow", { workflow: wf }).catch(() => {});
      await loadWorkflows();
    });
    card.querySelector(".run")!.addEventListener("click", async () => {
      try {
        await invoke("run_workflow", { name: wf.name });
      } catch (e) {
        alert(`Run failed: ${e}`);
      }
    });
    card.querySelector(".edit")!.addEventListener("click", () => openEditor(wf));
    card.querySelector(".del")!.addEventListener("click", async () => {
      if (confirm(`Delete "${wf.name}"?`)) {
        await invoke("delete_workflow", { name: wf.name }).catch(() => {});
        await loadWorkflows();
      }
    });
    root.appendChild(card);
  }
}

function stepSummary(s: Step): string {
  switch (s.type) {
    case "ghostty":
      return `${STEP_LABELS.ghostty} (${s.tabs.length})`;
    case "shell":
      return `Run: ${s.command}`;
    case "open_app":
      return `Open ${s.name}`;
    case "open_url":
      return `Open ${s.url}`;
    case "focus":
      return s.enabled ? "DND on" : "DND off";
    case "delay":
      return `Wait ${s.ms}ms`;
    case "applescript":
      return "AppleScript";
  }
}

// ================= Editor =================
let editing: Workflow | null = null;

function blankStep(type: Step["type"]): Step {
  switch (type) {
    case "ghostty":
      return { type, tabs: [{ dir: "", command: "" }] };
    case "shell":
      return { type, command: "" };
    case "open_app":
      return { type, name: "" };
    case "open_url":
      return { type, url: "" };
    case "focus":
      return { type, enabled: true };
    case "delay":
      return { type, ms: 1000 };
    case "applescript":
      return { type, script: "" };
  }
}

function openEditor(wf: Workflow | null) {
  editing = wf
    ? structuredClone(wf)
    : { id: "", name: "", trigger_phrases: [], enabled: true, steps: [] };
  $("editor_title").textContent = wf ? "Edit workflow" : "New workflow";
  elInput("wf_name").value = editing.name;
  elInput("wf_triggers").value = editing.trigger_phrases.join(", ");
  $("editor_status").textContent = "";
  renderSteps();
  $("editor_backdrop").classList.remove("hidden");
}

function closeEditor() {
  editing = null;
  $("editor_backdrop").classList.add("hidden");
}

function renderSteps() {
  const root = $("steps");
  root.innerHTML = "";
  if (!editing) return;
  editing.steps.forEach((step, i) => root.appendChild(stepRow(step, i)));
}

function stepRow(step: Step, index: number): HTMLElement {
  const row = document.createElement("div");
  row.className = "step";
  const head = document.createElement("div");
  head.className = "row spread";
  head.innerHTML = `<b>${index + 1}. ${STEP_LABELS[step.type]}</b>`;
  const ctrls = document.createElement("div");
  ctrls.className = "row";
  ctrls.append(
    iconBtn("↑", () => moveStep(index, -1)),
    iconBtn("↓", () => moveStep(index, 1)),
    iconBtn("✕", () => removeStep(index)),
  );
  head.appendChild(ctrls);
  row.appendChild(head);

  const body = document.createElement("div");
  body.className = "step-body";
  buildStepFields(step, body);
  row.appendChild(body);
  return row;
}

function buildStepFields(step: Step, body: HTMLElement) {
  switch (step.type) {
    case "shell":
      body.appendChild(textField("Command", step.command, (v) => (step.command = v)));
      break;
    case "open_app":
      body.appendChild(textField("App name", step.name, (v) => (step.name = v)));
      break;
    case "open_url":
      body.appendChild(textField("URL", step.url, (v) => (step.url = v)));
      break;
    case "applescript":
      body.appendChild(textField("AppleScript", step.script, (v) => (step.script = v)));
      break;
    case "delay":
      body.appendChild(
        textField("Milliseconds", String(step.ms), (v) => (step.ms = parseInt(v) || 0)),
      );
      break;
    case "focus": {
      const lbl = document.createElement("label");
      lbl.className = "inline";
      const sel = document.createElement("select");
      sel.innerHTML = `<option value="on">Turn on</option><option value="off">Turn off</option>`;
      sel.value = step.enabled ? "on" : "off";
      sel.addEventListener("change", () => (step.enabled = sel.value === "on"));
      lbl.append("Do Not Disturb ", sel);
      body.appendChild(lbl);
      break;
    }
    case "ghostty":
      renderTabs(step, body);
      break;
  }
}

function renderTabs(step: { type: "ghostty"; tabs: GhosttyTab[] }, body: HTMLElement) {
  body.innerHTML = "";
  step.tabs.forEach((tab, i) => {
    const tabRow = document.createElement("div");
    tabRow.className = "tab-row";
    const head = document.createElement("div");
    head.className = "row spread";
    head.innerHTML = `<span class="muted small">Tab ${i + 1}</span>`;
    head.appendChild(
      iconBtn("✕", () => {
        step.tabs.splice(i, 1);
        renderTabs(step, body);
      }),
    );
    tabRow.appendChild(head);

    // folder picker
    const folder = document.createElement("div");
    folder.className = "row";
    const path = document.createElement("input");
    path.type = "text";
    path.placeholder = "~/repos/…";
    path.value = tab.dir;
    path.addEventListener("input", () => (tab.dir = path.value));
    const browse = document.createElement("button");
    browse.className = "ghost";
    browse.textContent = "Choose…";
    browse.addEventListener("click", async () => {
      const picked = await openDialog({ directory: true });
      if (typeof picked === "string") {
        tab.dir = picked;
        path.value = picked;
      }
    });
    folder.append(path, browse);
    tabRow.appendChild(folder);

    tabRow.appendChild(
      textField("Command (optional)", tab.command ?? "", (v) => (tab.command = v)),
    );
    body.appendChild(tabRow);
  });

  const add = document.createElement("button");
  add.className = "ghost";
  add.textContent = "+ Add tab";
  add.addEventListener("click", () => {
    step.tabs.push({ dir: "", command: "" });
    renderTabs(step, body);
  });
  body.appendChild(add);
}

function textField(label: string, value: string, onInput: (v: string) => void): HTMLElement {
  const wrap = document.createElement("label");
  wrap.textContent = label;
  const input = document.createElement("input");
  input.type = "text";
  input.value = value;
  input.addEventListener("input", () => onInput(input.value));
  wrap.appendChild(input);
  return wrap;
}

function iconBtn(label: string, onClick: () => void): HTMLButtonElement {
  const b = document.createElement("button");
  b.className = "icon ghost";
  b.textContent = label;
  b.addEventListener("click", onClick);
  return b;
}

function moveStep(i: number, dir: number) {
  if (!editing) return;
  const j = i + dir;
  if (j < 0 || j >= editing.steps.length) return;
  [editing.steps[i], editing.steps[j]] = [editing.steps[j], editing.steps[i]];
  renderSteps();
}
function removeStep(i: number) {
  if (!editing) return;
  editing.steps.splice(i, 1);
  renderSteps();
}

$("add_step").addEventListener("click", () => {
  if (!editing) return;
  const type = (elInput("add_step_type").value || "ghostty") as Step["type"];
  editing.steps.push(blankStep(type));
  renderSteps();
});

function collect(): Workflow | null {
  if (!editing) return null;
  const name = elInput("wf_name").value.trim();
  if (!name) {
    $("editor_status").textContent = "Please give the workflow a name.";
    return null;
  }
  editing.name = name;
  editing.trigger_phrases = elInput("wf_triggers")
    .value.split(",")
    .map((s) => s.trim())
    .filter(Boolean);
  return editing;
}

async function saveWorkflow(): Promise<boolean> {
  const wf = collect();
  if (!wf) return false;
  try {
    await invoke("save_workflow", { workflow: wf });
    await loadWorkflows();
    return true;
  } catch (e) {
    $("editor_status").textContent = `Error: ${e}`;
    return false;
  }
}

$("wf_save").addEventListener("click", async () => {
  if (await saveWorkflow()) closeEditor();
});
$("wf_run").addEventListener("click", async () => {
  const name = elInput("wf_name").value.trim();
  if (!(await saveWorkflow())) return;
  try {
    await invoke("run_workflow", { name });
    closeEditor();
  } catch (e) {
    $("editor_status").textContent = `Run failed: ${e}`;
  }
});
$("new_workflow").addEventListener("click", () => openEditor(null));
$("editor_close").addEventListener("click", closeEditor);

function escapeHtml(s: string): string {
  const d = document.createElement("div");
  d.textContent = s;
  return d.innerHTML;
}

// ================= init =================
void loadSettings();
void refreshDnd();
void loadWorkflows();
