//! Native vector rendering of LaTeX math for the Markdown preview.
//!
//! Math is parsed with RaTeX into a [`DisplayList`] (glyph placements, fraction bars,
//! radicals as bezier paths) and painted directly with GPUI's vector primitives — no
//! intermediate raster image. This keeps formulas crisp at any zoom and lets the theme
//! text color be applied at paint time (no re-render on theme change).

use ab_glyph::{Font as _, FontRef, OutlineCurve, Point as AbPoint};
use collections::{HashMap, HashSet};
use gpui::{
    AbsoluteLength, App, Bounds, Context, DefiniteLength, Element, Font, FontId as GpuiFontId,
    FontStyle, FontWeight, GlobalElementId, GlyphId, Hsla, InspectorElementId, IntoElement,
    LayoutId, Length, PathBuilder, Pixels, Point, Rgba, SharedString, Size, Style, Window, fill,
    font, point, px, size,
};
use ratex_types::{Color, DisplayItem, DisplayList, PathCommand};
use std::borrow::Cow;
use std::cell::Cell;
use std::rc::Rc;
use std::sync::{Arc, OnceLock};
use theme::ActiveTheme as _;

use crate::parser::MarkdownEvent;

use super::{Markdown, ParsedMarkdown};

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct MathKey {
    latex: SharedString,
    display: bool,
}

/// Caches the parsed [`DisplayList`] for each formula. Parsing+layout is cheap and theme
/// independent, so the result is reused across frames and themes; `Err` marks a formula
/// that failed to parse (the caller falls back to showing the raw source).
#[derive(Default, Clone)]
pub(crate) struct MathState {
    /// `Err` carries a human-readable parse error to display in place of the formula.
    cache: HashMap<MathKey, Result<Arc<DisplayList>, SharedString>>,
}

impl MathState {
    pub(crate) fn clear(&mut self) {
        self.cache.clear();
    }

    /// Parse any math in `parsed` that isn't cached yet and drop entries no longer present.
    /// Inline math is cached per breakable fragment (so a long formula can wrap at top-level
    /// operators); block math is cached whole.
    pub(crate) fn update(&mut self, parsed: &ParsedMarkdown, _cx: &mut Context<Markdown>) {
        let mut keys = std::collections::HashSet::new();
        for (_, event) in parsed.events.iter() {
            match event {
                MarkdownEvent::InlineMath(latex) => {
                    for fragment in split_inline_math(latex) {
                        keys.insert(self.ensure_cached(fragment.into(), false));
                    }
                }
                MarkdownEvent::DisplayMath(latex) => {
                    keys.insert(self.ensure_cached(latex.clone(), true));
                }
                _ => {}
            }
        }
        self.cache.retain(|key, _| keys.contains(key));
    }

    fn ensure_cached(&mut self, latex: SharedString, display: bool) -> MathKey {
        let key = MathKey { latex, display };
        if !self.cache.contains_key(&key) {
            let result = match parse_to_display_list(&key.latex, key.display) {
                Ok(display_list) => Ok(Arc::new(display_list)),
                Err(error) => {
                    log::warn!("failed to render math `{}`: {error}", key.latex);
                    Err(SharedString::from(format!("Math error: {error}")))
                }
            };
            self.cache.insert(key.clone(), result);
        }
        key
    }

    /// The cached parse result for a formula: `Some(Ok(_))` when ready, `Some(Err(msg))` on a
    /// parse error (with a message to display), `None` when not cached yet.
    fn result(&self, latex: &str, display: bool) -> Option<Result<Arc<DisplayList>, SharedString>> {
        self.cache
            .get(&MathKey {
                latex: SharedString::from(latex),
                display,
            })
            .cloned()
    }
}

/// Parse a LaTeX string into a positioned [`DisplayList`]. `display` selects display vs
/// inline (text) math style.
fn parse_to_display_list(latex: &str, display: bool) -> anyhow::Result<DisplayList> {
    if latex.trim().is_empty() {
        anyhow::bail!("empty math expression");
    }
    let nodes =
        ratex_parser::parse(latex).map_err(|error| anyhow::anyhow!("{}", error.message))?;
    let style = if display {
        ratex_types::MathStyle::Display
    } else {
        ratex_types::MathStyle::Text
    };
    let options = ratex_layout::LayoutOptions::default().with_style(style);
    let layout_box = ratex_layout::layout(&nodes, &options);
    Ok(ratex_layout::to_display_list(&layout_box))
}

/// Approximate body line height as a multiple of the em (the preview paragraph uses a
/// ~1.3rem line height over ~0.92rem text, giving roughly this ratio).
const LINE_HEIGHT_EM: f32 = 1.4;
/// Horizontal gap (in em) inserted before each fragment after the first to approximate the
/// inter-operator spacing lost when an inline formula is split for wrapping.
const INTER_FRAGMENT_EM: f32 = 0.22;

