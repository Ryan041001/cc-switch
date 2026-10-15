import { fireEvent, render, screen } from "@testing-library/react";
import { describe, expect, it, vi } from "vitest";
import { RequestDetailPanel } from "@/components/usage/RequestDetailPanel";
import type { RequestLog } from "@/types/usage";

const useRequestDetailMock = vi.hoisted(() => vi.fn());

vi.mock("react-i18next", () => ({
  useTranslation: () => ({
    t: (key: string, options?: Record<string, unknown>) =>
      options && "value" in options ? `${key}:${options.value}` : key,
    i18n: { resolvedLanguage: "en", language: "en" },
  }),
}));

vi.mock("@/lib/query/usage", () => ({
  useRequestDetail: (id: string) => useRequestDetailMock(id),
}));

const log = (overrides: Partial<RequestLog>): RequestLog => ({
  requestId: "req_1",
  providerId: "p1",
  providerName: "GitHub Copilot",
  appType: "claude",
  model: "claude-opus-5-5",
  costMultiplier: "1",
  inputTokens: 3_066,
  outputTokens: 1_512,
  cacheReadTokens: 71_300,
  cacheCreationTokens: 2_048,
  inputCostUsd: "0.0123",
  outputCostUsd: "0.0302",
  cacheReadCostUsd: "0.0143",
  cacheCreationCostUsd: "0.0102",
  totalCostUsd: "0.0670",
  isStreaming: true,
  latencyMs: 26_400,
  firstTokenMs: 3_200,
  statusCode: 200,
  createdAt: 1_759_212_000,
  ...overrides,
});

const renderWith = (request: RequestLog) => {
  useRequestDetailMock.mockReturnValue({ data: request, isLoading: false });
  render(
    <RequestDetailPanel requestId={request.requestId} onClose={vi.fn()} />,
  );
};

