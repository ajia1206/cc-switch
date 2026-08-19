import { QueryClientProvider } from "@tanstack/react-query";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { CodexAccountsManager } from "@/components/codex/CodexAccountsPanel";
import { createTestQueryClient } from "../utils/testQueryClient";

const apiMocks = vi.hoisted(() => ({
  list: vi.fn(),
  captureCurrent: vi.fn(),
  switch: vi.fn(),
  rename: vi.fn(),
  rollback: vi.fn(),
  restartCodex: vi.fn(),
}));

const toastMocks = vi.hoisted(() => ({
  success: vi.fn(),
  error: vi.fn(),
  warning: vi.fn(),
}));

vi.mock("@/lib/api", () => ({
  codexAccountsApi: apiMocks,
}));

vi.mock("sonner", () => ({ toast: toastMocks }));

vi.mock("@/lib/query", () => ({
  useCodexAllQuotas: () => ({
    data: undefined,
    isFetching: false,
    refetch: vi.fn(),
  }),
  useCodexQuotaForecasts: () => ({ data: undefined, refetch: vi.fn() }),
  useSettingsQuery: () => ({
    data: { codexQuotaRefreshInterval: 300, usageAdaptiveRefresh: false },
  }),
  useSaveSettingsMutation: () => ({ isPending: false, mutateAsync: vi.fn() }),
}));

describe("CodexAccountsManager", () => {
  beforeEach(() => {
    vi.clearAllMocks();
    apiMocks.list.mockResolvedValue([
      {
        accountKey: "account-a",
        profileName: "API Account",
        emailMasked: "",
        plan: "",
        authMode: "apikey",
        isActive: false,
        lastUsedAt: null,
      },
      {
        accountKey: "account-b",
        profileName: "Account B",
        emailMasked: "b***@example.com",
        plan: "plus",
        authMode: "chatgpt",
        isActive: true,
        lastUsedAt: null,
      },
    ]);
    apiMocks.restartCodex.mockResolvedValue({
      wasRunning: true,
      quitRequested: true,
      quitGraceful: true,
      forceQuitUsed: false,
      opened: true,
      runningAfter: true,
      launchMethod: "bundleId",
      message: "Codex App 已重启",
    });
  });

  it("locks conflicting account actions while a switch is pending", async () => {
    apiMocks.switch.mockImplementation(() => new Promise(() => {}));
    const queryClient = createTestQueryClient();

    render(
      <QueryClientProvider client={queryClient}>
        <CodexAccountsManager />
      </QueryClientProvider>,
    );

    const renameButtons = await screen.findAllByRole("button", {
      name: "重命名账号",
    });
    fireEvent.click(renameButtons[0]);
    expect(screen.getByText("apikey")).toBeInTheDocument();
    const renameInput = screen.getByDisplayValue("API Account");
    const switchButton = screen.getByRole("button", { name: "切换并重启" });
    fireEvent.click(switchButton);

    await waitFor(() => {
      expect(apiMocks.switch).toHaveBeenCalledWith("account-a");
      expect(screen.getByRole("button", { name: "回滚" })).toBeDisabled();
      expect(screen.getByRole("button", { name: "重启 Codex" })).toBeDisabled();
      expect(switchButton).toBeDisabled();
    });

    fireEvent.keyDown(renameInput, { key: "Enter" });
    expect(apiMocks.rename).not.toHaveBeenCalled();
  });

  it("restarts Codex after switching to a different auth mode", async () => {
    apiMocks.switch.mockResolvedValue({
      previousAccountKey: "account-b",
      activeAccountKey: "account-a",
      backupPath: "/tmp/auth.backup.json",
      restartRecommended: true,
    });
    const queryClient = createTestQueryClient();

    render(
      <QueryClientProvider client={queryClient}>
        <CodexAccountsManager />
      </QueryClientProvider>,
    );

    fireEvent.click(await screen.findByRole("button", { name: "切换并重启" }));

    await waitFor(() => {
      expect(apiMocks.switch).toHaveBeenCalledWith("account-a");
      expect(apiMocks.restartCodex).toHaveBeenCalledTimes(1);
      expect(toastMocks.success).toHaveBeenCalledWith("Codex App 已重启");
    });
  });

  it("reports a restart failure without claiming the account switch failed", async () => {
    apiMocks.switch.mockResolvedValue({
      previousAccountKey: "account-b",
      activeAccountKey: "account-a",
      backupPath: "/tmp/auth.backup.json",
      restartRecommended: true,
    });
    apiMocks.restartCodex.mockRejectedValue(new Error("launch failed"));
    const queryClient = createTestQueryClient();

    render(
      <QueryClientProvider client={queryClient}>
        <CodexAccountsManager />
      </QueryClientProvider>,
    );

    fireEvent.click(await screen.findByRole("button", { name: "切换并重启" }));

    await waitFor(() => {
      expect(toastMocks.warning).toHaveBeenCalledWith(
        expect.stringContaining("账号已切换"),
      );
      expect(toastMocks.error).not.toHaveBeenCalled();
    });
  });

  it("restarts Codex after rolling back an account switch", async () => {
    apiMocks.rollback.mockResolvedValue({
      previousAccountKey: "account-a",
      activeAccountKey: "account-b",
      backupPath: "/tmp/rollback.backup.json",
      restartRecommended: true,
    });
    const queryClient = createTestQueryClient();

    render(
      <QueryClientProvider client={queryClient}>
        <CodexAccountsManager />
      </QueryClientProvider>,
    );

    fireEvent.click(await screen.findByRole("button", { name: "回滚" }));

    await waitFor(() => {
      expect(apiMocks.rollback).toHaveBeenCalledTimes(1);
      expect(apiMocks.restartCodex).toHaveBeenCalledTimes(1);
      expect(toastMocks.success).toHaveBeenCalledWith(
        expect.stringContaining("已回滚"),
      );
    });
  });

  it("reports a restart failure without claiming rollback failed", async () => {
    apiMocks.rollback.mockResolvedValue({
      previousAccountKey: "account-a",
      activeAccountKey: "account-b",
      backupPath: "/tmp/rollback.backup.json",
      restartRecommended: true,
    });
    apiMocks.restartCodex.mockRejectedValue(new Error("launch failed"));
    const queryClient = createTestQueryClient();

    render(
      <QueryClientProvider client={queryClient}>
        <CodexAccountsManager />
      </QueryClientProvider>,
    );

    fireEvent.click(await screen.findByRole("button", { name: "回滚" }));

    await waitFor(() => {
      expect(toastMocks.warning).toHaveBeenCalledWith(
        expect.stringContaining("已回滚"),
      );
      expect(toastMocks.error).not.toHaveBeenCalled();
    });
  });
});