/// Outcome of rendering a block (display) formula.
pub(crate) enum MathBlock {
    Element(MathElement),
    /// Parse error message to show in place of the formula.
    Error(SharedString),
    /// Not parsed yet.
    Pending,
}

/// Outcome of rendering an inline formula (possibly split into breakable fragments).
pub(crate) enum MathInline {
    Fragments(Vec<InlineFragment>),
    Error(SharedString),
    Pending,
}

/// Builds the element for a block (display) formula.
pub(crate) fn render_math_block(
    math_state: &MathState,
    latex: &SharedString,
    em_px: Pixels,
) -> MathBlock {
    match math_state.result(latex, true) {
        Some(Ok(display_list)) => MathBlock::Element(MathElement {
            display_list,
            em_px,
            bounds_slot: None,
        }),
        Some(Err(message)) => MathBlock::Error(message),
        None => MathBlock::Pending,
    }
}

/// One piece of a (possibly split) inline formula, with its vertical offset (to share a
/// common baseline with the other pieces) and left gap (operator spacing).
pub(crate) struct InlineFragment {
    pub(crate) element: MathElement,
    pub(crate) top: Pixels,
    pub(crate) left: Pixels,
}

/// Builds the breakable pieces of an inline formula. The formula is split at top-level
/// operators so the surrounding flex line can wrap between pieces; all pieces share one
/// baseline so they still read as a single formula. Returns `None` (fall back to raw text)
/// if any piece fails to parse.
pub(crate) fn render_math_inline(
    math_state: &MathState,
    latex: &SharedString,
    em_px: Pixels,
) -> MathInline {
    let fragments = split_inline_math(latex);
    let mut lists = Vec::with_capacity(fragments.len());
    for fragment in &fragments {
        match math_state.result(fragment, false) {
            Some(Ok(display_list)) => lists.push(display_list),
            Some(Err(message)) => return MathInline::Error(message),
            None => return MathInline::Pending,
        }
    }
    if lists.is_empty() {
        return MathInline::Pending;
    }

    // Center the tallest piece in the line; every piece shares that baseline.
    let (height_of_tallest, total_of_tallest) = lists.iter().fold((0.0f32, 0.0f32), |acc, dl| {
        let total = (dl.height + dl.depth) as f32;
        if total > acc.1 {
            (dl.height as f32, total)
        } else {
            acc
        }
    });
    let common_baseline = (LINE_HEIGHT_EM - total_of_tallest) / 2.0 + height_of_tallest;

    let mut out = Vec::with_capacity(lists.len());
    for (index, display_list) in lists.into_iter().enumerate() {
        let top = em_px * (common_baseline - display_list.height as f32);
        let left = if index == 0 {
            px(0.0)
        } else {
            em_px * INTER_FRAGMENT_EM
        };
        out.push(InlineFragment {
            element: MathElement {
                display_list,
                em_px,
                bounds_slot: None,
            },
            top,
            left,
        });
    }
    MathInline::Fragments(out)
}

/// Returns true for a `\command` that acts as a top-level binary operator or relation, at
/// which an inline formula may be broken for wrapping.
fn is_break_command(command: &str) -> bool {
    matches!(
        command,
        "\\pm" | "\\mp"
            | "\\times"
            | "\\cdot"
            | "\\div"
            | "\\ast"
            | "\\star"
            | "\\le"
            | "\\ge"
            | "\\leq"
            | "\\geq"
            | "\\ne"
            | "\\neq"
            | "\\equiv"
            | "\\approx"
            | "\\sim"
            | "\\simeq"
            | "\\cong"
            | "\\propto"
            | "\\to"
            | "\\rightarrow"
            | "\\leftarrow"
            | "\\Rightarrow"
            | "\\Leftarrow"
            | "\\mapsto"
            | "\\cup"
            | "\\cap"
            | "\\in"
            | "\\subset"
            | "\\supset"
            | "\\subseteq"
            | "\\supseteq"
            | "\\oplus"
            | "\\otimes"
            | "\\wedge"
            | "\\vee"
    )
}

fn is_break_char(c: char) -> bool {
    matches!(c, '+' | '=' | '<' | '>' | '-')
}

