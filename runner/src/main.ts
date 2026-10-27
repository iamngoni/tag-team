import { RUNNER_PROTOCOL_VERSION } from "./protocol.js";

// The Agent SDK loop is wired by the runner implementation unit. Keeping this
// entry point runnable now makes the package-level contract exercises real.
process.stdout.write(
  JSON.stringify({ v: RUNNER_PROTOCOL_VERSION, type: "ready", tools: [], mcp_servers: [] }) + "\n",
);