describe("RequestDetailPanel", () => {
  it("shows speed, first token and duration for routed requests", () => {
    renderWith(log({}));

    expect(screen.getByText("usage.requestDetail")).toBeInTheDocument();
    // 1512 / (26.4 - 3.2) ≈ 65
    expect(screen.getByText("usage.speedValue:65")).toBeInTheDocument();
    expect(screen.getByText("3.2s")).toBeInTheDocument();
    expect(screen.getByText("26.4s")).toBeInTheDocument();
    expect(screen.getByText("$0.0670")).toBeInTheDocument();
  });

  it("prices native Antigravity rows from the local table like other providers", () => {
    renderWith(
      log({
        requestId: "antigravity_native:test",
        providerId: "antigravity-acp",
        providerName: "antigravity-acp",
        appType: "pi",
        dataSource: "pi_session",
        model: "gemini-3.8-flash-n",
        inputTokens: 100,
        outputTokens: 1800,
        cacheReadTokens: 300,
        inputCostUsd: "0.0001",
        outputCostUsd: "0.0068",
        cacheReadCostUsd: "0.0001",
        cacheCreationCostUsd: "0",
        totalCostUsd: "0.0070",
      }),
    );
    // Cost is computed locally like every other session importer, so a priced
    // row shows real dollars rather than a permanent 「未报告」.
    expect(screen.getByText("$0.0070")).toBeInTheDocument();
    expect(screen.queryByText("usage.sources.nativeCostUnknown")).toBeNull();
    expect(screen.getByText("Antigravity")).toBeInTheDocument();
  });

  it("shows unpriced for native Antigravity rows whose model has no price yet", () => {
    renderWith(
      log({
        requestId: "antigravity_native:test",
        providerId: "antigravity-acp",
        providerName: "antigravity-acp",
        appType: "pi",
        dataSource: "pi_session",
        model: "gemini-unknown-model",
        inputTokens: 100,
        outputTokens: 1800,
        cacheReadTokens: 300,
        inputCostUsd: "0",
        outputCostUsd: "0",
        cacheReadCostUsd: "0",
        cacheCreationCostUsd: "0",
        totalCostUsd: "0",
      }),
    );
    // Same fallback as Pi/Claude/Codex: no pricing row means 「未计价」, which
    // becomes real cost once the user adds a price (auto back-fill).
    expect(screen.getByText("usage.unpriced")).toBeInTheDocument();
    expect(screen.queryByText("usage.sources.nativeCostUnknown")).toBeNull();
  });

  it.each(["pi_observed", "pi_observed_turn"])(
    "shows measured Pi timing without presenting it as an HTTP proxy: %s",
    (dataSource) => {
      renderWith(log({ appType: "pi", dataSource }));
      expect(screen.getByText("usage.speedValue:65")).toBeInTheDocument();
      expect(screen.getByText("usage.detail.firstToken")).toBeInTheDocument();
      expect(screen.getByText("usage.detail.duration")).toBeInTheDocument();
      expect(screen.queryByText("usage.detail.durationEstimated")).toBeNull();
      expect(screen.queryByText("usage.detail.sourceProxy")).toBeNull();
      expect(
        screen.getByText(
          dataSource === "pi_observed"
            ? "usage.detail.sourcePiObserved"
            : "usage.detail.sourcePiObservedTurn",
        ),
      ).toBeInTheDocument();
    },
  );

  it("shows native Antigravity TTFT without inventing TPS for a buffered burst", () => {
    renderWith(
      log({
        appType: "pi",
        dataSource: "antigravity_native",
        latencyMs: 6_002,
        firstTokenMs: 6_000,
        outputTokens: 1_200,
      }),
    );
    expect(
      screen.getByText("usage.detail.sourceAntigravityNative"),
    ).toBeInTheDocument();
    expect(screen.getByText("usage.detail.firstToken")).toBeInTheDocument();
    expect(screen.queryByText("usage.detail.firstTokenEstimated")).toBeNull();
    expect(screen.queryByText(/usage.speedValue:/)).toBeNull();
    expect(screen.queryByText(/usage.speedEstimatedValue:/)).toBeNull();
  });

  it("explains why short outputs have no speed", () => {
    renderWith(
      log({ outputTokens: 64, latencyMs: 2_400, firstTokenMs: 1_900 }),
    );

    expect(screen.getByText("usage.detail.speedTooFew")).toBeInTheDocument();
  });

  it("shows an estimated speed and duration for session logs", () => {
    renderWith(
      log({
        dataSource: "session_log",
        firstTokenMs: undefined,
        outputTokens: 1_800,
        latencyMs: 20_000,
      }),
    );

    // 1800 / 20 s，含首字等待
    expect(
      screen.getByText("usage.speedEstimatedValue:90"),
    ).toBeInTheDocument();
    expect(
      screen.getByText("usage.detail.durationEstimated"),
    ).toBeInTheDocument();
    expect(screen.getByText("20.0s")).toBeInTheDocument();
    expect(screen.queryByText("usage.detail.firstToken")).toBeNull();
  });

  it("shows the estimated first token for session logs that have one", () => {
    renderWith(
      log({
        dataSource: "codex_session",
        firstTokenMs: 3_300,
        outputTokens: 1_800,
        latencyMs: 20_000,
      }),
    );

    expect(
      screen.getByText("usage.detail.firstTokenEstimated"),
    ).toBeInTheDocument();
    expect(screen.getByText("3.3s")).toBeInTheDocument();
    // 速度仍按整段耗时估
    expect(
      screen.getByText("usage.speedEstimatedValue:90"),
    ).toBeInTheDocument();
    expect(screen.queryByText("usage.detail.firstToken")).toBeNull();
  });

  it("explains why short session-log outputs have no estimate", () => {
    renderWith(
      log({
        dataSource: "session_log",
        firstTokenMs: undefined,
        outputTokens: 120,
        latencyMs: 6_000,
      }),
    );

    expect(
      screen.getByText("usage.detail.speedEstimateTooFew"),
    ).toBeInTheDocument();
    expect(screen.getByText("6.0s")).toBeInTheDocument();
  });

  it("says session logs have no timing data", () => {
    renderWith(
      log({
        dataSource: "codex_session",
        firstTokenMs: undefined,
        latencyMs: 0,
      }),
    );

    expect(
      screen.getByText("usage.detail.noTimingSession"),
    ).toBeInTheDocument();
    expect(screen.getByText("usage.detail.sourceSession")).toBeInTheDocument();
  });

  it("shows the error and a retry button when loading fails", () => {
    const refetch = vi.fn();
    useRequestDetailMock.mockReturnValue({
      data: undefined,
      isLoading: false,
      error: "ambiguous column name: created_at",
      refetch,
    });
    render(<RequestDetailPanel requestId="req_1" onClose={vi.fn()} />);

    expect(screen.getByText("usage.requestLoadFailed")).toBeInTheDocument();
    expect(
      screen.getByText("ambiguous column name: created_at"),
    ).toBeInTheDocument();
    expect(screen.queryByText("usage.requestNotFound")).toBeNull();

    fireEvent.click(screen.getByRole("button", { name: "common.retry" }));
    expect(refetch).toHaveBeenCalledTimes(1);
  });

  it("says the request is not found only when the query returned nothing", () => {
    useRequestDetailMock.mockReturnValue({
      data: null,
      isLoading: false,
      error: null,
    });
    render(<RequestDetailPanel requestId="req_1" onClose={vi.fn()} />);

    expect(screen.getByText("usage.requestNotFound")).toBeInTheDocument();
    expect(screen.queryByText("usage.requestLoadFailed")).toBeNull();
  });
});
