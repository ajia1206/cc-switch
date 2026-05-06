<div align="center">

# CC Switch · Multi-Account Usage Monitor Edition

### A personal enhanced fork of [farion1231/cc-switch](https://github.com/farion1231/cc-switch), adding real-time multi-account usage monitoring for Codex

[![Version](https://img.shields.io/badge/version-3.14.1-blue.svg)](https://github.com/ajia1206/cc-switch/releases)
[![Platform](https://img.shields.io/badge/platform-macOS%20%7C%20Windows%20%7C%20Linux-lightgrey.svg)](https://github.com/ajia1206/cc-switch/releases)
[![Built with Tauri](https://img.shields.io/badge/built%20with-Tauri%202-orange.svg)](https://tauri.app/)
[![License](https://img.shields.io/badge/license-MIT-green.svg)](LICENSE)

[中文](README.md) | English | [Upstream](https://github.com/farion1231/cc-switch)

</div>

---

## 📌 About This Fork

This is a personal enhanced version of [farion1231/cc-switch](https://github.com/farion1231/cc-switch). The upstream project is an excellent account-switching manager for Claude Code / Codex / Gemini CLI / OpenCode / OpenClaw. This fork **focuses on solving the usage-visibility pain point for users with multiple accounts**.

If you manage multiple Codex official accounts, frequently hit the 5-hour or 7-day rate limits, and need to know which account still has capacity at a glance — this fork is for you.

---

## ✨ Key Enhancements Over Upstream

### 🎯 Real-time Multi-Account Codex Usage Monitoring

| Feature | Upstream | This Fork |
|---------|:--------:|:---------:|
| Single account usage query | ✅ | ✅ |
| **Concurrent multi-account queries** | ❌ | ✅ |
| **Per-account 5h / 7d display** | ❌ | ✅ |
| **Reset countdown** | ❌ | ✅ |
| **Configurable refresh interval** | ❌ | ✅ |
| **Manual refresh button** | ❌ | ✅ |

### Detailed Features

- **🔄 Concurrent All-Account Querying** — A new backend command `get_all_account_quotas` walks through every Codex snapshot and queries usage in parallel. No need to switch accounts to inspect them
- **📊 Per-Card Display** — Each account card shows `5h Remaining: XX% · resets in X hours` and `7d Remaining: XX% · resets in X days`
- **🎨 Tiered Color Coding** — ≥30% remaining = green / ≥10% = orange / <10% = red
- **⏱ Configurable Refresh Interval** — Dropdown to pick `1 / 5 / 30 / 60 min`, defaults to 5 min
- **⚡ Manual Refresh Button** — One-click sync when you want fresh numbers right now
- **💾 Persisted Settings** — Refresh interval saved to `~/.cc-switch/settings.json`, restored on restart

---

## 📦 Installation

### Option 1: Download DMG (macOS Apple Silicon)

Grab `CC Switch_3.14.1_aarch64.dmg` from the [Releases](https://github.com/ajia1206/cc-switch/releases) page.

```bash
# Double-click the dmg → drag to Applications
# On first launch if macOS says "cannot verify developer",
# go to System Settings → Privacy & Security → Open Anyway
```

### Option 2: Build from Source

```bash
# 1. Clone the repository
git clone https://github.com/ajia1206/cc-switch.git
cd cc-switch

# 2. Install dependencies (requires Node 22+ and the Rust toolchain)
pnpm install

# 3. Run in dev mode
pnpm dev

# 4. Build for release
pnpm tauri build
# Output: src-tauri/target/release/bundle/dmg/
```

**Prerequisites:**
- Node.js 22.12+ (recommend [fnm](https://github.com/Schniz/fnm) for version management)
- Rust 1.77+ (`rustup install stable`)
- pnpm 9+ (`npm i -g pnpm`)
- macOS: Xcode Command Line Tools

---

## 🎮 Usage

### View Multi-Account Codex Usage

1. Open CC Switch → switch to the **Codex** tab on top
2. Click **"Codex Official Account Snapshots"**
3. Each account card automatically shows its usage:

```
┌─────────────────────────────────────┐
│ 📦 My Primary Account     [Active]  │
│  ⏱ 5h Remaining: 73%  · resets in 2h│
│  📅 7d Remaining: 45%  · resets in 5d│
└─────────────────────────────────────┘
```

### Adjust Refresh Strategy

- **Top dropdown menu**: pick refresh interval (1 / 5 / 30 / 60 min)
- **🔄 Manual refresh button**: trigger a concurrent query of all accounts immediately

### Switch Accounts

Click "Switch to this account" on any account card. The app will:
1. Back up the current `~/.codex/auth.json` to the active account's snapshot
2. Restore the target account's snapshot to `~/.codex/auth.json`
3. Restart Codex-related processes so the credentials take effect

---

## 🛠 Technical Implementation

| Module | File | Description |
|--------|------|-------------|
| Multi-account query | `src-tauri/src/codex_accounts.rs` | `get_all_account_quotas` queries all snapshots concurrently |
| Tauri command | `src-tauri/src/commands/codex_accounts.rs` | `get_all_codex_quotas` exposed to frontend |
| Usage cache | `src-tauri/src/services/subscription.rs` | Per-account independent TTL cache |
| Settings persistence | `src-tauri/src/settings.rs` | `codex_quota_refresh_interval` field |
| Frontend query | `src/lib/query/subscription.ts` | `useAllCodexQuotas` hook |
| Account panel | `src/components/codex/CodexAccountsPanel.tsx` | Card usage rendering + refresh controls |

---

## 🙏 Acknowledgements

- Upstream: [@farion1231/cc-switch](https://github.com/farion1231/cc-switch) by Jason Young
- For full upstream features (Claude/Codex/Gemini/OpenCode/OpenClaw multi-provider switching), see the [original README](https://github.com/farion1231/cc-switch/blob/main/README.md)
- The usage-query approach in this fork was inspired by [ericjypark/codex-island](https://github.com/ericjypark/codex-island)

---

## 📄 License

This project inherits the upstream [MIT License](LICENSE). Copyright belongs to Jason Young and respective contributors.

---

## 🔗 Links

- This fork: [github.com/ajia1206/cc-switch](https://github.com/ajia1206/cc-switch)
- Upstream: [github.com/farion1231/cc-switch](https://github.com/farion1231/cc-switch)
- Releases: [github.com/ajia1206/cc-switch/releases](https://github.com/ajia1206/cc-switch/releases)
- Issues: [github.com/ajia1206/cc-switch/issues](https://github.com/ajia1206/cc-switch/issues)
