---
title: Fable モデルを使ったリファクタリングの運用を確立する
created: 2026-08-14
urgency: low
importance: medium
priority: P2
scope: [ops]
---

# Fable モデルを使ったリファクタリングの運用を確立する

## As-is（現状）

開発用の `refactor-agent`（`.claude/agents/refactor-agent.md`）は `model: inherit` で、
root セッションと同じモデルで動く。コーディング特化の Claude Fable 5 は未活用で、
まとまったリファクタリングパスを回すコスト・速度の最適化がされていない。

## To-be（あるべき姿）

リファクタリング作業を Fable で回す運用が確立している。開発者（root セッション）は
構造改善のタスクを Fable 駆動の `refactor-agent` に委譲し、結果を通常のレビュー経路
（`/review-panel`）で検証して取り込める。効果が確認できれば常用し、
効果がなければその記録を残して `inherit` に戻す。

## 影響範囲

- **クレート**: なし（直接は）。試験パスの結果として `crates/` に通常のリファクタリング
  変更が入るが、それは既存の規約（振る舞いを変えない・`make check` 緑）に従う
- **不変条件**: なし（開発プロセスの変更。成果物は通常のレビューと `make check` を通る）
- **設定・ドキュメント**: `.claude/agents/refactor-agent.md`（`model: fable`）、
  必要なら `.claude/rules/orchestration.md` にモデル選定の指針を追記
- **利用者への影響**: なし（リポジトリの利用者には見えない）

## 達成条件

- [ ] `refactor-agent` を `model: fable` で起動する設定変更が入る
- [ ] 範囲を絞った試験パスを 1 回実施する（候補: `crates/infrastructure` の
      重複・命名・分割の見直し）。振る舞いを変えないこと・既存テストが同一集合で
      通ることを確認する
- [ ] 結果を `/review-panel` に通し、採用・見送りを判断する
- [ ] 所感（指摘の質・速度・体感コスト）をこの issue のメモに記録し、
      常用するか `inherit` に戻すかを決める
- [ ] `make check` が緑（試験パス適用後）

## 優先度の根拠

開発スループットの改善であり、リファクタリングが安く速く回るほどゴール 2（境界の維持）を
支える（重要度 medium）。ただし実験的な性格で、いつやってもコストは変わらない
（緊急度 low）。

## メモ

- `model: fable` はサブエージェント定義の frontmatter で指定できる
  （`sonnet` / `opus` / `haiku` / `fable` / フルモデル ID / `inherit`）
- 評価は同一タスクを inherit で流した場合との比較ができれば理想だが、
  コストに見合わなければ試験パス 1 回の定性評価でよい
