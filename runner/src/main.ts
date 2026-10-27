import { createSdkMcpServer, query, tool, type Options, type Query, type SDKMessage, type SDKUserMessage } from "@anthropic-ai/claude-agent-sdk";
import { randomBytes, randomUUID } from "node:crypto";
import { once } from "node:events";
import type { Readable, Writable } from "node:stream";
import { z } from "zod";
import {
  hubToRunnerSchema,
  remoteCallSchema,
  RUNNER_PROTOCOL_VERSION,
  type HubToRunner,
  type RemoteCall,
  type RunnerToHub,
} from "./protocol.js";

const MAX_LINE_BYTES = 1024 * 1024;
const MAX_PENDING_TOOLS = 32;
const REMOTE_TOOLS = ["Bash", "Read", "Write", "Edit", "Glob", "Grep", "AskMain", "ReportProgress", "Finish"] as const;
const BUILTIN_WEB_TOOLS = ["WebFetch", "WebSearch"] as const;
const MCP_SERVER = "tagteam";

type DeferredToolResult = {
  resolve: (result: { text: string; is_error: boolean }) => void;
  reject: (error: Error) => void;
  timeout: NodeJS.Timeout;
};

export type RunnerDependencies = {
  input: Readable;
  output: Writable;
  stderr?: (text: string) => void;
  runQuery?: (params: { prompt: string | AsyncIterable<SDKUserMessage>; options: Options }) => Query;
};

class MessageStream implements AsyncIterable<SDKUserMessage> {
  private readonly queued: SDKUserMessage[] = [];
  private waiting: ((value: IteratorResult<SDKUserMessage>) => void) | undefined;
  private closed = false;

  push(text: string): void {
    if (this.closed) return;
    const message: SDKUserMessage = {
      type: "user",
      message: { role: "user", content: text },
      parent_tool_use_id: null,
      client_composed: true,
      priority: "now",
    };
    if (this.waiting) {
      const resolve = this.waiting;
      this.waiting = undefined;
      resolve({ done: false, value: message });
    } else {
      this.queued.push(message);
    }
  }

  close(): void {
    if (this.closed) return;
    this.closed = true;
    if (this.waiting) {
      const resolve = this.waiting;
      this.waiting = undefined;
      resolve({ done: true, value: undefined });
    }
  }

  [Symbol.asyncIterator](): AsyncIterator<SDKUserMessage> {
    return this;
  }

  next(): Promise<IteratorResult<SDKUserMessage>> {
    const message = this.queued.shift();
    if (message) return Promise.resolve({ done: false, value: message });
    if (this.closed) return Promise.resolve({ done: true, value: undefined });
    return new Promise((resolve) => { this.waiting = resolve; });
  }
}

const CROCKFORD = "0123456789ABCDEFGHJKMNPQRSTVWXYZ";

function ulid(now = Date.now()): string {
  let time = BigInt(now);
  let timestamp = "";
  for (let index = 0; index < 10; index += 1) {
    timestamp = CROCKFORD[Number(time & 31n)]! + timestamp;
    time >>= 5n;
  }
  let random = BigInt(`0x${randomBytes(10).toString("hex")}`);
  let randomText = "";
  for (let index = 0; index < 16; index += 1) {
    randomText = CROCKFORD[Number(random & 31n)]! + randomText;
    random >>= 5n;
  }
  return `${timestamp}${randomText}`;
}

class BoundedJsonlReader {
  private buffer = Buffer.alloc(0);
  private readonly queued: string[] = [];
  private waiting: { resolve: (value: IteratorResult<string>) => void; reject: (error: Error) => void } | undefined;
  private ended = false;
  private failure: Error | undefined;

  constructor(private readonly input: Readable) {
    input.on("data", this.onData);
    input.once("end", this.onEnd);
    input.once("error", this.onError);
  }

  next(): Promise<IteratorResult<string>> {
    const line = this.queued.shift();
    if (line !== undefined) return Promise.resolve({ done: false, value: line });
    if (this.failure) return Promise.reject(this.failure);
    if (this.ended) return Promise.resolve({ done: true, value: undefined });
    return new Promise((resolve, reject) => { this.waiting = { resolve, reject }; });
  }

