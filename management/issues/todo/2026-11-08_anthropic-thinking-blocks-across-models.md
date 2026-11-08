---
title: 拡張思考の有効時に、フェイルオーバーやモデル切り替えをまたぐと Anthropic に 400 で拒否されないか確かめる
created: 2026-11-08
urgency: medium
importance: medium
priority: P2
scope: [infrastructure, domain, docs]
---

# 拡張思考の有効時に、フェイルオーバーやモデル切り替えをまたぐと Anthropic に 400 で拒否されないか確かめる

## As-is（現状）

`AGENT_THINKING=adaptive` のとき、応答の `thinking` / `redacted_thinking` ブロックは
そのまま履歴に残し、同じ会話の後続リクエストへ送り返しています（署名検証のため）。
次の 3 つの状況で、**後続のリクエストが拒否されて会話が止まる疑いがあります**（いずれも未再現）。

1. **別のモデル・別のアカウントへ思考ブロックを送り返す。** 履歴上の思考ブロックは
   どの Anthropic プロバイダ・モデルが書いたものも同じ印（dialect `"anthropic"`）で、
   区別されません。フェイルオーバー先・`--model` の切り替え先・ルーティング先が別モデルや
   別アカウントだと、署名を検証できずに拒否されうる
2. **ツールループの途中で OpenAI 互換のフォールバックを挟む。** その間の assistant の `tool_use`
   ターンには思考ブロックが無い。Anthropic に戻ったとき、思考有効下で「最後の assistant ターンが
   思考ブロックで始まっていない」として 400 になりうる。**400 はフェイルオーバーの対象外**なので、
   そのまま実行失敗になる
3. **思考ブロックだけの assistant ターン**（思考中に `max_tokens` に達した）をそのまま送り返し、
   拒否されうる。こちらはフェイルオーバーと無関係に、`adaptive` の利用者なら誰でも当たりうる

## To-be（あるべき姿）

- 思考を有効にしたままフェイルオーバーやモデルの切り替えが起きても、会話が 400 で止まらない
- 思考中に出力上限に達しても、次のターンで会話を続けられる
- どれも起きないと確認できたものは、確認済みとして記録されている

## 影響範囲

- **クレート**: `infrastructure/src/llm/anthropic/`（`blocks.rs` の `DIALECT` と `from_blocks`、
  `mod.rs` の送出時の Opaque の扱い）、`infrastructure/src/llm/failover.rs`、
  必要なら `application/src/agent/loop_runner.rs`（思考だけのターンを履歴に積むか）。
  `domain/src/model/message.rs` の `ContentBlock::Opaque { dialect, data }` は文字列なので
  型の変更は不要の見込み（dialect にモデル・プロバイダを含めれば足りる）
- **不変条件**: §4「すべての `tool_call` に結果を返す」に隣接（履歴の形がプロバイダに拒否される、
  という同じ種類の失敗）。思考ブロックを落とす・書き換える修正でも `tool_use` / `tool_result` の
  対は崩さないこと。§5（テストは実モデルを要求しない）— 再現は実 API で行うが、固定するテストは
  モックサーバで書く
- **設定・ドキュメント**: `README.md` §8 の `AGENT_THINKING` の説明（「同じ会話の後続リクエストに
  そのまま送り返す」）、フェイルオーバーの節
- **利用者への影響**: なし（失敗していたものが通るようになる）

## 達成条件

- [ ] **まず再現する**: 実 API で 3 つの状況それぞれを試し、拒否されるか・どのエラー文かを
      メモに記録する（API キーを要するので root がユーザーに確認してから）
- [ ] 再現したものについて、そのリクエストが組み立てられないこと（送る前に思考ブロックを
      落とす・dialect が一致しないものを送らない等）を `crates/infrastructure/tests/anthropic_http.rs`
      のモックサーバで、実際に送出される JSON に対して固定する
- [ ] 思考ブロックだけのターンの後で、次のユーザー入力から会話を続けられることを固定する
- [ ] 再現しなかったものは、その旨と試した条件（モデル、API バージョン）をメモに残す
- [ ] README §8 の `AGENT_THINKING` の説明が実装と一致している
- [ ] `make check` が緑

## 優先度の根拠

当たれば会話がそこで止まるが、思考を切る・フェイルオーバーを組まない・`AGENT_MAX_TOKENS` を
上げるという回避策があり、未再現（重要度 `medium`）。フェイルオーバーと思考を併用する構成が
増えるほど潜在的な失敗が増え、実 LLM 検証（`todo/2026-08-14_verify-against-a-real-llm.md`、P0）
と同じ機会に確かめると安い（緊急度 `medium`）。score 6 → P2。

## メモ

- 状況 1 の修正方向: dialect を `"anthropic"` から `"anthropic:<provider id>:<model>"` のように
  書き出し元まで含めたものにし、送出時に一致しないものは落とす。署名が実際にどの単位
  （モデル／アカウント／組織）で検証されるかを再現で確かめてから粒度を決める
- 状況 2 の修正方向: 最後の assistant ターンに思考ブロックが無ければ、そのリクエストだけ
  `thinking` を送らない、などが考えられる。Anthropic 側の要件（思考有効時に何が必須か）を
  `claude-api` スキル／公式ドキュメントで先に確認すること
- 状況 3: `stop_reason: "max_tokens"` で内容が思考ブロックだけの応答を履歴に積むかどうか
- 関連: `done/2026-08-14_cost-latency-router-and-failover.md`（400 は対象外という設計）、
  `progress/2026-11-08_anthropic-prompt-caching.md`（思考ブロックの扱いがキャッシュの前提にも効く）、
  `todo/2026-08-14_verify-against-a-real-llm.md`
- 出典: 2026-11-08 のレビュー（correctness）で見送ったもの
