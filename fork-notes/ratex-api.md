# RaTeX の API と GPUI での描画

math-preview ブランチの数式プレビューが使う RaTeX の API と、GPUI でサイズ・ベースラインを決める方法。experience（`dotfiles-private/experience/zed/zed-math-ratex-api.md`、2026-08-30 時点）から移した。

数式描画に使う RaTeX 3クレートの確定 API とパイプライン（embed_glyphs 必須・ベースラインは shape_line probe で解決）

Zed Markdown プレビューの数式描画（`zed-math-preview-plan`（dotfiles-private の experience/archive））で採用する RaTeX の API。
GitHub erweixin/RaTeX のソースで確認済み（2026-06 時点、crates 0.1.12）。

確定パイプライン:
```rust
// deps: ratex-parser, ratex-layout, ratex-svg(features=["standalone","embed-fonts"]), ratex-types
let nodes = ratex_parser::parser::parse(latex)?;          // -> ParseResult<Vec<ParseNode>>
let opts  = ratex_layout::LayoutOptions::default()
    .with_style(if display { MathStyle::Display } else { MathStyle::Text }) // ::Text=インライン
    .with_color(color);                                    // ratex-types::Color（hex/CSS名/0-1 RGBA）
let lb    = ratex_layout::layout(&nodes, &opts);           // -> LayoutBox
let dl    = ratex_layout::to_display_list(&lb);            // -> DisplayList{width,height,depth,items}
let svg   = ratex_svg::render_to_svg(&dl, &SvgOptions{ font_size:40.0, embed_glyphs:true, ..Default::default() });
// SvgOptions: { font_size=40.0, padding=10.0, stroke_width=1.5, embed_glyphs=false, font_dir="" }
```

要点:
- `embed_glyphs:true` + feature `standalone`（+`embed-fonts`）で**グリフが `<path>` 化された自己完結 SVG**になり、外部 KaTeX CSS/フォント不要 → Zed の usvg/resvg でそのまま綺麗にラスタライズ可（確認済み）。デフォルト（embed_glyphs:false）は `<text>`＋KaTeX CSS 前提で **resvg では描けない**。
- **ベースライン**: `DisplayList.height`（baseline 上）/`depth`（baseline 下）が em 単位で取れる。px = ×font_size。baseline は viewBox 上端から `height*font_size` px。インライン縦位置はこれで精密計算（推測不要）。
- **色**: `LayoutOptions.with_color(Color)`。Zed の theme.text_color(Hsla) を hex 化して渡す。テーマ変更時はキャッシュ無効化＋再描画。
- **DPI**: `svg_renderer.render_single_frame(bytes, window.scale_factor())` でクリスプ化（resvg が内部で SMOOTH_SVG_SCALE_FACTOR=2.0 を適用）。
- 成熟度: erweixin/RaTeX は新しい（~6週）。個人 fork 用途ならリスク低。KaTeX 互換 99.5%、mhchem 対応、golden test あり。代替は typst-svg+mitex（重い・LaTeX→Typst 変換）。

GPUI での正しいサイズ/ベースライン解決（2026-07 実装・実測で確定）:
- **em・行高・ベースラインは全部 Element::request_layout 内で解決する**。div は子の layout を `window.with_text_style(...)` 内で走らせるので `window.text_style()` がカスケード済み（見出しの font_size 等を含む）。ビルド時に固定した px を持ち込むと見出し・表で破綻する。
- **罠: `TextSystem::baseline_offset`（FontMetrics 由来）と実際のテキスト描画のベースラインは大きくズレる**（実測: em12.88/行高18px でメトリクス 17.37px vs 実描画 13.83px、サイズ比例で拡大）。テキスト描画は shaped line の ascent/descent（`line.rs` の `(lh−a−d)/2+a`）を使うため。正解は **1文字 probe を `shape_line()` してその ascent/descent から同式で計算**（LineLayoutCache が効くので実質無料）。CJK フォールバックは行メトリクスに影響しない（Latin/CJK で同値を実測）。
- インライン数式は `.relative().top()` オフセットでなく、レイアウトボックス自体を `max(行高, 数式実高)` にして top を非負に（表セルからのはみ出し防止・行高計算に反映）。