  close(): void {
    if (this.ended) return;
    this.ended = true;
    this.detach();
    this.input.pause();
    this.waiting?.resolve({ done: true, value: undefined });
    this.waiting = undefined;
  }

  private readonly onData = (chunk: Buffer | string): void => {
    const bytes = Buffer.isBuffer(chunk) ? chunk : Buffer.from(chunk);
    let offset = 0;
    while (offset < bytes.length) {
      const newline = bytes.indexOf(0x0a, offset);
      const end = newline === -1 ? bytes.length : newline;
      const part = bytes.subarray(offset, end);
      if (this.buffer.length + part.length > MAX_LINE_BYTES) {
        this.fail(new Error(`input JSONL line exceeds ${MAX_LINE_BYTES} bytes`));
        return;
      }
      this.buffer = Buffer.concat([this.buffer, part]);
      if (newline !== -1) {
        this.deliver(this.buffer.toString("utf8").replace(/\r$/, ""));
        this.buffer = Buffer.alloc(0);
        offset = newline + 1;
      } else {
        offset = bytes.length;
      }
    }
  };

  private readonly onEnd = (): void => {
    if (this.buffer.length > 0) this.deliver(this.buffer.toString("utf8").replace(/\r$/, ""));
    this.buffer = Buffer.alloc(0);
    this.finish();
  };

  private readonly onError = (error: Error): void => this.fail(error);

  private deliver(line: string): void {
    if (this.waiting) {
      this.waiting.resolve({ done: false, value: line });
      this.waiting = undefined;
    } else {
      this.queued.push(line);
    }
  }

  private fail(error: Error): void {
    this.failure = error;
    this.finish();
  }

  private finish(): void {
    this.ended = true;
    this.detach();
    if (this.waiting) {
      if (this.failure) this.waiting.reject(this.failure);
      else this.waiting.resolve({ done: true, value: undefined });
      this.waiting = undefined;
    }
  }

  private detach(): void {
    this.input.off("data", this.onData);
    this.input.off("end", this.onEnd);
    this.input.off("error", this.onError);
  }
}

async function writeMessage(output: Writable, message: RunnerToHub): Promise<void> {
  const line = `${JSON.stringify(message)}\n`;
  if (Buffer.byteLength(line) > MAX_LINE_BYTES) throw new Error(`output JSONL line exceeds ${MAX_LINE_BYTES} bytes`);
  if (!output.write(line)) await once(output, "drain");
}

