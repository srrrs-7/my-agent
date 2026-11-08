---
title: composition root をテストから組み立てられるようにする
created: 2026-08-14
urgency: high
importance: high
priority: P1
started: 2026-11-08
scope: [cli, docs]
---

# composition root をテストから組み立てられるようにする

## As-is（現状）

**配線を決めている層だけが、テストされていません。**

`crates/cli/tests/end_to_end.rs` は「全層が参加する」テストという触れ込みですが、
`agent_cli` を**一度も import していません**。`AgentDependencies` も `ToolRegistry` も
テストが自分で組み立てています。つまりそこで検証されるのは**テストの配線**であって、
プログラムの配線ではありません。

差は具体的です。

| | 本物（`composition.rs`） | E2E テスト |
|---|---|---|
| ファイルツール | 5 つ（read / write / edit / list / search） | 2 つ（read / write） |
| タイムアウト | 全ツールを `TimeoutTool` で包む | 包まない |
| `web_fetch` / `run_command` | 設定で条件付き登録 | なし |
| 会話ログ | 設定で `FileConversationLog` か `NullConversationLog` | 常にファイル |

`composition.rs` の間違い — ツールの登録漏れ、ログの結線忘れ、承認ゲートの取り違え —
は、テストスイート全体から見えません。

**在庫になっている穴が既に 3 つあります。**どれも完了 issue に「テストで踏んでいない」と
記録してあります。

- `AGENT_SESSION_LOG=false` のとき `NullConversationLog` が選ばれること
- `chat::start` の再開フォールバック（記録がない・壊れている・空）
- `composition::apply_retention` が起動時に走ること

**security 上の分岐もここにあります。** `build_command_runner` は「要求されたサンドボックスが
得られなければ起動を失敗させる」— `invariants.md` §2 の「黙って弱くならない」契約そのもの
ですが、この判断は `composition.rs` にあり、テストがありません。

### 何が塞いでいるか

2 段構えです。

**① `crates/cli` は bin only。** `Cargo.toml` に `[[bin]]` しかなく lib target がないので、
`tests/` から `composition` / `args` / `commands` に触れません。

**② 設定がプロセス環境からしか入りません。** `composition::build` は
`resolve_settings` → `Settings::from_env()` を呼びます。テストで環境変数を書き換えるのは
`invariants.md` §5 が禁じています（グローバルな可変状態で、並列テストが干渉する）。

②の方が本質です。`apply_cli_overrides` には
「Split out so the precedence is testable without touching the process environment」という
コメントが既にあり、**同じ壁に当たって半分だけ解決した跡**が残っています。

## To-be（あるべき姿）

`composition.rs` を変更したとき、それが壊れたかどうかがテストで分かる。

「このツールが登録されている」「この設定でログが書かれない／書かれる」
「サンドボックスが得られなければ起動しない」を、実際に組み立てた `Application` に対して
確かめられる。

E2E テストが名前どおりのものになる — 手で組み直した配線ではなく、
`agent` が実際に使う配線を通る。

## 影響範囲

- **クレート**: `cli`（`composition.rs` の入口の形、必要なら lib target の追加）。
  他クレートの変更は想定なし
- **不変条件**: **なし。**ただし **§5「テストはネットワークも実モデルも要求しない」が
  この issue の設計を決めます** — `std::env::set_var` は使わず、既存の
  `Settings::from_source(&MapEnv::new(...))` に合わせて注入します。
  §2 の「サンドボックスは黙って弱くならない」を composition 側でも実証できるようにするのが、
  この issue の狙いの 1 つです
- **設定・ドキュメント**: `management/docs/architecture.md`（lib target を足すなら層の図）、
  `README.md` §7（テストの説明。現在「E2E … HTTP → provider → routing → retry → loop →
  tools → 実ファイルシステム」と書いてあり、composition を通らないことは書かれていない）
- **利用者への影響**: なし（テストの足場。`agent` の挙動は変えない）

