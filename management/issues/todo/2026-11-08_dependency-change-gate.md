---
title: 依存の追加・削除を検知し、追加・削除される依存を示した専用の承認を挟む
created: 2026-11-08
urgency: medium
importance: high
priority: P1
scope: [domain, application, infrastructure, cli, docs]
---

# 依存の追加・削除を検知し、追加・削除される依存を示した専用の承認を挟む

## As-is（現状）

- モデルが `Cargo.toml` や `package.json` を書き換えて依存を足しても、他のファイル編集と同じ扱いです。
  既定の `read-only` ポリシーなら承認は出ますが、**表示は全体で 160 バイト、文字列引数は 60 バイトで
  切られます**（`dispatch.rs` の `MAX_SUMMARY_BYTES` / `MAX_ARGUMENT_BYTES`）。どの依存が増えるかは、
  承認画面からはほぼ読み取れません
- `auto` ポリシーや、承認で `a`（以降すべて許可）と答えた後は、依存の追加に誰も気づきません
- `run_command` で `cargo add` / `npm install` / `cargo update` を実行すると、マニフェストや
  ロックファイルが（推移的な依存を含めて）変わります。コマンドは承認されても、結果として何が
  入ったかは表示されません
- 入った依存に既知の脆弱性があるかは確かめていません

## To-be（あるべき姿）

- マニフェスト・ロックファイルを変えるファイル操作の前に、**追加・削除される依存（名前とバージョン）を
  並べた専用の承認**が出る。`auto` でも、`a` と答えた後でも出る
- `run_command` の前後でマニフェスト・ロックファイルが変わったら、何が変わったかの警告が出る
  （実行後なので止められないが、気づける）。モデルにも伝わる
- 任意で、追加された依存を脆弱性データベースに照会し、結果を承認画面に出せる
  （既定は無効。外部に問い合わせるため）

## 影響範囲

- **クレート**:
  - `domain` … 承認要求に「依存の変更」を表す種類（`ApprovalRequest` は今、呼び出し・安全区分・
    1 行要約しか持たない）、依存差分の値、脆弱性照会のポート（照会を入れるなら）
  - `application` … 書き込み系ツールの実行前に差分を出す場所（ディスパッチャの承認段か、ツール側か）、
    `run_command` の前後比較（ファイルは `FileSystem` ポート経由で読む）
  - `infrastructure` … マニフェスト・ロックファイルの解析（エコシステムごと）、脆弱性 DB の
    HTTP クライアント（既定無効）
  - `cli` … `approval.rs`（`Auto` と `approve_all` を素通りさせない分岐、差分の表示）、
    `composition.rs`、`notify.rs`（承認待ち通知に載るか）
- **不変条件**:
  - §3 — 拒否は `is_error` の `tool_result`。ループは続く
  - §5 — 脆弱性照会のテストはモックサーバで
  - §6 の趣旨 — 解析のために TOML パーサなどの依存を足すと、それ自体がサプライチェーンの表面積になる
  - §2 子プロセス節 — `run_command` の中では `build.rs` が既に実行されている。検知は事後でしかない
- **設定・ドキュメント**: `README.md` §3（承認）・§5・§8（ゲートの有効／無効、脆弱性照会の有効化と宛先）、
  `management/docs/architecture.md`
- **利用者への影響**: `auto` でも依存変更の承認が出るようになる。**非対話の `agent run`（`auto` での
  無人実行）での扱いは設計確認中** — 拒否にすると、これまで通っていた無人実行が止まる

## 達成条件

- [ ] 対象エコシステム（最初にどれに対応するかは設計で決める。少なくとも Cargo）のマニフェストを
      `write_file` / `edit_file` で変えると、追加・削除される依存の一覧つきで承認が求められる
- [ ] `auto` ポリシーでも、`a`（以降すべて許可）と答えた後でも、その承認が出る
- [ ] 依存の集合が変わらない編集（コメントや無関係なフィールド）では専用の承認が出ない
- [ ] 拒否するとファイルは変わらず、理由が `is_error` の `tool_result` でモデルに返る
- [ ] `run_command` の前後でマニフェスト・ロックファイルが変わると、変更内容の警告が stderr と
      モデルへの結果の両方に出る（実プロセスで固定）
