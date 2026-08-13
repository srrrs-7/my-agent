---
name: add-provider
description: my-agent に新しい LLM ベンダのクライアントを追加する。infrastructure 層の HTTP クライアント実装、ProviderKind の追加、factory への結線、テスト、設定ドキュメントまでの手順。「〜に対応させて」「〜のAPIを使えるようにして」と言われたときに使う。
argument-hint: [vendor-name]
---

# 新しい LLM プロバイダを追加する

$ARGUMENTS

**まず確認**: そのベンダは OpenAI 互換の `/chat/completions` を話しますか。
話すなら**コードは不要**です。`AGENT_BASE_URL` と `AGENT_MODEL` を設定するだけで動きます
（Ollama, vLLM, LM Studio, llama.cpp, OpenRouter, Groq, Together はこれに該当）。

独自プロトコルの場合だけ以下に進みます。参考実装: `crates/infrastructure/src/llm/anthropic.rs`。

## 1. クライアントを実装する

`crates/infrastructure/src/llm/<vendor>.rs`:

```rust
pub struct XxxProvider { id, client, base_url, api_key, default_model, timeout }

#[async_trait]
impl LlmProvider for XxxProvider {
    fn id(&self) -> ProviderId;
    fn capabilities(&self) -> ProviderCapabilities;
    async fn chat(&self, request: ChatRequest) -> Result<ChatResponse, LlmError>;
}
```

写像で気をつけること:

- **ドメインの `Message` はブロック構造**（Anthropic 寄り）。ベンダ形式への変換はこのファイルの責務
- `Role::Tool` の扱いはベンダで違う。OpenAI は独立した `role:"tool"` メッセージ、
  Anthropic は `user` ターン内の `tool_result` ブロック（かつ同一ロールの連続はマージが必要）
- 応答の wire 型は `#[serde(default)]` を厚めに。実装ごとの差異でパース全体を落とさない
- 未知の列挙値は落とさず無視する（`#[serde(other)] Unknown`）
- `StopReason` は**内容から決める**。`finish_reason` のラベルを信用しない
  （Ollama は tool_calls があっても `"stop"` を返す）
- エラーは `super::map_http_failure(status, retry_after, &body)` に通す。
  `LlmError::is_retryable()` の判定が全プロバイダで揃う
- タイムアウトは `reqwest::Error::is_timeout()` を見て `LlmError::Timeout` に変換する

## 2. 設定を追加する

1. `crates/infrastructure/src/config/kinds.rs` の `ProviderKind` に variant を足し、
   `as_str` / `FromStr` / `default_base_url` / `default_api_key_env` を全て埋める
   （`FromStr` は表記ゆれを許容し、エラーメッセージに受理可能な値を列挙する）
2. `crates/infrastructure/src/llm/factory.rs` の `build_client` に分岐を追加
3. `crates/infrastructure/src/llm/mod.rs` で `pub use`

API キー必須のベンダなら、`provider_from_env` で**設定読み込み時に**欠落を弾いてください
（実行中に 401 で気づくより早い）。

## 3. テストする

| 種類 | 場所 | 内容 |
|---|---|---|
| 写像 | 同ファイルの `#[cfg(test)]` | 送出ペイロードの形、応答のデコード、ロール変換、未知ブロックの無視 |
| HTTP | `crates/infrastructure/tests/` | `agent_test_support::MockLlmServer` でステータス写像と実際の送出 JSON |
| 設定 | `config/kinds.rs`, `config/env.rs` | 表記ゆれの受理、キー必須の検証 |

`MockLlmServer::start(vec![Response::status("429 Too Many Requests", body)])` のように
失敗系も必ず入れてください。

## 4. ドキュメントを更新する

- `.env.example` — 接続例をコメントで追加
- `README.md` §2（接続例）と §8（変数表）

## 5. 確認する

```bash
make check
make doctor   # 実際に疎通する場合
```

## ルーティングとの関係

新しいプロバイダは自動的にルーティングの対象になります。
`AGENT_PROVIDERS=local,cloud` のように別名で並べれば、`--model cloud/<model>` で選択できます。

ルーティングの規則自体を変えたい場合は、このスキルではなく `LlmRouter` の実装追加です
（`.claude/rules/architecture.md` を参照）。
