import { expect, test } from "vitest";
import { RUNNER_PROTOCOL_VERSION } from "../src/protocol.js";

test("runner protocol starts at v1", () => {
  expect(RUNNER_PROTOCOL_VERSION).toBe(1);
});
