---
title: プロセスグループから抜けた子孫（setsid / setpgid）も、コマンド終了時とエージェント終了時に確実に止める
created: 2026-11-08
urgency: medium
importance: high
priority: P1
scope: [infrastructure, cli, docs]
---

# プロセスグループから抜けた子孫（setsid / setpgid）も、コマンド終了時とエージェント終了時に確実に止める

## As-is（現状）

`run_command` の子と MCP サーバは、終了時・drop 時に**プロセスグループごと** SIGKILL されます。
ところが子孫が自分で `setsid` / `setpgid` を呼ぶとグループから抜け、この kill が届きません。

- 抜けたプロセスはコマンドの完了後も、エージェントの終了後も生き残ります
  （Landlock の封じ込めは継承されるので、ワークスペース外への書き込みや egress の制限は効いたまま）
- **依存ファイル検知の前提が崩れます。** README §5 は「`(sleep 20; echo x >> Cargo.toml) &` のように
  終了後に動く孫も、事後の依存検知の前に止まります」と書いていますが、
  `setsid sh -c 'sleep 20; echo x >> Cargo.toml' &` は止まらず、検知が終わった後で
  マニフェストを書き換えられます。承認したのは `npm test` なのに、依存の承認を通らずに依存が増えます
- 現状は README §5（コマンド実行・MCP の「残余リスク」）と `exec/capture.rs` の `//!` に
  制限として記載されているだけです

## To-be（あるべき姿）

- `run_command` が返った時点で、そのコマンドが生んだプロセスは（グループを抜けたものも含めて）
  1 つも残っていない。依存ファイル検知の「事後」が本当に事後になる
- MCP サーバを止めたとき・エージェントが正常終了したとき、サーバの子孫が残らない
- 確実な後始末が得られない環境では、それが `agent doctor` 等で分かる
  （黙って「止めたつもり」にならない）

## 影響範囲

- **クレート**: `infrastructure/src/exec/`（`process.rs` の `setsid` 起動と `kill(-pid)`、
  `capture.rs` の drop guard、`confined.rs`）、`infrastructure/src/mcp/server.rs`（サーバの停止）、
  `cli/src/main.rs` または `composition.rs`（エージェント自身を subreaper にするならプロセス全体の設定）
- **不変条件**: **§2 子プロセス節に触れる**（「黙って弱くならない」「外部バイナリに依存しない」
  「サンドボックスの主張はテストで実証する」）。cgroup v2 が使えない環境で何をするか
  （起動失敗か、subreaper のみで続行して表示するか）は root の判断が要る
- **設定・ドキュメント**: `README.md` §5（コマンド実行の残余リスク、MCP の「封じ込められないもの」）、
  `management/docs/architecture.md`（L204, L290 付近のプロセスグループの記述）
- **利用者への影響**: 破壊的変更は想定なし。ただし「グループから抜けたデーモン」を意図的に
  立ち上げるコマンド（`docker compose up -d` 相当）はコマンド終了とともに止まるようになる

## 達成条件

- [ ] `setsid` で抜けた孫が `run_command` の完了時点で生存していないことを、実プロセスを起動する
      テストで固定する（`crates/infrastructure/tests/sandbox.rs`）
- [ ] `setsid sh -c 'sleep N; echo x >> Cargo.toml' &` を実行するコマンドの後、N 秒待っても
      `Cargo.toml` が変わらないことを固定する（依存ファイル検知の前提）。sleep を使わずに
      書けるならそれを優先する（`testing.md` の禁止事項）
- [ ] MCP サーバの子孫が `setsid` で抜けても、サーバ停止時に止まることを固定する
      （`crates/infrastructure/tests/mcp_sandbox.rs`）
- [ ] 機構が得られない環境での振る舞いが決まっており、`agent doctor` に表示される
- [ ] README §5 の「残余リスク」から該当項目が消えるか、残る範囲（例: エージェント自身が
      SIGKILL された場合）だけに書き換わっている
- [ ] `make check` が緑

## 優先度の根拠

承認したコマンドの子孫が依存ファイル検知の後で書き込めるため、依存ゲートの保証に穴が
空いています（重要度 `high`。封じ込め自体は効いているので `xhigh` ではない）。
Ctrl-C 中断（progress）が同じ kill 経路を触っており、そちらと設計を合わせないと手戻りになる
（緊急度 `medium`）。score 8 → P1。

## メモ

- 候補は 2 つ。どちらも Linux 限定（サンドボックス自体が Linux 限定なので実害はない）
  - **`prctl(PR_SET_CHILD_SUBREAPER, 1)`** をエージェントに設定。抜けた子孫の親はエージェントに
    付け替わるので、`/proc/self/task/*/children` を辿って kill → reap できる。
    **注意: tokio は自分が spawn した pid しか `waitpid` しないため、付け替わってきた孤児は
    誰も reap せずゾンビとして溜まる**。孤児の reap を誰がやるか（`waitpid(-1)` を回すと
    tokio の子の終了コードを横取りする）が設計の山。どのコマンドの子孫かの帰属も別途必要
  - **cgroup v2 の `cgroup.kill`**。コマンドごとに子 cgroup を作れば帰属も後始末も 1 回で済む。
    ただしコンテナ内では cgroup が委譲されていない（書き込めない）ことが多く、
    「外部バイナリ不要・特権不要」と両立するかは実測が要る
  - PID 名前空間は `CLONE_NEWUSER` が要り、Docker 既定 seccomp で拒否されることを
    `todo/2026-08-14_netns-isolated-command-sandbox.md` のメモで実測済み
- エージェントが SIGKILL された場合はどちらの案でも残る（subreaper は親が死ねば無効）。
  これは別の残余として README に残る見込み
- 関連:
  - `progress/2026-11-08_interrupt-turn-with-ctrl-c.md` — 「Ctrl-C でエージェントが死ぬと
    `run_command` の子が裏で走り続けうる」。原因は違うが同じ kill 経路を触る
  - `todo/2026-11-08_agent-environ-readable-by-children.md` — 同じく起動時の `prctl` で
    エージェント自身を固める案。まとめて入れるなら置き場所を揃える
  - `done/2026-11-08_sandboxed-mcp-client.md`、`done/2026-08-14_sandboxed-command-execution-tool.md`
- 出典: 2026-11-08 のレビュー（security）で見送ったもの
- **実測（2026-11-08、dev コンテナ）:** `/sys/fs/cgroup` は `cgroup2 ro` でマウントされ、`mkdir` は
  `Read-only file system`。cgroup 案は基準環境で実証できないため採らず、subreaper +
  「エージェントの子で、tokio が起動したものではなく、別セッションにいるもの」だけを pid 指定で
  回収する方式にした（`infrastructure/src/exec/reaper.rs`）。得られない環境では起動は失敗させず、
  `agent doctor` の `detached children` 行とツール説明に表示する（root の決定）
