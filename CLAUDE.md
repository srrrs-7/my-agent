# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## 目的とゴール

**my-agent** は Rust 製の CLI コーディングエージェントです。LLM に prompt / context / tools を渡し、
ツール呼び出しのループを回してワークスペース内のファイルを操作します。

このリポジトリが同時に追求している 3 つのゴール:

1. **ループエンジニアリングの実装** — エージェントループとコンテキスト構築を、隠れた前提なしに読める形で持つ
2. **境界の維持** — Clean Architecture の依存方向を、規約ではなく Cargo の依存関係とテストで強制する
3. **サプライチェーンの隔離** — 依存の取得・ビルド・実行をホストから切り離し、コンテナ内に閉じ込める

迷ったときの優先順位は **正しさ > 境界の明示 > 読みやすさ > 速度** です。

## 曲げてはいけない不変条件

これらは設計の帰結ではなく前提です。破る変更は、たとえテストが通っても差し戻してください。

- **依存は内向きのみ** — `application` は `infrastructure` を知らない。ランタイム（tokio）にも依存しない
- **ファイルアクセスは `WorkspacePath` 経由のみ** — 生の文字列からパスを組み立てるコードを増やさない
- **ツール失敗は実行失敗ではない** — モデルに `tool_result` として返し、ループは継続する
- **すべての `tool_call` に結果を返す** — 欠けると次のリクエストがプロバイダに拒否される
- **テストはネットワークも実モデルも要求しない**
- **`cargo` をホストで実行しない** — すべて `make` 経由でコンテナ内

詳細と根拠は `.claude/rules/invariants.md` にあります。

## コマンド

すべてコンテナ内で実行されます（`docker compose run --rm`）。ホストに Rust は不要です。

```bash
make setup                    # 初回: .env 作成 + イメージビルド
make check                    # fmt-check + clippy -D warnings + 全テスト（完了前に必ず実行）
make test                     # テストのみ
make build / make lint / make fmt
make doctor                   # 設定表示 + LLM 疎通確認
make ask Q="..."              # エージェントを 1 回実行
make chat                     # 対話セッション
make cargo CMD="<cargo args>" # 任意の cargo コマンド（依存追加など）
make exec  CMD="<shell>"      # 任意のシェルコマンド
```

単一テストの実行（`-p` に渡すのは**パッケージ名**。ディレクトリ名ではない）:

```bash
make cargo CMD="test -p agent-domain workspace::tests::refuses_traversal"  # 名前で絞る
make cargo CMD="test -p agent-cli --test end_to_end"                       # 統合テスト単位
make cargo CMD="test --workspace -- --nocapture"                           # 出力を見る
```

`build` / `test` / `check` は `.env` なしで動きます。`ask` / `chat` / `doctor` は
LLM 接続先が要るので、先に `make env` で `.env` を作ってください（`.env` は git 管理外）。

ビルド成果物は named volume の `/target` にあります。ホストの `target/` が空でも異常ではありません。

## アーキテクチャの全体像

```
crates/cli            (agent-cli)            プレゼンテーション + 合成ルート（DI はここだけ）
  ├→ crates/application    (agent-application)    ループ、ディスパッチャ、ツール実装
  └→ crates/infrastructure (agent-infrastructure) LLM クライアント、FS、設定、ログ
        └→ crates/domain   (agent-domain)         エンティティ・値オブジェクト・ポート(trait)
crates/test-support   (agent-test-support)   統合テスト用モック LLM サーバ（dev のみ）
```

括弧内が Cargo のパッケージ名です。バイナリ名は `agent`。

読む順序に迷ったら次の 4 ファイルで全体像がつかめます。

| ファイル | わかること |
|---|---|
| `crates/domain/src/ports/` | システムが外界と接する全境界 |
| `crates/application/src/agent/loop_runner.rs` | エージェントループ本体 |
| `crates/application/src/agent/dispatch.rs` | ツール実行の不変条件（順序・並列・承認） |
| `crates/cli/src/composition.rs` | 具象と抽象が結線される唯一の場所 |