/// Split an inline LaTeX string at top-level (brace depth 0) binary operators and relations,
/// keeping each operator at the start of the following fragment. Operators that begin a
/// fragment (i.e. unary, or right after another operator) are not split on. If there are no
/// top-level break points the whole string is returned as a single fragment.
fn split_inline_math(latex: &str) -> Vec<String> {
    let chars: Vec<char> = latex.chars().collect();
    let mut fragments = Vec::new();
    let mut current = String::new();
    let mut depth: i32 = 0;
    let mut prev_was_operator = true; // a leading operator is unary, never a break point
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '{' | '(' | '[' => {
                depth += 1;
                current.push(c);
                prev_was_operator = false;
                i += 1;
            }
            '}' | ')' | ']' => {
                depth -= 1;
                current.push(c);
                prev_was_operator = false;
                i += 1;
            }
            '\\' => {
                let mut command = String::from("\\");
                i += 1;
                while i < chars.len() && chars[i].is_ascii_alphabetic() {
                    command.push(chars[i]);
                    i += 1;
                }
                // Non-letter control sequence like `\,` or `\\`.
                if command.len() == 1 && i < chars.len() {
                    command.push(chars[i]);
                    i += 1;
                }
                let is_op = is_break_command(&command);
                if depth == 0 && is_op && !current.trim().is_empty() && !prev_was_operator {
                    fragments.push(std::mem::take(&mut current));
                }
                current.push_str(&command);
                prev_was_operator = is_op;
            }
            _ if depth == 0
                && is_break_char(c)
                && !current.trim().is_empty()
                && !prev_was_operator =>
            {
                fragments.push(std::mem::take(&mut current));
                current.push(c);
                prev_was_operator = true;
                i += 1;
            }
            _ => {
                if !c.is_whitespace() {
                    prev_was_operator = false;
                }
                current.push(c);
                i += 1;
            }
        }
    }
    if !current.trim().is_empty() {
        fragments.push(current);
    }
    if fragments.is_empty() {
        fragments.push(latex.to_string());
    }
    fragments
}

/// A shared slot that [`MathElement`] writes its final painted bounds into during prepaint.
/// The markdown selection layer reads these to hit-test and highlight where the math is actually
/// drawn; the invisible copyable source anchor lives at unrelated coordinates and cannot be used
/// for that. `Option` is `None` until the element has been laid out at least once this frame.
pub(crate) type MathBoundsSlot = Rc<Cell<Option<Bounds<Pixels>>>>;

/// A GPUI element that paints a RaTeX [`DisplayList`] as native vector graphics.
pub(crate) struct MathElement {
    display_list: Arc<DisplayList>,
    em_px: Pixels,
    /// Set by [`push_markdown_math`](crate::markdown) so prepaint can publish the painted bounds.
    bounds_slot: Option<MathBoundsSlot>,
}

impl MathElement {
    fn width_px(&self) -> Pixels {
        self.em_px * self.display_list.width.max(0.0) as f32
    }

    fn height_px(&self) -> Pixels {
        self.em_px * (self.display_list.height + self.display_list.depth).max(0.0) as f32
    }

    /// Attach the shared slot that prepaint publishes the painted bounds into.
    pub(crate) fn set_bounds_slot(&mut self, slot: MathBoundsSlot) {
        self.bounds_slot = Some(slot);
    }
}

