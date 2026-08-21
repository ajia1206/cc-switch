import {
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
} from "react";
import { useTranslation } from "react-i18next";
import { keepPreviousData, useQueryClient } from "@tanstack/react-query";
import {
  ChevronDown,
  ChevronLeft,
  ChevronUp,
  CircleDollarSign,
  Gauge,
  RefreshCw,
  X,
} from "lucide-react";
import {
  currentMonitor,
  getCurrentWindow,
  LogicalPosition,
  LogicalSize,
} from "@tauri-apps/api/window";
import { Button } from "@/components/ui/button";
import { useUsageEventBridge } from "@/hooks/useUsageEventBridge";
import {
  usageKeys,
  useTrayUsageOverview,
} from "@/lib/query/usage";
import {
  createCCIslandLayout,
  getAvailableScreenBounds,
  type CCIslandMode,
} from "@/lib/ccIslandLayout";
import type { UsageSummary } from "@/types/usage";
import { reportFrontendError } from "@/lib/frontendLogger";
import { cn } from "@/lib/utils";
import {
  fmtUsd,
  formatTokensShort,
  getResolvedLang,
  parseFiniteNumber,
} from "./format";
import { TrayUsagePanel } from "./TrayUsagePanel";

const emptySummary: UsageSummary = {
  totalRequests: 0,
  totalCost: "0",
  totalInputTokens: 0,
  totalOutputTokens: 0,
  totalCacheCreationTokens: 0,
  totalCacheReadTokens: 0,
  successRate: 0,
  realTotalTokens: 0,
  cacheHitRate: 0,
};

function isMacPlatform(): boolean {
  try {
    const ua = navigator.userAgent || "";
    const platform = (navigator.platform || "").toLowerCase();
    return /mac/i.test(ua) || platform.includes("mac");
  } catch {
    return false;
  }
}

function aggregateSummaries(items: UsageSummary[]): UsageSummary {
  if (items.length === 0) return emptySummary;

  let totalRequests = 0;
  let successfulRequests = 0;
  let totalCost = 0;
  let input = 0;
  let output = 0;
  let cacheCreation = 0;
  let cacheRead = 0;

  for (const item of items) {
    totalRequests += item.totalRequests;
    successfulRequests += Math.round(
      (item.totalRequests * item.successRate) / 100,
    );
    totalCost += parseFiniteNumber(item.totalCost) ?? 0;
    input += item.totalInputTokens;
    output += item.totalOutputTokens;
    cacheCreation += item.totalCacheCreationTokens;
    cacheRead += item.totalCacheReadTokens;
  }

  const cacheableInput = input + cacheCreation + cacheRead;
  return {
    totalRequests,
    totalCost: totalCost.toFixed(6),
    totalInputTokens: input,
    totalOutputTokens: output,
    totalCacheCreationTokens: cacheCreation,
    totalCacheReadTokens: cacheRead,
    successRate:
      totalRequests > 0 ? (successfulRequests / totalRequests) * 100 : 0,
    realTotalTokens: input + output + cacheCreation + cacheRead,
    cacheHitRate: cacheableInput > 0 ? cacheRead / cacheableInput : 0,
  };
}

function formatCost(value: unknown): string {
  const cost = parseFiniteNumber(value);
  if (cost == null) return "--";
  return fmtUsd(cost, Math.abs(cost) >= 1 ? 2 : 4);
}

function formatPercent(value: number): string {
  if (!Number.isFinite(value)) return "--";
  return `${Math.round(value)}%`;
}

export function CCIslandWindow() {
  const macOS = useMemo(isMacPlatform, []);

  if (!macOS) {
    return <TrayUsagePanel />;
  }

  return <MacCCIslandWindow />;
}

