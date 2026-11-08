---
title: 秘密ファイルをツールから保護し、既知の秘密の値を出力と通知から伏せる
created: 2026-11-08
started: 2026-11-08
completed: 2026-11-08
urgency: high
importance: xhigh
priority: P0
scope: [domain, infrastructure, cli, docs]
---

# 秘密ファイルをツールから保護し、既知の秘密の値を出力と通知から伏せる

## As-is（現状）

**モデルはワークスペース内の秘密を、他のファイルと同じように読み書きできます。**

- `read_file .env` で API キーがそのまま返る。`.agent/sessions/*.jsonl`（会話の原文。
  過去にモデルが読んだファイルの中身を含む）も読める
- `write_file .env` も通る。`AGENT_BASE_URL` を書き換えられると、**次の起動で API キーが
  別のホストへ送られる**（compose の `env_file` と `main.rs` の `dotenvy::dotenv()` が起動時に読む）
- セッションログへの書き込みは、`--resume` で再生される偽の履歴になる（再開のたびに効く
  プロンプトインジェクション）
- `run_command` の子プロセスは Landlock で読み取りが全域許可なので、`cat .env` の出力が
  そのままモデルに返る

ここに Webhook 通知（`progress/2026-11-08_webhook-push-notifications.md`）が加わり、
**外への出口ができました。** プロンプトインジェクション → `.env` を読む → 回答の冒頭に書く →
ターン完了通知で外部サーバへ、という経路が、Webhook URL を設定した利用者には成立しています。

## To-be（あるべき姿）

- 既定で `.env` / `.env.*`（`.env.example` は除く）とセッションログの置き場所は、ファイル
  ツールから**読むことも書くこともできない**。シンボリックリンクを経由しても同じ。
  モデルには「保護されたファイルなので、必要ならユーザーに頼むこと」が伝わり、ループは続く
- 操作者は保護する場所を足せる
- 既知の秘密の値（API キー、Webhook URL、秘密らしい名前の環境変数）は、ツール出力に現れても
  伏せられる。`run_command` で `cat .env` されても、モデル・セッションログ・端末・通知の
  どこにも原文が残らない
- ターン完了通知の抜粋からも、既知の秘密の値が伏せられる
- `agent doctor` で何が保護されているかが分かる

## 影響範囲

- **クレート**:
  - `domain` … 既定の拒否リスト（字句の段。`WorkspacePath` と同じくビジネスルールとして）、
    保護対象を表す `FsError` の variant（モデル向けの文言）
  - `infrastructure` … `fs/local.rs` の `guard()`（正規化後のパスで再判定。シンボリックリンク対策）、
    `fs/search.rs`（下のメモの抜け道）、伏せ字の装飾子（`tools/timeout.rs` の `TimeoutTool` と
    同じ形で全ツールを包む）、`exec/env.rs` の `is_secret` を伏せ字側と共有
  - `cli` … `composition.rs`（装飾子の結線、`AGENT_SESSION_DIR` を拒否リストに入れる）、
    `notify.rs`（抜粋の伏せ字）、`doctor`
  - `application` … 原則変更なし（`FsError` → `ToolError` の既存の写像に乗る）
- **不変条件**: **§2 を強化する**（「ワークスペース内なら触れる」から「ワークスペース内でも
  保護対象は触れない」へ）。着手前に root の判断が要る。
  §2 の子プロセス節にも触れる — **パスによる拒否は `run_command` の子には効きません。**
  Landlock は読み取り可能なツリーの中の一部だけを拒否する表現を持たない（`.git` を守れないのと
  同じ制約。`todo/2026-08-14_netns-isolated-command-sandbox.md`）。子の経路で効くのは伏せ字だけで、
  この限界は「黙って弱くならない」に従い明記する。
  §3（拒否は `is_error` の `tool_result`。ループは止めない）
- **設定・ドキュメント**: `README.md` §5（保護の節を追加）、§3（通知の注意書きを更新）、
  §8（保護する場所を足す変数を作るなら）、`management/docs/architecture.md` §3.2。
  `.claude/rules/invariants.md` §2 と CLAUDE.md の不変条件の記述（root が更新）。
  `AGENTS.md`（エージェント自身が「保護されたファイルは読めない」ことを知っておくべきか）
- **利用者への影響**: **破壊的。** モデルに `.env` を編集させる使い方は動かなくなる。
  保護を解除する手段を設けるかは設計確認中

## 達成条件

- [x] `read_file` / `write_file` / `edit_file` で `.env`・`.env.local`・セッションログが拒否され、
      モデルに次の行動を示すメッセージが `is_error` の `tool_result` で返る（ループは継続）
