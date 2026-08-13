# Issues

## ルール

| | |
|---|---|
| ステータス | ディレクトリで表す（`todo/` → `progress/` → `done/`）。ファイル内には書かない |
| 遷移 | `git mv` でファイルを移動する |
| ファイル名 | `<作成日>_<英小文字ケバブのタイトル>.md`（例: `2026-08-14_streaming-response-support.md`）。**移動しても変えない** |
| 起票 | `TEMPLATE.md` をコピーして埋める |
| 優先度 | 緊急度 × 重要度から機械的に導出（下表） |

## 優先度

`score = 重要度 × 2 + 緊急度`（`xhigh`=4, `high`=3, `medium`=2, `low`=1）

| 重要度＼緊急度 | low | medium | high | xhigh |
|---|---|---|---|---|
| **xhigh** | P1 | P1 | **P0** | **P0** |
| **high** | P2 | P1 | P1 | P1 |
| **medium** | P2 | P2 | P2 | P1 |
| **low** | P3 | P3 | P2 | P2 |

並び順は **P 昇順 → score 降順 → 作成日昇順**。

各値の意味と、必須セクションの書き方は
[`.claude/rules/issues.md`](../../.claude/rules/issues.md) にあります。

## Claude Code から使う

```
/issue-new     起票する
/issue-work    着手 → 実装 → 完了まで進める
/issue-triage  棚卸しして次の 1 件を決める
```

## 一覧を見る

```bash
ls management/issues/todo
grep -H '^priority:' management/issues/todo/*.md | sort -t: -k3
```
