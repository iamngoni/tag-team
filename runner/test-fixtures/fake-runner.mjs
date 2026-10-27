#!/usr/bin/env node
//! Deterministic Agent SDK runner substitute for the cross-process execution spine.
//!
//! It exercises only the public JSON-lines protocol: ready, one remote Bash call,
//! and finish. The hub owns the node; this process never touches the checkout.

import { createInterface } from "node:readline";

const version = 1;
const input = createInterface({ input: process.stdin, crlfDelay: Infinity });
const pending = [];
let waiter;

function receive(line) {
  const message = JSON.parse(line);
  if (waiter) {
    const resolve = waiter;
    waiter = undefined;
    resolve(message);
  } else {
    pending.push(message);
  }
}

input.on("line", receive);

function next() {
  const message = pending.shift();
  if (message) return Promise.resolve(message);
  return new Promise((resolve) => {
    waiter = resolve;
  });
}

function send(message) {
  process.stdout.write(`${JSON.stringify({ v: version, ...message })}\n`);
}

const start = await next();
if (start.type !== "start") throw new Error("first message was not start");

send({
  type: "ready",
  session_id: "fake-runner-session",
  claude_code_version: "fixture",
  tools: [
    "mcp__tagteam__Bash",
    "mcp__tagteam__Read",
    "mcp__tagteam__Write",
    "mcp__tagteam__Edit",
    "mcp__tagteam__Glob",
    "mcp__tagteam__Grep",
    "mcp__tagteam__ReportProgress",
    "mcp__tagteam__Finish",
  ],
  mcp_servers: ["tagteam"],
});

if (String(start.prompt).includes("cancellation")) {
  send({
    type: "tool_call",
    call_id: "fake-progress-1",
    call: { tool: "report_progress", note: "cancel-ready", percent: null },
  });
  await next();
  send({ type: "note", text: "cancel-ready" });
  await new Promise((resolve) => setTimeout(resolve, 500));
  send({
    type: "tool_call",
    call_id: "fake-cancel-bash-1",
    call: { tool: "bash", command: "sleep 30", timeout_ms: 30000 },
  });
  for (;;) {
    const message = await next();
    if (message.type === "cancel") break;
  }
  process.exit(0);
}

if (String(start.prompt).includes("cancel-during-check")) {
  send({
    type: "tool_call",
    call_id: "fake-check-finish-1",
    call: {
      tool: "finish",
      outcome: "completed",
      summary: "Fake runner requested an authoritative check.",
      assumptions: [],
      checks: [],
    },
  });
  await next();
  process.exit(0);
}

send({
  type: "tool_call",
  call_id: "fake-bash-1",
  call: { tool: "bash", command: "printf 'written by fake runner\\n' > result.txt", timeout_ms: 30000 },
});
const bashResult = await next();
if (bashResult.type !== "tool_result" || bashResult.is_error) {
  throw new Error(`remote Bash failed: ${JSON.stringify(bashResult)}`);
}

send({
  type: "tool_call",
  call_id: "fake-finish-1",
  call: {
    tool: "finish",
    outcome: "completed",
    summary: "Fake runner wrote the result file.",
    assumptions: [],
    checks: [],
  },
});
await next();
