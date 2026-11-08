---
title: 実行中のターンを Ctrl-C で中断できるようにする（会話は残す）
created: 2026-11-08
urgency: medium
importance: high
priority: P1
scope: [domain, application, infrastructure, cli, docs]
---

# 実行中のターンを Ctrl-C で中断できるようにする（会話は残す）

## As-is（現状）

- `agent chat` でも `agent run` でも、**Ctrl-C はプロセスごと落とします。** chat の会話はメモリから
  消え、続けるには `agent chat --resume` で起動し直すしかありません。しかも再開時、ログの末尾にある
  未回答の `tool_call` は捨てられる（`drop_trailing_unanswered_calls`）ので、中断したターンの途中経過は
  失われます
- 応答が流れている途中で「方向が違う」と分かっても、止める手段がプロセスを殺すことしかありません。
  ストリーミングの issue（`done/2026-08-14_streaming-chat-responses.md`）の To-be には
  「Ctrl-C で打ち切れる」とありましたが、達成条件には入っておらず、未実装です
- **（推測・未検証）`run_command` の実行中に Ctrl-C すると、コマンドが裏で走り続けうる。**
  子プロセスは端末への書き込みを防ぐため `setsid` で別セッションに置かれており、端末の Ctrl-C を
  受け取りません。エージェント側はシグナルで即死するので `kill_on_drop` も働きません。
  `make chat`（コンテナ、`init: true`）ではコンテナの終了と一緒に消えますが、ホストで直接動かす
  配布形態では残ります

## To-be（あるべき姿）

- **chat**: Ctrl-C でそのターンだけが止まり、会話は残ってプロンプトに戻る。次の入力でそのまま
  続けられ、モデルも「直前のターンがユーザーに中断された」ことを知っている
- ターン実行中の 2 回目の Ctrl-C、またはプロンプト待ち中の Ctrl-C で chat が終了する
- **run**: Ctrl-C で終了コード 130 で終わる
- 実行中のコマンドは中断とともに止まる（孫プロセスを含む）
- 承認プロンプトの途中で中断しても、次に打った入力が化けたり消えたりしない

## 影響範囲

- **クレート**:
  - `cli` … `main.rs`（シグナルの受信。tokio の `signal` 機能はワークスペースで既に有効）、
    `commands/chat.rs`・`commands/run.rs`、`input.rs`、`approval.rs`、`notify.rs`
    （中断を `failed` として通知しない）
  - `application` … `agent/loop_runner.rs`（中断点）、`agent/dispatch.rs`（中断した呼び出しにも
    結果を合成する）
  - `domain` … 中断を表す語彙（`FinishReason` / `AgentEvent`、必要なら中断シグナルの型）
  - `infrastructure` … `exec/capture.rs`（中断時にプロセスグループを止める。今グループを kill するのは
    タイムアウトの経路だけで、future を捨てたときは `kill_on_drop` が直接の子を止めるだけ）、
    `tools/timeout.rs`
- **不変条件**:
  - §1 — `application` に tokio を入れずに中断を伝える
  - §3 — 中断は実行失敗ではない。`AgentLoop::run` が `Err` を返すのはプロバイダが壊れたときだけ、
    という契約を保つ（中断は結果の一種として返す）
  - §4 — 中断時点で結果のない `tool_call` すべてに `tool_result` を返す。会話とセッションログの両方で
  - §2 子プロセス節 — 中断で子を確実に止める
- **設定・ドキュメント**: `README.md` §3（使い方）。設定の追加は想定なし
- **利用者への影響**: Ctrl-C の意味が変わる（chat では 1 回目で終了しなくなる）。
  `run` の 130 はシェルの慣習どおり

## 達成条件

- [ ] chat でターン実行中に中断すると、会話が残ってプロンプトに戻る。続けて送った入力のリクエストに、
      直前のターンの経過（中断された旨を含む）が載る
- [ ] 中断時点で結果のない `tool_call` すべてに「ユーザーが中断した」旨の `tool_result` が付き、
      次のリクエストが拒否されない形であることをループのテストで固定する
- [ ] セッションログにも同じ結果が記録され、`--resume` で中断したターンの途中経過が戻る
- [ ] ターン実行中の 2 回目の中断、またはプロンプト待ち中の中断で chat が終了する
- [ ] `run` が中断で 130 を返す（実バイナリに SIGINT を送るテスト。`tests/notify_e2e.rs` と同じ方式）
- [ ] `run_command` 実行中の中断で孫プロセスまで止まることを、実プロセスで固定する
      （`sh -c 'sleep 1000 &'` の形）