function toolSchemas(sendToolCall: (call: RemoteCall) => Promise<{ text: string; is_error: boolean }>) {
  const forward = <S extends z.ZodRawShape>(name: string, description: string, shape: S, remoteTool: RemoteCall["tool"]) =>
    tool(name, description, shape, async (args) => {
      const call = remoteCallSchema.parse({ tool: remoteTool, ...args });
      const result = await sendToolCall(call);
      return { content: [{ type: "text", text: result.text }], isError: result.is_error };
    });

  return [
    forward("Bash", "Run a shell command in the assigned Tag Team checkout.", {
      command: z.string(), timeout_ms: z.number().int().nonnegative().optional(),
    }, "bash"),
    forward("Read", "Read a file from the assigned Tag Team checkout.", {
      path: z.string(), offset: z.number().int().nonnegative().optional(), limit: z.number().int().nonnegative().optional(),
    }, "read"),
    forward("Write", "Write a file in the assigned Tag Team checkout.", {
      path: z.string(), content: z.string(),
    }, "write"),
    forward("Edit", "Replace text in a file in the assigned Tag Team checkout.", {
      path: z.string(), old: z.string(), new: z.string(), replace_all: z.boolean().optional(),
    }, "edit"),
    forward("Glob", "Find files by glob pattern in the assigned Tag Team checkout.", {
      pattern: z.string(), path: z.string().nullable().optional(),
    }, "glob"),
    forward("Grep", "Search files in the assigned Tag Team checkout.", {
      pattern: z.string(), path: z.string().nullable().optional(), glob: z.string().nullable().optional(),
      ignore_case: z.boolean().optional(), context: z.number().int().nonnegative().nullable().optional(), limit: z.number().int().nonnegative().nullable().optional(),
    }, "grep"),
    tool(
      "AskMain",
      "Ask the user's main Claude session a question when a required decision cannot be inferred safely. The call stays pending until the coordinator answers or the deadline expires.",
      {
        text: z.string().min(1),
        options: z.array(z.string()).max(16).optional(),
        context: z.string().nullable().optional(),
      },
      async (args) => {
        const call = remoteCallSchema.parse({
          tool: "ask_main",
          question_id: ulid(),
          text: args.text,
          options: args.options ?? [],
          context: args.context ?? null,
        });
        const result = await sendToolCall(call);
        return { content: [{ type: "text", text: result.text }], isError: result.is_error };
      },
    ),
    forward("ReportProgress", "Send progress to the Tag Team coordinator.", {
      note: z.string(), percent: z.number().int().min(0).max(100).nullable().optional(),
    }, "report_progress"),
    forward("Finish", "Finish the assigned Tag Team subtask with its outcome and checks.", {
      outcome: z.enum(["completed", "partial", "no_change", "blocked"]), summary: z.string(),
      assumptions: z.array(z.object({ what: z.string(), why: z.string(), from_timeout: z.boolean() })).optional(),
      checks: z.array(z.object({ what: z.string(), command: z.string().nullable().optional(), passed: z.boolean(), output_tail: z.string() })).optional(),
    }, "finish"),
  ];
}

function sdkOptions(start: Extract<HubToRunner, { type: "start" }>, server: ReturnType<typeof createSdkMcpServer>, abortController: AbortController): Options {
  const allowedTools = [
    ...REMOTE_TOOLS.map((name) => `mcp__${MCP_SERVER}__${name}`),
    ...BUILTIN_WEB_TOOLS,
  ];
  return {
    abortController,
    cwd: start.cwd,
    maxTurns: start.max_turns,
    ...(start.resume ? { resume: start.resume } : {}),
    systemPrompt: { type: "preset", preset: "claude_code", append: start.system_append },
    tools: [...BUILTIN_WEB_TOOLS],
    allowedTools,
    disallowedTools: [
      "Agent", "AgentOutput", "AskUserQuestion", "Bash", "Config", "EnterPlanMode", "EnterWorktree",
      "ExitPlanMode", "ExitWorktree", "Glob", "Grep", "KillShell", "ListMcpResources", "ListMcpServers",
      "NotebookEdit", "Read", "RemoteControl", "RequestLocalCommand", "SendMessage", "Skill", "Task",
      "TaskOutput", "TeamCreate", "TeamDelete", "TodoWrite", "Write", "Edit",
      "WorktreeCreate", "WorktreeRemove",
    ],
    permissionMode: "default",
    canUseTool: async (toolName, input) => {
      if (allowedTools.includes(toolName)) return { behavior: "allow", updatedInput: input };
      return { behavior: "deny", message: `Tool ${toolName} is not allowed by Tag Team runner policy.` };
    },
    mcpServers: { [MCP_SERVER]: server },
    strictMcpConfig: true,
    settingSources: ["user", "project", "local"],
  };
}

