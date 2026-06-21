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

const $ = (id: string) => {
  const el = document.getElementById(id);
  if (!el) throw new Error(`element #${id} not found`);
  return el;
};
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
  size: number;
}

const elSel = (id: string) => document.getElementById(id) as HTMLSelectElement;

const PROVIDER_DEFAULTS: Record<string, { base_url: string; model: string; label: string }> = {
  zai: { base_url: "https://api.z.ai/api/anthropic", model: "glm-5.2", label: "Z.AI" },
  anthropic: {
    base_url: "https://api.anthropic.com",
    model: "claude-sonnet-4-6",
    label: "Anthropic",
  },
  openai: { base_url: "https://api.openai.com/v1", model: "gpt-4o", label: "OpenAI" },
  openrouter: {
    base_url: "https://openrouter.ai/api/v1",
    model: "openai/gpt-4o",
    label: "OpenRouter",
  },
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
  elInput("size").value = String(s.size);
  $("size_val").textContent = `${s.size.toFixed(2)}×`;
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
      // The backend takes a single `settings: Settings` struct (Tauri
      // deserializes this object straight into it) + a separate `apiKey`.
      settings: {
        provider: elSel("provider").value,
        base_url: elInput("base_url").value.trim(),
        model: elInput("model").value.trim(),
        developer_mode: elInput("dev_mode").checked,
        size: parseFloat(elInput("size").value),
        stt_model: currentSttModel(),
      },
      apiKey: key.length > 0 ? key : null,
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

// ================= Bit size =================
// Live-preview (resize the overlay as you drag) and persist on release.
const sizeInput = elInput("size");
sizeInput.addEventListener("input", () => {
  const v = parseFloat(sizeInput.value);
  $("size_val").textContent = `${v.toFixed(2)}×`;
  void invoke("set_bit_size", { scale: v }).catch(() => {});
});
sizeInput.addEventListener("change", () => {
  void persistSettings();
});

// ================= Do Not Disturb =================
async function refreshDnd() {
  const ready = await invoke<boolean>("dnd_status").catch(() => false);
  $("dnd_status").textContent = ready ? "Ready." : "Not set up yet — one-time setup needed.";
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
    card.querySelector<HTMLInputElement>(".switch input")?.addEventListener("change", async (e) => {
      wf.enabled = (e.target as HTMLInputElement).checked;
      await invoke("save_workflow", { workflow: wf }).catch(() => {});
      await loadWorkflows();
    });
    card.querySelector(".run")?.addEventListener("click", async () => {
      try {
        await invoke("run_workflow", { name: wf.name });
      } catch (e) {
        alert(`Run failed: ${e}`);
      }
    });
    card.querySelector(".edit")?.addEventListener("click", () => openEditor(wf));
    card.querySelector(".del")?.addEventListener("click", async () => {
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
  editing.steps.forEach((step, i) => {
    root.appendChild(stepRow(step, i));
  });
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
        textField("Milliseconds", String(step.ms), (v) => (step.ms = parseInt(v, 10) || 0)),
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

// ================= MCP connections =================
// Gallery-first UX: preset buttons (Gmail, …) that only need a credential,
// with a raw stdio editor hidden under “Advanced” for power users.

interface PresetFieldView {
  env_key: string;
  label: string;
  placeholder: string;
  secret: boolean;
}
interface PresetView {
  id: string;
  label: string;
  description: string;
  command: string;
  args: string[];
  fields: PresetFieldView[];
}
interface McpServerView {
  name: string;
  transport: string;
  command: string;
  args: string[];
  env: Record<string, string>;
  url: string;
  enabled: boolean;
  preset: string;
  disabled_tools: string[];
  connected: boolean;
  tool_count: number;
  error: string | null;
}
interface McpServer {
  name: string;
  transport: string;
  command: string;
  args: string[];
  env: Record<string, string>;
  url: string;
  enabled: boolean;
  preset: string;
  disabled_tools: string[];
}
interface ToolView {
  name: string;
  description: string;
  destructive: boolean;
  enabled: boolean;
}

let presets: PresetView[] = [];
/// The preset the credential editor is collecting fields for (null when closed).
let editingPreset: PresetView | null = null;

async function loadPresets() {
  presets = await invoke<PresetView[]>("get_mcp_presets").catch(() => []);
  renderPresets();
}

function renderPresets() {
  const root = $("mcp_presets");
  root.innerHTML = "";
  // Only show presets the user hasn’t added yet (avoid duplicate connections).
  const added = new Set(mcpServers.map((s) => s.preset));
  for (const p of presets) {
    if (added.has(p.id)) continue;
    const btn = document.createElement("button");
    btn.type = "button";
    btn.className = "preset-btn";
    btn.innerHTML = `+ ${escapeHtml(p.label)}<span class="preset-desc">${escapeHtml(p.description)}</span>`;
    btn.addEventListener("click", () => openPresetEditor(p));
    root.appendChild(btn);
  }
}

let mcpServers: McpServerView[] = [];

async function loadMcpServers() {
  mcpServers = await invoke<McpServerView[]>("get_mcp_servers").catch(() => []);
  renderMcpServers();
  renderPresets();
}

function renderMcpServers() {
  const root = $("mcp_list");
  root.innerHTML = "";
  if (mcpServers.length === 0) return;
  for (const s of mcpServers) {
    const card = document.createElement("div");
    card.className = "conn-card";
    const status = s.connected
      ? `<span class="conn-status ok">Connected · ${s.tool_count} tools</span>`
      : s.error
        ? `<span class="conn-status err">${escapeHtml(s.error)}</span>`
        : `<span class="conn-status pending">Not connected</span>`;
    const badge = s.transport === "http" ? ` <span class="tag">web</span>` : "";
    card.innerHTML = `
      <div class="row spread">
        <b>${escapeHtml(s.name)}${badge}</b>
        <label class="switch"><input type="checkbox" ${s.enabled ? "checked" : ""}/> on</label>
      </div>
      <div class="conn-status">${status}</div>
      <div class="row">
        <button class="tools ghost">Manage tools (${s.tool_count})</button>
        <button class="test ghost">Test connection</button>
        <button class="del ghost danger">Remove</button>
      </div>`;

    card.querySelector<HTMLInputElement>(".switch input")?.addEventListener("change", async (e) => {
      const enabled = (e.target as HTMLInputElement).checked;
      await invoke<McpServerView>("save_mcp_server", {
        server: { ...toServer(s), enabled },
      }).catch(() => {});
      await loadMcpServers();
    });
    card.querySelector(".tools")?.addEventListener("click", () => openToolsEditor(s.name));
    card.querySelector(".test")?.addEventListener("click", async (el) => {
      const btn = el.currentTarget as HTMLButtonElement;
      btn.disabled = true;
      btn.textContent = "Testing…";
      try {
        await invoke<number>("test_mcp_server", { name: s.name });
      } catch {
        // status refresh will surface the error message
      }
      btn.disabled = false;
      btn.textContent = "Test connection";
      await loadMcpServers();
    });
    card.querySelector(".del")?.addEventListener("click", async () => {
      if (confirm(`Remove the ${s.name} connection?`)) {
        await invoke("delete_mcp_server", { name: s.name }).catch(() => {});
        await loadMcpServers();
      }
    });
    root.appendChild(card);
  }
}

/// Flatten a server view back into the persistable shape (drops transient status).
function toServer(s: McpServerView): McpServer {
  return {
    name: s.name,
    transport: s.transport,
    command: s.command,
    args: s.args,
    env: s.env,
    url: s.url,
    enabled: s.enabled,
    preset: s.preset,
    disabled_tools: s.disabled_tools,
  };
}

function openPresetEditor(p: PresetView) {
  editingPreset = p;
  $("mcp_editor_title").textContent = `Add ${p.label}`;
  $("mcp_editor_desc").textContent = p.description;
  $("mcp_editor_status").textContent = "";
  // Render the credential fields the preset needs.
  const fields = $("mcp_fields");
  fields.innerHTML = "";
  for (const f of p.fields) {
    const wrap = document.createElement("label");
    wrap.textContent = f.label;
    const input = document.createElement("input");
    input.type = f.secret ? "password" : "text";
    input.dataset.envKey = f.env_key;
    input.placeholder = f.placeholder;
    input.autocomplete = "off";
    input.spellcheck = false;
    wrap.appendChild(input);
    fields.appendChild(wrap);
  }
  // Show per-preset help (currently Gmail only).
  $("mcp_help_gmail").classList.toggle("hidden", p.id !== "gmail");
  $("mcp_editor_backdrop").classList.remove("hidden");
}

function closePresetEditor() {
  editingPreset = null;
  $("mcp_editor_backdrop").classList.add("hidden");
}

async function savePreset() {
  if (!editingPreset) return;
  const env: Record<string, string> = {};
  let missing = false;
  for (const f of editingPreset.fields) {
    const input = $("mcp_fields").querySelector<HTMLInputElement>(`[data-env-key="${f.env_key}"]`);
    const val = input?.value.trim() ?? "";
    if (!val) missing = true;
    env[f.env_key] = val;
  }
  if (missing) {
    $("mcp_editor_status").textContent = "Please fill in all fields.";
    return;
  }
  const server: McpServer = {
    name: editingPreset.id,
    transport: "stdio",
    command: editingPreset.command,
    args: editingPreset.args,
    env,
    url: "",
    enabled: true,
    preset: editingPreset.id,
    disabled_tools: [],
  };
  $("mcp_editor_status").textContent = "Connecting…";
  try {
    await invoke("save_mcp_server", { server });
    await loadMcpServers();
    closePresetEditor();
  } catch (e) {
    $("mcp_editor_status").textContent = `Error: ${e}`;
  }
}

$("mcp_save").addEventListener("click", savePreset);
$("mcp_cancel").addEventListener("click", closePresetEditor);
$("mcp_editor_close").addEventListener("click", closePresetEditor);

// ================= Per-tool toggles =================
// Modal listing a server's tools with checkboxes + a “⚠ destructive” badge on
// any Bit flags possibly-destructive (real annotations when the server gives
// them, else a name heuristic). Toggling sends the full new denylist; saving is
// instant (no reconnect — filtering happens at advertise/call time).
let toolsServer: string | null = null;
let toolsList: ToolView[] = [];

async function openToolsEditor(name: string) {
  toolsServer = name;
  toolsList = [];
  $("mcp_tools_title").textContent = `${name} · tools`;
  $("mcp_tools_desc").textContent =
    "Turn off tools you don’t want the agent to have. ⚠ marks tools that may change or remove data.";
  $("mcp_tools_status").textContent = "Loading…";
  renderToolsList();
  $("mcp_tools_backdrop").classList.remove("hidden");
  try {
    toolsList = await invoke<ToolView[]>("get_mcp_tools", { name });
    // Show destructive first so the dangerous ones are easy to find/disable.
    toolsList.sort((a, b) => Number(b.destructive) - Number(a.destructive));
    $("mcp_tools_status").textContent = `${toolsList.length} tools`;
  } catch (e) {
    $("mcp_tools_status").textContent = `Couldn’t list tools: ${e}`;
  }
  renderToolsList();
}

function closeToolsEditor() {
  toolsServer = null;
  toolsList = [];
  $("mcp_tools_backdrop").classList.add("hidden");
}

function renderToolsList() {
  const root = $("mcp_tools_list");
  root.innerHTML = "";
  for (const t of toolsList) {
    const row = document.createElement("label");
    row.className = "tool-row";
    const warn = t.destructive ? ` <span class="tag warn">⚠ destructive</span>` : "";
    row.innerHTML = `<input type="checkbox" ${t.enabled ? "checked" : ""}/> <b>${escapeHtml(t.name)}</b>${warn}<br/><span class="muted small">${escapeHtml(t.description)}</span>`;
    row.querySelector<HTMLInputElement>("input")?.addEventListener("change", (e) => {
      t.enabled = (e.target as HTMLInputElement).checked;
      void persistTools();
    });
    root.appendChild(row);
  }
}

async function persistTools() {
  if (!toolsServer) return;
  // disabled = everything not currently enabled (the denylist).
  const disabled = toolsList.filter((t) => !t.enabled).map((t) => t.name);
  try {
    await invoke("set_mcp_disabled_tools", { name: toolsServer, disabled });
  } catch (e) {
    $("mcp_tools_status").textContent = `Couldn’t save: ${e}`;
  }
}

$("mcp_tools_close").addEventListener("click", closeToolsEditor);
$("mcp_tools_disable_dest")?.addEventListener("click", async () => {
  for (const t of toolsList) if (t.destructive) t.enabled = false;
  renderToolsList();
  await persistTools();
});
$("mcp_tools_enable_all")?.addEventListener("click", async () => {
  for (const t of toolsList) t.enabled = true;
  renderToolsList();
  await persistTools();
});

// Advanced: custom stdio server. Split the args + env text fields into arrays/map.
$("mcp_custom_add")?.addEventListener("click", async () => {
  const name = elInput("mcp_custom_name").value.trim();
  const command = elInput("mcp_custom_command").value.trim();
  const argsText = elInput("mcp_custom_args").value.trim();
  const envText = ($("mcp_custom_env") as HTMLTextAreaElement).value.trim();
  const status = $("mcp_custom_status");
  if (!name || !command) {
    status.textContent = "Name and command are required.";
    return;
  }
  const env: Record<string, string> = {};
  for (const line of envText.split("\n")) {
    const i = line.indexOf("=");
    if (i > 0) env[line.slice(0, i).trim()] = line.slice(i + 1);
  }
  const server: McpServer = {
    name,
    transport: "stdio",
    command,
    args: argsText ? argsText.split(/\s+/) : [],
    env,
    url: "",
    enabled: true,
    preset: "",
    disabled_tools: [],
  };
  status.textContent = "Connecting…";
  try {
    await invoke("save_mcp_server", { server });
    elInput("mcp_custom_name").value = "";
    elInput("mcp_custom_command").value = "";
    elInput("mcp_custom_args").value = "";
    ($("mcp_custom_env") as HTMLTextAreaElement).value = "";
    status.textContent = "Added. See the connection status above.";
    await loadMcpServers();
  } catch (e) {
    status.textContent = `Error: ${e}`;
  }
});

// Add a remote (HTTP) MCP server by URL. This kicks off the OAuth flow: the
// backend opens a browser for sign-in and runs a loopback callback server on
// :8473. Name defaults to the URL host so users only type one thing.
const deriveName = (url: string): string => {
  try {
    return new URL(url).hostname.replace(/^mcp\./, "").split(".")[0] || "service";
  } catch {
    return "service";
  }
};

$("mcp_url_add")?.addEventListener("click", async () => {
  const urlInput = elInput("mcp_url");
  const url = urlInput.value.trim();
  const status = $("mcp_url_status");
  if (!url) {
    status.textContent = "Paste a service URL first.";
    return;
  }
  if (!/^https?:\/\//i.test(url)) {
    status.textContent = "URL should start with http:// or https://";
    return;
  }
  const name = deriveName(url);
  status.textContent = "Opening your browser to sign in…";
  const btn = $("mcp_url_add") as HTMLButtonElement | null;
  if (btn) btn.disabled = true;
  try {
    // Blocks until the user signs in (or 5min timeout). The backend opens the
    // browser itself; the user just needs to complete consent there. Returns
    // the URL actually used (may differ from input if /sse → /mcp was applied).
    const resolved = await invoke<string>("add_http_server", { name, url });
    status.textContent =
      resolved !== url
        ? `Connected (using ${resolved} — Bit rewrote the /sse URL to /mcp, which it supports).`
        : "Connected. See the connection status above.";
    urlInput.value = "";
    await loadMcpServers();
  } catch (e) {
    status.textContent = `Couldn’t connect: ${e}`;
  } finally {
    if (btn) btn.disabled = false;
  }
});

// The backend wraps the autostart plugin in its own commands so it can refuse in
// dev builds (the debug binary can’t find its UI after a reboot). Nothing to
// persist — the OS registration is the source of truth.
async function loadAutostart() {
  try {
    elInput("autostart").checked = await invoke<boolean>("autostart_state");
  } catch (e) {
    // Reading can’t fail in a way the user needs to know about; just log it.
    console.warn("[bit] autostart state unavailable:", e);
  }
}

elInput("autostart")?.addEventListener("change", async () => {
  const on = elInput("autostart").checked;
  try {
    await invoke("set_autostart", { enabled: on });
  } catch (e) {
    elInput("autostart").checked = !on; // revert on failure
    // The dev-build refusal message is actionable — show it as an alert so it
    // isn’t lost in the console.
    alert(`${e}`);
  }
});

// ================= Transcription models =================
// A picker (Handy-style): each model is a card with name, size, languages, a
// download button (with progress), an activate button, and an “active” badge.
// First run with no model → the backend opens Settings; this list shows what
// to do.

interface SttModelView {
  id: string;
  name: string;
  description: string;
  languages: string;
  size_mb: number;
  downloaded: boolean;
  active: boolean;
}
let sttModels: SttModelView[] = [];

async function loadSttModels() {
  sttModels = await invoke<SttModelView[]>("get_stt_models").catch(() => []);
  renderSttModels();
}

function renderSttModels() {
  const root = $("stt_models");
  root.innerHTML = "";
  for (const m of sttModels) {
    const card = document.createElement("div");
    card.className = `model-card${m.active ? " active" : ""}`;
    const activeBadge = m.active ? ` <span class="tag">active</span>` : "";
    card.innerHTML = `
      <div class="row spread">
        <b>${escapeHtml(m.name)}${activeBadge}</b>
        <span class="muted small">${m.size_mb} MB</span>
      </div>
      <div class="model-meta">${escapeHtml(m.description)} · ${escapeHtml(m.languages)}</div>
      <div class="row">
        ${actionButton(m)}
      </div>`;
    card.querySelector(".activate")?.addEventListener("click", async () => {
      try {
        await invoke("set_stt_model", { modelId: m.id });
        await loadSttModels();
      } catch (e) {
        $("stt_status").textContent = `Couldn’t switch: ${e}`;
      }
    });
    card.querySelector(".download")?.addEventListener("click", async (el) => {
      const btn = el.currentTarget as HTMLButtonElement;
      btn.disabled = true;
      btn.textContent = "Downloading…";
      $("stt_status").textContent = `Downloading ${m.name} (~${m.size_mb} MB)…`;
      try {
        await invoke("download_stt_model", { modelId: m.id });
        $("stt_status").textContent = `${m.name} downloaded.`;
      } catch (e) {
        $("stt_status").textContent = `Download failed: ${e}`;
      }
      await loadSttModels();
    });
    card.querySelector(".del")?.addEventListener("click", async () => {
      if (confirm(`Delete the ${m.name} model files from your Mac?`)) {
        try {
          await invoke("delete_stt_model", { modelId: m.id });
        } catch (e) {
          $("stt_status").textContent = `${e}`;
        }
        await loadSttModels();
      }
    });
    root.appendChild(card);
  }
}

function actionButton(m: SttModelView): string {
  if (m.active) {
    return `<span class="muted small">In use</span>`;
  }
  if (!m.downloaded) {
    return `<button type="button" class="download ghost">Download</button>`;
  }
  return `<button type="button" class="activate">Use this</button>
          <button type="button" class="del ghost danger">Delete</button>`;
}

/// The active model id, for `persistSettings` to round-trip. Falls back to the
/// default if nothing's loaded yet (shouldn't happen — loadSttModels runs first).
function currentSttModel(): string {
  return sttModels.find((m) => m.active)?.id ?? "parakeet-v2";
}

// ================= init =================
void loadSettings();
void refreshDnd();
void loadWorkflows();
void loadPresets();
void loadMcpServers();
void loadSttModels();
void loadAutostart();