- [x] `.env.example` は読める
- [x] シンボリックリンク経由（`link -> .env`、`dir -> .agent/sessions`）でも拒否されることを
      テストで固定する
- [x] `search_files` が `path: ".env"`・`path: ".agent/sessions"`・`include` に `.env` を含む glob の
      いずれでも中身を返さないことをテストで固定する。**着手時にまず現状で読めるかを再現し、
      結果をこの issue に記録する**（メモ参照。コード読みからの推測）
- [x] `AGENT_SESSION_DIR` をワークスペース内の別の場所にしても、その場所が保護される
- [x] 既知の秘密の値（`AGENT_API_KEY`、プロバイダ別のキー、Webhook URL、`is_secret` に該当する
      環境変数の値）がツール出力に現れたら伏せられ、会話・セッションログ・端末・通知のいずれにも
      原文が残らない。`run_command` で `cat .env` した出力を含む
- [x] 伏せ字は出力の切り詰め（`AGENT_MAX_TOOL_OUTPUT_BYTES`）より前に行う
      （切り詰めで秘密の前半だけが残らない）
- [x] ターン完了通知の抜粋から既知の秘密の値が伏せられる
- [x] 短すぎる値（下限は設計で決める）は伏せ字の対象にせず、誤検知でツール出力が壊れない
- [x] 装飾子の結線がテストで踏まれている（付け忘れが composition の無テスト部分に埋もれない）
- [x] 子プロセス経路の限界（パスでの拒否が効かない、egress 許可先へ値を送る経路は伏せ字では
      止まらない）が README §5 に書かれている
- [x] `make check` が緑

## 優先度の根拠

API キーが外部に出る経路が、プロンプトインジェクション 1 回で成立するセキュリティ上の穴で、
§2 の強化にあたる（重要度 `xhigh`）。Webhook 通知が入ったことで出口が実在し、双方向通知（C）と
MCP クライアント（F）はさらに外部との接点を増やすため、これらの前提になる（緊急度 `high`）。
score 11 → P0。

## メモ

- **設計確認中。** 拒否リストの既定値、解除手段の有無、伏せ字の対象と下限は着手時に決める
- 調査で分かったこと（2026-11-08, issue-agent。いずれもコード読みからの推測で、未再現）:
  - **`search_files` の抜け道。** `fs/search.rs` は `hidden(true)` と `.gitignore` で `.env` を
    隠しているが、`ignore` クレートのフィルタは走査の**起点（depth 0）には掛からない**。
    `path` 引数に `.env` や `.agent/sessions` を直接渡すと読めるはず。また `include` の glob は
    オーバーライド（whitelist）として渡しており、オーバーライドは hidden 判定より優先される
  - **ハードリンク。** `run_command` で `ln .env notes.txt` とすれば、パスによる判定はすり抜ける
    （Landlock はワークスペース内のリンク作成を許可している）。`nlink > 1` の拒否で塞ぐか、
    伏せ字を第 2 層として頼るか
  - `AGENT_SYSTEM_PROMPT_FILE` がワークスペース内を指していると、書き込みで恒久的なインジェクション
    になる。拒否リストに含めるかは設計で決める
  - `web_fetch` の URL に秘密を載せる経路は、既定ポリシーなら毎回 URL 全体を見せて確認するが、
    `auto` では止まらない。ツール**入力**側の伏せ字をこの issue に含めるかは設計で決める
  - 値の集合は `exec/env.rs` の `is_secret`（名前ベース）とプロバイダ設定・Webhook URL から作れる
- 関連:
  - `progress/2026-11-08_webhook-push-notifications.md` — 出口を作った issue。そのメモに
    この経路が既に書かれている
  - `todo/2026-08-14_macos-seatbelt-command-sandbox.md` — Seatbelt は `deny` を書けるので、
    子プロセスからの `.env` 読み取りも塞げる（Landlock ではできない）
  - `todo/2026-08-14_netns-isolated-command-sandbox.md` — 同じ「部分拒否を表現できない」制約
  - `todo/2026-08-14_composition-root-test-harness.md` — 装飾子の付け忘れが見えない問題
  - `todo/2026-11-08_approve-from-phone-via-ntfy.md`、`todo/2026-11-08_sandboxed-mcp-client.md`
    — この issue を前提にする

- 2026-11-08 完了: 字句段はツール解決時ではなくアダプタ（guard / searcher / context）で適用（迂回経路を塞ぐため）。既存の穴 3 件（リンク先不在のリンク経由の外部書き込み、外部を指すリンクでの全文検索、外部を指す AGENTS.md）も同時に閉じた。AGENT_SESSION_DIR=ワークスペースルートは起動エラー。残余リスクは README §5
