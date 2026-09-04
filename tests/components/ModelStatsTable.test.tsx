import { render, screen } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { ModelStatsTable } from "@/components/usage/ModelStatsTable";

const useModelStatsMock = vi.hoisted(() => vi.fn());

vi.mock("react-i18next", () => ({
  useTranslation: () => ({
    t: (key: string, fallback?: string) => {
      const translations: Record<string, string> = {
        "usage.subscription": "Subscription",
        "usage.subscriptionIncluded": "Included",
      };
      return translations[key] ?? fallback ?? key;
    },
  }),
}));

vi.mock("@/lib/query/usage", () => ({
  useModelStats: (...args: unknown[]) => useModelStatsMock(...args),
}));

describe("ModelStatsTable", () => {
  beforeEach(() => {
    useModelStatsMock.mockReset();
  });

  it("presents subscription models without exposing billing metadata and keeps zero-dollar cost visible", () => {
    useModelStatsMock.mockReturnValue({
      isLoading: false,
      data: [
        {
          model: "chatgpt/gpt-5.6-sol#billing=subscription",
          requestCount: 0,
          totalTokens: 97_126_000,
          totalCost: "0",
          avgCostPerRequest: "0",
        },
      ],
    });

    render(
      <ModelStatsTable range={{ preset: "today" }} refreshIntervalMs={0} />,
    );

    expect(screen.getByText("GPT-5.6 Sol")).toBeInTheDocument();
    expect(screen.getByText("Subscription")).toBeInTheDocument();
    expect(screen.getByText("$0.0000")).toBeInTheDocument();
    expect(screen.getByText("$0.000000")).toBeInTheDocument();
    expect(
      screen.queryByText("chatgpt/gpt-5.6-sol#billing=subscription"),
    ).not.toBeInTheDocument();
  });

  it("evaluates total and average subscription costs independently", () => {
    useModelStatsMock.mockReturnValue({
      isLoading: false,
      data: [
        {
          model: "gpt-5.3-codex-spark#billing=subscription",
          requestCount: 1,
          totalTokens: 100,
          totalCost: "0",
          avgCostPerRequest: "1.25",
        },
      ],
    });

    render(
      <ModelStatsTable range={{ preset: "today" }} refreshIntervalMs={0} />,
    );

    expect(screen.getByText("$0.0000")).toBeInTheDocument();
    expect(screen.getByText("$1.250000")).toBeInTheDocument();
  });
});
