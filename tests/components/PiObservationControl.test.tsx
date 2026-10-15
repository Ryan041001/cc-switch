import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { PiObservationControl } from "@/components/usage/PiObservationControl";

const api = vi.hoisted(() => ({
  getObservationStatus: vi.fn(),
  setObservationEnabled: vi.fn(),
}));
vi.mock("@/lib/api/pi", () => ({ piApi: api }));
vi.mock("react-i18next", () => ({
  useTranslation: () => ({ t: (key: string) => key }),
}));

const initial = {
  enabled: false,
  canManage: true,
  extensionPath: "/tmp/pi-test/extensions/cc-switch-metrics.ts",
};

function mount(active = true) {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
  });
  return render(
    <QueryClientProvider client={client}>
      <PiObservationControl active={active} />
    </QueryClientProvider>,
  );
}

beforeEach(() => {
  api.getObservationStatus.mockResolvedValue(initial);
  api.setObservationEnabled.mockResolvedValue({ ...initial, enabled: true });
});

describe("Pi observation installation control", () => {
  it("installs explicitly, reflects disk state and explains reload", async () => {
    mount();
    const toggle = await screen.findByRole("switch");
    await waitFor(() => expect(toggle).toBeEnabled());
    expect(toggle).not.toBeChecked();
    fireEvent.click(toggle);
    await waitFor(() =>
      expect(api.setObservationEnabled).toHaveBeenCalledWith(true),
    );
    await waitFor(() => expect(toggle).toBeChecked());
    expect(
      screen.getByText("usage.sources.piMetricsReload"),
    ).toBeInTheDocument();
    expect(screen.getByText(initial.extensionPath)).toBeInTheDocument();
  });

  it("does not overwrite a conflicting extension", async () => {
    api.getObservationStatus.mockResolvedValue({
      ...initial,
      canManage: false,
    });
    mount();
    expect(await screen.findByRole("alert")).toHaveTextContent(
      "usage.sources.piMetricsConflict",
    );
    expect(screen.getByRole("switch")).toBeDisabled();
    expect(api.setObservationEnabled).not.toHaveBeenCalled();
  });

  it("reports installation failures without claiming to be enabled", async () => {
    api.setObservationEnabled.mockRejectedValue(
      new Error("read-only filesystem"),
    );
    mount();
    const toggle = await screen.findByRole("switch");
    await waitFor(() => expect(toggle).toBeEnabled());
    fireEvent.click(toggle);
    expect(await screen.findByRole("alert")).toHaveTextContent(
      "read-only filesystem",
    );
    expect(toggle).not.toBeChecked();
  });

  it("does not access Pi files while the data-sources sheet is closed", () => {
    mount(false);
    expect(api.getObservationStatus).not.toHaveBeenCalled();
    expect(screen.getByRole("switch")).toBeDisabled();
  });
});
