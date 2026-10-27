/** Versioned JSONL contracts; durable deliveries are identified and acknowledged explicitly. */
import { z } from "zod";

export const RUNNER_PROTOCOL_VERSION = 2 as const;

const optionalNullable = <T extends z.ZodType>(schema: T) => schema.nullable().optional();

export const assumptionSchema = z.object({
  what: z.string(),
  why: z.string(),
  from_timeout: z.boolean(),
});

export const checkSchema = z.object({
  what: z.string(),
  command: optionalNullable(z.string()),
  passed: z.boolean(),
  output_tail: z.string(),
});

export const remoteCallSchema = z.discriminatedUnion("tool", [
  z.object({ tool: z.literal("bash"), command: z.string(), timeout_ms: optionalNullable(z.number().int().nonnegative()) }),
  z.object({ tool: z.literal("read"), path: z.string(), offset: optionalNullable(z.number().int().nonnegative()), limit: optionalNullable(z.number().int().nonnegative()) }),
  z.object({ tool: z.literal("write"), path: z.string(), content: z.string() }),
  z.object({ tool: z.literal("edit"), path: z.string(), old: z.string(), new: z.string(), replace_all: z.boolean().optional().default(false) }),
  z.object({ tool: z.literal("glob"), pattern: z.string(), path: optionalNullable(z.string()) }),
  z.object({ tool: z.literal("grep"), pattern: z.string(), path: optionalNullable(z.string()), glob: optionalNullable(z.string()), ignore_case: z.boolean().optional().default(false), context: optionalNullable(z.number().int().nonnegative()), limit: optionalNullable(z.number().int().nonnegative()) }),
  z.object({ tool: z.literal("ask_main"), question_id: z.string().min(1), text: z.string().min(1), options: z.array(z.string()).optional().default([]), context: optionalNullable(z.string()) }),
  z.object({ tool: z.literal("report_progress"), note: z.string(), percent: optionalNullable(z.number().int().min(0).max(100)) }),
  z.object({ tool: z.literal("finish"), outcome: z.enum(["completed", "partial", "no_change", "blocked"]), summary: z.string(), assumptions: z.array(assumptionSchema).optional().default([]), checks: z.array(checkSchema).optional().default([]) }),
]);

export const hubToRunnerSchema = z.discriminatedUnion("type", [
  z.object({ v: z.literal(RUNNER_PROTOCOL_VERSION), type: z.literal("start"), subtask: z.string(), prompt: z.string(), system_append: z.string(), cwd: z.string(), max_turns: z.number().int().nonnegative(), wall_clock_ms: z.number().int().nonnegative(), tool_timeout_ms: z.number().int().nonnegative(), resume: optionalNullable(z.string()) }),
  z.object({ v: z.literal(RUNNER_PROTOCOL_VERSION), type: z.literal("tool_result"), call_id: z.string(), text: z.string(), is_error: z.boolean() }),
  z.object({ v: z.literal(RUNNER_PROTOCOL_VERSION), type: z.literal("cancel"), reason: z.string() }),
  z.object({ v: z.literal(RUNNER_PROTOCOL_VERSION), type: z.literal("steer"), delivery_id: z.string().min(1), text: z.string() }),
  z.object({ v: z.literal(RUNNER_PROTOCOL_VERSION), type: z.literal("answer"), delivery_id: z.string().min(1), question_id: z.string(), question_text: z.string(), call_id: z.string(), text: z.string(), timed_out: z.boolean() }),
]);

export type HubToRunner = z.infer<typeof hubToRunnerSchema>;
export type RemoteCall = z.infer<typeof remoteCallSchema>;

export type RunnerToHub =
  | { v: 2; type: "ready"; session_id: string; claude_code_version: string; tools: string[]; mcp_servers: string[] }
  | { v: 2; type: "tool_call"; call_id: string; call: RemoteCall }
  | { v: 2; type: "usage"; usage: { input_tokens: number; output_tokens: number; cache_read_tokens: number; turns: number; commands: number } }
  | { v: 2; type: "note"; text: string }
  | { v: 2; type: "rate_limited"; detail: string }
  | { v: 2; type: "exited"; subtype: string; error: string | null; session_id: string | null; cost_usd: number | null }
  | { v: 2; type: "delivery_ack"; delivery_id: string };
