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
  type Entry = {
    type: string;
    id: string;
    timestamp: string;
    message: { role: string };
  };
  const entries: Entry[] = [];
  const ctx = {
    sessionManager: {
      getEntry: vi.fn((id: string) => entries.find((entry) => entry.id === id)),
      getBranch: vi.fn(() => entries),
    },
  };
  ccSwitchMetrics(api);
  return {
    appendEntry,
    ctx,
    emit: (name: string, event: unknown = {}, at?: number) => {
      if (at !== undefined) now = at;
      handlers.get(name)?.(event as never, ctx as never);
      // Pi persists message_end only after extension handlers have returned.
      if (name === "message_end") {
        const message = (event as { message?: { role: string } }).message;
        if (
          message &&
          ["assistant", "user", "toolResult"].includes(message.role)
        ) {
          entries.push({
            type: "message",
            id: `persisted-${entries.length + 1}`,
            timestamp: `2026-10-15T00:00:0${entries.length}.000Z`,
            message,
          });
        }
      }
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
  it("measures every provider request in a tool-using turn, ignoring empty and metadata events", () => {
    const h = harness();
    const toolCall = { role: "assistant" };
    const finalReply = { role: "assistant" };
    h.emit("turn_start", {}, 0);
    h.emit("before_provider_request", {}, 100);
    h.emit("message_update", delta("start"), 120);
    h.emit("message_update", delta("text_delta", ""), 130);
    h.emit("message_update", delta("thinking_delta", "reason"), 350);
    h.emit("message_update", delta("text_delta", "hello"), 500);
    h.emit("message_end", { message: toolCall }, 2_100);
    h.emit("message_end", { message: { role: "toolResult" } }, 3_500);
    h.emit(
      "turn_end",
      { message: toolCall, messageEntryId: "persisted-1" },
      3_900,
    );
    // Tool execution must not inflate either model generation window.
    h.emit("turn_start", {}, 4_000);
    h.emit("before_provider_request", {}, 4_100);
    h.emit("message_update", delta("text_delta", "done"), 4_500);
    h.emit("message_end", { message: finalReply }, 6_000);
    h.emit(
      "turn_end",
      { message: finalReply, messageEntryId: "persisted-3" },
      8_000,
    );
    expect(h.appendEntry).toHaveBeenCalledTimes(2);
    expect(h.appendEntry).toHaveBeenNthCalledWith(1, "cc-switch.metrics.v1", {
      schemaVersion: 1,
      messageEntryId: "persisted-1",
      messageEntryTimestamp: "2026-10-15T00:00:00.000Z",
      latencyMs: 2_000,
      firstTokenMs: 250,
      timingBasis: "provider_request",
    });
    expect(h.appendEntry).toHaveBeenNthCalledWith(2, "cc-switch.metrics.v1", {
      schemaVersion: 1,
      messageEntryId: "persisted-3",
      messageEntryTimestamp: "2026-10-15T00:00:02.000Z",
      latencyMs: 1_900,
      firstTokenMs: 400,
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

  it.each([
    "openai",
    "anthropic",
    "google",
    "my-models-json-provider",
    "my-extension-provider",
  ])(
    "observes %s without provider registrations, credentials or URL interception",
    (provider) => {
      const h = harness();
      const message = { role: "assistant", provider };
      h.emit("turn_start", {}, 0);
      h.emit("context", {}, 500);
      h.emit("message_update", delta("thinking_delta", "reason"), 750);
      h.emit("message_end", { message }, 2_500);
      h.emit("turn_end", { message, messageEntryId: "persisted-1" }, 9_000);
      expect(h.appendEntry.mock.calls[0][1]).toEqual({
        schemaVersion: 1,
        messageEntryId: "persisted-1",
        messageEntryTimestamp: "2026-10-15T00:00:00.000Z",
        latencyMs: 2_000,
        firstTokenMs: 250,
        timingBasis: "model_call",
      });
    },
  );

  it("prefers the request hook over context and supports provider switches in one session", () => {
    const h = harness();
    h.emit("turn_start", {}, 0);
    h.emit("context", {}, 100);
    h.emit("before_provider_request", {}, 200);
    h.emit("message_update", delta("text_delta", "a"), 400);
    h.emit("message_end", { message: assistant }, 1_200);
    h.emit("turn_end", endTurn);
    expect(h.appendEntry.mock.calls[0][1]).toMatchObject({
      timingBasis: "provider_request",
      firstTokenMs: 200,
      latencyMs: 1_000,
    });
    h.emit("turn_start", {}, 2_000);
    const secondAssistant = { role: "assistant" };
    h.emit("context", {}, 2_100);
    h.emit("message_update", delta("toolcall_delta", "{}"), 2_400);
    h.emit("message_end", { message: secondAssistant }, 3_100);
    h.emit("turn_end", {
      message: secondAssistant,
      messageEntryId: "persisted-2",
    });
    expect(h.appendEntry.mock.calls[1][1]).toMatchObject({
      messageEntryId: "persisted-2",
      timingBasis: "model_call",
      firstTokenMs: 300,
      latencyMs: 1_000,
    });
  });

  it("uses exact persisted message identity on Pi versions without messageEntryId", () => {
    const h = harness();
    h.ctx.sessionManager.getBranch.mockReturnValue([
      {
        type: "message",
        id: "wrong",
        timestamp: "2026-10-15T00:00:00.000Z",
        message: { ...assistant },
      },
      {
        type: "message",
        id: "correct",
        timestamp: "2026-10-15T00:00:01.000Z",
        message: assistant,
      },
      {
        type: "message",
        id: "tool",
        timestamp: "2026-10-15T00:00:02.000Z",
        message: { role: "toolResult" },
      },
    ]);
    h.emit("turn_start", {}, 0);
    h.emit("context", {}, 10);
    h.emit("message_update", delta("text_delta", "hello"), 50);
    h.emit("message_end", { message: assistant }, 500);
    h.emit("turn_end", { message: assistant });
    expect(h.appendEntry.mock.calls[0][1]).toMatchObject({
      messageEntryId: "correct",
      messageEntryTimestamp: "2026-10-15T00:00:01.000Z",
    });
  });

  it("never guesses the latest assistant or matches message contents", () => {
    const h = harness();
    h.ctx.sessionManager.getBranch.mockReturnValue([
      {
        type: "message",
        id: "other",
        timestamp: "2026-10-15T00:00:00.000Z",
        message: { ...assistant },
      },
    ]);
    h.emit("turn_start", {}, 0);
    h.emit("message_end", { message: assistant }, 100);
    h.emit("turn_end", { message: assistant });
    expect(h.appendEntry).not.toHaveBeenCalled();
  });

  it("discards stale completed timings at agent end and rejects a different turn-end message", () => {
    const h = harness();
    h.emit("turn_start", {}, 0);
    h.emit("message_end", { message: assistant }, 100);
    h.emit("turn_end", { ...endTurn, message: { ...assistant } });
    h.emit("turn_start", {}, 200);
    h.emit("message_end", { message: assistant }, 300);
    h.emit("agent_end");
    h.emit("turn_end", endTurn);
    expect(h.appendEntry).not.toHaveBeenCalled();
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
    const oldSessionMessage = { role: "assistant" };
    h.emit("message_end", { message: oldSessionMessage }, 100);
    h.emit("session_switch");
    h.emit("turn_end", {
      message: oldSessionMessage,
      messageEntryId: "persisted-1",
    });
    const nonFinalTurnMessage = { role: "assistant" };
    h.emit("turn_start", {}, 200);
    h.emit("message_end", { message: nonFinalTurnMessage }, 300);
    h.emit("turn_end", { message: { ...nonFinalTurnMessage } });
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
