import { createInterface } from "node:readline";
import { PassThrough } from "node:stream";
import type { Options, Query, SDKMessage, SDKUserMessage } from "@anthropic-ai/claude-agent-sdk";
import { expect, test } from "vitest";
import { runRunner } from "../src/main.js";
import { hubToRunnerSchema, remoteCallSchema, RUNNER_PROTOCOL_VERSION } from "../src/protocol.js";

const start = {
  v: 1,
  type: "start",
  subtask: "01JY0000000000000000000001",
  prompt: "Fix the flaky test and prove it with cargo test.",
  system_append: "You run on behalf of the main Tag Team session.",
  cwd: "/tmp/tag-team-test",
  max_turns: 60,
  wall_clock_ms: 60_000,
  tool_timeout_ms: 60_000,
  resume: null,
};

test("runner protocol starts at v1 and accepts the Rust start and finish fixtures", async () => {
  expect(RUNNER_PROTOCOL_VERSION).toBe(1);
  const fixtureStart = await import("../../tests/fixtures/runner-protocol/start.json", { with: { type: "json" } });
  const fixtureFinish = await import("../../tests/fixtures/runner-protocol/finish.json", { with: { type: "json" } });
  expect(hubToRunnerSchema.parse(fixtureStart.default).type).toBe("start");
  const finish = (fixtureFinish.default as { call: unknown }).call;
  expect(remoteCallSchema.parse(finish)).toMatchObject({ tool: "finish", outcome: "completed" });
});

test("hub messages with a different protocol version fail validation", () => {
  expect(() => hubToRunnerSchema.parse({ ...start, v: 2 })).toThrow();
});

test("stdio hub tool calls are streamed and wait for their matching result", async () => {
  const input = new PassThrough();
  const output = new PassThrough();
  input.write(`${JSON.stringify(start)}\n`);
  const outputLines = createInterface({ input: output });
  const iterator = outputLines[Symbol.asyncIterator]();
  let capturedOptions: Options | undefined;

  const runQuery = ({ options }: { prompt: string | AsyncIterable<SDKUserMessage>; options: Options }): Query => {
    capturedOptions = options;
    let release: () => void = () => undefined;
    const sequence = (async function* () {
      yield sdkInit();
      const server = options.mcpServers?.tagteam;
      if (!server || !("instance" in server)) throw new Error("Tag Team MCP server missing");
      const registered = (server.instance as unknown as {
        _registeredTools: Record<string, { handler: (args: unknown, extra: unknown) => Promise<{ content: Array<{ text?: string }> }> }>;
      })._registeredTools;
      const result = await registered.Bash!.handler({ command: "pwd" }, {});
      expect(result.content[0]?.text).toBe("/workspace");
      yield sdkResult();
      await new Promise<void>((resolve) => { release = resolve; });
    })();
    return Object.assign(sequence, { close: () => release() }) as Query;
  };

  const runner = runRunner({ input, output, runQuery });
  expect(JSON.parse((await iterator.next()).value!).type).toBe("ready");
  const call = JSON.parse((await iterator.next()).value!) as { type: string; call_id: string; call: { tool: string } };
  expect(call).toMatchObject({ type: "tool_call", call: { tool: "bash", command: "pwd" } });
  input.write(`${JSON.stringify({ v: 1, type: "tool_result", call_id: call.call_id, text: "/workspace", is_error: false })}\n`);
  expect(JSON.parse((await iterator.next()).value!).type).toBe("usage");
  input.write(`${JSON.stringify({ v: 1, type: "cancel", reason: "test complete" })}\n`);
  expect(JSON.parse((await iterator.next()).value!)).toMatchObject({ type: "exited", subtype: "cancelled" });
  await runner;

  expect(capturedOptions).toMatchObject({
    tools: ["WebFetch", "WebSearch"],
    allowedTools: [
      "mcp__tagteam__Bash", "mcp__tagteam__Read", "mcp__tagteam__Write", "mcp__tagteam__Edit",
      "mcp__tagteam__Glob", "mcp__tagteam__Grep", "mcp__tagteam__AskMain",
      "mcp__tagteam__ReportProgress", "mcp__tagteam__Finish", "WebFetch", "WebSearch",
    ],
    strictMcpConfig: true,
    settingSources: ["user", "project", "local"],
    permissionMode: "default",
  });
  expect(Object.keys(capturedOptions?.mcpServers ?? {})).toEqual(["tagteam"]);
  const originalInput = { command: "pwd" };
  const permissionOptions = { signal: new AbortController().signal, toolUseID: "tool-1", requestId: "request-1" };
  const allowed = await capturedOptions?.canUseTool?.("mcp__tagteam__Bash", originalInput, permissionOptions);
  const web = await capturedOptions?.canUseTool?.("WebSearch", originalInput, permissionOptions);
  const denied = await capturedOptions?.canUseTool?.("Bash", originalInput, permissionOptions);
  expect(allowed).toMatchObject({ behavior: "allow", updatedInput: originalInput });
  expect(web).toMatchObject({ behavior: "allow", updatedInput: originalInput });
  expect(denied).toMatchObject({ behavior: "deny" });
  outputLines.close();
});

