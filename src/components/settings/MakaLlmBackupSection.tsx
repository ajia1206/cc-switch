import { useState } from "react";
import { useTranslation } from "react-i18next";
import {
  Download,
  KeyRound,
  RotateCcw,
  ShieldCheck,
  Trash2,
} from "lucide-react";
import { toast } from "sonner";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Button } from "@/components/ui/button";
import { useMakaLlmBackupManager } from "@/hooks/useMakaLlmBackupManager";
import { extractErrorMessage } from "@/utils/errorUtils";

function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  return `${(bytes / 1024).toFixed(1)} KB`;
}

function formatDate(value: string): string {
  const date = new Date(value);
  return Number.isNaN(date.getTime()) ? value : date.toLocaleString();
}

export function MakaLlmBackupSection() {
  const { t } = useTranslation();
  const {
    backups,
    isLoading,
    create,
    isCreating,
    restore,
    isRestoring,
    remove,
    isDeleting,
  } = useMakaLlmBackupManager();
  const [restoreFilename, setRestoreFilename] = useState<string | null>(null);
  const [deleteFilename, setDeleteFilename] = useState<string | null>(null);

  const handleCreate = async () => {
    try {
      const backup = await create();
      toast.success(t("settings.makaLlmBackup.createSuccess"), {
        description: t("settings.makaLlmBackup.backupSummary", {
          connections: backup.connectionCount,
          credentials: backup.credentialCount,
        }),
      });
    } catch (error) {
      toast.error(
        extractErrorMessage(error) || t("settings.makaLlmBackup.createFailed"),
      );
    }
  };

  const handleRestore = async () => {
    if (!restoreFilename) return;
    try {
      const result = await restore(restoreFilename);
      setRestoreFilename(null);
      toast.success(t("settings.makaLlmBackup.restoreSuccess"), {
        description: result.safetyBackupFilename
          ? t("settings.makaLlmBackup.safetyBackupCreated", {
              filename: result.safetyBackupFilename,
            })
          : undefined,
        duration: 6000,
        closeButton: true,
      });
    } catch (error) {
      toast.error(
        extractErrorMessage(error) || t("settings.makaLlmBackup.restoreFailed"),
      );
    }
  };

  const handleDelete = async () => {
    if (!deleteFilename) return;
    try {
      await remove(deleteFilename);
      setDeleteFilename(null);
      toast.success(t("settings.makaLlmBackup.deleteSuccess"));
    } catch (error) {
      toast.error(
        extractErrorMessage(error) || t("settings.makaLlmBackup.deleteFailed"),
      );
    }
  };

  const busy = isCreating || isRestoring || isDeleting;

  return (
    <section className="space-y-3 border-t border-border/60 pt-4">
      <div className="flex items-start justify-between gap-3">
        <div>
          <div className="flex items-center gap-2">
            <KeyRound className="h-4 w-4 text-fuchsia-500" />
            <h4 className="text-sm font-medium">
              {t("settings.makaLlmBackup.title")}
            </h4>
          </div>
          <p className="mt-1 text-xs text-muted-foreground">
            {t("settings.makaLlmBackup.description")}
          </p>
        </div>
        <Button
          variant="outline"
          size="sm"
          className="h-7 shrink-0 px-2 text-xs"
          disabled={busy}
          onClick={handleCreate}
        >
          <Download className="mr-1 h-3 w-3" />
          {isCreating
            ? t("settings.makaLlmBackup.creating")
            : t("settings.makaLlmBackup.create")}
        </Button>
      </div>

      <div className="flex gap-2 rounded-lg border border-amber-500/25 bg-amber-500/5 px-3 py-2 text-xs text-muted-foreground">
        <ShieldCheck className="mt-0.5 h-4 w-4 shrink-0 text-amber-500" />
        <span>{t("settings.makaLlmBackup.securityNotice")}</span>
      </div>

      {isLoading ? (
        <div className="py-2 text-sm text-muted-foreground">
          {t("common.loading")}
        </div>
      ) : backups.length === 0 ? (
        <div className="py-2 text-sm text-muted-foreground">
          {t("settings.makaLlmBackup.empty")}
        </div>
      ) : (
        <div className="max-h-48 space-y-1.5 overflow-y-auto">
          {backups.map((backup) => (
            <div
              key={backup.filename}
              className="flex items-center justify-between gap-2 rounded-lg bg-muted/30 px-3 py-2 text-sm transition-colors hover:bg-muted/50"
            >
              <div className="min-w-0 flex-1">
                <div className="flex items-center gap-1.5">
                  <span className="truncate font-mono text-xs">
                    {backup.filename}
                  </span>
                  {backup.backupType === "safety" ? (
                    <span className="shrink-0 rounded bg-amber-500/15 px-1.5 py-0.5 text-[10px] text-amber-600 dark:text-amber-400">
                      {t("settings.makaLlmBackup.safety")}
                    </span>
                  ) : null}
                </div>
                <div className="text-xs text-muted-foreground">
                  {formatDate(backup.createdAt)} ·{" "}
                  {formatBytes(backup.sizeBytes)} ·{" "}
                  {t("settings.makaLlmBackup.backupSummary", {
                    connections: backup.connectionCount,
                    credentials: backup.credentialCount,
                  })}
                </div>
              </div>
              <div className="flex shrink-0 items-center gap-1">
                <Button
                  variant="ghost"
                  size="icon"
                  className="h-7 w-7 text-destructive hover:text-destructive"
                  disabled={busy}
                  onClick={() => setDeleteFilename(backup.filename)}
                  title={t("settings.makaLlmBackup.delete")}
                >
                  <Trash2 className="h-3 w-3" />
                </Button>
                <Button
                  variant="ghost"
                  size="sm"
                  className="h-7 px-2 text-xs"
                  disabled={busy}
                  onClick={() => setRestoreFilename(backup.filename)}
                >
                  <RotateCcw className="mr-1 h-3 w-3" />
                  {t("settings.makaLlmBackup.restore")}
                </Button>
              </div>
            </div>
          ))}
        </div>
      )}

      <Dialog
        open={!!restoreFilename}
        onOpenChange={(open) => !open && setRestoreFilename(null)}
      >
        <DialogContent className="max-w-md" zIndex="alert">
          <DialogHeader>
            <DialogTitle>
              {t("settings.makaLlmBackup.restoreTitle")}
            </DialogTitle>
            <DialogDescription>
              {t("settings.makaLlmBackup.restoreConfirm")}
            </DialogDescription>
          </DialogHeader>
          <DialogFooter>
            <Button
              variant="outline"
              onClick={() => setRestoreFilename(null)}
              disabled={isRestoring}
            >
              {t("common.cancel")}
            </Button>
            <Button onClick={handleRestore} disabled={isRestoring}>
              {isRestoring
                ? t("settings.makaLlmBackup.restoring")
                : t("settings.makaLlmBackup.restore")}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      <Dialog
        open={!!deleteFilename}
        onOpenChange={(open) => !open && setDeleteFilename(null)}
      >
        <DialogContent className="max-w-md" zIndex="alert">
          <DialogHeader>
            <DialogTitle>{t("settings.makaLlmBackup.deleteTitle")}</DialogTitle>
            <DialogDescription>
              {t("settings.makaLlmBackup.deleteConfirm")}
            </DialogDescription>
          </DialogHeader>
          <DialogFooter>
            <Button
              variant="outline"
              onClick={() => setDeleteFilename(null)}
              disabled={isDeleting}
            >
              {t("common.cancel")}
            </Button>
            <Button
              variant="destructive"
              onClick={handleDelete}
              disabled={isDeleting}
            >
              {t("settings.makaLlmBackup.delete")}
            </Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
    </section>
  );
}
