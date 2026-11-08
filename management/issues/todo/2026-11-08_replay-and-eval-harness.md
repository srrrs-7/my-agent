---
title: 記録の再生（replay）と実 LLM 評価（eval）で、プロンプトやループの変更の良し悪しを数字で比べる
created: 2026-11-08
urgency: medium
importance: high
priority: P1
scope: [application, infrastructure, cli, docs, ops]
---

# 記録の再生（replay）と実 LLM 評価（eval）で、プロンプトやループの変更の良し悪しを数字で比べる

## As-is（現状）

プロンプト・ループ・圧縮の設定を変えたとき、良くなったかを確かめる方法が
「`make ask` を何回か試して眺める」しかありません。結果は再現せず、数字も残りません。

単体テストは振る舞いの契約を守っているかは教えてくれますが、実モデルでタスクがどれだけ成功するか、
何回反復したか、トークンと時間をどれだけ使ったかは分かりません。
過去のセッションで圧縮や刈り込みがどう効いたかも、ログを手で読むしかありません。

## To-be（あるべき姿）

- **`agent replay <session>`** — 保存されたセッションを、LLM を呼ばずに決定的に再生し、
  圧縮・刈り込みがどこで何回起き、履歴の大きさがどう推移したかを出す。予算などの設定を変えて
  同じ記録を流し直し、効き方を比べられる
- **`agent eval <tasks>`** — タスク定義の集合を実 LLM で実行し、タスクごとと全体の成功率・反復回数・
  トークン・レイテンシを JSON で出す。変更の前後で同じタスク集合を流し、差を数字で比べられる
- 進捗は stderr、結果の JSON は stdout（`> result.json` で保存できる）

## 影響範囲

- **クレート**:
  - `cli` … `args.rs` にサブコマンド 2 つ、`commands/replay.rs`・`commands/eval.rs`、
    `composition.rs`（評価用の組み立て）
  - `application` … replay で `fit_history` 相当を LLM なしで動かす入口。圧縮の要約は LLM を呼ぶので、
    再生では固定の要約を返すフェイクが要る。計測は `EventSink` を集計する実装で足りるかを確認する
  - `infrastructure` … `session/store.rs`（既存の `load` で足りるか）、タスク定義の読み込み、
    タスクごとの使い捨て作業ディレクトリ
  - `domain` … 原則なし
- **不変条件**:
  - §5 — eval 自体は実 LLM を使うが、**eval / replay のテストはモックサーバとフェイクだけで書く**
  - §1 — 時間の計測は `cli` / `infrastructure` 側で行う
  - §2 — eval はタスクごとに `WorkspaceRoot` を作り直す。実ワークスペースを触らないこと
- **設定・ドキュメント**: `README.md` §3（使い方）・§7（評価の回し方）、`Makefile`（`make eval` /
  `make replay` を足すなら）、`management/docs/architecture.md` §5
- **利用者への影響**: サブコマンドの追加のみ

## 達成条件

- [ ] `agent replay <ID>` が LLM にもネットワークにも出ずに完走し、圧縮・刈り込みの発生回数と、
      各時点の履歴の大きさを出す
- [ ] 同じ記録・同じ設定で replay を 2 回流すと、出力が一致する
- [ ] 予算などの設定を変えて replay すると、その差が出力に現れる
- [ ] `agent eval <tasks>` がタスクごとの成功／失敗・反復回数・入出力トークン・レイテンシと、
      全体の成功率を JSON（stdout）で出す
- [ ] タスクの成否判定の方法をタスク定義に書ける（方式は設計で決める）
- [ ] eval はタスクごとに使い捨ての作業ディレクトリで動き、実ワークスペースを変更しない
- [ ] JSON の形式と、2 回分の結果を比べる手順が README に書かれている
- [ ] eval / replay のテストがモックサーバとフェイクだけで通る
- [ ] `make check` が緑

## 優先度の根拠

ループエンジニアリング（ゴール 1）の変更を評価する手段がなく、回避策の手動試行は再現も数値化も
できない（重要度 `high`）。TODO ツール・プロンプトキャッシュ・ルーティング・圧縮の調整と、
ループに効く変更が続く。基準値を取る前に入った変更ほど、後から比べられなくなる（緊急度 `medium`）。
score 8 → P1。

## メモ

- **設計確認中。** タスク定義の形式、成否判定の方式、試行回数の扱い
- 調査で分かったこと（2026-11-08, issue-agent）:
  - **セッションログには、トークン数・レイテンシ・要約の結果が残っていない。** 各行はメッセージと
    時刻だけ（`session/log.rs` の `Record`）。ログは圧縮の**前**に書かれるので、要約メッセージそのものも
    残らない。replay で出せるのはバイト数ベースの推移までで、トークンは eval（実行時の計測）側で取る
  - タスク定義を TOML / YAML にすると新しい依存（サプライチェーンの表面積）が増える。
    `serde_json` は既にある
  - eval は非対話で回すので、承認をどうするか（使い捨てディレクトリで `auto` 相当にするか）を決める
  - 成否判定に「`cargo test` が通るか」のようなコマンドを使うなら、`run_command` と同じサンドボックスの中で
  - 実 LLM の結果はばらつく。同じタスクを複数回流す指定があると比較の信頼度が上がる
- 関連:
  - `todo/2026-08-14_verify-against-a-real-llm.md`（P0）— その確認項目（`list_directory` /
    `read_file` / 承認拒否の認識）はそのまま eval のタスクにできる。verify で動くモデルを確定させ、
    eval で継続的に測る順が素直
  - `todo/2026-11-08_anthropic-prompt-caching.md` — `TokenUsage` の形を共有する
  - `todo/2026-08-14_cost-latency-router-and-failover.md`（B）— プロバイダ別の成績比較に使える
  - `todo/2026-08-14_session-log-single-file-growth.md` — replay も記録を丸ごと読む

### 設計確認の結論（2026-11-08 domain-specialist → root 決定）

- **eval を先にやる。replay（現行ループへの記録流し込み）は作らない**。replay をやるなら `fit_history` を部品に切り出して記録を流すシミュレーションとして後回し（価値は中〜低）
- 前提: **composition の分割**（「設定 + ルート → 配線済みエージェント」を返す関数を run / chat / eval が共有）。P1 issue `composition-root-test-harness` を D の中で先に行う
- 置き場所: `application/src/eval/`（タスク型・検査 `verify`・`MetricsSink: EventSink`・集計）、`infrastructure/src/fs/scratch.rs`（フィクスチャを一時ディレクトリへ複製、std のみ、接頭辞付き、`--keep`、シンボリックリンク拒否）、cli `commands/eval.rs`
- タスク定義は JSON 1 文書（`version`, `tasks[{id, fixture, prompt_file, max_iterations, checks[file_contains|command]}]`）、パスは `WorkspacePath`、`deny_unknown_fields`、検査 0 件は拒否、id は newtype で一意
- 検査コマンドは `SandboxedCommandRunner` を通す（タスクごとにスクラッチをルートに作成）。要求サンドボックスが得られなければ LLM を呼ぶ前に失敗。`confines()` が偽なら eval では run_command を登録しない
- **既存バグ修正**: 圧縮の要約リクエストの usage が捨てられている（`compaction.rs:213-219`, `loop_runner.rs:174`）→ `CompactionReport` に usage を持たせ session.usage に積む
- stdout は JSON 報告専用、進捗は stderr。会話ログは `<out>/sessions/`。報告に provider/model/設定/sandbox/システムプロンプト指紋（FNV-1a、DefaultHasher は使わない）。`--repeat N` を v1 に入れる
