---
title: 刈り込み後の履歴が assistant で始まりうる（プロバイダに拒否される可能性）
created: 2026-08-14
urgency: medium
importance: medium
priority: P2
scope: [domain, infrastructure]
---

# 刈り込み後の履歴が assistant で始まりうる（プロバイダに拒否される可能性）

## As-is（現状）

長いセッションで `AGENT_COMPACT=false` にしている、あるいは要約が失敗した場合、
`Conversation::trim_to_budget` が履歴の先頭を **assistant メッセージ**にすることがあります。

刈り込みは先頭に残った `tool` メッセージだけを避けており（対応する `tool_call` が消えて
孤児になるため）、assistant については何もしていません。エージェントループの履歴は
`[user, assistant(call), tool, assistant(call), tool, ...]` の形になりやすく、
`keep_recent` ちょうどで切ると assistant に着地します。

**Anthropic の Messages API は最初のメッセージが `user` であることを要求します**
（"The first message must use the user role"）。この状態でリクエストを送ると
400 で拒否され、ユーザーから見ると「長い会話の途中で突然エラーになる」形になります。

**未検証です。** 実エンドポイントに投げて確認したわけではなく、API ドキュメントの記述と
既存コードの読みから導いた推測です。着手時にまず再現を確認してください。

## To-be（あるべき姿）

刈り込みだけが働いた場合でも、モデルに送る履歴は必ず `user` メッセージから始まり、
長いセッションが途中で拒否されない。

## 影響範囲

- **クレート**: `domain`（`Conversation::trim_to_budget` の後処理）。プロバイダ側で
  補正する案を採る場合は `infrastructure/src/llm/anthropic.rs`
- **不変条件**: `invariants.md` §4 に隣接。孤児 `tool_result` を作らない規律と同じ場所
- **設定・ドキュメント**: 挙動が変わるなら `management/docs/architecture.md`
- **利用者への影響**: 拒否されていたケースが通るようになる。破壊的変更なし

## 達成条件

- [ ] Anthropic が実際に assistant 先頭を拒否するかを確認し、結果を issue に記録する
      （拒否しないなら本 issue は「対応不要」で閉じてよい）
- [ ] 拒否する場合、刈り込み後の履歴が必ず `user` で始まることをテストで固定する
- [ ] エージェントループの典型的な履歴（user 1 通のあと assistant/tool が延々続く）で、
      修正が刈り込みを無効化しないことを確認する
- [ ] `make check` が緑

## 優先度の根拠

長いセッションでのみ、かつ Anthropic でのみ起きうる不具合で、圧縮が既定で有効な今は
主経路では顕在化しません（重要度 medium）。ただし発生したときの症状は「突然エラー」で
原因が分かりにくく、放置するほど踏んだ人が増えます（緊急度 medium）。

## メモ

- 素朴な修正（先頭が user になるまで捨てる）には落とし穴があります。エージェントループの
  履歴には user メッセージがターンの先頭に 1 通しかないため、そこまで捨てると
  **履歴が空になり得ます**。逆に「直前の user まで戻す」と刈り込みが実質無効になります。
- 実現可能な案の 1 つは、圧縮と同じ発想で**合成した user メッセージを先頭に置く**こと
  （「ここより前の履歴は文脈予算のため省略されました」）。圧縮が有効なときの要約と
  同じ位置・同じロールになるので、経路が 1 本にまとまります。
- 関連: `management/issues/done/2026-08-14_context-compaction-by-summarization.md`