export async function runRunner({ input, output, stderr = () => undefined, runQuery = query }: RunnerDependencies): Promise<void> {
  const lines = new BoundedJsonlReader(input);
  let start: Extract<HubToRunner, { type: "start" }> | undefined;
  try {
    while (!start) {
      const next = await lines.next();
      if (next.done) return;
      if (!next.value.trim()) continue;
      const parsed = hubToRunnerSchema.parse(JSON.parse(next.value) as unknown);
      if (parsed.type !== "start") throw new Error("the first runner message must be start");
      start = parsed;
    }
  } catch (error) {
    await writeMessage(output, { v: RUNNER_PROTOCOL_VERSION, type: "exited", subtype: "protocol_error", error: errorMessage(error), session_id: null, cost_usd: null });
    return;
  }

  const abortController = new AbortController();
  const pending = new Map<string, DeferredToolResult>();
  const sessionId = { value: null as string | null };
  const latestCost = { value: null as number | null };
  const commandCount = { value: 0 };
  const canceled = { value: false };
  const messages = new MessageStream();
  messages.push(start.prompt);
  let closingReason: string | null = null;
  let queryHandle: Query | undefined;

  const settlePending = (error: Error) => {
    for (const item of pending.values()) {
      clearTimeout(item.timeout);
      item.reject(error);
    }
    pending.clear();
  };

  const sendToolCall = (call: RemoteCall) => new Promise<{ text: string; is_error: boolean }>((resolve, reject) => {
    if (pending.size >= MAX_PENDING_TOOLS) {
      reject(new Error("too many outstanding Tag Team tool calls"));
      return;
    }
    const callId = randomUUID();
    if (call.tool === "bash") commandCount.value += 1;
    const timeout = setTimeout(() => {
      pending.delete(callId);
      reject(new Error(`Tag Team tool call ${callId} timed out`));
    }, Math.max(1, start.tool_timeout_ms));
    timeout.unref();
    pending.set(callId, { resolve, reject, timeout });
    void writeMessage(output, { v: RUNNER_PROTOCOL_VERSION, type: "tool_call", call_id: callId, call }).catch((error: unknown) => {
      const item = pending.get(callId);
      if (item) {
        clearTimeout(item.timeout);
        pending.delete(callId);
        item.reject(asError(error));
      }
    });
  });

  const server = createSdkMcpServer({
    name: MCP_SERVER,
    version: "1.0.0",
    timeout: Math.max(1000, start.tool_timeout_ms),
    alwaysLoad: true,
    tools: toolSchemas(sendToolCall),
  });
  const wallTimer = setTimeout(() => {
    closingReason = "wall_clock_limit";
    abortController.abort(new Error("runner wall clock limit reached"));
    queryHandle?.close();
    settlePending(new Error("runner wall clock limit reached"));
  }, start.wall_clock_ms);
  wallTimer.unref();

  const inboundPump = (async () => {
    try {
      for (;;) {
        const next = await lines.next();
        if (next.done) {
          if (!canceled.value) {
            closingReason ??= "input_closed";
            abortController.abort(new Error("hub input closed"));
            queryHandle?.close();
            settlePending(new Error("hub input closed"));
          }
          messages.close();
          return;
        }
        if (!next.value.trim()) continue;
        let message: HubToRunner;
        try {
          message = hubToRunnerSchema.parse(JSON.parse(next.value) as unknown);
        } catch (error) {
          stderr(`Ignoring invalid hub message: ${errorMessage(error)}\n`);
          continue;
        }
        if (message.type === "tool_result") {
          const item = pending.get(message.call_id);
          if (!item) continue;
          clearTimeout(item.timeout);
          pending.delete(message.call_id);
          item.resolve({ text: message.text, is_error: message.is_error });
        } else if (message.type === "cancel") {
          canceled.value = true;
          closingReason = message.reason || "cancelled";
          abortController.abort(new Error(closingReason));
          queryHandle?.close();
          messages.close();
          settlePending(new Error(closingReason));
          return;
        } else if (message.type === "steer") {
          messages.push(`Tag Team steering update from the main session. Apply this direction to the assigned subtask:\n\n${message.text}`);
          await writeMessage(output, { v: RUNNER_PROTOCOL_VERSION, type: "note", text: "steering message delivered to the agent session" });
        } else if (message.type === "answer") {
          const item = pending.get(message.call_id);
          if (!item) {
            await writeMessage(output, { v: RUNNER_PROTOCOL_VERSION, type: "note", text: `answer ${message.question_id} arrived after its tool call closed` });
            continue;
          }
          clearTimeout(item.timeout);
          pending.delete(message.call_id);
          const text = message.timed_out
            ? `${message.text}\n\n[Tag Team deadline expired: record the assumption you used with from_timeout=true.]`
            : message.text;
          item.resolve({ text, is_error: false });
        } else {
          await writeMessage(output, { v: 1, type: "note", text: "Ignoring duplicate start message." });
        }
      }
    } catch (error) {
      closingReason = `input_error: ${errorMessage(error)}`;
      abortController.abort(asError(error));
      queryHandle?.close();
      settlePending(asError(error));
    }
  })();

  let subtype = "success";
  let failure: string | null = null;
  try {
    queryHandle = runQuery({ prompt: messages, options: sdkOptions(start, server, abortController) });
    for await (const message of queryHandle) {
      if (message.type === "system" && message.subtype === "init") {
        sessionId.value = message.session_id;
        const unexpectedMcp = message.mcp_servers.length !== 1
          || message.mcp_servers[0]?.name !== MCP_SERVER
          || message.mcp_servers[0]?.source !== "sdk";
        if (message.permissionMode === "bypassPermissions" || unexpectedMcp) {
          failure = message.permissionMode === "bypassPermissions"
            ? "SDK entered bypassPermissions; refusing to run remote tools."
            : "SDK loaded an MCP server outside the Tag Team allowlist; refusing to run remote tools.";
          closingReason = message.permissionMode === "bypassPermissions" ? "unsafe_permission_mode" : "unexpected_mcp_server";
          abortController.abort(new Error(failure));
          queryHandle.close();
          settlePending(new Error(failure));
          break;
        }
        await writeMessage(output, {
          v: 1,
          type: "ready",
          session_id: message.session_id,
          claude_code_version: message.claude_code_version,
          tools: [...REMOTE_TOOLS],
          mcp_servers: [MCP_SERVER],
        });
      } else if (message.type === "rate_limit_event" && message.rate_limit_info.status === "rejected") {
        await writeMessage(output, { v: 1, type: "rate_limited", detail: JSON.stringify(message.rate_limit_info) });
      } else if (message.type === "result") {
        subtype = message.subtype;
        if ("total_cost_usd" in message && Number.isFinite(message.total_cost_usd)) latestCost.value = message.total_cost_usd;
        if ("usage" in message) {
          await writeMessage(output, {
            v: 1,
            type: "usage",
            usage: {
              input_tokens: message.usage.input_tokens ?? 0,
              output_tokens: message.usage.output_tokens ?? 0,
              cache_read_tokens: message.usage.cache_read_input_tokens ?? 0,
              turns: message.num_turns,
              commands: commandCount.value,
            },
          });
        }
        if (message.subtype !== "success") failure = message.errors.join("; ") || message.subtype;
        else if (message.is_error) failure = message.result;
      }
    }
  } catch (error) {
    subtype = "error_during_execution";
    failure = errorMessage(error);
  } finally {
    clearTimeout(wallTimer);
    messages.close();
    lines.close();
    settlePending(new Error("runner session ended"));
    await inboundPump;
  }

  const endedSubtype = canceled.value ? "cancelled"
    : closingReason === "wall_clock_limit" ? "wall_clock_limit"
      : closingReason === "unsafe_permission_mode" || closingReason === "unexpected_mcp_server" ? closingReason
        : subtype;
  await writeMessage(output, {
    v: RUNNER_PROTOCOL_VERSION,
    type: "exited",
    subtype: endedSubtype,
    error: failure ?? (closingReason && closingReason !== "input_closed" ? closingReason : null),
    session_id: sessionId.value,
    cost_usd: latestCost.value,
  });
}

function asError(error: unknown): Error {
  return error instanceof Error ? error : new Error(String(error));
}

function errorMessage(error: unknown): string {
  return asError(error).message;
}

if (import.meta.url === `file://${process.argv[1]}`) {
  void runRunner({ input: process.stdin, output: process.stdout, stderr: (text) => process.stderr.write(text) }).catch((error: unknown) => {
    process.stderr.write(`${errorMessage(error)}\n`);
    process.exitCode = 1;
  });
}
