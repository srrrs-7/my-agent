---
title: 実 LLM に接続してエージェントループが動くことを確認する
created: 2026-08-14
urgency: high
importance: xhigh
priority: P0
scope: [ops, docs]
---

# 実 LLM に接続してエージェントループが動くことを確認する

## As-is（現状）

`make ask` / `make chat` は実装済みだが、**実際のモデルに繋いで動かした実績がない**。
自動テストはすべてモックサーバ相手で、128 本すべて緑ではあるものの、
実モデル特有の挙動（ツール呼び出しの引数の揺れ、`finish_reason` の食い違い、
小さいモデルがツールを正しく選べるか）は一度も観測できていない。

`make doctor` は設定表示と疎通確認までは動くことを確認済みだが、モデルが未取得のため
接続エラーで終わる。ユーザーは「このエージェントが実際に使えるのか」を確かめられない。

## To-be（あるべき姿）

ユーザーが `make ollama-up && make ollama-pull && make ask Q="..."` の 3 コマンドで、
エージェントがファイルを読んで質問に答えるところまで確認できる。

うまく動かないモデルがある場合、それが README に書いてあるので、
ユーザーは動くモデルを選ぶところから始められる。

## 影響範囲

- **クレート**: なし（想定）。実行して初めて分かる不具合が出た場合のみ
  `infrastructure/src/llm/openai.rs` の写像に手が入る可能性がある
- **不変条件**: なし
- **設定・ドキュメント**: `.env.example`（動作確認済みモデルの例）、`README.md` §2
- **利用者への影響**: なし（確認作業。破壊的変更は伴わない）

## 達成条件

- [ ] `make ollama-up` → `make ollama-pull MODEL=<model>` → `make doctor` が `ok` を返す
- [ ] `make ask Q="crates/domain にあるファイルを一覧して"` が `list_directory` を呼び、
      その結果に基づいた回答を返す
- [ ] `make ask Q="README.md の 1 行目を教えて"` が `read_file` を呼んで正しく答える
- [ ] 書き込み系（`write_file` / `edit_file`）の承認プロンプトが出て、`n` で拒否したとき
      モデルがその旨を認識した応答を返す
- [ ] 動作を確認したモデル名を `README.md` §2 と `.env.example` に記載する
- [ ] 途中で見つかった実装の不具合は別 issue に切り出す（この issue では直さない）
- [ ] `make check` が緑

## 優先度の根拠

このリポジトリの主張（「LLM に prompt / context / tools を渡してファイルを操作する」）が
成立しているかを未検証のまま他の機能を積むと、土台の欠陥に後から気づくことになる。
重要度は最上位。一方でモデル取得に数 GB のダウンロードが要り、ユーザーの環境と判断が要るため、
緊急度は `xhigh` ではなく `high`。

## メモ

- モデルの取得はユーザーの確認なしに実行しない（`.claude/rules/workflow.md`）
- ツール呼び出しの精度はモデル依存。`qwen3:8b` で駄目なら、より大きいモデルか
  クラウド LLM（`AGENT_PROVIDER=anthropic`）でも試して、結果を README に残す
- 実 API キーでの Anthropic 経路の疎通も未確認。分けるなら別 issue
