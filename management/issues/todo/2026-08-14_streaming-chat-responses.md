---
title: ストリーミング応答（LlmProvider に chat_stream を追加）
created: 2026-08-14
urgency: medium
importance: medium
priority: P2
scope: [domain, application, infrastructure, cli, docs]
---

# ストリーミング応答（LlmProvider に chat_stream を追加）

## As-is（現状）

`make chat` / `make ask` では、モデルの応答は**全文が生成し終わるまで何も表示されない**。
ローカル LLM はトークン生成が遅く、長い応答では数十秒間なにも出ないため、
処理中なのか固まっているのか区別がつかない。

## To-be（あるべき姿）

応答が生成と同時に流れて表示され、最初の文が 1〜2 秒で見え始める。
体感の待ち時間が大きく下がり、途中で方向が違うと分かったら Ctrl-C で打ち切れる。

## 影響範囲

- **クレート**: `domain`（`LlmProvider` に `chat_stream` を追加。既定実装は
  非ストリーミングへのフォールバック。`ProviderCapabilities.supports_streaming` は定義済み）、
  `infrastructure`（OpenAI / Anthropic の SSE パース、`RetryingProvider` /
  `RoutingProvider` の対応）、`application`（ループがストリームを消費し、
  差分イベントを発行）、`cli`（`TerminalRenderer` の逐次描画）
- **不変条件**: §1 — `application` は tokio 非依存のまま。ストリームは
  `futures::Stream` で表現する。§4 — ストリーミング中の `tool_call` は断片で届くため、
  **完全に集約してから** dispatch する（部分的な引数 JSON で実行しない）
- **設定・ドキュメント**: 必要なら `AGENT_STREAM`（既定 on で良いか要検討）、README §3
- **利用者への影響**: 表示の変化のみ。stdout=散文 / stderr=ツール活動のチャネル分離は維持する

## 達成条件

- [ ] `make chat` で応答が逐次表示される（Ollama 実機で確認）
- [ ] 非対応プロバイダ・ルータ経由でも自動でフォールバックし、機能が壊れない
- [ ] ストリーミング中の `tool_call` は完全に集約されてから実行される（テストで固定）
- [ ] リトライはストリーム開始前の失敗のみ対象とする（途中切断の再開はスコープ外と明記）
- [ ] `application` に tokio 依存が入っていない
- [ ] `agent run "..." > answer.md` の出力が非ストリーミング時と同一
- [ ] `make check` が緑

## 優先度の根拠

機能自体は成立しており UX 改善（重要度 medium）。ただしイベント境界・プロバイダ境界の
上に機能が積み上がるほど、後からストリーミングを差し込む手戻りが大きくなる
（緊急度 medium）。

## メモ

- SSE パースを自前で書くか依存を足すかはサプライチェーン方針（60 行で書けるなら書く）
  に従って判断
- 実 LLM 検証（P0）と同時に進めると、ストリーミングの検証環境がそのまま使える
