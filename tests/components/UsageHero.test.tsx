import { render, screen } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { UsageHero } from "@/components/usage/UsageHero";

const useUsageSummaryByAppMock = vi.hoisted(() => vi.fn());

vi.mock("react-i18next", () => ({
  useTranslation: () => ({
    t: (key: string, fallback?: string) => fallback ?? key,
    i18n: { resolvedLanguage: "en", language: "en" },
  }),
}));

vi.mock("framer-motion", () => ({
  motion: {
    div: ({ children, ...props }: any) => <div {...props}>{children}</div>,
  },
}));

vi.mock("@/lib/query/usage", () => ({
  useUsageSummaryByApp: (...args: unknown[]) =>
    useUsageSummaryByAppMock(...args),
}));

vi.mock("@/components/usage/UsageActivityHeatmap", () => ({
  UsageActivityHeatmap: () => null,
}));

const summary = (totalCost: string) => ({
  totalRequests: 0,
  totalCost,
  totalInputTokens: 100,
  totalOutputTokens: 20,
  totalCacheCreationTokens: 0,
  totalCacheReadTokens: 0,
  successRate: 0,
  realTotalTokens: 120,
  cacheHitRate: 0,
});

const renderHero = () =>
  render(
    <UsageHero
      range={{ preset: "today" }}
      model="chatgpt/gpt-5.6-sol#billing=subscription"
      refreshIntervalMs={0}
    />,
  );

describe("UsageHero subscription cost", () => {
  beforeEach(() => {
    useUsageSummaryByAppMock.mockReset();
  });

  it("does not turn an invalid aggregated cost into subscription-included zero", () => {
    useUsageSummaryByAppMock.mockReturnValue({
      isLoading: false,
      data: [
        { appType: "cindy", summary: summary("0") },
        { appType: "pi", summary: summary("0garbage") },
      ],
    });

    renderHero();

    expect(screen.getByText("--")).toBeInTheDocument();
    expect(screen.queryByText("订阅内")).not.toBeInTheDocument();
  });

  it("shows a valid nonzero aggregate instead of subscription-included copy", () => {
    useUsageSummaryByAppMock.mockReturnValue({
      isLoading: false,
      data: [
        { appType: "cindy", summary: summary("0") },
        { appType: "pi", summary: summary("1") },
      ],
    });

    renderHero();

    expect(screen.getByText("$1.0000")).toBeInTheDocument();
    expect(screen.queryByText("订阅内")).not.toBeInTheDocument();
  });
});
