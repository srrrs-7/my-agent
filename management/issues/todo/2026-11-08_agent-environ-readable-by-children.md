---
title: 子プロセスがエージェントの /proc/<pid>/environ から API キー等を読めるかを確かめ、読めるなら塞ぐ
created: 2026-11-08
urgency: medium
importance: high
priority: P1
scope: [infrastructure, cli, docs]
---

# 子プロセスがエージェントの /proc/<pid>/environ から API キー等を読めるかを確かめ、読めるなら塞ぐ

## As-is（現状）

`run_command` の子と MCP サーバの環境からは秘密の変数を除去していますが、
**エージェント本体の環境**には全部が残っています — プロバイダの API キー、MCP の `env_from` で
渡す値、`.env` から読み込んだもの。子は同じ uid で動き、Landlock は読み取りを広く許可しているため、
`cat /proc/$PPID/environ`（MCP サーバの孫なら祖先を辿る）で丸ごと読める**可能性があります**。

- README §5 は MCP の「封じ込められないもの」に「同じユーザーの `/proc/<エージェントの pid>/environ`
  を読まれる経路は残ります」と書いていますが、**実際に読めるかは検証されていません**
- 出力に既知の秘密の値があれば伏せ字になりますが、ワークスペースへのエンコード書き込みや
  許可ドメイン（`AGENT_SHELL_NETWORK_ALLOW` / MCP の `allow_hosts`）への送信は伏せ字を通りません
- `make` 経由（compose）でも、キーは実環境変数としてエージェントに入るので同じです

## To-be（あるべき姿）

- エージェントが起動した子・孫は、エージェントの環境変数（API キーを含む）を読めない。
  操作者は「`env_from` で渡したもの以外の秘密は子に見えない」と言い切れる
- 読めないことが実プロセスのテストで固定されていて、README の記述がそれと一致している

## 影響範囲

- **クレート**: `cli/src/main.rs`（起動直後にエージェント自身へ `prctl` を掛けるならここ）、
  または `infrastructure/src/exec/`（`linux.rs` / `confined.rs`。子の側で塞ぐなら）。
  テストは `crates/infrastructure/tests/sandbox.rs` と `mcp_sandbox.rs`
- **不変条件**: **§2 子プロセス節に触れる**（「サンドボックスの主張はテストで実証する」。
  現在の README の主張がそもそも未実証）
- **設定・ドキュメント**: `README.md` §5（コマンド実行の Landlock 節、MCP の「封じ込められないもの」の
  `/proc/<pid>/environ` の記述）
- **利用者への影響**: `PR_SET_DUMPABLE=0` を採ると、エージェントのコアダンプが出なくなり、
  同じユーザーからの `gdb -p` / `strace -p` でのデバッグもできなくなる（意図した副作用）

## 達成条件

- [ ] **まず検証する**: `AGENT_SHELL_SANDBOX=confined` の `run_command` と MCP サーバのそれぞれから
      エージェントの `/proc/<pid>/environ` を読めるかを、実プロセスのテストで確かめ、結果を
      この issue のメモに記録する
- [ ] 読めない（既に塞がっている）場合: 読めないことを assert するテストを残し、README の
      記述を事実に合わせる
- [ ] 読める場合: 塞いだうえで、読めないことを同じテストで固定する
- [ ] `AGENT_SHELL_SANDBOX=none`（封じ込めなし）で読めるかどうかも README に書かれている
- [ ] `make check` が緑

## 優先度の根拠

読めるなら、承認された 1 回のコマンドや任意の MCP サーバが API キーを持ち出せるセキュリティ上の
穴です。ただし後述のとおり Landlock の ptrace 制限で既に塞がっている見込みもあり、未検証のため
`xhigh` ではなく重要度 `high` としました（検証で読めると分かれば `xhigh` に上げるべき）。
検証自体は安く、README の主張が未実証のまま残っているので緊急度 `medium`。score 8 → P1。

## メモ

- **仮説: confined の子からは既に読めない可能性が高い。** `/proc/<pid>/environ` の読み取りは
  `ptrace_may_access(PTRACE_MODE_READ_FSCREDS)` を通り、LSM の `ptrace_access_check` フックが呼ばれる。
  Landlock は「サンドボックス内のプロセスは、自分と同じかより内側のドメインにしか ptrace できない」
  という制限をこのフックで課しているため、ドメインの外にいるエージェント本体の environ は拒否される
  はず。ここを実機（dev コンテナのカーネル）で確かめるのが第一歩
- 候補の修正: エージェント起動時の `prctl(PR_SET_DUMPABLE, 0)`。非 dumpable なプロセスの
  `/proc/<pid>/*` は root 所有になり、同 uid でも `CAP_SYS_PTRACE` なしでは読めない。
  Landlock がない `none` の場合にも効くのが利点。ただし `/proc/self` を読む自前の処理
  （subreaper 案で `/proc/self/task/*/children` を読むなど）に影響しないかを確認すること
- `main.rs` は `.env` を `set_var` で実環境に入れるので、`.env` の値も environ に載る
- 関連:
  - `todo/2026-11-08_reap-children-that-leave-the-process-group.md` — 起動時の `prctl` の置き場所を揃える
  - `done/2026-11-08_secret-file-protection.md`（伏せ字と環境変数の除去）、
    `done/2026-11-08_sandboxed-mcp-client.md`（`env_from`）
- 出典: 2026-11-08 のレビュー（security）で見送ったもの
