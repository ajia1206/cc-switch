import { useMemo, useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { useTranslation } from "react-i18next";
import {
  Check,
  Loader2,
  RefreshCw,
  RotateCcw,
  Save,
  Search,
  Undo2,
} from "lucide-react";
import { toast } from "sonner";
import { codexAccountsApi } from "@/lib/api";
import type { CodexAccountSummary } from "@/lib/api/codexAccounts";
import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Badge } from "@/components/ui/badge";
import { cn } from "@/lib/utils";
import { extractErrorMessage } from "@/utils/errorUtils";

const QUERY_KEY = ["codex", "account-snapshots"];

export function CodexAccountsPanel() {
  const { t } = useTranslation();
  const queryClient = useQueryClient();
  const [label, setLabel] = useState("");

  const accountsQuery = useQuery({
    queryKey: QUERY_KEY,
    queryFn: () => codexAccountsApi.list(),
  });

  const accounts = useMemo(
    () => accountsQuery.data ?? [],
    [accountsQuery.data],
  );

  const invalidate = async () => {
    await queryClient.invalidateQueries({ queryKey: QUERY_KEY });
  };

  const captureMutation = useMutation({
    mutationFn: () => codexAccountsApi.captureCurrent(label),
    onSuccess: async () => {
      setLabel("");
      await invalidate();
      toast.success(
        t("codexAccounts.captureSuccess", {
          defaultValue: "当前 Codex 账号已保存",
        }),
      );
    },
    onError: (error: Error) => {
      toast.error(
        t("codexAccounts.captureFailed", {
          defaultValue: "保存账号失败：{{error}}",
          error: extractErrorMessage(error),
        }),
      );
    },
  });

  const switchMutation = useMutation({
    mutationFn: (accountKey: string) => codexAccountsApi.switch(accountKey),
    onSuccess: async (result) => {
      await invalidate();
      toast.success(
        result.restartRecommended
          ? t("codexAccounts.switchSuccessRestart", {
              defaultValue: "账号已切换，建议重启 Codex App",
            })
          : t("codexAccounts.switchSuccess", {
              defaultValue: "账号已是当前账号",
            }),
      );
    },
    onError: (error: Error) => {
      toast.error(
        t("codexAccounts.switchFailed", {
          defaultValue: "切换账号失败：{{error}}",
          error: extractErrorMessage(error),
        }),
      );
    },
  });

  const rollbackMutation = useMutation({
    mutationFn: () => codexAccountsApi.rollback(),
    onSuccess: async () => {
      await invalidate();
      toast.success(
        t("codexAccounts.rollbackSuccess", {
          defaultValue: "已回滚到上一次 Codex 账号",
        }),
      );
    },
    onError: (error: Error) => {
      toast.error(
        t("codexAccounts.rollbackFailed", {
          defaultValue: "回滚失败：{{error}}",
          error: extractErrorMessage(error),
        }),
      );
    },
  });

  const restartMutation = useMutation({
    mutationFn: () => codexAccountsApi.restartCodex(),
    onSuccess: (result) => {
      toast.success(
        result.message ||
          t("codexAccounts.restartSuccess", {
            defaultValue: "Codex App 已重启",
          }),
      );
    },
    onError: (error: Error) => {
      toast.error(
        t("codexAccounts.restartFailed", {
          defaultValue: "重启 Codex App 失败：{{error}}",
          error: extractErrorMessage(error),
        }),
      );
    },
  });

  const scanMutation = useMutation({
    mutationFn: () => codexAccountsApi.list(),
    onSuccess: (nextAccounts) => {
      queryClient.setQueryData(QUERY_KEY, nextAccounts);
      toast.success(
        t("codexAccounts.scanSuccess", {
          defaultValue: "已扫描到 {{count}} 个 Codex 账号快照",
          count: nextAccounts.length,
        }),
      );
    },
    onError: (error: Error) => {
      toast.error(
        t("codexAccounts.scanFailed", {
          defaultValue: "扫描账号快照失败：{{error}}",
          error: extractErrorMessage(error),
        }),
      );
    },
  });

  return (
    <div className="px-6 flex flex-col flex-1 min-h-0 overflow-hidden">
      <div className="flex-1 overflow-y-auto overflow-x-hidden pb-12 px-1">
        <div className="max-w-5xl mx-auto space-y-4">
          <div className="flex flex-col gap-3 sm:flex-row sm:items-center sm:justify-between">
            <div className="min-w-0">
              <h2 className="text-xl font-semibold tracking-normal">
                {t("codexAccounts.title", {
                  defaultValue: "Codex 官方账号快照",
                })}
              </h2>
              <p className="mt-1 text-sm text-muted-foreground">
                {t("codexAccounts.subtitle", {
                  defaultValue:
                    "保存、切换和回滚 ~/.codex/auth.json 中的官方登录账号。",
                })}
              </p>
            </div>
            <div className="flex items-center gap-2">
              <Button
                variant="outline"
                size="sm"
                onClick={() => void accountsQuery.refetch()}
                disabled={accountsQuery.isFetching}
              >
                {accountsQuery.isFetching ? (
                  <Loader2 className="w-4 h-4 animate-spin" />
                ) : (
                  <RefreshCw className="w-4 h-4" />
                )}
                {t("common.refresh", { defaultValue: "刷新" })}
              </Button>
              <Button
                variant="outline"
                size="sm"
                onClick={() => scanMutation.mutate()}
                disabled={scanMutation.isPending}
              >
                {scanMutation.isPending ? (
                  <Loader2 className="w-4 h-4 animate-spin" />
                ) : (
                  <Search className="w-4 h-4" />
                )}
                {t("codexAccounts.scanAccounts", {
                  defaultValue: "扫描账号快照",
                })}
              </Button>
              <Button
                variant="outline"
                size="sm"
                onClick={() => rollbackMutation.mutate()}
                disabled={rollbackMutation.isPending}
              >
                {rollbackMutation.isPending ? (
                  <Loader2 className="w-4 h-4 animate-spin" />
                ) : (
                  <Undo2 className="w-4 h-4" />
                )}
                {t("codexAccounts.rollback", { defaultValue: "回滚" })}
              </Button>
              <Button
                variant="outline"
                size="sm"
                onClick={() => restartMutation.mutate()}
                disabled={restartMutation.isPending}
              >
                {restartMutation.isPending ? (
                  <Loader2 className="w-4 h-4 animate-spin" />
                ) : (
                  <RotateCcw className="w-4 h-4" />
                )}
                {t("codexAccounts.restart", { defaultValue: "重启 Codex" })}
              </Button>
            </div>
          </div>

          <div className="rounded-lg border bg-card p-4">
            <div className="flex flex-col gap-3 sm:flex-row sm:items-center">
              <Input
                value={label}
                onChange={(event) => setLabel(event.target.value)}
                placeholder={t("codexAccounts.labelPlaceholder", {
                  defaultValue: "给当前账号起个名字，例如：Plus 个人号",
                })}
                className="sm:max-w-sm"
              />
              <Button
                onClick={() => captureMutation.mutate()}
                disabled={captureMutation.isPending}
              >
                {captureMutation.isPending ? (
                  <Loader2 className="w-4 h-4 animate-spin" />
                ) : (
                  <Save className="w-4 h-4" />
                )}
                {t("codexAccounts.captureCurrent", {
                  defaultValue: "保存当前账号",
                })}
              </Button>
            </div>
          </div>

          {accountsQuery.isLoading ? (
            <div className="space-y-3">
              {[0, 1].map((index) => (
                <div
                  key={index}
                  className="h-24 rounded-lg border border-dashed bg-muted/40"
                />
              ))}
            </div>
          ) : accounts.length === 0 ? (
            <div className="rounded-lg border border-dashed px-6 py-10 text-center text-sm text-muted-foreground">
              {t("codexAccounts.empty", {
                defaultValue:
                  "还没有保存的 Codex 账号。先登录 Codex，再点击“保存当前账号”。",
              })}
            </div>
          ) : (
            <div className="space-y-3">
              {accounts.map((account) => (
                <AccountRow
                  key={account.accountKey}
                  account={account}
                  switchingKey={
                    switchMutation.isPending
                      ? switchMutation.variables
                      : undefined
                  }
                  onSwitch={(accountKey) => switchMutation.mutate(accountKey)}
                />
              ))}
            </div>
          )}
        </div>
      </div>
    </div>
  );
}