test("runner fails closed if the SDK reports bypassPermissions", async () => {
  const input = new PassThrough();
  const output = new PassThrough();
  input.write(`${JSON.stringify(start)}\n`);
  const outputLines = createInterface({ input: output });
  const iterator = outputLines[Symbol.asyncIterator]();
  const runQuery = ({ options }: { prompt: string | AsyncIterable<SDKUserMessage>; options: Options }): Query => {
    let closed = false;
    const sequence = (async function* () {
      yield { ...sdkInit(), permissionMode: "bypassPermissions" } as unknown as SDKMessage;
      await new Promise<void>((resolve) => {
        const timer = setInterval(() => {
          if (closed) { clearInterval(timer); resolve(); }
        }, 1);
      });
    })();
    return Object.assign(sequence, { close: () => { closed = true; } }) as Query;
  };
  const runner = runRunner({ input, output, runQuery });
  expect(JSON.parse((await iterator.next()).value!)).toMatchObject({ type: "exited", subtype: "unsafe_permission_mode" });
  await runner;
  outputLines.close();
});

test("runner fails closed if init reports a non-Tag Team MCP server", async () => {
  const input = new PassThrough();
  const output = new PassThrough();
  input.write(`${JSON.stringify(start)}\n`);
  const outputLines = createInterface({ input: output });
  const iterator = outputLines[Symbol.asyncIterator]();
  const runQuery = (): Query => {
    let closed = false;
    const sequence = (async function* () {
      yield {
        ...sdkInit(),
        mcp_servers: [
          { name: "tagteam", source: "sdk", status: "connected" },
          { name: "project-server", source: "project", status: "connected" },
        ],
      } as unknown as SDKMessage;
      await new Promise<void>((resolve) => {
        const timer = setInterval(() => {
          if (closed) { clearInterval(timer); resolve(); }
        }, 1);
      });
    })();
    return Object.assign(sequence, { close: () => { closed = true; } }) as Query;
  };
  const runner = runRunner({ input, output, runQuery });
  expect(JSON.parse((await iterator.next()).value!)).toMatchObject({ type: "exited", subtype: "unexpected_mcp_server" });
  await runner;
  outputLines.close();
});

test("normal SDK completion stops the hub reader while stdin remains open", async () => {
  const input = new PassThrough();
  const output = new PassThrough();
  input.write(`${JSON.stringify(start)}\n`);
  const outputLines = createInterface({ input: output });
  const iterator = outputLines[Symbol.asyncIterator]();
  const runQuery = (): Query => {
    const sequence = (async function* () {
      yield sdkInit();
      yield sdkResult();
    })();
    return Object.assign(sequence, { close: () => undefined }) as Query;
  };
  const runner = runRunner({ input, output, runQuery });
  expect(JSON.parse((await iterator.next()).value!).type).toBe("ready");
  expect(JSON.parse((await iterator.next()).value!).type).toBe("usage");
  await runner;
  expect(JSON.parse((await iterator.next()).value!)).toMatchObject({ type: "exited", subtype: "success" });
  expect(input.destroyed).toBe(false);
  outputLines.close();
  input.destroy();
});