impl IntoElement for MathElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for MathElement {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<gpui::ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size = Size {
            width: Length::Definite(DefiniteLength::Absolute(AbsoluteLength::Pixels(
                self.width_px(),
            ))),
            height: Length::Definite(DefiniteLength::Absolute(AbsoluteLength::Pixels(
                self.height_px(),
            ))),
        };
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Self::PrepaintState {
        // Publish the final painted bounds (after block centering / inline flex layout) so the
        // markdown selection layer can hit-test and highlight the math where it is actually drawn.
        if let Some(slot) = &self.bounds_slot {
            slot.set(Some(bounds));
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let color = cx.theme().colors().text;
        paint_display_list(&self.display_list, bounds, self.em_px, color, window);
    }
}

fn paint_display_list(
    display_list: &DisplayList,
    bounds: Bounds<Pixels>,
    em_px: Pixels,
    theme_text: Hsla,
    window: &mut Window,
) {
    // Fonts referenced by this formula. With the `embed-fonts` feature the loader uses the
    // bundled KaTeX fonts, so the font directory is irrelevant.
    let fonts = match ratex_font_loader::load_fonts_for_items("", &display_list.items) {
        Ok(fonts) => fonts,
        Err(error) => {
            log::warn!("failed to load math fonts: {error}");
            return;
        }
    };
    let origin = bounds.origin;
    let em = f32::from(em_px);
    let scale = window.scale_factor();

    for item in &display_list.items {
        match item {
            DisplayItem::GlyphPath {
                x,
                y,
                scale: glyph_scale,
                font,
                char_code,
                color,
            } => {
                paint_glyph_outline(
                    window,
                    &fonts,
                    font,
                    *char_code,
                    *x,
                    *y,
                    *glyph_scale,
                    origin,
                    em,
                    resolve_math_color(*color, theme_text),
                );
            }
            DisplayItem::Line {
                x,
                y,
                width,
                thickness,
                color,
                // `\hdashline` dashing isn't reproduced yet; render solid as before.
                dashed: _,
            } => {
                // `y` is the center of the rule. Snap to the device-pixel grid so thin rules
                // (fraction bars, roots, delimiter strips) stay crisp and never antialias away.
                let top = (*y - thickness / 2.0) as f32;
                let rect = Bounds {
                    origin: point(origin.x + px(*x as f32 * em), origin.y + px(top * em)),
                    size: size(px(*width as f32 * em), px(*thickness as f32 * em)),
                };
                window.paint_quad(fill(
                    snap_rect_to_device(rect, scale),
                    resolve_math_color(*color, theme_text),
                ));
            }
            DisplayItem::Rect {
                x,
                y,
                width,
                height,
                color,
            } => {
                let rect = Bounds {
                    origin: point(origin.x + px(*x as f32 * em), origin.y + px(*y as f32 * em)),
                    size: size(px(*width as f32 * em), px(*height as f32 * em)),
                };
                window.paint_quad(fill(
                    snap_rect_to_device(rect, scale),
                    resolve_math_color(*color, theme_text),
                ));
            }
            DisplayItem::Path {
                x,
                y,
                commands,
                fill: filled,
                color,
            } => {
                paint_vector_path(
                    window,
                    commands,
                    *filled,
                    *x,
                    *y,
                    origin,
                    em,
                    resolve_math_color(*color, theme_text),
                );
            }
        }
    }
}

/// Resolve a RaTeX item color to a paint color. RaTeX emits `BLACK` for ordinary math, which we
/// map to the theme's text color so formulas follow light/dark themes. An explicit `\color{...}`
/// (any non-black color) is honored as authored.
fn resolve_math_color(color: Color, theme_text: Hsla) -> Hsla {
    // Only RaTeX's default opaque black (0,0,0,1) follows the theme. Authored colors — including
    // transparent/alpha black like `\color{transparent}` — are honored exactly as given.
    if color == Color::BLACK {
        theme_text
    } else {
        Rgba {
            r: color.r,
            g: color.g,
            b: color.b,
            a: color.a,
        }
        .into()
    }
}

/// Snap a rectangle to the physical device-pixel grid, keeping at least one device pixel in each
/// dimension. Used for rules and filled boxes so hairlines render crisply at any scale factor.
fn snap_rect_to_device(rect: Bounds<Pixels>, scale: f32) -> Bounds<Pixels> {
    let left = (f32::from(rect.left()) * scale).round();
    let top = (f32::from(rect.top()) * scale).round();
    let right = (f32::from(rect.right()) * scale).round().max(left + 1.0);
    let bottom = (f32::from(rect.bottom()) * scale).round().max(top + 1.0);
    Bounds::from_corners(
        point(px(left / scale), px(top / scale)),
        point(px(right / scale), px(bottom / scale)),
    )
}

// --- Glyph atlas rendering (C1) --------------------------------------------------------------
// Render math glyphs through GPUI's glyph atlas — the same hinting/subpixel path and sprite cache
// as editor text — when the KaTeX face registers and resolves; otherwise fall back to per-paint
// outline tessellation. This stays inside the markdown crate (no `gpui`/`gpui_wgpu` changes): the
// embedded KaTeX TTFs are registered via the public `add_fonts`, and any symbol face the text
// backend rejects (no 'm' glyph) simply uses the outline fallback.

/// One embedded KaTeX face, with the family/weight/style it advertises to the font database.
struct KatexFace {
    id: ratex_font::FontId,
    family: &'static str,
    weight: FontWeight,
    style: FontStyle,
}

const KATEX_FACES: &[KatexFace] = &[
    KatexFace { id: ratex_font::FontId::AmsRegular, family: "KaTeX_AMS", weight: FontWeight::NORMAL, style: FontStyle::Normal },
    KatexFace { id: ratex_font::FontId::CaligraphicRegular, family: "KaTeX_Caligraphic", weight: FontWeight::NORMAL, style: FontStyle::Normal },
    KatexFace { id: ratex_font::FontId::FrakturRegular, family: "KaTeX_Fraktur", weight: FontWeight::NORMAL, style: FontStyle::Normal },
    KatexFace { id: ratex_font::FontId::FrakturBold, family: "KaTeX_Fraktur", weight: FontWeight::BOLD, style: FontStyle::Normal },
    KatexFace { id: ratex_font::FontId::MainBold, family: "KaTeX_Main", weight: FontWeight::BOLD, style: FontStyle::Normal },
    KatexFace { id: ratex_font::FontId::MainBoldItalic, family: "KaTeX_Main", weight: FontWeight::BOLD, style: FontStyle::Italic },
    KatexFace { id: ratex_font::FontId::MainItalic, family: "KaTeX_Main", weight: FontWeight::NORMAL, style: FontStyle::Italic },
    KatexFace { id: ratex_font::FontId::MainRegular, family: "KaTeX_Main", weight: FontWeight::NORMAL, style: FontStyle::Normal },
    KatexFace { id: ratex_font::FontId::MathBoldItalic, family: "KaTeX_Math", weight: FontWeight::BOLD, style: FontStyle::Italic },
    KatexFace { id: ratex_font::FontId::MathItalic, family: "KaTeX_Math", weight: FontWeight::NORMAL, style: FontStyle::Italic },
    KatexFace { id: ratex_font::FontId::SansSerifBold, family: "KaTeX_SansSerif", weight: FontWeight::BOLD, style: FontStyle::Normal },
    KatexFace { id: ratex_font::FontId::SansSerifItalic, family: "KaTeX_SansSerif", weight: FontWeight::NORMAL, style: FontStyle::Italic },
    KatexFace { id: ratex_font::FontId::SansSerifRegular, family: "KaTeX_SansSerif", weight: FontWeight::NORMAL, style: FontStyle::Normal },
    KatexFace { id: ratex_font::FontId::ScriptRegular, family: "KaTeX_Script", weight: FontWeight::NORMAL, style: FontStyle::Normal },
    KatexFace { id: ratex_font::FontId::Size1Regular, family: "KaTeX_Size1", weight: FontWeight::NORMAL, style: FontStyle::Normal },
    KatexFace { id: ratex_font::FontId::Size2Regular, family: "KaTeX_Size2", weight: FontWeight::NORMAL, style: FontStyle::Normal },
    KatexFace { id: ratex_font::FontId::Size3Regular, family: "KaTeX_Size3", weight: FontWeight::NORMAL, style: FontStyle::Normal },
    KatexFace { id: ratex_font::FontId::Size4Regular, family: "KaTeX_Size4", weight: FontWeight::NORMAL, style: FontStyle::Normal },
    KatexFace { id: ratex_font::FontId::TypewriterRegular, family: "KaTeX_Typewriter", weight: FontWeight::NORMAL, style: FontStyle::Normal },
];

/// Load a KaTeX face's embedded TTF bytes via the font loader (avoids an extra `ratex-katex-fonts`
/// dependency). A one-item probe routes the load through the same path the painter uses.
fn load_katex_font_bytes(font_id: ratex_font::FontId) -> Option<Vec<u8>> {
    let probe = DisplayItem::GlyphPath {
        x: 0.0,
        y: 0.0,
        scale: 1.0,
        font: font_id.as_str().to_string(),
        char_code: 0x41,
        color: Color::BLACK,
    };
    let set = ratex_font_loader::load_fonts_for_items("", std::slice::from_ref(&probe)).ok()?;
    set.get(&font_id).map(|bytes| bytes.to_vec())
}

/// Patch a KaTeX face's real style into its font bytes. KaTeX TTFs ship with no style bits set:
/// `fsSelection` is REGULAR and `head.macStyle` is zero on every face, italic/bold ones included.
/// Font databases then can't tell Main-Regular from Main-Italic, family matching degrades to
/// registration order, and rasterizing one face with another's glyph indices draws shifted glyphs
/// (`=` becomes `@`). Returns `None` if the sfnt tables can't be located (caller fail-closes atlas
/// for that face). Only OS/2 and head metadata change — cmap and glyph order are untouched.
fn apply_style_bits(bytes: &[u8], italic: bool, bold: bool) -> Option<Vec<u8>> {
    let mut data = bytes.to_vec();
    let num_tables = u16::from_be_bytes([*data.get(4)?, *data.get(5)?]) as usize;
    let mut fs_selection_at = None;
    let mut mac_style_at = None;
    for i in 0..num_tables {
        let record = 12 + i * 16;
        let tag = data.get(record..record + 4)?;
        let table_offset =
            u32::from_be_bytes(data.get(record + 8..record + 12)?.try_into().ok()?) as usize;
        match tag {
            // OS/2 `fsSelection` lives at byte 62 of the table in every version.
            b"OS/2" => fs_selection_at = Some(table_offset + 62),
            // `macStyle` lives at byte 44 of `head`.
            b"head" => mac_style_at = Some(table_offset + 44),
            _ => {}
        }
    }

    fn patch_u16(data: &mut [u8], at: usize, set: u16, clear: u16) -> Option<()> {
        let raw: [u8; 2] = data.get(at..at + 2)?.try_into().ok()?;
        let value = (u16::from_be_bytes(raw) & !clear) | set;
        data.get_mut(at..at + 2)?
            .copy_from_slice(&value.to_be_bytes());
        Some(())
    }

    // fsSelection: bit 0 = ITALIC, bit 5 = BOLD, bit 6 = REGULAR (clear it, it's exclusive).
    let fs_bits = (italic as u16) | ((bold as u16) << 5);
    patch_u16(&mut data, fs_selection_at?, fs_bits, 1 << 6)?;
    // macStyle: bit 0 = bold, bit 1 = italic.
    let mac_bits = (bold as u16) | ((italic as u16) << 1);
    patch_u16(&mut data, mac_style_at?, mac_bits, 0)?;
    Some(data)
}

/// Register the embedded KaTeX faces with GPUI's text system exactly once per process, returning
/// the set of faces that are safe to render via the atlas. A face is excluded (and left to the
/// outline fallback) if its bytes can't be loaded, or — for italic/bold faces — if its style bits
/// can't be patched: registering an unpatched styled face would let the family matcher pick a
/// sibling face and rasterize the wrong glyphs (the `=` -> `@` bug). Unstyled faces need no patch.
fn register_katex_fonts(window: &Window) -> &'static HashSet<ratex_font::FontId> {
    static REGISTERED: OnceLock<HashSet<ratex_font::FontId>> = OnceLock::new();
    REGISTERED.get_or_init(|| {
        let mut eligible = HashSet::default();
        let mut fonts: Vec<Cow<'static, [u8]>> = Vec::new();
        for face in KATEX_FACES {
            let Some(bytes) = load_katex_font_bytes(face.id) else {
                continue;
            };
            let italic = face.style == FontStyle::Italic;
            let bold = face.weight == FontWeight::BOLD;
            let prepared = if italic || bold {
                match apply_style_bits(&bytes, italic, bold) {
                    Some(patched) => Cow::Owned(patched),
                    None => {
                        log::warn!(
                            "math: could not patch style bits for {}; using outline for it",
                            face.id.as_str()
                        );
                        continue;
                    }
                }
            } else {
                Cow::Owned(bytes)
            };
            fonts.push(prepared);
            eligible.insert(face.id);
        }
        if let Err(error) = window.text_system().add_fonts(fonts) {
            log::error!("math: failed to register KaTeX fonts: {error}");
            return HashSet::default();
        }
        eligible
    })
}