interface AccountRowProps {
  account: CodexAccountSummary;
  switchingKey?: string;
  onSwitch: (accountKey: string) => void;
}

function AccountRow({ account, switchingKey, onSwitch }: AccountRowProps) {
  const { t } = useTranslation();
  const isSwitching = switchingKey === account.accountKey;

  return (
    <div
      className={cn(
        "rounded-lg border bg-card p-4 transition-colors",
        account.isActive && "border-emerald-500/60 bg-emerald-500/5",
      )}
    >
      <div className="flex flex-col gap-3 sm:flex-row sm:items-center sm:justify-between">
        <div className="min-w-0 space-y-1">
          <div className="flex flex-wrap items-center gap-2">
            <h3 className="truncate text-base font-medium tracking-normal">
              {account.profileName}
            </h3>
            {account.isActive && (
              <Badge
                variant="secondary"
                className="border-emerald-500/30 bg-emerald-500/10 text-emerald-600 dark:text-emerald-400"
              >
                <Check className="mr-1 h-3 w-3" />
                {t("provider.inUse", { defaultValue: "使用中" })}
              </Badge>
            )}
            {account.plan && <Badge variant="outline">{account.plan}</Badge>}
          </div>
          <div className="flex flex-wrap gap-x-4 gap-y-1 text-xs text-muted-foreground">
            <span>{account.emailMasked || account.authMode}</span>
            {account.lastUsedAt && (
              <span>
                {new Date(account.lastUsedAt * 1000).toLocaleString()}
              </span>
            )}
          </div>
        </div>
        <Button
          size="sm"
          disabled={account.isActive || isSwitching}
          onClick={() => onSwitch(account.accountKey)}
          className="w-full sm:w-auto"
        >
          {isSwitching ? (
            <Loader2 className="w-4 h-4 animate-spin" />
          ) : account.isActive ? (
            <Check className="w-4 h-4" />
          ) : (
            <RotateCcw className="w-4 h-4" />
          )}
          {account.isActive
            ? t("provider.inUse", { defaultValue: "使用中" })
            : t("codexAccounts.switchTo", { defaultValue: "切换" })}
        </Button>
      </div>
    </div>
  );
}
