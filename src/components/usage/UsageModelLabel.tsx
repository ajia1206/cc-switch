import { useTranslation } from "react-i18next";
import { cn } from "@/lib/utils";

export type UsageModelBilling = "subscription" | undefined;

export interface UsageModelPresentation {
  label: string;
  billing: UsageModelBilling;
}

const SUBSCRIPTION_BILLING_FRAGMENT = "billing=subscription";
const STRICT_DECIMAL_COST = /^[+-]?(?:\d+(?:\.\d*)?|\.\d+)$/;
const CHATGPT_PREFIX = /^chatgpt\//i;
const GPT_MODEL_ID = /^gpt-(\d+(?:\.\d+)?)(?:-(.+))?$/i;

function humanizeSubscriptionModel(model: string): string {
  const withoutChatGptPrefix = model.replace(CHATGPT_PREFIX, "");
  const match = withoutChatGptPrefix.match(GPT_MODEL_ID);
  if (!match) return withoutChatGptPrefix;

  const suffix = match[2]
    ?.split("-")
    .filter(Boolean)
    .map((part) => part.charAt(0).toUpperCase() + part.slice(1).toLowerCase())
    .join(" ");
  return `GPT-${match[1]}${suffix ? ` ${suffix}` : ""}`;
}

/**
 * Cindy appends transport metadata to some model identifiers. Keep the raw
 * identifier in filters and storage, but turn known subscription metadata into
 * a compact label for people. Unknown fragments remain untouched so the UI
 * never silently discards meaningful model identity.
 */
export function parseUsageModel(model: string): UsageModelPresentation {
  const fragmentIndex = model.indexOf("#");
  if (fragmentIndex < 0) return { label: model, billing: undefined };

  const baseModel = model.slice(0, fragmentIndex);
  if (model.slice(fragmentIndex + 1) !== SUBSCRIPTION_BILLING_FRAGMENT) {
    return { label: model, billing: undefined };
  }

  return {
    label: humanizeSubscriptionModel(baseModel),
    billing: "subscription",
  };
}

export function resolveEffectiveUsageModel(
  pricingModel: string | null | undefined,
  responseModel: string | null | undefined,
  requestModel?: string | null,
): string {
  for (const model of [pricingModel, responseModel, requestModel]) {
    if (model?.trim()) return model;
  }
  return "unknown";
}

export function parseStrictUsageCost(cost: unknown): number | null {
  if (typeof cost === "number") return Number.isFinite(cost) ? cost : null;
  if (typeof cost !== "string") return null;

  const normalizedCost = cost.trim();
  if (!STRICT_DECIMAL_COST.test(normalizedCost)) return null;
  const parsed = Number(normalizedCost);
  return Number.isFinite(parsed) ? parsed : null;
}

export function shouldShowSubscriptionIncluded(
  model: string | null | undefined,
  cost: unknown,
): boolean {
  // Subscription access does not change the accounting basis. Usage is always
  // valued with the official model price table, so a zero amount is a real
  // zero (or an unpriced row), never an instruction to hide the cost.
  void model;
  void cost;
  return false;
}

interface UsageModelLabelProps {
  model: string;
  className?: string;
  badgeClassName?: string;
}

export function UsageModelLabel({
  model,
  className,
  badgeClassName,
}: UsageModelLabelProps) {
  const { t } = useTranslation();
  const presentation = parseUsageModel(model);

  return (
    <span className={cn("inline-flex min-w-0 items-center gap-1.5", className)}>
      <span className="truncate">{presentation.label}</span>
      {presentation.billing === "subscription" && (
        <span
          className={cn(
            "shrink-0 rounded-full border border-emerald-500/30 bg-emerald-500/10 px-1.5 py-0.5 font-sans text-[10px] font-medium leading-none text-emerald-600 dark:text-emerald-400",
            badgeClassName,
          )}
        >
          {t("usage.subscription", "订阅")}
        </span>
      )}
    </span>
  );
}
