---
title: 権限ルールの照合が同一ターン内の先行コマンドの結果を見ない（TOCTOU）
created: 2026-11-08
urgency: low
importance: medium
priority: P2
started: 2026-11-08
completed: 2026-11-08
scope: [application, cli]
---

# 権限ルールの照合が同一ターン内の先行コマンドの結果を見ない（TOCTOU）

## As-is（現状）

`RuleGate` はディスパッチャのフェーズ 1（全呼び出しの承認を先にまとめて取る段階）で `preview` し、
`PlannedWrite.target`（シンボリックリンク解決後の実体）でパスのルールを照合する。同じターンで
先に `run_command ln -s ../.github/workflows/ci.yml src/x` を実行し、続く `write_file src/x` を
`allow` されたパス（`src/`）として通す、という順序はフェーズ 1 の時点では見えない
（その時点ではリンクがまだ無い）。

`deny` のルールで守っているつもりのファイルが、同じターン内の 2 手で書き換えられうる。
依存ファイルのゲートはフェーズ 2（invoke の直前）で preview しているので同じ穴は無い。

## To-be（あるべき姿）

パスのルール（とりわけ `deny`）は、実際に書き込む直前の実体に対して評価される。
フェーズ 1 で通った呼び出しでも、invoke 直前に実体が `deny` / `ask` に変わっていれば実行しない
（`ask` なら改めて聞く）。

## 影響範囲

- **クレート**: `application`（`dispatch.rs` のフェーズ 2 に、依存ゲートと同じ位置でルールの再照合を足す、
  または `RuleGate` の照合を Tool 装飾子として invoke 直前に行う）、`cli`（合成の積み順）
- **不変条件**: §3・§4（拒否は tool_result で返し、全 tool_call に応答）。ディスパッチャの不変条件に触れる
  ので、設計は root が判断する
- **設定・ドキュメント**: README §3 の権限ルールの節に「照合のタイミング」を追記
- **利用者への影響**: なし（守りが強くなるだけ）

## 達成条件

- [x] 同じターンで先にシンボリックリンクを作り、続く書き込みで `deny` のファイルに届く順序が拒否される
      （実ファイルシステムの E2E で固定）
- [x] フェーズ 1 で `allow` だった呼び出しが、invoke 直前に `ask` 対象になっていれば改めて確認する
- [x] 依存ゲートとプランモードの既存の振る舞いに退行がない
- [x] `make check` が緑

## 優先度の根拠

`deny` のルールを迂回できるので守りの穴ではあるが、モデルが意図的に 2 手を組む必要があり、
`run_command` は既定で無効・有効時も承認を経る。ルールは「誤操作の柵」と位置づけている（重要度 medium）。
いつ直してもコストは変わらない（緊急度 low）。score 5 → P2。

## メモ

- 2026-11-08 §2 権限ルールの実装担当が発見・報告（root が起票）
- 依存ゲートの `confirm_dependencies`（フェーズ 2）と同じ位置で `preview` 結果を共有すれば、
  preview の二重実行を避けられる
- 2026-11-08 実装: `ApprovalGate::reauthorize_write(&request, &[PlannedWrite])` を既定実装なしで追加（案 A。転送漏れはコンパイルエラー。人間ゲートとフェイクは Approve を明示実装）。
  ディスパッチャのフェーズ 2 で 1 回の preview を再照合と依存ゲートで共有（再照合が先）。
  `PlanModeGate` / `HookGate` は素通し（フックは再実行しない）、`RuleGate` だけが判定する。
  1 回目の判定は保持せず、「名前のパスだけで評価した verdict」を 1 回目の下限として比較する
  （名前のパスはフェーズ間で変わらないため）。既知の副作用: 既にリンクがあり実体が `ask` の
  書き込みは確認が 2 回出る（2 回目は届き先を表示）
- E2E: `crates/cli/tests/composition.rs` の `a_link_made_earlier_in_the_turn_cannot_carry_a_write_past_*`
  （実 FS + `run_command`、sandbox none）