## 達成条件

- [x] 解決済みの `Settings` を渡して `Application` を組み立てられる。
      プロセス環境を触らずに設定を変えたテストが書ける
- [x] `agent-test-support` のモックサーバ相手に、**本物の composition で組んだ**
      `Application` で 1 ターン走らせられる
- [ ] 在庫の 3 つが塞がる
  - [x] `AGENT_SESSION_LOG=false` で 1 バイトも書かれない／`true` で書かれる
  - [ ] `chat::start` が、記録なし・壊れた記録・空の記録のそれぞれで新規セッションを返す
  - [ ] 起動時に保持ポリシーが適用され、`--resume` が名指ししたセッションは残る
- [x] 登録されるツールの一覧が、設定（`AGENT_SHELL` / `AGENT_WEB_FETCH`）に応じて
      期待どおりであることを固定する
- [x] 要求したサンドボックスが得られないときに起動が失敗することを固定する。
      **この環境で実証できない場合は、なぜできないかを issue に記録して次善の形にする**
- [ ] `std::env::set_var` を使っていない（`grep` で確認できる形）
- [ ] `tests/end_to_end.rs` が手で組んだ配線を捨てて composition を使う。
      **採らない判断をした場合は理由を残す**
- [x] README §7 のテストの説明が実態と合っている
- [x] `make check` が緑

## 優先度の根拠

配線はツール一式とサンドボックス要求を決める層で、**唯一まったくテストのない層**です。
そこを覆っているように見える E2E スイートが実は自前の配線を組み直しているため、
「テストがある」という誤解つきで放置されています（重要度 `high`）。

機能を足すたびに未検証の分岐が 1 つずつ増えており、既に 3 つ在庫があります
（緊急度 `medium`）。score 8 → P1。

## メモ

- **案 A（最小）: 入口を 2 つに割る。**
  `build(cli, interactive)` は今までどおり環境を読み、
  `build_with(settings, cli, interactive)` が解決済みの `Settings` を受け取る。
  テストは `Settings::from_source(&MapEnv::new(...))` で組み立てて後者を呼ぶ。
  **lib target は要りません** — `composition.rs` 内の `#[cfg(test)] mod tests` から書けます
  （`crates/cli/src/` には既に 7 つの `mod tests` があり、バイナリ内の単体テストは動いています）
- **案 B（本命）: `crates/cli` に lib target を足す。**
  `main.rs` は薄いシムにして、`tests/end_to_end.rs` から `agent_cli::composition::build_with`
  を使う。E2E が本物の配線を通るようになるのはこちら。案 A を先に入れてから案 B、が素直
- モックサーバの URL は `AGENT_BASE_URL` として `MapEnv` に入れれば済みます。
  E2E は既に `MockLlmServer` を使っているので、道具は揃っています
- `build` は `std::fs::canonicalize(&settings.workspace)` を呼ぶので、テストは
  `tempfile::tempdir()` をワークスペースにする必要があります（E2E が既にそうしています）
- サンドボックスの失敗経路は、dev コンテナでは Landlock が使えるため素直には再現できません。
  `AGENT_SHELL_SANDBOX` に未実装の要求（`SandboxKind` の一覧を確認）を渡す形が候補
- 関連: `done/2026-08-14_conversation-log-append-only.md`、
  `done/2026-08-14_session-persistence-and-resume.md`、
  `done/2026-08-14_session-log-rotation.md`（いずれも「やらなかったこと」でこの穴を挙げている）
- 関連（2026-11-08 追記）: 次の issue がいずれも `composition.rs` に配線を足す。特に A は
  伏せ字の装飾子の付け忘れがそのままセキュリティ上の穴になるため、この足場の有無が効く
  - `progress/2026-11-08_secret-file-protection.md`（装飾子の結線）
  - `todo/2026-11-08_interrupt-turn-with-ctrl-c.md`（`chat::start` / `run`）
  - `todo/2026-11-08_replay-and-eval-harness.md`（新しいサブコマンド）
  - `todo/2026-11-08_dependency-change-gate.md`、`todo/2026-11-08_todo-plan-tool.md`、
    `todo/2026-11-08_approve-from-phone-via-ntfy.md`、`todo/2026-11-08_sandboxed-mcp-client.md`

