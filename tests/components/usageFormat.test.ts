import { describe, expect, it } from "vitest";
import {
  formatProviderCost,
  formatProviderLatency,
  formatTokensShort,
  getLocaleFromLanguage,
} from "@/components/usage/format";

describe("usage format helpers", () => {
  it("formats Traditional Chinese token units with Traditional characters", () => {
    expect(formatTokensShort(12_345, "zh-TW")).toBe("1.2 萬");
    expect(formatTokensShort(123_456_789, "zh-Hant", 2)).toBe("1.23 億");
  });

  it("resolves Traditional Chinese locale aliases", () => {
    expect(getLocaleFromLanguage("zh_TW")).toBe("zh-TW");
    expect(getLocaleFromLanguage("zh-HK")).toBe("zh-TW");
  });

  it("marks estimated provider cost without presenting false precision", () => {
    expect(formatProviderCost("6.4294776", true)).toBe("≈$6.4295");
    expect(formatProviderCost("6.4294776", false)).toBe("$6.4295");
  });

  it("renders unknown provider latency as a dash", () => {
    expect(formatProviderLatency(null)).toBe("—");
    expect(formatProviderLatency(279_797)).toBe("279797ms");
  });
});