構造の詳細・各ポートの実装対応・設計判断の根拠は `.claude/rules/architecture.md` と
`management/docs/architecture.md` にあります。

## マルチエージェント開発

このプロジェクトは **root セッションが orchestrator として専門エージェントを束ねる**方式で開発します。
root は計画・分解・統合・最終判断だけを行い、実装や調査そのものは委譲します。

| エージェント | 役割 | 権限 |
|---|---|---|
| `impl-agent` | 機能実装・バグ修正 | 編集可 |
| `refactor-agent` | 構造改善（振る舞いは変えない） | 編集可 |
| `test-agent` | テスト設計・追加・失敗調査 | 編集可 |
| `domain-specialist-agent` | 層の境界とドメインモデルの妥当性 | 読み取り専用 |
| `security-review-agent` | サンドボックス・秘密情報・サプライチェーン | 読み取り専用 |
| `performance-review-agent` | 計算量・確保・並行性 | 読み取り専用 |
| `issue-agent` | issue の起票・整形・棚卸し | `management/issues/` のみ編集可 |

運用の詳細（いつ誰に投げるか、ブリーフィングの型、レビューの並列実行、統合の手順）は
`/orchestrate` スキル、および `.claude/rules/orchestration.md` を参照してください。

## Issue 管理

作業のバックログは `management/issues/` で管理します。GitHub Issues ではなくリポジトリ内の
Markdown なので、コードと同じコミットで状態が動きます。

- **ステータスはディレクトリ**（`todo/` → `progress/` → `done/`）。ファイル内には書かない。遷移は `git mv`
- **ファイル名** は `<作成日>_<英小文字ケバブのタイトル>.md`。移動しても変えない
- **必須 4 点** — As-is / To-be（ユーザー視点）、影響範囲、達成条件、緊急度と重要度
- **優先度は導出する** — 緊急度 × 重要度（`xhigh` / `high` / `medium` / `low`）から表で機械的に決まる。
  議論すべきなのは優先順位ではなく、その 2 つの値です

セッション内の手順分解は `TodoWrite`、セッションを跨ぐバックログが issue です。

規約は `.claude/rules/issues.md`、雛形は `management/issues/TEMPLATE.md`。
操作は `/issue-new`（起票）、`/issue-work`（着手〜完了）、`/issue-triage`（棚卸し・次の 1 件）。

## リポジトリ内の設定の置き場所

抽象的な方針はこのファイルに、具体的な手順・規約・エージェント定義は `.claude/` に置きます。

```
.claude/agents/       専門エージェント定義（frontmatter + system prompt）
.claude/skills/       手順化されたワークフロー
                      /orchestrate /review-panel /add-tool /add-provider
                      /issue-new /issue-work /issue-triage
.claude/rules/        参照用の詳細規約。必要になった時に読む（下表）
.claude/settings.json 権限設定（ホストでの cargo 実行を機械的に禁止）
management/issues/    作業バックログ（todo / progress / done）
management/docs/      リポジトリの読者向けドキュメント
```

`.claude/rules/` はこのファイルから外に出した詳細です。**着手前に該当するものを読んでください。**

| ファイル | いつ読むか |
|---|---|
| `invariants.md` | コードを変える前に必ず。6 つの契約の根拠と検証コマンド |
| `architecture.md` | 新しい概念・ポート・依存を足すとき。「どこに何を書くか」の判断基準 |
| `workflow.md` | ビルド・依存追加・実 LLM での確認・git・ドキュメント同期 |
| `testing.md` | テストを書く／直すとき。層ごとの手法、既存フェイクの所在、禁止事項 |
| `style.md` | コメント・エラーメッセージ・型・命名の規約 |
| `orchestration.md` | 専門エージェントに委譲するとき。ブリーフィングの型と返却契約 |
| `issues.md` | issue を起票・遷移・棚卸しするとき |

`AGENTS.md`（リポジトリ直下）は**このプロジェクトが作っているエージェント自身**が読む指示書です。
Claude Code 向けではありませんが、方針が矛盾しないよう変更時は両方を確認してください。
