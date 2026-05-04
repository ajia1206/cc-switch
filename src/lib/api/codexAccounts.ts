import { invoke } from "@tauri-apps/api/core";

export interface CodexAccountSummary {
  accountKey: string;
  profileName: string;
  emailMasked: string;
  plan: string;
  authMode: string;
  isActive: boolean;
  lastUsedAt: number | null;
}

export interface CodexAccountSwitchResult {
  previousAccountKey: string | null;
  activeAccountKey: string;
  backupPath: string;
  restartRecommended: boolean;
}

export const codexAccountsApi = {
  async list(): Promise<CodexAccountSummary[]> {
    return await invoke("codex_list_account_snapshots");
  },

  async captureCurrent(label?: string): Promise<CodexAccountSummary> {
    return await invoke("codex_capture_current_account", {
      label: label?.trim() || undefined,
    });
  },

  async switch(accountKey: string): Promise<CodexAccountSwitchResult> {
    return await invoke("codex_switch_account", { accountKey });
  },

  async rollback(): Promise<CodexAccountSwitchResult> {
    return await invoke("codex_rollback_last_account_switch");
  },

  async restartCodex(): Promise<boolean> {
    return await invoke("codex_restart_app");
  },
};
