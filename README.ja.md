# ssctl

`ssctl` は、Superset の terminal-backed agent session をオーケストレーションするための Rust 製ヘルパー CLI です。Superset CLI を置き換えるものではなく、ローカルの role ベース session 管理、既存 session への送信、handoff message、report pointer の配送を補完します。

英語版ドキュメントは [README.md](README.md) を参照してください。

## 概要

`ssctl` は、Codex などの main thread から Superset の terminal-backed agent を subagent のように扱うワークフローを想定しています。公開 Superset 操作は Superset CLI に委譲し、既存 session の確認と書き込みには実験的な terminal-host protocol v2 を使います。

詳細な設計メモは [docs/local/architecture.md](docs/local/architecture.md) にあります。

## 要件

| 要件 | 詳細 |
| --- | --- |
| Superset CLI | 既定では `~/.superset/bin/superset` を使います。`--superset-bin <path>` で上書きできます。 |
| Superset home | 既定では `~/.superset` を使います。`--superset-home <path>` で上書きできます。 |
| terminal-host | 既存 session の確認と書き込みには `~/.superset/terminal-host.sock` と `~/.superset/terminal-host.token` が必要です。 |
| Rust toolchain | ソースからビルドまたは実行する場合に必要です。 |

## クイックスタート

1. Superset CLI と terminal-host の状態を確認します。

   ```sh
   ssctl status
   ```

2. 利用可能な Superset agent を一覧表示します。

   ```sh
   ssctl agents list
   ```

3. agent session を起動し、role として登録します。

   ```sh
   ssctl spawn --agent codex --role worker-a --workspace <workspace-id> --prompt task.md
   ```

4. active な terminal-host session を確認します。

   ```sh
   ssctl sessions
   ```

5. 登録済み role に follow-up message を送信します。

   ```sh
   ssctl send --role worker-a --file followup.md
   ```

## コマンド

| コマンド | 用途 | 主なオプション |
| --- | --- | --- |
| `ssctl status` | Superset CLI と terminal-host の利用可否を確認します。 | `--json` |
| `ssctl agents list` | Superset agent を一覧表示します。 | `--json`, `--local`, `--host <host-id>` |
| `ssctl sessions` | terminal-host session を一覧表示します。 | `--json` |
| `ssctl spawn` | agent session を起動し、local role として登録します。 | `--agent <agent-id>`, `--role <role>`, `--workspace <workspace-id>`, `--prompt <file-or-text>`, `--json` |
| `ssctl send` | 登録済み role または検証済み session に入力を送信します。 | `--role <role>`, `--session <session-id>`, `--file <path>`, `--stdin`, `--dry-run` |
| `ssctl handoff` | 別 role に structured handoff message を送信します。 | `--to <role>`, `--file <path>` |
| `ssctl report` | report copy を保存し、report pointer message を送信します。 | `--to <role>`, `--file <path>` |

未登録 session への forced send には、明示的な session と workspace が必要です。

```sh
ssctl send --session <session-id> --stdin --force-unregistered-session --workspace <workspace-id>
```

## 状態ファイル

| パス | 用途 |
| --- | --- |
| `.ssctl/registry.json` | local role から session への対応を保存します。 |
| `.ssctl/` | lock file など registry 関連ファイルを保持します。 |
| `.agent-results/` | `ssctl report` が作成した report copy を保存します。 |
| `~/.superset/terminal-host.sock` | 既存 session 操作用の terminal-host protocol v2 Unix socket です。 |
| `~/.superset/terminal-host.token` | terminal-host protocol v2 の authentication token です。 |

local registry は atomic write、`0600` のファイル権限、stale-session cleanup、forced unregistered send の audit logging を使います。

## 安全上の注意

- 公開 Superset 操作には公開 Superset CLI を使います。
- private terminal-host adapter の用途は、既存 session の確認と書き込みに限定します。
- 通常の send は registry で検証された session のみを対象にします。
- 未登録 session へ送信するには、`--force-unregistered-session` と `--workspace <workspace-id>` の両方が必要です。
- 大きすぎる inline message は、terminal に直接 paste せず pointer message に変換します。
- `report` は report copy を `.agent-results/` に保存し、target role には pointer message だけを送ります。

## アーキテクチャ

ローカル設計は [docs/local/architecture.md](docs/local/architecture.md) に記載されています。リリースノートは [CHANGELOG.md](CHANGELOG.md) で管理しています。
