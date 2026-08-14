---
title: make test でテストカバレッジを出せるようにする
created: 2026-08-14
urgency: medium
importance: medium
priority: P2
scope: [ops, docs]
---

# make test でテストカバレッジを出せるようにする

## As-is（現状）

`make test` は 276 本のテストが通ったことしか教えてくれない。
どのコードがテストで通っていないかは分からないので、
「テストは全部緑だが、この分岐は一度も実行されていない」を誰も検知できない。

テストを足すとき、どこが薄いかを判断する材料が読み込みと勘しかない。
レビューで「テストが足りているか」を見るときも同じで、
`test-agent` が「カバーされていない不変条件」を探す作業が総当たりになっている。

## To-be（あるべき姿）

`make test` を実行すると、テスト結果に続けてクレート別のカバレッジ要約が出る。
どのクレートが薄いかがその場で分かる。

詳細を見たいときは、ホスト側に出力された HTML レポートをブラウザで開けば、
実行されなかった行が色で分かる。テストを書く前に「どこを埋めるか」を決められる。

CI のログにも同じ要約が残るので、変更でカバレッジが落ちたことに PR 上で気づける。

## 影響範囲

- **クレート**: なし（プロダクションコードは変更しない）。計測対象は
  `domain` / `application` / `infrastructure` / `cli` / `test-support` の 5 つ
- **不変条件**: なし
- **設定・ドキュメント**:
  - `Makefile` … `test` ターゲット
  - `docker/Dockerfile` … dev ステージに計測ツールを追加
  - `rust-toolchain.toml` … `llvm-tools-preview` を `components` に追加（llvm-cov を採る場合）
  - `.github/workflows/ci.yml` … 要約をログに残す
  - `.gitignore` … レポート出力先
  - `README.md` §7
- **利用者への影響**: `make test` の所要時間が伸びる（計測ビルドは `RUSTFLAGS` が変わり、
  通常ビルドとキャッシュを共有できないため、切り替えのたびに再コンパイルが走る）。
  初回は `make image` によるイメージ再ビルドが必要。
  `.env` / CLI 引数への破壊的変更はなし

## 達成条件

- [ ] `make test` の出力に、クレート別の行カバレッジとリージョンカバレッジの要約が含まれる
- [ ] HTML レポートが `/workspace` 配下（バインドマウント側）に出力され、ホストのブラウザで開ける
      — `/target` は名前付きボリュームなのでホストからは見えない
- [ ] レポートの出力先が `.gitignore` に入っていて、`git status` を汚さない
- [ ] 計測ツールが dev イメージに焼かれていて、`make test` のたびに
      `cargo install` の待ち時間が発生しない
- [ ] テストコード自体（`crates/*/tests/`）と `test-support` は集計から除外されている
- [ ] `make test ARGS="--package agent-domain"` のような既存の使い方が壊れていない
- [ ] CI のログにカバレッジ要約が出る
- [ ] README §7 に、出力先・読み方・計測にかかる追加時間を記載する
- [ ] 導入時点の実測値（クレート別）をこの issue のメモに残す
- [ ] `make check` が緑

## 優先度の根拠

テストが薄い場所を機械的に見つけられるようになる改善で、
今も「テストを読む」という回避策で判断はできているため重要度は `medium`。
テストが増えるほど後から穴を探すコストは上がるが、導入自体はいつやっても同じコストなので
緊急度も `medium`（score 6 → P2）。

## メモ

- **計測方式の候補**
  - `cargo-llvm-cov`（LLVM source-based）が第一候補。`llvm-tools-preview` が要る
  - `cargo-tarpaulin` は ptrace ベース。`compose.yml` の `cap_drop: ALL` と
    `no-new-privileges:true` の下で動くか要確認。動かないなら候補から外す
    （コンテナの隔離を緩めてまでカバレッジを取る価値はない）
- **`make test` を常に計測にするかは着手時に実測して決める。**
  計測ビルドは通常ビルドとキャッシュを共有できないため、`make check` の所要時間が
  無視できないほど伸びるなら、`make test`（素）と `make coverage`（計測）に分ける。
  その場合でも「`make test` を打った人がカバレッジに辿り着ける」導線は README に残す
- `make audit` が `cargo-audit` をオンデマンドで入れている前例はあるが、
  テストは毎回走るのでイメージに焼く方を採る
- 「外部バイナリに依存しない」（`.claude/rules/invariants.md` §2）は
  配布するサンドボックスに対する制約で、開発用ツールチェーンには掛からない
- **カバレッジ閾値で CI を落とす（カバレッジゲート）はこの issue の範囲外。**
  まず実測値を知ってから、必要なら別 issue にする