/// Resolve a RaTeX font to a GPUI font id suitable for `paint_glyph`, or `None` if the face can't
/// be registered/resolved (the caller then uses the outline fallback). `resolve_font` silently
/// substitutes the default stack for a missing family, so the resolved id is verified to map back
/// to the requested KaTeX family — otherwise its glyph indices would draw the wrong font.
///
/// The returned `GpuiFontId` is an index into the *current* `TextSystem`, so it is deliberately not
/// cached across calls: a process-global cache would hand a stale index to a different `TextSystem`
/// (e.g. a second `App` in tests), pointing at the wrong loaded face. `TextSystem::resolve_font`
/// already memoizes internally, so resolving on every call is cheap.
fn resolve_katex_font(window: &Window, font_id: ratex_font::FontId) -> Option<GpuiFontId> {
    let face = KATEX_FACES.iter().find(|face| face.id == font_id)?;
    if !register_katex_fonts(window).contains(&font_id) {
        return None;
    }
    let mut requested: Font = font(face.family);
    requested.weight = face.weight;
    requested.style = face.style;
    let text_system = window.text_system();
    let candidate = text_system.resolve_font(&requested);
    let family_matches = text_system
        .get_font_for_id(candidate)
        .is_some_and(|resolved| resolved.family.as_ref() == face.family);
    family_matches.then_some(candidate)
}

