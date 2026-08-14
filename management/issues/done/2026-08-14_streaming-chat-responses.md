---
title: ストリーミング応答（LlmProvider に chat_stream を追加）
created: 2026-08-14
started: 2026-08-14
completed: 2026-08-14
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

- [ ] `make chat` で応答が逐次表示される（Ollama 実機で確認）→ **未実施**（下記「結果」参照）
- [x] 非対応プロバイダ・ルータ経由でも自動でフォールバックし、機能が壊れない
- [x] ストリーミング中の `tool_call` は完全に集約されてから実行される（テストで固定）
- [x] リトライはストリーム開始前の失敗のみ対象とする（途中切断の再開はスコープ外と明記）
- [x] `application` に tokio 依存が入っていない
- [x] `agent run "..." > answer.md` の出力が非ストリーミング時と同一
- [x] `make check` が緑

## 優先度の根拠

機能自体は成立しており UX 改善（重要度 medium）。ただしイベント境界・プロバイダ境界の
上に機能が積み上がるほど、後からストリーミングを差し込む手戻りが大きくなる
（緊急度 medium）。

## メモ

- SSE パースを自前で書くか依存を足すかはサプライチェーン方針（60 行で書けるなら書く）
  に従って判断
- 実 LLM 検証（P0）と同時に進めると、ストリーミングの検証環境がそのまま使える

## 結果

- **domain**: `LlmProvider::chat_stream` を追加（`ports/llm.rs`）。プロトコルは
  `TextDelta*`（表示専用の散文断片）→ 必ず最後に `Completed(ChatResponse)`（非ストリーミングと
  同一の完成形。tool_call は集約済みでここにのみ載る）。既定実装は `chat()` への
  フォールバックで `Completed` のみを返すため、未対応プロバイダ（Anthropic）も
  ルータ・リトライ越しでも自動フォールバックする。`futures-core`（`Stream` trait のみ、
  ランタイム非依存）を domain の許可依存に追加
- **infrastructure**: SSE フレーミングを `llm/sse.rs`（手書き ~50 行、依存追加なし）、
  ステータス検査付きバイトストリーム取得を `http::send_streaming` に実装。
  OpenAI 互換クライアントは `stream: true` + `stream_options.include_usage` を送り、
  `StreamAccumulator` が断片を集約して既存の `decode_response` に流す —
  非ストリーミングと同一の不変条件（id フォールバック・引数パース・stop 理由の導出）が
  そのまま適用される。`RetryingProvider` はストリーム**開始前**の失敗のみ再試行
  （途中切断の再開はスコープ外、`chat_stream` の doc に明記）
- **application**: ループは `AGENT_STREAM`（既定 true）で `chat_stream` を消費し、
  `AssistantDelta` イベントを発行。空白のみの先頭デルタは保留し（qwen 系が tool_call 前に
  改行を吐くケース）、非ストリーミング時と stdout が byte 同一になることを保証
- **cli**: `TerminalRenderer` がデルタを改行なしで逐次出力し、`AssistantMessage` は
  行終端のみに（二重出力の抑止）
- テスト 129 → 144 本（SSE フレーミング 6、ループ 5、HTTP 2、E2E 2、設定 1... 断片化された
  tool_call 引数の集約、途中切断のエラー化、空白デルタの保留、明示的な stream=false を固定）

### やらなかったこと

- **Ollama 実機確認は未実施。** モデル取得（~2.5GB）の確認に対しユーザーが
  「モック検証のみで done」を選択。実機での逐次表示確認は
  `2026-08-14_verify-against-a-real-llm.md`（P0）の達成条件に自然に含まれる
  （`make chat` を叩けば既定でストリーミングが動く）ため、そちらに委ねる
- **Anthropic のネイティブストリーミングは未実装**（フォールバックで機能はする）。
  SSE のイベント形式が異なるため、必要になったら `llm/sse.rs` の枠組を再利用して別途
- `stream_options` を拒否する古いサーバへの対処は `AGENT_STREAM=false` の逃げ道で対応