test("AskMain stays pending until the matching hub answer arrives", async () => {
  const input = new PassThrough();
  const output = new PassThrough();
  input.write(`${JSON.stringify(start)}\n`);
  const outputLines = createInterface({ input: output });
  const iterator = outputLines[Symbol.asyncIterator]();
  let answerText: string | undefined;
  let questionId: string | undefined;

  const runQuery = ({ options }: { prompt: string | AsyncIterable<SDKUserMessage>; options: Options }): Query => {
    const sequence = (async function* () {
      yield sdkInit();
      const server = options.mcpServers?.tagteam;
      if (!server || !("instance" in server)) throw new Error("Tag Team MCP server missing");
      const registered = (server.instance as unknown as {
        _registeredTools: Record<string, { handler: (args: unknown, extra: unknown) => Promise<{ content: Array<{ text?: string }> }> }>;
      })._registeredTools;
      const result = await registered.AskMain!.handler({ text: "Which API should I preserve?", options: ["v1", "v2"] }, {});
      answerText = result.content[0]?.text;
      yield sdkResult();
    })();
    return Object.assign(sequence, { close: () => undefined }) as Query;
  };

  const runner = runRunner({ input, output, runQuery });
  expect(JSON.parse((await iterator.next()).value!).type).toBe("ready");
  const call = JSON.parse((await iterator.next()).value!) as {
    type: string;
    call_id: string;
    call: { tool: string; question_id: string; text: string; options: string[] };
  };
  expect(call).toMatchObject({
    type: "tool_call",
    call: { tool: "ask_main", text: "Which API should I preserve?", options: ["v1", "v2"] },
  });
  questionId = call.call.question_id;
  expect(questionId).toMatch(/^[0-9A-HJKMNP-TV-Z]{26}$/);
  input.write(`${JSON.stringify({
    v: 1,
    type: "answer",
    question_id: questionId,
    call_id: call.call_id,
    text: "Preserve v2 and add a compatibility adapter.",
    timed_out: false,
  })}\n`);
  expect(JSON.parse((await iterator.next()).value!).type).toBe("usage");
  expect(JSON.parse((await iterator.next()).value!)).toMatchObject({ type: "exited", subtype: "success" });
  await runner;
  expect(answerText).toBe("Preserve v2 and add a compatibility adapter.");
  outputLines.close();
});

test("steering is delivered on the streaming query input", async () => {
  const input = new PassThrough();
  const output = new PassThrough();
  input.write(`${JSON.stringify(start)}\n`);
  const outputLines = createInterface({ input: output });
  const iterator = outputLines[Symbol.asyncIterator]();
  let seen: string[] = [];
  let release: () => void = () => undefined;
  const steered = new Promise<void>((resolve) => { release = resolve; });

  const runQuery = ({ prompt }: { prompt: string | AsyncIterable<SDKUserMessage>; options: Options }): Query => {
    if (typeof prompt === "string") throw new Error("runner must use streaming input");
    void (async () => {
      for await (const message of prompt) {
        const content = message.message.content;
        const text = typeof content === "string" ? content : content.map((part) => "text" in part ? part.text : "").join("");
        seen.push(text);
        if (text.includes("Use the other migration path")) {
          release();
          return;
        }
      }
    })();
    let closed = false;
    const sequence = (async function* () {
      yield sdkInit();
      await steered;
      yield sdkResult();
    })();
    return Object.assign(sequence, { close: () => { closed = true; } }) as Query;
  };

  const runner = runRunner({ input, output, runQuery });
  expect(JSON.parse((await iterator.next()).value!).type).toBe("ready");
  input.write(`${JSON.stringify({ v: 1, type: "steer", text: "Use the other migration path." })}\n`);
  expect(JSON.parse((await iterator.next()).value!)).toMatchObject({ type: "note" });
  expect(JSON.parse((await iterator.next()).value!).type).toBe("usage");
  expect(JSON.parse((await iterator.next()).value!)).toMatchObject({ type: "exited", subtype: "success" });
  await runner;
  expect(seen[0]).toBe(start.prompt);
  expect(seen.some((text) => text.includes("Use the other migration path"))).toBe(true);
  outputLines.close();
});

function sdkInit(): SDKMessage {
  return {
    type: "system", subtype: "init", session_id: "session-1", claude_code_version: "2.1.0", cwd: "/tmp/tag-team-test",
    permissionMode: "default", mcp_servers: [{ name: "tagteam", source: "sdk", status: "connected" }], tools: [],
  } as unknown as SDKMessage;
}

function sdkResult(): SDKMessage {
  return {
    type: "result", subtype: "success", is_error: false, usage: { input_tokens: 10, output_tokens: 2, cache_read_input_tokens: 1 },
    num_turns: 1, total_cost_usd: 0.01, errors: [], session_id: "session-1",
  } as unknown as SDKMessage;
}
