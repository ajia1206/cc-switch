import { useMutation, useQuery } from "@tanstack/react-query";
import { backupsApi } from "@/lib/api";

export function useMakaLlmBackupManager() {
  const {
    data: backups = [],
    isLoading,
    refetch,
  } = useQuery({
    queryKey: ["maka-llm-backups"],
    queryFn: () => backupsApi.listMakaLlmBackups(),
  });

  const createMutation = useMutation({
    mutationFn: () => backupsApi.createMakaLlmBackup(),
    onSuccess: () => refetch(),
  });

  const restoreMutation = useMutation({
    mutationFn: (filename: string) => backupsApi.restoreMakaLlmBackup(filename),
    onSuccess: () => refetch(),
  });

  const deleteMutation = useMutation({
    mutationFn: (filename: string) => backupsApi.deleteMakaLlmBackup(filename),
    onSuccess: () => refetch(),
  });

  return {
    backups,
    isLoading,
    create: createMutation.mutateAsync,
    isCreating: createMutation.isPending,
    restore: restoreMutation.mutateAsync,
    isRestoring: restoreMutation.isPending,
    remove: deleteMutation.mutateAsync,
    isDeleting: deleteMutation.isPending,
  };
}