#[allow(clippy::too_many_arguments)]
fn paint_glyph_outline(
    window: &mut Window,
    fonts: &ratex_font_loader::FontSet,
    font: &str,
    char_code: u32,
    x: f64,
    y: f64,
    scale: f64,
    origin: Point<Pixels>,
    em: f32,
    color: Hsla,
) {
    let Some(font_id) = ratex_font::FontId::parse(font) else {
        return;
    };
    let Some(bytes) = fonts.get(&font_id) else {
        return;
    };
    let Ok(font_ref) = FontRef::try_from_slice(bytes) else {
        return;
    };
    // Map the display list's Unicode scalar to the character the KaTeX `.ttf` actually keys its
    // outline by. Mathematical-alphanumeric code points (e.g. bold/italic letters) are stored in
    // the shipped fonts under ASCII letters/digits, so a plain `char::from_u32` would miss them.
    let ch = ratex_font::katex_ttf_glyph_char(font_id, char_code);
    let glyph_id = font_ref.glyph_id(ch);

    // Atlas-first: paint through GPUI's glyph atlas (the shared sprite cache + hinting/subpixel
    // path as editor text) when the KaTeX face resolves. `paint_glyph` takes the baseline pen
    // position and a font size in pixels (not the font-units scale used for outlines). The
    // ab_glyph cmap index matches GPUI's because both read the same embedded TTF. On any failure
    // we fall through to outline tessellation below, so rendering degrades gracefully.
    if glyph_id.0 != 0
        && let Some(gpui_font_id) = resolve_katex_font(window, font_id)
    {
        let baseline = point(
            px(f32::from(origin.x) + x as f32 * em),
            px(f32::from(origin.y) + y as f32 * em),
        );
        match window.paint_glyph(
            baseline,
            gpui_font_id,
            GlyphId(u32::from(glyph_id.0)),
            px(scale as f32 * em),
            color,
        ) {
            Ok(()) => return,
            Err(error) => {
                log::warn!("math: atlas glyph paint failed, using outline fallback: {error}");
            }
        }
    }

    let Some(curves) =
        ratex_font_loader::outline_cache::get_or_compute_outline(font_id, &font_ref, glyph_id)
    else {
        return;
    };
    let units_per_em = font_ref.units_per_em().unwrap_or(1000.0);
    // Font units -> pixels. Glyph outlines are y-up from the baseline.
    let glyph_scale = scale as f32 * em / units_per_em;
    let baseline_x = f32::from(origin.x) + x as f32 * em;
    let baseline_y = f32::from(origin.y) + y as f32 * em;
    let map = |p: AbPoint| {
        point(
            px(baseline_x + p.x * glyph_scale),
            px(baseline_y - p.y * glyph_scale),
        )
    };

    let mut builder = PathBuilder::fill();
    let mut last_end: Option<AbPoint> = None;
    for curve in curves.iter() {
        match curve {
            OutlineCurve::Line(a, b) => {
                if last_end != Some(*a) {
                    builder.move_to(map(*a));
                }
                builder.line_to(map(*b));
                last_end = Some(*b);
            }
            OutlineCurve::Quad(a, ctrl, b) => {
                if last_end != Some(*a) {
                    builder.move_to(map(*a));
                }
                builder.curve_to(map(*b), map(*ctrl));
                last_end = Some(*b);
            }
            OutlineCurve::Cubic(a, c1, c2, b) => {
                if last_end != Some(*a) {
                    builder.move_to(map(*a));
                }
                builder.cubic_bezier_to(map(*b), map(*c1), map(*c2));
                last_end = Some(*b);
            }
        }
    }
    if let Ok(path) = builder.build() {
        window.paint_path(path, color);
    }
}