- [ ] 承認プロンプト中に中断しても、次に入力した 1 行が失われない
- [ ] 中断したターンで何を通知するか（しないか）が決まっていて、テストで固定されている
- [ ] `make chat`（`docker compose run`）経由でも同じ挙動であることを手で確かめ、ここに記録する
- [ ] `make check` が緑

## 優先度の根拠

方向違いのターンを止める手段がプロセスを殺すことしかなく、中断したターンの経過も失う。ループ制御
（ゴール 1）と §4 に直結し、ホスト実行では子プロセスが残りうる（重要度 `high`）。中断はループ・
ディスパッチャ・承認ゲート・ツールのすべてに「途中で止まる」経路を要求するため、TODO ツール・
依存ゲート・双方向通知・MCP がそこに手を入れるほど後付けの箇所が増える（緊急度 `medium`）。
score 8 → P1。

## メモ

- **設計確認中。**
- 調査で分かったこと（2026-11-08, issue-agent）:
  - **入力待ちが取り消せない。** `input.rs` の `prompt_line` は `spawn_blocking` で stdin を読む。
    future を捨てても読み取りスレッドは止まらず、**次に打った 1 行を飲み込む**。承認プロンプト中の
    中断で効いてくる。双方向通知（C）の「端末と遠隔の早い方を採る」も同じ問題を踏むので、
    ここで取り消せる入力待ちを作れば共有できる
  - ループはアシスタントの `tool_call` を会話に積んでセッションログへ書いた**後で**ディスパッチする。
    その間で中断すると、メモリ上の会話は結果の欠けた `tool_call` で終わる
  - 中断の伝え方は `application` が tokio に依存しない形で（`futures` の `select` / `Abortable`、
    あるいは domain に中断シグナルのポートを置く）
  - 「2 回目で終了」は時間窓ではなく状態（ターン実行中か否か）で判定する方が決定的にテストできる
  - ストリーミング中の中断で、受け取った途中までの本文を履歴に残すか捨てるかを決める
- 関連:
  - `done/2026-08-14_streaming-chat-responses.md` — To-be に Ctrl-C の打ち切りがあった
  - `todo/2026-11-08_approve-from-phone-via-ntfy.md`（C）— 取り消せる入力待ちを前提にする
  - `todo/2026-11-08_sandboxed-mcp-client.md`（F）— 実行中の `tools/call` の取り消し
  - `todo/2026-08-14_sub-agent-support.md` — 子ループの中断
  - `todo/2026-08-14_composition-root-test-harness.md` — `chat::start` / `run` の配線は無テスト

### 設計確認の結論（2026-11-08 domain-specialist → root 決定）

- **協調的中断**。`application/src/agent/interrupt.rs` に std のみの具象 `Interrupt`（`fire()` / `is_fired()` / 待てる `fired()`、Clone で共有、一度発火したら戻らない、複数 waker 対応）。ポートにしない。ターンごとの引数で渡す（`run_until(session, input, &Interrupt)`、既存 `run` は発火しない Interrupt で委譲）
- 中断の確認点はイテレーション先頭・圧縮呼び出し・モデル要求/ストリーム・ディスパッチャ内部（承認待ち・並列・直列）のみ
- **ディスパッチャが中断時も入力と同じ長さの結果を要求順で返す**（完了済みは実結果、未開始は「実行されていない」、実行中は「一部反映されている可能性がある。状態を確かめてから」）。`is_error: true`、文面は dispatch.rs。実行中の call には `ToolCallFinished{is_error:true}` を発行
- 順序は push → record → 中断なら return。会話から呼び出しを**削除して修復しない**（追記専用ログと食い違い再開で孤児になる）
- `FinishReason::Interrupted` を追加し `Ok(AgentOutcome)` で返す（Err にしない）。`notify.rs` の `kind()` のワイルドカードに任せず明示分岐。中断は通知しない（root 決定）
- ストリーミング途中の断片は履歴に残さない。何も注入しない
- `TerminalRenderer` は `RunFinished` で streaming フラグを下ろす（既存の取りこぼしも直る）
- **run_command の孫プロセス**: `infrastructure/src/exec/capture.rs` に drop guard（drop 時に kill_group、`child.wait()` 後に解除、reap 前に kill）。実プロセスのテストで孫が ESRCH になることを固定
- cli: stdin は**ランタイム外の専用リーダースレッド + チャネル**にする（`spawn_blocking` 毎回方式は承認中断で次の行を飲み込む）。`ctrl_c()` は chat / run の中だけで登録。状態機械: プロンプト待ち中 → 終了(0) / ターン中 → fire / 後始末中に再度 → future 放棄して終了(130)。`agent run` の中断は 130、drain は待つ
- やってはいけない: run の future を drop して中断する（終了経路以外）、tool_call の削除、Err で返す、application に tokio、`[interrupted]` の書き足し、`std::process::exit`
