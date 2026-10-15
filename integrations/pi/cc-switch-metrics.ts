/**
 * CC Switch's opt-in, local-only Pi timing observer.
 * No HTTP interception, credentials, prompts, raw events or token/cost copies.
 * Loaded by Pi from <agent-dir>/extensions/cc-switch-metrics.ts.
 * Structural types keep this file independent of Pi's npm package name.
 */
interface Message {
  role: string;
}

interface Timing {
  latencyMs: number;
  firstTokenMs: number | null;
  timingBasis: "provider_request" | "turn_start";
}

interface Events {
  turn_start: unknown;
  before_provider_request: unknown;
  message_update: {
    assistantMessageEvent: { type: string; delta?: string };
  };
  message_end: { message: Message };
  turn_end: { message: Message; messageEntryId?: string };
  session_start: unknown;
  session_switch: unknown;
}

interface Context {
  sessionManager: {
    getEntry(id: string): { timestamp: string } | undefined;
  };
}

interface MetricsAPI {
  on<K extends keyof Events>(
    name: K,
    handler: (event: Events[K], ctx: Context) => void,
  ): unknown;
  appendEntry(type: string, data: Record<string, unknown>): void;
}

export default function ccSwitchMetrics(pi: MetricsAPI) {
  let startedAt: number | undefined;
  let firstTokenAt: number | undefined;
  let timingBasis: Timing["timingBasis"] = "turn_start";
  let completed: Timing | undefined;

  const reset = () => {
    startedAt = undefined;
    firstTokenAt = undefined;
    completed = undefined;
    timingBasis = "turn_start";
  };

  pi.on("session_start", reset);
  pi.on("session_switch", reset);
  pi.on("turn_start", () => {
    reset();
    // Custom providers (including ACP) need not emit HTTP lifecycle events.
    // In that case this is a measured client-turn interval, not network TTFT.
    startedAt = performance.now();
  });
  pi.on("before_provider_request", () => {
    if (startedAt === undefined) return;
    // An HTTP retry starts a new attempt; don't keep a failed attempt's TTFT.
    startedAt = performance.now();
    firstTokenAt = undefined;
    completed = undefined;
    timingBasis = "provider_request";
  });
  pi.on("message_update", ({ assistantMessageEvent: event }) => {
    if (startedAt === undefined || firstTokenAt !== undefined) return;
    // Ignore stream start/keepalive/usage/empty deltas. Reasoning and tool JSON
    // are generated output too, so count their first nonempty delta as well.
    if (
      (event.type === "text_delta" ||
        event.type === "thinking_delta" ||
        event.type === "toolcall_delta") &&
      typeof event.delta === "string" &&
      event.delta.length > 0
    ) {
      firstTokenAt = performance.now();
    }
  });
  pi.on("message_end", ({ message }) => {
    if (message.role !== "assistant" || startedAt === undefined) return;
    const endedAt = performance.now();
    completed = {
      latencyMs: Math.max(0, Math.round(endedAt - startedAt)),
      firstTokenMs:
        firstTokenAt === undefined
          ? null
          : Math.max(0, Math.round(firstTokenAt - startedAt)),
      timingBasis,
    };
    startedAt = undefined;
  });
  pi.on("turn_end", (event, ctx) => {
    if (!completed || event.message.role !== "assistant") return;
    const timing = completed;
    completed = undefined;
    // turn_end is the first boundary where the assistant has a persisted ID.
    // Never use timestamps or model names alone to guess which usage row it is.
    const id = event.messageEntryId;
    const entry = id ? ctx.sessionManager.getEntry(id) : undefined;
    if (!id || !entry) return;
    pi.appendEntry("cc-switch.metrics.v1", {
      schemaVersion: 1,
      messageEntryId: id,
      messageEntryTimestamp: entry.timestamp,
      ...timing,
    });
  });
}
