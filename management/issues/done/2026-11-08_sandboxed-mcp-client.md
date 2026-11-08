---
title: MCP サーバ（stdio）を接続してツールをモデルに渡す（サーバはサンドボックスの中で起動）
created: 2026-11-08
urgency: low
importance: medium
priority: P2
started: 2026-11-08
completed: 2026-11-08
scope: [domain, application, infrastructure, cli, test-support, docs]
---

# MCP サーバ（stdio）を接続してツールをモデルに渡す（サーバはサンドボックスの中で起動）

## As-is（現状）

モデルが使えるのは組み込みのツール（ファイル系 5 つ、`web_fetch`、`run_command`）だけです。
MCP サーバとして提供されている外部ツール（issue トラッカー、DB、ブラウザなど）をつなぐ手段が
ありません。CLI を `run_command` で叩く回避策はありますが、入出力が構造化されず、
ネットワーク許可もコマンドごとに考える必要があります。

## To-be（あるべき姿）

- 設定した MCP サーバ（stdio）が起動時に立ち上がり、そのツールが組み込みツールと同じように
  `agent tools` に並び、モデルが呼べる
- サーバは `run_command` と同じ OS サンドボックス（ワークスペース外への書き込み不可）と
  egress 許可リストの中で動く。サーバのコードを信用しなくても、できることは封じ込められている
- サンドボックスが得られない環境では MCP サーバは起動せず、理由が表示される
  （封じ込めなしで黙って動くことはない）
- MCP ツールの呼び出しは承認ゲートを通る
- エージェントが終わるとサーバも残らず止まる

## 影響範囲

- **クレート**:
  - `domain` … 長く動き続ける子プロセスとの対話を表すポート。**今の `CommandRunner` は 1 回実行して
    出力を返す形**（stdin は null、タイムアウトで終了）で、stdio で話し続けるサーバには使えない
  - `infrastructure` … `exec/`（長寿命プロセスへのサンドボックス適用、egress プロキシの寿命を
    サーバに合わせる、子の環境変数の除去との両立）、MCP クライアント（JSON-RPC、初期化、
    `tools/list`、`tools/call`）、設定ファイルの読み込み
  - `application` … MCP ツールを `Tool` として扱う（名前空間、`ToolSafety` の決め方）
  - `cli` … `composition.rs`（起動時にサーバを立ち上げ、ツールを登録し、`TimeoutTool` と A の
    伏せ字の装飾子で包む）、`tools` / `doctor` の表示、終了時の停止
  - `test-support` … ネットワークなしで動くテスト用の MCP サーバ
- **不変条件**:
  - §2 子プロセス節 — **サンドボックスは黙って弱くならない。** 得られなければ起動を失敗させ、
    主張は実プロセスを起動するテストで実証する（`crates/infrastructure/tests/sandbox.rs` の流儀）
  - §1 — `application` に tokio を入れない（プロセス管理は `infrastructure`）
  - §3 — サーバの異常終了・不正な応答は `is_error` の `tool_result`
  - §4、§5
  - §6 の趣旨 — MCP の SDK クレートを使うか自前で書くか（サプライチェーンの表面積）
- **設定・ドキュメント**: `README.md` §4（tools）・§5（サンドボックス）・§8、
  `management/docs/architecture.md`（新しいポート）、`.claude/rules/architecture.md` のポート表（root が更新）
- **利用者への影響**: 既定は無効（設定しなければ何も起動しない）。macOS では Seatbelt が未実装の
  ためサンドボックスが得られず、MCP は使えない

## 達成条件

- [x] 設定した stdio の MCP サーバが起動し、そのツールが `agent tools` に並び、モデルが呼んで
      結果を受け取れる（テスト用サーバで E2E）
- [x] サーバがワークスペース外に書き込めない・許可リスト外に接続できないことを、実プロセスで固定する
- [x] サンドボックスが得られない、または要求を満たせない環境では、サーバが起動せず理由が表示される
- [x] MCP ツールの呼び出しが承認ゲートを通る。サーバが申告する「読み取り専用」のヒントを無条件に
      信用しない（決めた扱いをテストで固定する）
- [x] サーバの異常終了・タイムアウト・不正な応答が `is_error` の `tool_result` になり、ループが続く
- [x] MCP ツールの名前が組み込みツールや他のサーバのツールと衝突しない
- [x] 設定ファイルの信頼モデル（ユーザー確認中）が実装され、README に書かれている
- [x] エージェントの終了時に、サーバのプロセスが孫まで含めて残らない
- [x] `make check` が緑

## 優先度の根拠

外部ツールをつなげる能力の拡張で、ゴール 1〜3 のどれかが壊れているわけではなく、`run_command` で
CLI を叩く回避策がある（重要度 `medium`）。設定ファイルの信頼モデルがユーザー確認中で、秘密の保護（A）・
中断（Ctrl-C）・依存ゲート（E）・Seatbelt が先に揃うほど作りやすくなり、待ってもコストは増えない
（緊急度 `low`）。score 5 → P2。

## メモ

- **設計確認中。設定ファイルの信頼モデルはユーザーに確認中**（リポジトリに同梱された設定で
  任意のコードが走る。信頼していないリポジトリで agent を起動しただけでサーバが立ち上がる）