- 2026-11-08: 緊急度を medium → high に変更（score 9）。新規 7 機能が composition.rs に配線を足すため、かつ D（eval）の前提として D の中で先に実施する（root 判断）

- 2026-11-08（refactor-agent、D の前提として実施）: **案 B（lib target）を採用**
  - `crates/cli` を lib + 薄い bin に分割（`src/lib.rs` + `src/main.rs`。`main.rs` は
    `.env`・ログ・ランタイム・終了コードだけを持つ）。案 A（bin 内の `mod tests`）では
    `tests/` から本物の配線でモックサーバ相手の 1 ターンを回せず、eval（D）が呼ぶ入口とも
    共有にならないため
  - 入口を 2 段に分割した。振る舞いは変えていない（既存 663 本は同じ集合のまま緑）
    - `composition::build(cli, interactive)` … 環境 + CLI 引数から `Settings` を作り、
      `TerminalRenderer`・`--model`・プロセス環境（伏せ字の材料）・`--resume` を
      `Wiring` に詰めて次を呼ぶだけ
    - `composition::assemble(settings, Wiring) -> Result<Application>` … 配線の本体。
      run / chat / sessions / tools / doctor はすべてここを通る。eval もタスクごとにこれを呼ぶ想定
    - `Wiring { interactive, events, model, environment, retention: Option<Resume> }`。
      ワークスペースと会話ログの置き場は**別引数にしていない** — `settings.workspace` /
      `settings.session.dir` がそれで、2 か所に持つと食い違うため。
      `retention: None` で起動時の刈り込みを行わない（テスト・eval 用）
    - 承認ゲートは引数にしていない。eval は `interactive: false` の `CliApprovalGate`
      （設定どおりに自動承認／拒否）で足りるため。必要になった時点で足す
  - 通知の分岐は `webhook(&NotifySettings) -> Result<Option<WebhookNotifier>>` に切り出し、
    「URL なし → 送り手なし（`NullNotifier`）」を単体テストで固定
  - `crates/cli/tests/composition.rs`（13 本）: 既定のツール集合（7 つ）、`web_fetch` /
    `run_command` の条件付き登録、`AGENT_SHELL_SANDBOX=isolated`（どのプラットフォームでも
    未実装）で起動失敗、既定サンドボックスは「起動するなら confines()」か「起動失敗」の
    どちらか、伏せ字（API キーが `[REDACTED]` になる）、タイムアウト（応答しない
    サーバへの `web_fetch` が `AGENT_TOOL_TIMEOUT_SECS=0` で切られる）、依存監視は
    destructive（`run_command`）だけ、`AGENT_ALLOW_DEPENDENCY_CHANGES` の有無で
    無人 auto 実行のマニフェスト編集が拒否／通過、`AGENT_SESSION_LOG` の on/off
  - **残り（この issue を done にする前に要判断）**
    - `chat::start` の再開フォールバック 3 種と、起動時の保持ポリシー適用（`--resume` の除外）は
      未着手。`assemble` の `retention` 引数と `agent_cli::commands` が公開されたので書ける状態
    - `tests/end_to_end.rs` の composition への移行は未着手。14 本が `AgentLoopConfig` や
      プロンプトビルダを直接差し替えて分岐を突いており、`Settings` 経由に直すと
      テストの意図が変わるものがある。移行するか、`composition.rs` 側を本物の配線の
      担当として end_to_end は「ループ + アダプタの結合」と位置付け直すかを判断する
    - `std::env::set_var` は `crates/infrastructure/tests/sandbox.rs:447` に既存の使用がある
      （この issue の追加分では使っていない）

