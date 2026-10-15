import { afterEach, describe, expect, it, vi } from "vitest";
import ccSwitchMetrics from "../../integrations/pi/cc-switch-metrics";

type API = Parameters<typeof ccSwitchMetrics>[0];
type Handler = (event: never, ctx: never) => void;

function harness() {
  let now = 0;
  vi.spyOn(performance, "now").mockImplementation(() => now);
  const handlers = new Map<string, Handler>();
  const appendEntry = vi.fn();
  const api: API = {
    on: (name, handler) => handlers.set(name, handler as Handler),
    appendEntry,
  };
  const ctx = {
    sessionManager: {
      getEntry: vi.fn(() => ({ timestamp: "2026-10-15T00:00:00.000Z" })),
    },
  };
  ccSwitchMetrics(api);
  return {
    appendEntry,
    ctx,
    emit: (name: string, event: unknown = {}, at?: number) => {
      if (at !== undefined) now = at;
      handlers.get(name)?.(event as never, ctx as never);
    },
  };
}

const assistant = { role: "assistant" };
const endTurn = { message: assistant, messageEntryId: "abc12345" };
const delta = (type: string, value?: string) => ({
  assistantMessageEvent: { type, delta: value },
});

afterEach(() => vi.restoreAllMocks());

describe("Pi local timing observer", () => {
  it("measures from the provider request, ignoring empty and metadata events", () => {
    const h = harness();
    h.emit("turn_start", {}, 0);
    h.emit("before_provider_request", {}, 100);
    h.emit("message_update", delta("start"), 120);
    h.emit("message_update", delta("text_delta", ""), 130);
    h.emit("message_update", delta("thinking_delta", "reason"), 350);
    h.emit("message_update", delta("text_delta", "hello"), 500);
    h.emit("message_end", { message: assistant }, 2_100);
    // Tool execution must not inflate the model generation window or erase it.
    h.emit("before_provider_request", {}, 4_000);
    h.emit("turn_end", endTurn, 8_000);
    expect(h.appendEntry).toHaveBeenCalledTimes(1);
    expect(h.appendEntry).toHaveBeenCalledWith("cc-switch.metrics.v1", {
      schemaVersion: 1,
      messageEntryId: "abc12345",
      messageEntryTimestamp: "2026-10-15T00:00:00.000Z",
      latencyMs: 2_000,
      firstTokenMs: 250,
      timingBasis: "provider_request",
    });
  });

  it("measures custom providers from turn start and counts tool argument output", () => {
    const h = harness();
    h.emit("turn_start", {}, 50);
    h.emit("message_update", delta("toolcall_delta", '{"path"'), 550);
    h.emit("message_end", { message: assistant }, 1_050);
    h.emit("turn_end", endTurn);
    expect(h.appendEntry.mock.calls[0][1]).toMatchObject({
      latencyMs: 1_000,
      firstTokenMs: 500,
      timingBasis: "turn_start",
    });
  });

  it("never fabricates first-token timing for buffered or nonstreaming responses", () => {
    const h = harness();
    h.emit("turn_start", {}, 0);
    h.emit("message_update", delta("text_end"), 50);
    h.emit("message_end", { message: assistant }, 100);
    h.emit("turn_end", endTurn);
    expect(h.appendEntry.mock.calls[0][1].firstTokenMs).toBeNull();
  });

  it("resets the clock and first token for a retried HTTP request", () => {
    const h = harness();
    h.emit("turn_start", {}, 0);
    h.emit("before_provider_request", {}, 10);
    h.emit("message_update", delta("text_delta", "partial"), 50);
    h.emit("before_provider_request", {}, 1_000);
    h.emit("message_update", delta("text_delta", "retry"), 1_200);
    h.emit("message_end", { message: assistant }, 2_000);
    h.emit("turn_end", endTurn);
    expect(h.appendEntry.mock.calls[0][1]).toMatchObject({
      latencyMs: 1_000,
      firstTokenMs: 200,
    });
  });

  it("does not attach timings to a different session or an unpersisted message", () => {
    const h = harness();
    h.emit("turn_start", {}, 0);
    h.emit("message_end", { message: assistant }, 100);
    h.emit("session_switch");
    h.emit("turn_end", endTurn);
    h.emit("turn_start", {}, 200);
    h.emit("message_end", { message: assistant }, 300);
    h.emit("turn_end", { message: assistant });
    h.emit("turn_end", endTurn);
    expect(h.appendEntry).not.toHaveBeenCalled();
  });

  it("ignores tool/user messages and provider calls outside the main agent turn", () => {
    const h = harness();
    h.emit("before_provider_request", {}, 0);
    h.emit("message_end", { message: assistant }, 100);
    h.emit("turn_end", endTurn);
    expect(h.appendEntry).not.toHaveBeenCalled();
    h.emit("turn_start", {}, 200);
    h.emit("message_end", { message: { role: "toolResult" } }, 300);
    h.emit("message_end", { message: assistant }, 400);
    h.emit("turn_end", endTurn);
    h.emit("turn_end", endTurn);
    expect(h.appendEntry).toHaveBeenCalledTimes(1);
    expect(h.appendEntry.mock.calls[0][1].latencyMs).toBe(200);
  });
});
