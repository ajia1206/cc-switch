import { describe, expect, it } from "vitest";
import {
  parseUsageModel,
  resolveEffectiveUsageModel,
  shouldShowSubscriptionIncluded,
} from "@/components/usage/UsageModelLabel";

describe("usage model presentation", () => {
  it("turns Cindy subscription model metadata into a friendly label", () => {
    expect(parseUsageModel("chatgpt/gpt-5.6-sol#billing=subscription")).toEqual(
      {
        label: "GPT-5.6 Sol",
        billing: "subscription",
      },
    );
    expect(parseUsageModel("gpt-5.3-codex-spark#billing=subscription")).toEqual(
      {
        label: "GPT-5.3 Codex Spark",
        billing: "subscription",
      },
    );
  });

  it("leaves ordinary and unknown metadata model identifiers untouched", () => {
    expect(parseUsageModel("claude-sonnet-4-5")).toEqual({
      label: "claude-sonnet-4-5",
      billing: undefined,
    });
    expect(parseUsageModel("gpt-5#variant=fast")).toEqual({
      label: "gpt-5#variant=fast",
      billing: undefined,
    });
    for (const model of [
      "gpt-5#billing=subscription&variant=fast",
      "gpt-5#variant=fast&billing=subscription",
      "gpt-5#billing=Subscription",
      "gpt-5#billing=subscription&billing=subscription",
    ]) {
      expect(parseUsageModel(model)).toEqual({
        label: model,
        billing: undefined,
      });
    }
  });

  it("uses the first non-empty effective pricing model", () => {
    expect(resolveEffectiveUsageModel("", "gpt-response", "gpt-request")).toBe(
      "gpt-response",
    );
    expect(
      resolveEffectiveUsageModel("gpt-priced", "gpt-response", "gpt-request"),
    ).toBe("gpt-priced");
  });

  it("never hides official list-price accounting behind subscription metadata", () => {
    const model = "gpt-5.3-codex-spark#billing=subscription";
    expect(shouldShowSubscriptionIncluded(model, "0.0000")).toBe(false);
    expect(shouldShowSubscriptionIncluded(model, 0)).toBe(false);
    expect(shouldShowSubscriptionIncluded(model, "1.25")).toBe(false);
    expect(shouldShowSubscriptionIncluded(model, "0 USD")).toBe(false);
    expect(shouldShowSubscriptionIncluded(model, "0garbage")).toBe(false);
    expect(shouldShowSubscriptionIncluded(model, "")).toBe(false);
    expect(shouldShowSubscriptionIncluded(model, null)).toBe(false);
    expect(shouldShowSubscriptionIncluded("gpt-5.3-codex-spark", "0")).toBe(
      false,
    );
  });
});
