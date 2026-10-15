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
  timingBasis: "provider_request" | "model_call" | "turn_start";
}

interface Events {
  turn_start: unknown;
  context: unknown;
  before_provider_request: unknown;
  message_update: {
    assistantMessageEvent: { type: string; delta?: string };
  };
  message_end: { message: Message };
  turn_end: { message: Message; messageEntryId?: string };
  session_start: unknown;
  session_switch: unknown;
  agent_end: unknown;
}

interface SessionEntry {
  id?: string;
  timestamp: string;
  type?: string;
  message?: Message;
}

interface Context {
  sessionManager: {
    getEntry(id: string): SessionEntry | undefined;
    getBranch?(): SessionEntry[];
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
  let inTurn = false;
  let startedAt: number | undefined;
  let firstTokenAt: number | undefined;
  let timingBasis: Timing["timingBasis"] = "turn_start";
  // A Pi turn can contain several assistant model calls (tool call, then final
  // answer). Keep every one until SessionManager has persisted it.
  let completed = new Map<Message, Timing>();

  const reset = () => {
    inTurn = false;
    startedAt = undefined;
    firstTokenAt = undefined;
    completed.clear();
    timingBasis = "turn_start";
  };

  pi.on("session_start", reset);
  pi.on("session_switch", reset);
  pi.on("agent_end", reset);
  pi.on("turn_start", () => {
    reset();
    inTurn = true;
    // Custom providers (including ACP) need not emit HTTP lifecycle events.
    // In that case this is a measured client-turn interval, not network TTFT.
    startedAt = performance.now();
  });
  pi.on("context", () => {
    if (!inTurn) return;
    // Pi's common pre-model context hook also runs for extension providers.
    // This excludes turn setup, but includes later context/auth preparation:
    // label it model_call, never pretend it is a wire-level request boundary.
    startedAt = performance.now();
    firstTokenAt = undefined;
    timingBasis = "model_call";
  });
  pi.on("before_provider_request", () => {
    if (!inTurn) return;
    // An HTTP retry starts a new attempt; don't keep a failed attempt's TTFT.
    startedAt = performance.now();
    firstTokenAt = undefined;
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
    completed.set(message, {
      latencyMs: Math.max(0, Math.round(endedAt - startedAt)),
      firstTokenMs:
        firstTokenAt === undefined
          ? null
          : Math.max(0, Math.round(firstTokenAt - startedAt)),
      timingBasis,
    });
    startedAt = undefined;
  });
  pi.on("turn_end", (event, ctx) => {
    if (event.message.role !== "assistant" || completed.size === 0) return;
    // turn_end is the first point where all assistant messages in a Pi turn
    // have been persisted. This includes intermediate tool-call generations.
    // Match only by in-memory message object identity; never guess by timestamp,
    // model, text, or "latest assistant".
    const entries = ctx.sessionManager.getBranch?.() ?? [];
    let finalEntry = entries.find(
      (candidate) =>
        candidate.type === "message" && candidate.message === event.message,
    );
    if (!finalEntry && event.messageEntryId) {
      const candidate = ctx.sessionManager.getEntry(event.messageEntryId);
      if (candidate?.message === event.message) finalEntry = candidate;
    }
    if (!finalEntry) {
      reset();
      return;
    }
    for (const [message, timing] of completed) {
      let entry = entries.find(
        (candidate) =>
          candidate.type === "message" && candidate.message === message,
      );
      // Recent Pi versions explicitly identify the final turn message. Use
      // this exact ID only as a fallback for runtimes without branch access.
      if (!entry && message === event.message && event.messageEntryId) {
        const candidate = ctx.sessionManager.getEntry(event.messageEntryId);
        if (candidate?.message === message) entry = candidate;
      }
      if (!entry?.id) continue;
      pi.appendEntry("cc-switch.metrics.v1", {
        schemaVersion: 1,
        messageEntryId: entry.id,
        messageEntryTimestamp: entry.timestamp,
        ...timing,
      });
    }
    reset();
  });
}
