export const RUNNER_PROTOCOL_VERSION = 1 as const;

export type JsonValue =
  | null
  | boolean
  | number
  | string
  | JsonValue[]
  | { [key: string]: JsonValue };
