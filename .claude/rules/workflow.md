# 開発ワークフロー

## コンテナファースト

ホストで `cargo` / `rustc` / `rustup` を実行しません（`.claude/settings.json` が拒否します）。
すべて `make` 経由でコンテナ内に閉じ込めます。理由は `invariants.md` §6。

`make` は `docker compose` / `docker-compose` を自動判別します。汎用の逃げ道が 2 つあります。

```bash
make cargo CMD="<cargo の引数>"    # 例: add --package agent-cli indicatif
make exec  CMD="<シェルコマンド>"  # 例: ls -la /target
```

ビルド成果物は named volume の `/target` にあり、bind mount には出てきません（macOS で速く、
ホストを汚さないため）。`ls target/` がホストで空でも異常ではありません。

## 変更を終える前に

```bash
make check   # fmt-check + clippy -D warnings + 全テスト
```

`make check` が緑でないまま「完了」と報告しないでください。
`make fmt` を先に走らせると fmt-check の差分でこけません。

clippy は `-D warnings` です。警告を握りつぶす `#[allow(...)]` を足す前に、
lint が指している設計上の指摘を検討してください（例: `into_*` は `self` を取るべき、など）。

## 依存を足す

```bash
make cargo CMD="add --package agent-<layer> <crate>"
```

追加前に確認すること:

- その層に許される依存か（`architecture.md` の表）
- 60 行程度の自前実装で済まないか（依存はサプライチェーンの表面積です）
- ワークスペース共通で使うなら `[workspace.dependencies]` に定義し、各 crate は `{ workspace = true }`

追加後は `make audit` で既知の脆弱性を確認できます。

## 実 LLM で試す

```bash
make ollama-up
make ollama-pull MODEL=qwen3:8b
make doctor          # 設定表示 + 疎通確認
make ask Q="..."
```

モデルの取得は数 GB かかります。**ユーザーの確認なしにダウンロードを開始しないでください。**

`make doctor` は設定を秘密情報をマスクした上で表示し、最小のリクエストで疎通を確認します。
接続エラーの多くは `AGENT_BASE_URL` の指定ミスです（コンテナ内の `localhost` はコンテナ自身を指します）。

## git

- `main` に直接コミットしない。ブランチを切る
- コミット・プッシュはユーザーに求められたときだけ
- `.env` はコミットしない（`.gitignore` 済み、`.claude/settings.json` が読み取りも拒否）

## ドキュメントの同期

コードを変えたら、対応するドキュメントが嘘になっていないか確認します。

| 変更 | 一緒に見るもの |
|---|---|
| 環境変数の追加・変更 | `.env.example`, `README.md` §8 |
| 層構成・設計判断の変更 | `management/docs/architecture.md`, `.claude/rules/architecture.md` |
| ツールの追加・削除 | `README.md` §4 の表 |
| テスト数の増減 | `README.md` §7, `management/docs/architecture.md` §5 |
| エージェント自身の作業規約 | `AGENTS.md`（リポジトリ直下）と CLAUDE.md の整合 |
