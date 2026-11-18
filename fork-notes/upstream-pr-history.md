# 本家の数式プレビューの PR の系譜

experience（`dotfiles-private/experience/zed/zed-math-upstream-pr-history.md`、2026-08-30 時点）から移した。PR の状態はその時点のもの。

本家の数式プレビュー PR 系譜: エンジンは RaTeX に収束、#57339 が公式レビュー中、メンテナは非侵襲を選好。我々は #54668 後期系統＋選択思想の立ち位置

zed-industries/zed の Markdown 数式プレビュー PR 系譜（Issue #40813 が feature request、OPEN のまま）。

**PR タイムライン / エンジン**:
- #41018 (CLOSED) LaTeX→Unicode 単純置換（エンジンなし）
- #48143 (CLOSED) 新 crate `latex_render` + **ReX** + Latin Modern Math 同梱、画像化。#44525 続き
- #53150 (CLOSED) pulldown ENABLE_MATH + **ReX** + 独自 `GpuiMathBackend`(輪郭→Path/罫線→Quad) + XITS Math。→#54668 favor でクローズ
- #54668 (OPEN/draft) 初期 ReX 画像 → 後に **RaTeX** + **paint_glyph アトラス**描画 + KaTeX TTF style bit patch(`=`→`@` 回避)。我々と同系統
- #58593 (CLOSED) **RaTeX** + gpui に `InlineReplacement` API 新規追加(+234行)/U+FFFC。#57339 の重複としてクローズ
- #57339 (**OPEN・公式レビュー中**) **RaTeX** + `ratex-svg` → GPUI SVG renderer で画像化。loomou 作

**エンジン収束**: Unicode置換 → ReX → **RaTeX**（Rust製 KaTeX 互換）。ReX 離脱理由=原 repo 不活発、KenyC fork は活発だが若く信頼性懸念（lnay 指摘）。

**描画3系統**:
1. 画像埋め込み(PNG/SVG→img) = #48143/#53150/#54668初期/#57339。選択・LaTeXコピー・ゼロコストテーマ切替を失う
2. GPUI グリフアトラス paint_glyph = #54668後期 / **我々**。エディタ文字と同一描画・ズーム鮮明
3. GPUI ネイティブテキスト(InlineReplacement) = #58593。行折返し/選択に参加するが gpui 改変必要

**我々の実装の立ち位置**: RaTeX + paint_glyph アトラス + `math_regions` で選択/コピー(LaTeX維持) + gpui 無改変。#54668後期系統に #58593 の選択思想を足し、#57339(SVG画像)より上位。

**メンテナ姿勢**: ConradIrwin「full latex は今は大げさ、ロードマップ寄りを」/大変更に慎重・非侵襲を選好。現在 #57339 を公式レビュー対象（#58593 をその重複扱い）。

関連: `ratex-api.md`