- 規模が最大なので、分割を検討する: 長寿命プロセスのサンドボックス（ポート）→ MCP クライアント →
  設定と信頼モデル
- 調査で分かったこと（2026-11-08, issue-agent）:
  - ツールの説明文はサーバが決め、system プロンプトのツール一覧（`agent/prompt.rs`）に入る。
    プロンプトインジェクションの入口になり、プロンプトキャッシュの前置きも変わる
  - ツール出力は信頼できない入力。A の伏せ字の装飾子で包む
  - `npx` / `uvx` で起動するサーバは起動時にパッケージを取得する。egress の既定（deny-all）と衝突し、
    取得そのものがサプライチェーンの経路になる（E）
  - 子の環境変数の除去（`exec/env.rs`）は `*_TOKEN` などを落とす。サーバが必要とするトークンは
    明示的に渡す設計が要る（渡したトークンはサーバから読める）
  - ツール一覧を起動時に固定するか、`tools/list_changed` に追従するか
  - Landlock の「同じポートなら別ホストにも届く」穴（`netns` の issue）は MCP サーバにも同じく残る
- 関連:
  - `todo/2026-08-14_macos-seatbelt-command-sandbox.md` — macOS で使えるようになる前提
  - `todo/2026-08-14_netns-isolated-command-sandbox.md`
  - `progress/2026-11-08_secret-file-protection.md`（A）
  - `todo/2026-11-08_interrupt-turn-with-ctrl-c.md` — 実行中の `tools/call` の取り消し
  - `todo/2026-11-08_dependency-change-gate.md`（E）
  - `todo/2026-08-14_composition-root-test-harness.md` — 登録の配線は無テスト

### 設計確認の結論（2026-11-08 domain-specialist → root 決定）

- **最重要: サーバにワークスペースへの書き込みを既定で渡さない**。常駐サーバは呼び出しの合間にも動けるので、AccessPolicy も E の事後検知も迂回して Cargo.toml や .git/hooks を書ける。既定はサーバごとの作業ディレクトリのみ書き込み可、ワークスペース書き込みはサーバ設定で明示したときだけ（その場合 E の監視対象に入る）。読み取りは閉じられない（.env・セッションログを読める）ことを README に明記
- ポート: domain に中立名の **`ToolServer`**（`server_name` / `sandbox()`＝実際に効いているもの / `tools()`＝起動時固定 / `call`）。stdio + JSON-RPC は `infrastructure/src/mcp/`（内部型は外に出さない、応答は id で振り分け、取り消しは future の drop → `notifications/cancelled`）。Tool としてのアダプタと方針（名前空間・安全分類・無害化・衝突検出）は application（テスト可能な関数に）
- 名前は `mcp__<server>__<tool>`（サーバ名 `[a-z0-9-]{1,16}`、ツール名は文字種置換・64 文字、呼び出しは生の名前を保持）。**変換後の名前の衝突は起動時に失敗**（registry は黙って上書きするため）
- 安全分類は一律 `Destructive`（readOnlyHint 等の自己申告は無視）。`preview` は明示的に `Ok(vec![])`
- 説明文・スキーマは信頼できない入力: 制御文字除去、説明 1 KiB・スキーマサイズ・サーバごと 64 ツールの上限、説明に `[MCP server <name>]` の出所を付ける。出力は v1 で text のみ、`redact_all` の前に登録し `TimeoutTool` で包む
- サンドボックス: `linux::confine` / `env::scrub` / `EgressProxy` / setsid / `GroupGuard` を流用。**先に refactor-agent で「サンドボックス付き Command の組み立て」を共通関数に切り出す**（`capture::run_capped` は 1 回実行専用）。サーバごとに EgressProxy。得られなければエージェント全体の起動を失敗。macOS は起動失敗（Linux 限定）
- ライフサイクル: 起動時に initialize → tools/list を 1 回（タイムアウト付き）で固定、`list_changed` は無視。クラッシュ後は以後の call を Execution エラー（v1 は再起動しない）。stderr は読み捨て続ける。1 メッセージ 4 MiB 上限。Drop で stdin を閉じ killpg。ハンドルは Application に保持
- 秘密: サーバに渡す値は設定で明示し、Redactor の秘密に加える
- root 決定: `AGENT_MCP_CONFIG` がワークスペース内を指したら起動失敗。起動バイナリがワークスペース内なら起動失敗。initialize 失敗はエージェント全体の起動失敗。MCP のサンドボックス要求は `AGENT_SHELL_SANDBOX` と分け、v1 では none を許さない。名前は中立（ToolServer）。C の遠隔承認の対象に含める
- v1 で削る: HTTP/SSE、list_changed 追従、自動再起動、text 以外、resources/prompts/sampling、安全分類の上書き、macOS

- 2026-11-08 完了。設計どおり + 追加: ワークスペース内のファイルを指す絶対パス引数も拒否（`node /workspace/server.js` 対策、ヒューリスティック）、作業ディレクトリは常にサーバ専用ディレクトリ、子は drop まで reap しない（pid 再利用なしで killpg が孫まで届く）、4 MiB 超のメッセージはそのサーバを停止扱い。残余: 読み取りは閉じない（.env・セッションログ・HOME）、SIGKILL 時はサーバが残る、起動タイムアウト 30 秒固定、大文字小文字違いの名前衝突は未検出