- [ ] 非対話（`agent run`）での扱いが決まっていて、テストで固定されている
- [ ] 脆弱性照会は既定で無効で、無効のとき外向きの通信が一切ない。有効時の照会と表示をモックサーバで固定
- [ ] `make check` が緑

## 優先度の根拠

サプライチェーンの隔離（ゴール 3）に直結し、`auto` や一括許可の後は依存の追加が素通りして気づく
手段がない（重要度 `high`）。MCP クライアント（サーバ起動時に npx などがパッケージを取得する）と
双方向通知（端末を見ずに承認する経路）が、依存を持ち込む・見ずに承認する経路を新しく足す。
先にゲートがあれば、それらはゲートを前提に作れる（緊急度 `medium`）。score 8 → P1。

## メモ

- **設計確認中。**
- 調査で分かったこと（2026-11-08, issue-agent）:
  - 承認の分岐は `cli/src/approval.rs` の `needs_confirmation`（`Auto` は即承認）と `approve_all` にある。
    「`auto` でも挟む」には、ポリシー判定より前に置くか、`ApprovalRequest` の種類で分岐する必要がある
  - 前後比較のハッシュは変更検知のためで、std の `Hasher` で足りる（暗号学的強度は要らない。
    新しい依存は不要）
  - `Cargo.toml` / `Cargo.lock` は TOML。`toml` クレートを足すとゴール 3 の表面積が増える。
    `Cargo.lock` の `[[package]]` の `name` / `version` 程度なら自前で読める範囲か検討する
  - 照会先の候補は OSV（エコシステム横断）。`web_fetch` の SSRF ガード付きフェッチャを流用できるか
- 関連:
  - `todo/2026-11-08_sandboxed-mcp-client.md`（F）— サーバ起動時のパッケージ取得
  - `todo/2026-11-08_approve-from-phone-via-ntfy.md`（C）— 依存変更の承認を遠隔で出すとき、差分をどこまで見せるか
  - `done/2026-08-14_sandboxed-command-execution-tool.md`

### 設計確認の結論（2026-11-08 domain-specialist → root 決定）

- **承認は dispatcher の Phase 2（逐次実行）で各 invoke の直前**。`TimeoutTool` の内側（ツールの execute / FS 装飾子）に置かない（60 秒で打ち切られ stdin を飲み込む）
- Tool ポートに `async fn preview(&self, args) -> Result<Vec<PlannedWrite>, ToolError>` を**既定実装なし**で追加（装飾子の転送漏れをコンパイルエラーにする）。write/edit 以外は空 Vec。edit の計算は execute と共通関数に
- ApprovalGate に専用メソッド `authorize_dependency_change(&DependencyApprovalRequest)`、**既定実装は Deny**。CLI 実装は interactive なら policy も approve_all も無視して必ず聞く。非対話は Deny（`--yes` を案内しない文言）
- **承認を挟む条件は「分類されたパス + 内容の不一致」**。パーサは差分の説明だけ（読めなくても必ず聞く）。分類は basename を ASCII 小文字で比較（`ManifestKind::classify` は domain `model/dependency.rs`）。解析は application `supply_chain/`（行ベース + serde_json、TOML/YAML パーサは足さない）。表示は件数上限・制御文字除去
- run_command 経由は infrastructure の装飾子 `DependencyWatchTool` を **TimeoutTool の外側**に置き、前後ハッシュ比較（node_modules/target/.git/.venv/vendor は除外、上限超過は注記）。注記は tool_result の**先頭**、端末へは `AgentEvent::DependenciesChanged`。これは検知であり防止ではない、と README に正直に書く
- OSV 照会は v1 から外し別 issue（既定 Null 実装の形で）
- root 決定: invariants.md には載せず README / architecture に範囲付きで記載。CI 用の逃げ道は `AGENT_ALLOW_DEPENDENCY_CHANGES=true`（既定 false、ApprovalPolicy とは別）。ask/read-only での二重プロンプトは v1 では許容。シンボリックリンク別名は v1 の既知の穴として記録
