# ssctl

`ssctl`は、Supersetの端末に紐づくエージェントセッションを制御するためのRust製補助コマンドラインツールです。Supersetのコマンドラインツールを置き換えるものではなく、ローカルの役割別セッション管理、既存セッションへの送信、引き継ぎメッセージ、報告参照の配送を補完します。Supersetのpty-daemon経由で端末セッションを終了することもできます。

英語版の説明は[README.md](README.md)を参照してください。

## 概要

`ssctl`は、Codexなどの主処理からSupersetの端末に紐づくエージェントを補助エージェントのように扱う作業の流れを想定しています。公開されているSuperset操作はSupersetのコマンドラインツールに委譲し、既存セッションの確認と書き込みには実験的なSupersetのpty-daemon通信仕様v2とSupersetホストデータベースの付帯情報を使います。

## 要件

| 要件 | 詳細 |
| --- | --- |
| Supersetコマンドラインツール | 既定では`~/.superset/bin/superset`を使います。`--superset-bin <path>`で上書きできます。 |
| Supersetホーム | 既定では`~/.superset`を使います。`--superset-home <path>`で上書きできます。 |
| pty-daemon | 既存セッションの確認、書き込み、終了には、`~/.superset/host/<organization-id>/`配下のpty-daemon定義ファイルとホストデータベースを持つSupersetホストが起動している必要があります。 |
| Rust開発環境 | ソースからビルドまたは実行する場合に必要です。 |

## クイックスタート

1. Supersetコマンドラインツール、terminal-host診断、pty-daemonの状態を確認します。

   ```sh
   ssctl status
   ```

2. 利用可能なSupersetエージェントを一覧表示します。

   ```sh
   ssctl agents list
   ```

3. エージェントセッションを起動し、役割として登録します。

   ```sh
   ssctl spawn --agent codex --role worker-a --workspace <workspace-id> --prompt task.md
   ```

4. 稼働中のpty-daemonセッションを確認します。

   ```sh
   ssctl sessions
   ```

5. 登録済みの役割に追加メッセージを送信します。

   ```sh
   ssctl send --role worker-a --file followup.md
   ```

6. 役割が不要になったら、登録済みセッションを終了します。

   ```sh
   ssctl close --role worker-a
   ```

## コマンド

| コマンド | 用途 | 主なオプション |
| --- | --- | --- |
| `ssctl status` | Supersetコマンドラインツール、terminal-host診断、pty-daemonの利用可否を確認します。 | `--json` |
| `ssctl agents list` | Supersetエージェントを一覧表示します。 | `--json`, `--local`, `--host <host-id>` |
| `ssctl sessions` | 稼働中のpty-daemonセッションをSupersetホストデータベースの付帯情報と結合して一覧表示します。 | `--json` |
| `ssctl spawn` | エージェントセッションを起動し、ローカルの役割として登録します。 | `--agent <agent-id>`, `--role <role>`, `--workspace <workspace-id>`, `--prompt <file-or-text>`, `--json` |
| `ssctl send` | 登録済みの役割または検証済みセッションに入力を送信します。 | `--role <role>`, `--session <session-id>`, `--file <path>`, `--stdin`, `--dry-run` |
| `ssctl close` | 登録済みの役割または明示的に検証したセッションを終了します。 | `--role <role>`, `--session <session-id>`, `--signal <signal>`, `--dry-run`, `--json` |
| `ssctl handoff` | 別の役割に構造化された引き継ぎメッセージを送信します。 | `--to <role>`, `--file <path>` |
| `ssctl report` | 報告の写しを保存し、報告参照メッセージを送信します。 | `--to <role>`, `--file <path>` |

未登録セッションへの強制送信と強制終了には、明示的なセッションとワークスペースが必要です。

```sh
ssctl send --session <session-id> --stdin --force-unregistered-session --workspace <workspace-id>
ssctl close --session <session-id> --force-unregistered-session --workspace <workspace-id>
```

`ssctl close`の既定のシグナルは`SIGHUP`です。対応するシグナルは`SIGHUP`、`SIGINT`、`SIGTERM`、`SIGKILL`です。登録済みの役割を終了した場合、pty-daemonが終了を確認した後にだけローカルの登録情報から役割を削除します。

## 状態ファイル

| パス | 用途 |
| --- | --- |
| `.ssctl/registry.json` | active な role と session の対応、および進行中の `pendingSpawns` を保存します。 |
| `.ssctl/registry.lock` | registry の読み書きだけを保護する短命のロックです。 |
| `.ssctl/` | `ssctl` のローカル実行状態を保持します。 |
| `.agent-results/` | `ssctl report`が作成した報告の写しを保存します。 |
| `~/.superset/host/<organization-id>/pty-daemon-manifest.json` | pty-daemonソケットと対応する通信仕様の版を記録します。 |
| `~/.superset/host/<organization-id>/host.db` | ptyセッションにワークスペースとライフサイクルの付帯情報を付与するためのSupersetホストデータベースです。 |
| 定義ファイルに記録されたpty-daemonソケット | 既存セッションの確認、書き込み、終了に使うUnixソケットです。 |

ローカルの登録情報は、原子的な書き込み、`0600`のファイル権限、古いセッションの整理、未登録セッションへの強制送信に対する監査ログ記録を使います。

## 安全上の注意

- 公開Superset操作には公開Supersetコマンドラインツールを使います。
- 非公開のpty-daemon接続処理の用途は、既存セッションの確認、書き込み、終了に限定します。
- 通常の送信と終了は、登録情報で検証されたセッションのみを対象にします。
- 未登録セッションへ送信または終了するには、`--force-unregistered-session`と`--workspace <workspace-id>`の両方が必要です。
- 大きすぎるメッセージは、端末へ直接貼り付けず参照メッセージに変換します。
- `report`は報告の写しを`.agent-results/`に保存し、対象の役割には参照メッセージだけを送ります。
- `close --dry-run`は対象を解決・検証しますが、終了要求の送信や登録情報の変更は行いません。