#[allow(clippy::too_many_arguments)]
fn paint_vector_path(
    window: &mut Window,
    commands: &[PathCommand],
    filled: bool,
    x: f64,
    y: f64,
    origin: Point<Pixels>,
    em: f32,
    color: Hsla,
) {
    let ox = f32::from(origin.x) + x as f32 * em;
    let oy = f32::from(origin.y) + y as f32 * em;
    if filled {
        // Match RaTeX: fill each subpath (split at `MoveTo`) independently so contours don't
        // interact through the fill rule.
        let mut start = 0;
        for i in 1..commands.len() {
            if matches!(commands[i], PathCommand::MoveTo { .. }) {
                paint_path_segment(window, &commands[start..i], true, ox, oy, em, color);
                start = i;
            }
        }
        paint_path_segment(window, &commands[start..], true, ox, oy, em, color);
    } else {
        // Strokes are emitted as a single open path.
        paint_path_segment(window, commands, false, ox, oy, em, color);
    }
}

fn paint_path_segment(
    window: &mut Window,
    commands: &[PathCommand],
    filled: bool,
    ox: f32,
    oy: f32,
    em: f32,
    color: Hsla,
) {
    if commands.is_empty() {
        return;
    }
    let map = |cx_: f64, cy_: f64| point(px(ox + cx_ as f32 * em), px(oy + cy_ as f32 * em));
    let mut builder = if filled {
        PathBuilder::fill()
    } else {
        PathBuilder::stroke(px((0.05 * em).max(1.0)))
    };
    for command in commands {
        match command {
            PathCommand::MoveTo { x, y } => builder.move_to(map(*x, *y)),
            PathCommand::LineTo { x, y } => builder.line_to(map(*x, *y)),
            PathCommand::QuadTo { x1, y1, x, y } => builder.curve_to(map(*x, *y), map(*x1, *y1)),
            PathCommand::CubicTo {
                x1,
                y1,
                x2,
                y2,
                x,
                y,
            } => builder.cubic_bezier_to(map(*x, *y), map(*x1, *y1), map(*x2, *y2)),
            PathCommand::Close => builder.close(),
        }
    }
    if let Ok(path) = builder.build() {
        window.paint_path(path, color);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Markdown, MarkdownOptions, ParsedMarkdown};
    use gpui::{AppContext, Context, IntoElement, Render, TestAppContext, Window};

    /// Build a [`ParsedMarkdown`] carrying only the events (all that [`MathState::update`] reads)
    /// with LaTeX math parsing enabled, as the preview does.
    fn parsed_with_math(text: &str) -> ParsedMarkdown {
        let data = crate::parser::parse_markdown_inner(text, false, false, false, true);
        ParsedMarkdown {
            events: data.events.into(),
            ..Default::default()
        }
    }

    /// Run `body` with a `Context<Markdown>` so cache tests can drive [`MathState::update`], which
    /// takes one for parity with the async mermaid path (our math parse is synchronous).
    fn with_markdown_cx(cx: &mut TestAppContext, body: impl FnOnce(&mut MathState, &mut Context<Markdown>)) {
        struct TestWindow;
        impl Render for TestWindow {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                gpui::Empty
            }
        }
        let (_, cx) = cx.add_window_view(|_, _| TestWindow);
        let markdown = cx.new(|cx| {
            Markdown::new_with_options("".into(), None, None, MarkdownOptions::default(), cx)
        });
        markdown.update(cx, |_, cx| {
            let mut state = MathState::default();
            body(&mut state, cx);
        });
    }

    #[test]
    fn test_parse_to_display_list_valid() {
        assert!(parse_to_display_list("x^2", false).is_ok());
        assert!(parse_to_display_list("\\frac{a}{b}", true).is_ok());
        let list = parse_to_display_list("x^2 + y^2", true).expect("valid display math parses");
        assert!(
            !list.items.is_empty(),
            "a non-trivial formula produces display items"
        );
    }

    #[test]
    fn test_parse_to_display_list_empty_is_error() {
        assert!(parse_to_display_list("", false).is_err());
        assert!(parse_to_display_list("   ", true).is_err());
    }

    #[test]
    fn test_split_inline_math_splits_on_top_level_operators() {
        assert_eq!(split_inline_math("x"), vec!["x".to_string()]);
        // A relation/operator starts the next fragment and is kept with it.
        assert_eq!(
            split_inline_math("a+b"),
            vec!["a".to_string(), "+b".to_string()]
        );
        // Braces protect their contents from being split.
        assert_eq!(split_inline_math("{a+b}"), vec!["{a+b}".to_string()]);
        // Splitting is lossless: fragments always reconstruct the original source.
        assert_eq!(split_inline_math("a+b=c").concat(), "a+b=c");
    }

    #[gpui::test]
    fn test_math_cache_populates_valid_and_errors(cx: &mut TestAppContext) {
        with_markdown_cx(cx, |state, cx| {
            state.update(&parsed_with_math("$x^2$ and $$\\frac{a}{b}$$"), cx);
            assert!(
                matches!(state.result("x^2", false), Some(Ok(_))),
                "inline formula cached as Ok"
            );
            assert!(
                matches!(state.result("\\frac{a}{b}", true), Some(Ok(_))),
                "display formula cached as Ok"
            );
            // A formula never seen is not cached.
            assert!(state.result("z^3", false).is_none());
        });
    }

    #[gpui::test]
    fn test_math_cache_is_reused_across_identical_updates(cx: &mut TestAppContext) {
        with_markdown_cx(cx, |state, cx| {
            let parsed = parsed_with_math("$\\alpha$ and $$\\gamma$$");
            state.update(&parsed, cx);
            let len_after_first = state.cache.len();
            assert_eq!(len_after_first, 2, "one inline + one display formula cached");
            state.update(&parsed, cx);
            assert_eq!(
                state.cache.len(),
                len_after_first,
                "re-parsing identical content adds no cache entries"
            );
        });
    }

    #[gpui::test]
    fn test_math_cache_retain_removes_edited_and_keeps_unrelated(cx: &mut TestAppContext) {
        with_markdown_cx(cx, |state, cx| {
            state.update(&parsed_with_math("$\\alpha$ then $$\\gamma$$"), cx);
            assert!(state.result("\\alpha", false).is_some());
            assert!(state.result("\\gamma", true).is_some());

            // Edit only the inline formula; the display formula is untouched.
            state.update(&parsed_with_math("$\\beta$ then $$\\gamma$$"), cx);
            assert!(
                state.result("\\alpha", false).is_none(),
                "stale inline formula is dropped after an edit"
            );
            assert!(
                state.result("\\beta", false).is_some(),
                "edited inline formula is cached"
            );
            assert!(
                state.result("\\gamma", true).is_some(),
                "unrelated display formula is retained across the edit"
            );
        });
    }
}
