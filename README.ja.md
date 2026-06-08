# ssctl

`ssctl` は、Superset の terminal-backed agent session をオーケストレーションするための Rust 製ヘルパー CLI です。Superset CLI を置き換えるものではなく、ローカルの role ベース session 管理、既存 session への送信、handoff message、report pointer の配送を補完します。Superset pty-daemon 経由で terminal session を close することもできます。

英語版ドキュメントは [README.md](README.md) を参照してください。

## 概要

`ssctl` は、Codex などの main thread から Superset の terminal-backed agent を subagent のように扱うワークフローを想定しています。公開 Superset 操作は Superset CLI に委譲し、既存 session の確認と書き込みには実験的な Superset pty-daemon protocol v2 と Superset host DB metadata を使います。

詳細な設計メモは [docs/local/architecture.md](docs/local/architecture.md) にあります。

## 要件

| 要件 | 詳細 |
| --- | --- |
| Superset CLI | 既定では `~/.superset/bin/superset` を使います。`--superset-bin <path>` で上書きできます。 |
| Superset home | 既定では `~/.superset` を使います。`--superset-home <path>` で上書きできます。 |
| pty-daemon | 既存 session の確認、書き込み、close には、`~/.superset/host/<organization-id>/` 配下の pty-daemon manifest と host DB を持つ Superset host が起動している必要があります。 |
| Rust toolchain | ソースからビルドまたは実行する場合に必要です。 |

## クイックスタート

1. Superset CLI、terminal-host diagnostics、pty-daemon の状態を確認します。

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

4. active な pty-daemon session を確認します。

   ```sh
   ssctl sessions
   ```

5. 登録済み role に follow-up message を送信します。

   ```sh
   ssctl send --role worker-a --file followup.md
   ```

6. role が不要になったら、登録済み session を close します。

   ```sh
   ssctl close --role worker-a
   ```

## コマンド

| コマンド | 用途 | 主なオプション |
| --- | --- | --- |
| `ssctl status` | Superset CLI、terminal-host diagnostics、pty-daemon の利用可否を確認します。 | `--json` |
| `ssctl agents list` | Superset agent を一覧表示します。 | `--json`, `--local`, `--host <host-id>` |
| `ssctl sessions` | active な pty-daemon session を Superset host DB metadata と join して一覧表示します。 | `--json` |
| `ssctl spawn` | agent session を起動し、local role として登録します。 | `--agent <agent-id>`, `--role <role>`, `--workspace <workspace-id>`, `--prompt <file-or-text>`, `--json` |
| `ssctl send` | 登録済み role または検証済み session に入力を送信します。 | `--role <role>`, `--session <session-id>`, `--file <path>`, `--stdin`, `--dry-run` |
| `ssctl close` | 登録済み role または明示的に検証した session を close します。 | `--role <role>`, `--session <session-id>`, `--signal <signal>`, `--dry-run`, `--json` |
| `ssctl handoff` | 別 role に structured handoff message を送信します。 | `--to <role>`, `--file <path>` |
| `ssctl report` | report copy を保存し、report pointer message を送信します。 | `--to <role>`, `--file <path>` |

未登録 session への forced send と forced close には、明示的な session と workspace が必要です。

```sh
ssctl send --session <session-id> --stdin --force-unregistered-session --workspace <workspace-id>
ssctl close --session <session-id> --force-unregistered-session --workspace <workspace-id>
```

`ssctl close` の既定 signal は `SIGHUP` です。対応 signal は `SIGHUP`、`SIGINT`、`SIGTERM`、`SIGKILL` です。登録済み role を close した場合、pty-daemon が close を確認した後にだけ local registry から role を削除します。

## 状態ファイル

| パス | 用途 |
| --- | --- |
| `.ssctl/registry.json` | local role から session への対応を保存します。 |
| `.ssctl/` | lock file など registry 関連ファイルを保持します。 |
| `.agent-results/` | `ssctl report` が作成した report copy を保存します。 |
| `~/.superset/host/<organization-id>/pty-daemon-manifest.json` | pty-daemon socket と対応 protocol version を記録します。 |
| `~/.superset/host/<organization-id>/host.db` | pty session に workspace と lifecycle metadata を付与するための Superset host DB です。 |
| manifest に記録された pty-daemon socket | 既存 session の確認、書き込み、close に使う Unix socket です。 |

local registry は atomic write、`0600` のファイル権限、stale-session cleanup、forced unregistered send の audit logging を使います。

## 安全上の注意

- 公開 Superset 操作には公開 Superset CLI を使います。
- private pty-daemon adapter の用途は、既存 session の確認、書き込み、close に限定します。
- 通常の send と close は registry で検証された session のみを対象にします。
- 未登録 session へ送信または close するには、`--force-unregistered-session` と `--workspace <workspace-id>` の両方が必要です。
- 大きすぎる inline message は、terminal に直接 paste せず pointer message に変換します。
- `report` は report copy を `.agent-results/` に保存し、target role には pointer message だけを送ります。
- `close --dry-run` は target を解決・検証しますが、close request の送信や registry の変更は行いません。

## アーキテクチャ

ローカル設計は [docs/local/architecture.md](docs/local/architecture.md) に記載されています。リリースノートは [CHANGELOG.md](CHANGELOG.md) で管理しています。
