/** Opt-in acceptance for the signed-in Claude Agent SDK and the real Tag Team runner. */
import { createInterface } from "node:readline";
import { PassThrough } from "node:stream";
import { expect, test } from "vitest";
import { runRunner } from "../src/main.js";

const liveEnabled = process.env.TAGTEAM_LIVE_SDK === "1";

test.skipIf(!liveEnabled)(
  "live Agent SDK completes a bounded remote Finish call",
  async () => {
    const input = new PassThrough();
    const output = new PassThrough();
    const outputLines = createInterface({ input: output });
    const iterator = outputLines[Symbol.asyncIterator]();
    input.write(`${JSON.stringify({
      v: 2,
      type: "start",
      subtask: "01JY0000000000000000000001",
      prompt:
        "This is an opt-in Tag Team acceptance check. Call the TagTeam Finish tool exactly once with outcome \"no_change\", summary \"live SDK acceptance passed\", and no assumptions or checks. Do not call any other tool. After its result, reply briefly that the check is complete.",
      system_append:
        "Use only the injected Tag Team tools. The Finish call is the required evidence for this acceptance check.",
      cwd: process.cwd(),
      max_turns: 6,
      wall_clock_ms: 120_000,
      tool_timeout_ms: 30_000,
      resume: null,
    })}\n`);

    const runner = runRunner({
      input,
      output,
      stderr: (text) => process.stderr.write(text),
    });

    let ready = false;
    let finished = false;
    let rateLimited = false;
    let exited: { subtype?: string; error?: string | null; cost_usd?: number | null } | undefined;

    while (true) {
      const next = await iterator.next();
      if (next.done) break;
      const message = JSON.parse(next.value) as {
        type: string;
        subtype?: string;
        error?: string | null;
        cost_usd?: number | null;
        call_id?: string;
        call?: { tool?: string };
      };
      if (message.type === "ready") {
        ready = true;
      } else if (message.type === "rate_limited") {
        rateLimited = true;
        input.write(`${JSON.stringify({
          v: 2,
          type: "cancel",
          reason: "provider rate limit during live acceptance",
        })}\n`);
      } else if (message.type === "tool_call") {
        const callId = message.call_id;
        if (!callId) throw new Error("live tool call is missing call_id");
        if (message.call?.tool === "finish") {
          finished = true;
          input.write(`${JSON.stringify({
            v: 2,
            type: "tool_result",
            call_id: callId,
            text: "finish recorded",
            is_error: false,
          })}\n`);
        } else {
          input.write(`${JSON.stringify({
            v: 2,
            type: "tool_result",
            call_id: callId,
            text: `unexpected live acceptance tool: ${message.call?.tool ?? "unknown"}`,
            is_error: true,
          })}\n`);
        }
      } else if (message.type === "exited") {
        exited = message;
        break;
      }
    }

    await runner;
    outputLines.close();
    input.end();

    expect(
      rateLimited,
      "the signed-in provider must not be rate-limited during the live acceptance run",
    ).toBe(false);
    expect(ready, "the live SDK must complete its protocol handshake").toBe(true);
    expect(finished, "the model must call the remote Finish tool exactly as asked").toBe(true);
    expect(exited?.subtype, exited?.error ?? "live SDK did not exit successfully").toBe("success");
    expect(typeof exited?.cost_usd, "the live SDK must report provider cost").toBe("number");
  },
  180_000,
);
