import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { MakaLlmBackupSection } from "@/components/settings/MakaLlmBackupSection";

const createMock = vi.fn();
const restoreMock = vi.fn();
const removeMock = vi.fn();

vi.mock("sonner", () => ({
  toast: {
    success: vi.fn(),
    error: vi.fn(),
  },
}));

vi.mock("react-i18next", () => ({
  useTranslation: () => ({
    t: (key: string, options?: Record<string, unknown>) =>
      key === "settings.makaLlmBackup.backupSummary"
        ? `${options?.connections} connections, ${options?.credentials} credentials`
        : key,
  }),
}));

vi.mock("@/hooks/useMakaLlmBackupManager", () => ({
  useMakaLlmBackupManager: () => ({
    backups: [
      {
        filename: "maka_llm_20260804_120000_safety.json",
        sizeBytes: 2048,
        createdAt: "2026-08-04T12:00:00+08:00",
        backupType: "safety",
        connectionCount: 6,
        credentialCount: 6,
        includesCredentials: true,
      },
    ],
    isLoading: false,
    create: createMock,
    isCreating: false,
    restore: restoreMock,
    isRestoring: false,
    remove: removeMock,
    isDeleting: false,
  }),
}));

describe("MakaLlmBackupSection", () => {
  beforeEach(() => {
    createMock.mockReset();
    restoreMock.mockReset();
    removeMock.mockReset();
    createMock.mockResolvedValue({
      connectionCount: 6,
      credentialCount: 6,
    });
  });

  it("shows protected Maka backup metadata without rendering credential values", () => {
    render(<MakaLlmBackupSection />);

    expect(
      screen.getByText("settings.makaLlmBackup.title"),
    ).toBeInTheDocument();
    expect(
      screen.getByText("settings.makaLlmBackup.securityNotice"),
    ).toBeInTheDocument();
    expect(
      screen.getByText("maka_llm_20260804_120000_safety.json"),
    ).toBeInTheDocument();
    expect(
      screen.getByText(/6 connections, 6 credentials/),
    ).toBeInTheDocument();
    expect(
      screen.getByText("settings.makaLlmBackup.safety"),
    ).toBeInTheDocument();
    expect(screen.queryByText(/secret|api[_-]?key/i)).not.toBeInTheDocument();
  });

  it("creates a Maka LLM backup from the backup button", async () => {
    render(<MakaLlmBackupSection />);

    fireEvent.click(
      screen.getByRole("button", { name: "settings.makaLlmBackup.create" }),
    );

    await waitFor(() => expect(createMock).toHaveBeenCalledTimes(1));
  });

  it("requires confirmation before restoring a backup", () => {
    render(<MakaLlmBackupSection />);

    fireEvent.click(
      screen.getByRole("button", { name: "settings.makaLlmBackup.restore" }),
    );

    expect(
      screen.getByText("settings.makaLlmBackup.restoreTitle"),
    ).toBeInTheDocument();
    expect(
      screen.getByText("settings.makaLlmBackup.restoreConfirm"),
    ).toBeInTheDocument();
    expect(restoreMock).not.toHaveBeenCalled();
  });
});
