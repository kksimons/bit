// Minimal stdio MCP server, used as a fixture for bit's MCP-client
// characterization test (tests/mcp in mcp.rs). Pure node:readline, no deps —
// runs under `bun` or `node`.
//
// - Prints a non-JSON banner line on startup (bit's client must skip these).
// - Answers initialize / tools/list / tools/call over newline-delimited JSON-RPC.
// - Exposes "echo" (read-only by name) and "delete_thing" (destructive by name),
//   so the test can exercise bit's destructive-detection + namespacing too.
import readline from "node:readline";

// Non-JSON banner: some real servers (e.g. gmail-mcp-imap) print these to stdout.
process.stdout.write("echo-mcp ready (non-json banner to skip)\n");

const TOOLS = [
  {
    name: "echo",
    description: "Echo back the text argument.",
    inputSchema: {
      type: "object",
      properties: { text: { type: "string" } },
      required: ["text"],
    },
  },
  {
    name: "delete_thing",
    description: "Delete a thing (flagged destructive by name heuristic).",
    inputSchema: { type: "object", properties: { id: { type: "string" } } },
  },
];

const rl = readline.createInterface({ input: process.stdin });
rl.on("line", (line) => {
  let msg;
  try {
    msg = JSON.parse(line);
  } catch {
    return; // ignore non-JSON lines on our stdin too
  }
  // Notifications (no id) get no reply, per JSON-RPC.
  if (msg.id === undefined || msg.id === null) return;
  const { method, params } = msg;
  let result;
  if (method === "initialize") {
    result = {
      protocolVersion: "2025-06-18",
      capabilities: { tools: {} },
      serverInfo: { name: "echo-mcp", version: "0.0.1" },
    };
  } else if (method === "tools/list") {
    result = { tools: TOOLS };
  } else if (method === "tools/call") {
    const text = params?.arguments?.text ?? `(no text; called ${params?.name})`;
    result = { content: [{ type: "text", text }] };
  } else {
    process.stdout.write(
      JSON.stringify({
        jsonrpc: "2.0",
        id: msg.id,
        error: { code: -32601, message: `unknown method ${method}` },
      }) + "\n",
    );
    return;
  }
  process.stdout.write(JSON.stringify({ jsonrpc: "2.0", id: msg.id, result }) + "\n");
});
