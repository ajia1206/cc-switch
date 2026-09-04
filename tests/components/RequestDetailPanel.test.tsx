import { render, screen } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { RequestDetailPanel } from "@/components/usage/RequestDetailPanel";

const useRequestDetailMock = vi.hoisted(() => vi.fn());

vi.mock("react-i18next", () => ({
  useTranslation: () => ({
    t: (key: string, fallback?: string) => {
      const translations: Record<string, string> = {
        "usage.subscription": "Subscription",
        "usage.subscriptionIncluded": "Included",
        "usage.unpriced": "Unpriced",
      };
      return translations[key] ?? fallback ?? key;
    },
    i18n: { language: "en" },
  }),
}));

vi.mock("@/lib/query/usage", () => ({
  useRequestDetail: (...args: unknown[]) => useRequestDetailMock(...args),
}));

describe("RequestDetailPanel", () => {
  beforeEach(() => {
    useRequestDetailMock.mockReset();
  });

  it("uses friendly subscription model labels and official-price total cost", () => {
    useRequestDetailMock.mockReturnValue({
      isLoading: false,
      data: {
        requestId: "request-1",
        providerId: "provider-1",
        providerName: "Cindy",
        appType: "cindy",
        model: "gpt-5.3-codex-spark#billing=subscription",
        requestModel: "chatgpt/gpt-5.6-sol#billing=subscription",
        pricingModel: "gpt-5.2-codex#billing=subscription",
        costMultiplier: "1",
        inputTokens: 100,
        outputTokens: 20,
        cacheReadTokens: 0,
        cacheCreationTokens: 0,
        inputCostUsd: "0.000500",
        outputCostUsd: "0.000600",
        cacheReadCostUsd: "0.000000",
        cacheCreationCostUsd: "0.000000",
        totalCostUsd: "0.001100",
        isStreaming: false,
        latencyMs: 100,
        statusCode: 200,
        createdAt: 1_700_000_000,
      },
    });

    render(<RequestDetailPanel requestId="request-1" onClose={vi.fn()} />);

    expect(screen.getByText("GPT-5.3 Codex Spark")).toBeInTheDocument();
    expect(screen.getByText("GPT-5.6 Sol")).toBeInTheDocument();
    expect(screen.getByText("GPT-5.2 Codex")).toBeInTheDocument();
    expect(screen.getAllByText("Subscription")).toHaveLength(3);
    expect(screen.getByText("$0.001100")).toBeInTheDocument();
    expect(screen.queryByText("Unpriced")).not.toBeInTheDocument();
    expect(
      screen.queryByText("gpt-5.3-codex-spark#billing=subscription"),
    ).not.toBeInTheDocument();
  });

  it("falls back from an empty pricing model and never renders malformed costs as numbers", () => {
    useRequestDetailMock.mockReturnValue({
      isLoading: false,
      data: {
        requestId: "request-2",
        providerId: "provider-1",
        providerName: "Cindy",
        appType: "cindy",
        model: "gpt-response",
        requestModel: "gpt-request",
        pricingModel: "",
        costMultiplier: "invalid",
        inputTokens: 100,
        outputTokens: 20,
        cacheReadTokens: 0,
        cacheCreationTokens: 0,
        inputCostUsd: "1junk",
        outputCostUsd: "invalid",
        cacheReadCostUsd: "0junk",
        cacheCreationCostUsd: "invalid",
        totalCostUsd: "0junk",
        isStreaming: false,
        latencyMs: 100,
        statusCode: 200,
        createdAt: 1_700_000_000,
      },
    });

    render(<RequestDetailPanel requestId="request-2" onClose={vi.fn()} />);

    expect(screen.getByText("gpt-response")).toBeInTheDocument();
    expect(screen.getAllByText("--")).toHaveLength(5);
    expect(screen.queryByText("$1.000000")).not.toBeInTheDocument();
    expect(screen.queryByText("$NaN")).not.toBeInTheDocument();
    expect(screen.queryByText("Unpriced")).not.toBeInTheDocument();
  });
});
