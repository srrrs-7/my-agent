---
title: <一文でのタイトル。日本語可>
created: <YYYY-MM-DD  date +%F で取得する>
urgency: <xhigh | high | medium | low>
importance: <xhigh | high | medium | low>
priority: <P0 | P1 | P2 | P3  .claude/rules/issues.md の表から導出>
scope: [<domain | application | infrastructure | cli | test-support | docs | ops>]
---

# <タイトル>

## As-is（現状）

<ユーザーから見て、今どうなっているか。何に困っているか。
 実装の話ではなく、観測できる事実を書く。>

## To-be（あるべき姿）

<ユーザーから見て、何がどう変わるか。
 「〜できるようになる」「〜しなくてよくなる」の形で書く。>

## 影響範囲

- **クレート**: <crates/... 触る範囲。無ければ「なし」>
- **不変条件**: <.claude/rules/invariants.md のどれに触れるか。無ければ「なし」>
- **設定・ドキュメント**: <.env.example / README.md §n / management/docs/... >
- **利用者への影響**: <破壊的変更の有無。既存の .env や CLI 引数が動かなくなるか>

## 達成条件

- [ ] <検証可能な条件を書く。「ちゃんと動く」は不可>
- [ ] <ユーザー視点の To-be が満たされたと言える条件>
- [ ] `make check` が緑

## 優先度の根拠

<なぜその緊急度・重要度なのかを 1〜2 文。
 後から自分以外が再評価できるだけの理由を残す。>

## メモ

<任意。調査結果、検討した代替案、関連 issue、参考リンク。
 着手中に分かったことはここに追記する。>