function MacCCIslandWindow() {
  const { t, i18n } = useTranslation();
  const queryClient = useQueryClient();
  const lang = getResolvedLang(i18n);
  const [mode, setMode] = useState<CCIslandMode>("compact");
  const [layoutReady, setLayoutReady] = useState(false);
  const modeRef = useRef<CCIslandMode>("compact");
  const collapseTimerRef = useRef<ReturnType<typeof setTimeout> | null>(
    null,
  );

  useUsageEventBridge();

  const { data: overview, isLoading, isFetching } = useTrayUsageOverview(
    { preset: "today" },
    {},
    {
      placeholderData: keepPreviousData,
      refetchInterval: 30000,
      refetchIntervalInBackground: true,
    },
  );

  const apps = overview?.summaryByApp ?? [];
  const summary = useMemo(
    () => aggregateSummaries(apps.map((item) => item.summary)),
    [apps],
  );
  const topApps = useMemo(
    () =>
      apps
        .slice()
        .sort(
          (a, b) => b.summary.realTotalTokens - a.summary.realTotalTokens,
        )
        .slice(0, 3),
    [apps],
  );

  const applyWindowLayout = useCallback(async (nextMode: CCIslandMode) => {
    const appWindow = getCurrentWindow();
    const monitor = await currentMonitor();
    const bounds = monitor
      ? (() => {
          const position = monitor.workArea.position.toLogical(
            monitor.scaleFactor,
          );
          const size = monitor.workArea.size.toLogical(monitor.scaleFactor);
          return {
            left: position.x,
            top: position.y,
            width: size.width,
            height: size.height,
          };
        })()
      : getAvailableScreenBounds(window.screen);
    const layout = createCCIslandLayout(nextMode, bounds);

    // The Rust tray window starts with fixed 380x560 constraints. Clear them
    // before applying the compact/expanded island dimensions.
    await appWindow.setMinSize(null);
    await appWindow.setMaxSize(null);
    await appWindow.setSize(
      new LogicalSize(layout.size.width, layout.size.height),
    );
    await appWindow.setPosition(
      new LogicalPosition(layout.position.x, layout.position.y),
    );
  }, []);

  const requestWindowLayout = useCallback(
    (nextMode: CCIslandMode) => {
      void applyWindowLayout(nextMode).catch((error) => {
        reportFrontendError("cc_island_window_layout", error);
      });
    },
    [applyWindowLayout],
  );

  const transitionTo = useCallback(
    (nextMode: CCIslandMode) => {
      modeRef.current = nextMode;
      setMode(nextMode);
      requestWindowLayout(nextMode);
    },
    [requestWindowLayout],
  );

  const refresh = useCallback(() => {
    void queryClient.invalidateQueries({ queryKey: usageKeys.all });
  }, [queryClient]);

  const hideWindow = useCallback(() => {
    transitionTo("compact");
    void getCurrentWindow().hide();
  }, [transitionTo]);

  useEffect(() => {
    document.documentElement.classList.add("cc-island-window");
    void applyWindowLayout("compact")
      .catch((error) => {
        reportFrontendError("cc_island_initial_layout", error);
      })
      .finally(() => setLayoutReady(true));

    return () => {
      document.documentElement.classList.remove("cc-island-window");
    };
  }, [applyWindowLayout]);

  useEffect(() => {
    let dispose: (() => void) | undefined;

    void getCurrentWindow()
      .onFocusChanged(({ payload: focused }) => {
        if (focused) {
          requestWindowLayout(modeRef.current);
          refresh();
        } else if (modeRef.current !== "compact") {
          modeRef.current = "compact";
          setMode("compact");
          requestWindowLayout("compact");
        }
      })
      .then((unlisten) => {
        dispose = unlisten;
      });

    return () => {
      dispose?.();
    };
  }, [refresh, requestWindowLayout]);

  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key !== "Escape") return;
      if (modeRef.current === "compact") {
        hideWindow();
      } else {
        transitionTo("compact");
      }
    };

    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [hideWindow, transitionTo]);

  useEffect(() => {
    return () => {
      if (collapseTimerRef.current) {
        clearTimeout(collapseTimerRef.current);
      }
    };
  }, []);

  const cancelCollapse = () => {
    if (collapseTimerRef.current) {
      clearTimeout(collapseTimerRef.current);
      collapseTimerRef.current = null;
    }
  };

  const scheduleCollapse = () => {
    cancelCollapse();
    if (modeRef.current !== "expanded") return;
    collapseTimerRef.current = setTimeout(() => {
      transitionTo("compact");
      collapseTimerRef.current = null;
    }, 180);
  };

  if (mode === "details") {
    return (
      <div className="relative h-screen bg-transparent">
        <TrayUsagePanel />
        <Button
          type="button"
          size="sm"
          variant="secondary"
          className="absolute bottom-4 right-4 z-50 h-8 rounded-full px-3 shadow-lg"
          onClick={() => transitionTo("expanded")}
        >
          <ChevronLeft className="mr-1 h-3.5 w-3.5" />
          {t("common.back", "Back")}
        </Button>
      </div>
    );
  }

  const compact = mode === "compact";
  const tokens = isLoading
    ? "--"
    : formatTokensShort(summary.realTotalTokens, lang, 2);
  const cost = isLoading ? "--" : formatCost(summary.totalCost);

  return (
    <div
      className={cn(
        "h-screen overflow-hidden bg-transparent p-1.5 text-white transition-opacity duration-100",
        layoutReady ? "opacity-100" : "opacity-0",
      )}
      onPointerEnter={() => {
        cancelCollapse();
        if (modeRef.current === "compact") transitionTo("expanded");
      }}
      onPointerLeave={scheduleCollapse}
    >
      <section
        data-tauri-drag-region
        className={cn(
          "relative flex h-full flex-col overflow-hidden border border-white/10 bg-[#090a0c]/95 shadow-[0_18px_56px_rgba(0,0,0,0.42)] backdrop-blur-2xl",
          "transition-[border-radius] duration-200",
          compact ? "rounded-[26px]" : "rounded-[22px]",
        )}
      >
        <div
          className={cn(
            "flex shrink-0 items-center gap-3 px-4",
            compact ? "h-full" : "h-16 border-b border-white/10",
          )}
        >
          <div className="relative flex h-8 w-8 shrink-0 items-center justify-center rounded-full bg-white/[0.08]">
            <Gauge className="h-4 w-4 text-emerald-300" />
            <span
              className={cn(
                "absolute -right-0.5 -top-0.5 h-2.5 w-2.5 rounded-full border-2 border-[#090a0c]",
                isFetching ? "animate-pulse bg-amber-300" : "bg-emerald-400",
              )}
            />
          </div>

          <div className="min-w-0 flex-1">
            <div className="truncate text-[12px] font-semibold tracking-tight">
              CC Switch
            </div>
            <div className="truncate text-[9px] uppercase tracking-[0.14em] text-white/45">
              {t("usage.presetToday", "Today")}
            </div>
          </div>

          <MetricCompact
            label={t("usage.realTotal", "Tokens")}
            value={tokens}
          />
          <MetricCompact
            label={t("usage.cost", "Cost")}
            value={cost}
            accent
          />

          <button
            type="button"
            data-tauri-no-drag
            className="flex h-8 w-8 shrink-0 items-center justify-center rounded-full text-white/55 transition-colors hover:bg-white/10 hover:text-white"
            aria-label={
              compact
                ? t("usage.expand", "Expand")
                : t("usage.collapse", "Collapse")
            }
            onClick={() =>
              transitionTo(compact ? "expanded" : "compact")
            }
          >
            {compact ? (
              <ChevronDown className="h-4 w-4" />
            ) : (
              <ChevronUp className="h-4 w-4" />
            )}
          </button>
        </div>

        {!compact && (
          <div className="min-h-0 flex-1 px-4 pb-4 pt-3" data-tauri-no-drag>
            <div className="grid grid-cols-3 gap-2">
              <MetricCard
                label={t("usage.totalRequests", "Requests")}
                value={
                  isLoading ? "--" : summary.totalRequests.toLocaleString()
                }
              />
              <MetricCard
                label={t("usage.successRate", "Success")}
                value={isLoading ? "--" : formatPercent(summary.successRate)}
              />
              <MetricCard
                label={t("usage.cacheHitRate", "Cache Hit")}
                value={
                  isLoading
                    ? "--"
                    : formatPercent(summary.cacheHitRate * 100)
                }
              />
            </div>

            <div className="mt-3 flex min-h-7 items-center gap-2 overflow-hidden">
              <span className="shrink-0 text-[9px] font-semibold uppercase tracking-[0.13em] text-white/35">
                {t("usage.trayPanel.apps", "Apps")}
              </span>
              <div className="flex min-w-0 gap-1.5 overflow-hidden">
                {topApps.length > 0 ? (
                  topApps.map((app) => (
                    <span
                      key={app.appType}
                      className="truncate rounded-full bg-white/[0.07] px-2 py-1 text-[10px] text-white/70"
                    >
                      {app.appType} · {formatTokensShort(
                        app.summary.realTotalTokens,
                        lang,
                      )}
                    </span>
                  ))
                ) : (
                  <span className="text-[10px] text-white/35">
                    {t("usage.noData", "No data")}
                  </span>
                )}
              </div>
            </div>

            <div className="mt-3 flex items-center justify-between gap-2">
              <Button
                type="button"
                size="sm"
                variant="ghost"
                className="h-8 rounded-full px-3 text-white/70 hover:bg-white/10 hover:text-white"
                onClick={refresh}
                disabled={isFetching}
              >
                <RefreshCw
                  className={cn(
                    "mr-1.5 h-3.5 w-3.5",
                    isFetching && "animate-spin",
                  )}
                />
                {t("common.refresh", "Refresh")}
              </Button>

              <div className="flex items-center gap-1.5">
                <Button
                  type="button"
                  size="sm"
                  className="h-8 rounded-full bg-white text-[#111214] hover:bg-white/90"
                  onClick={() => transitionTo("details")}
                >
                  <CircleDollarSign className="mr-1.5 h-3.5 w-3.5" />
                  {t("usage.title", "Usage Statistics")}
                </Button>
                <Button
                  type="button"
                  size="icon"
                  variant="ghost"
                  className="h-8 w-8 rounded-full text-white/55 hover:bg-white/10 hover:text-white"
                  aria-label={t("common.close", "Close")}
                  onClick={hideWindow}
                >
                  <X className="h-3.5 w-3.5" />
                </Button>
              </div>
            </div>
          </div>
        )}
      </section>
    </div>
  );
}

function MetricCompact({
  label,
  value,
  accent = false,
}: {
  label: string;
  value: string;
  accent?: boolean;
}) {
  return (
    <div className="shrink-0 text-right">
      <div className="text-[8px] font-semibold uppercase tracking-[0.12em] text-white/35">
        {label}
      </div>
      <div
        className={cn(
          "mt-0.5 max-w-24 truncate font-mono text-[12px] font-semibold tabular-nums",
          accent ? "text-emerald-300" : "text-white/90",
        )}
        title={value}
      >
        {value}
      </div>
    </div>
  );
}

function MetricCard({ label, value }: { label: string; value: string }) {
  return (
    <div className="rounded-xl border border-white/[0.07] bg-white/[0.045] px-3 py-2.5">
      <div className="text-[9px] font-semibold uppercase tracking-[0.12em] text-white/35">
        {label}
      </div>
      <div className="mt-1 truncate font-mono text-[16px] font-semibold tabular-nums text-white/90">
        {value}
      </div>
    </div>
  );
}
