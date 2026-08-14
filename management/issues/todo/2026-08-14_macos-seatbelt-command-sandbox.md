---
title: macOS でのコマンド実行サンドボックス（Seatbelt）
created: 2026-08-14
urgency: medium
importance: high
priority: P1
scope: [infrastructure, docs]
---

# macOS でのコマンド実行サンドボックス（Seatbelt）

## As-is（現状）

`run_command` ツールのサンドボックスは Linux（Landlock）のみ実装されている。
macOS 上で直接エージェントを動かすと保護が得られず、fail closed の方針により
**ツールが登録されない**。つまり Mac のユーザーは、コンテナを経由しない限り
モデルにテストを実行させられない。

このリポジトリの開発は Linux コンテナ内で行うため実害は出ていないが、
「他プロジェクトでも汎用に使いたい」という目的に対しては穴になっている。

## To-be（あるべき姿）

macOS 上で直接エージェントを起動しても、Linux と同じ保証
（ワークスペース外への書き込み不可・ネットワーク接続不可）でコマンドを実行できる。
Mac のユーザーがコンテナを用意しなくても、モデルが自分でテストを回せる。

## 影響範囲

- **クレート**: `infrastructure/exec/` に Seatbelt アダプタを追加し、能力検出に組み込む。
  `domain` の `CommandRunner` ポートと `application` のツールは**変更不要**
  （プラットフォーム差はアダプタに閉じる設計になっているため）
- **不変条件**: なし（既存のポート契約を満たす実装追加。fail closed の方針も維持）
- **設定・ドキュメント**: `README.md` §5 の対応プラットフォーム表、`.env.example` の注記
- **利用者への影響**: macOS で `run_command` が使えるようになる（既定は無効のまま）

## 達成条件

- [ ] macOS 上でワークスペース外への書き込みが拒否されることをテストで固定する
- [ ] macOS 上でネットワーク接続が拒否されることをテストで固定する
- [ ] 子プロセスが `.git` に書き込めないことをテストで固定する
      （Seatbelt は `deny` を書けるため、Linux/Landlock 層で表現できなかったこの条件を
      **この層では満たせる**。コマンド実行 issue から移送）
- [ ] `SandboxKind::Seatbelt` が `agent doctor` に表示される
- [ ] `AGENT_SHELL_SANDBOX=confined` が macOS でそのまま成功する
      （要求を機構名ではなく性質で表す設計になっているため、設定を変えずに動くこと）
- [ ] Linux 側の挙動に退行がない
- [ ] `make check` が緑（macOS 固有テストは `#[cfg(target_os = "macos")]` で切る）

## 前提（コマンド実行 issue の完了で確定した部分）

以下は既に実装済みで、この issue で作るのは `exec/macos.rs` と `detect_sandbox()` の
macOS 分岐だけです。

- `CommandRunner` ポート、`SandboxKind`、fail-closed の起動判定
- egress プロキシ（`exec/proxy.rs`、プラットフォーム非依存）
- 環境変数の除去、出力上限、タイムアウト、セッション分離（`exec/{env,capture}.rs`）
- `RunCommandTool` と `AGENT_SHELL*` 設定

## 優先度の根拠

**2026-08-14 に P2 → P1 へ引き上げ（重要度 medium→high, 緊急度 low→medium）。**
この bin を CLI / SDK として配布する方針が固まり、「どんなユーザー環境でも動く」ことが
前提になった。Mac は主要な配布先であり、これ無しでは Mac ユーザーが `run_command` を
一切使えない（fail closed で登録されない）。配布物の主要プラットフォームでの機能欠落は
ゴールへの影響が大きいため重要度を high に、コマンド実行本体
`2026-08-14_sandboxed-command-execution-tool.md` の完了直後に着手すべきなので緊急度を
medium に上げた。

## メモ

- `sandbox-exec` は本コンテナのホスト（Darwin 25.6）で動作確認済み。
  Apple はこのコマンドを deprecated としているが代替の公開 API がなく、
  実運用ツール（各種 CLI エージェント）でも現役で使われている。
  将来削除された場合に fail closed に落ちることを確認しておくこと
- SBPL プロファイルを生成して `sandbox-exec -p` に渡す方式が素直。
  `(deny default)` から始めて、ワークスペース配下の読み書き・
  ツールチェーンの読み取り・`(deny network*)` を明示する
- CI（GitHub Actions の macOS runner）で回すかは要検討。
  Linux runner だけなら macOS テストは開発者のローカル実行に頼ることになる
