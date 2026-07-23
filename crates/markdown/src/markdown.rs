pub mod html;
mod math;
mod mermaid;
pub mod parser;
mod path_range;
mod selection;

use base64::Engine as _;
use futures::FutureExt as _;
use gpui::EdgesRefinement;
use gpui::HitboxBehavior;
use gpui::UnderlineStyle;
use language::LanguageName;

use log::Level;
use math::{
    MathBlock, MathBoundsSlot, MathElement, MathInline, MathState, inline_math_box_height,
    inline_text_baseline, render_math_block, render_math_inline,
};
use mermaid::{
    MermaidState, ParsedMarkdownMermaidDiagram, extract_mermaid_diagrams, render_mermaid_diagram,
};
pub use path_range::{LineCol, PathWithRange};
use settings::Settings as _;
use theme_settings::ThemeSettings;
use util::maybe;

use std::borrow::Cow;
use std::cell::Cell;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::mem;
use std::ops::Range;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use collections::{HashMap, HashSet};
use gpui::{
    AnyElement, App, BorderStyle, Bounds, ClipboardItem, CursorStyle, DispatchPhase, Edges, Entity,
    FocusHandle, Focusable, FontStyle, FontWeight, GlobalElementId, Hitbox, Hsla, Image,
    ImageFormat, ImageSource, InspectorElementId, KeyContext, LayoutId, Length, LineFragment,
    MouseButton, MouseDownEvent, MouseEvent, MouseMoveEvent, MouseUpEvent, Point, ScrollHandle,
    ShapedLine, Size, Stateful, StrikethroughStyle, StyleRefinement, StyledImage, StyledText,
    Subscription, Task, TextAlign, TextLayout, TextRun, TextStyle, TextStyleRefinement, actions,
    canvas, img, point, quad,
};
use language::{CharClassifier, Language, LanguageRegistry, Rope};
use parser::CodeBlockMetadata;
use parser::{
    MarkdownEvent, MarkdownTag, MarkdownTagEnd, ParsedMetadataBlock, parse_links_only,
    parse_markdown_inner,
};
use pulldown_cmark::{Alignment, BlockQuoteKind};
use sum_tree::TreeMap;
use theme::SyntaxTheme;
use ui::{Checkbox, CopyButton, ScrollAxes, Scrollbars, Tooltip, WithScrollbar, prelude::*};
use util::ResultExt;

use crate::parser::CodeBlockKind;

/// A callback function that can be used to customize the style of links based on the destination URL.
/// If the callback returns `None`, the default link style will be used.
type LinkStyleCallback = Rc<dyn Fn(&str, &App) -> Option<TextStyleRefinement>>;
pub type CodeSpanLinkCallback = Arc<dyn Fn(&str, &App) -> Option<SharedString> + 'static>;
type UrlHoverCallback = Rc<dyn Fn(Option<SharedString>, &mut Window, &mut App)>;
type SourceClickCallback = Box<dyn Fn(usize, usize, &mut Window, &mut App) -> bool>;
type CheckboxToggleCallback = Rc<dyn Fn(Range<usize>, bool, &mut Window, &mut App)>;

#[derive(Clone, Copy, Default)]
pub struct BlockQuoteKindColors {
    pub note: Hsla,
    pub tip: Hsla,
    pub important: Hsla,
    pub warning: Hsla,
    pub caution: Hsla,
}

impl BlockQuoteKindColors {
    fn for_kind(&self, kind: Option<BlockQuoteKind>, default: Hsla) -> Hsla {
        match kind {
            Some(BlockQuoteKind::Note) => self.note,
            Some(BlockQuoteKind::Tip) => self.tip,
            Some(BlockQuoteKind::Important) => self.important,
            Some(BlockQuoteKind::Warning) => self.warning,
            Some(BlockQuoteKind::Caution) => self.caution,
            None => default,
        }
    }
}

#[derive(Clone, Default)]
pub struct HeadingLevelStyles {
    pub h1: Option<TextStyleRefinement>,
    pub h2: Option<TextStyleRefinement>,
    pub h3: Option<TextStyleRefinement>,
    pub h4: Option<TextStyleRefinement>,
    pub h5: Option<TextStyleRefinement>,
    pub h6: Option<TextStyleRefinement>,
}

#[derive(Clone)]
pub struct MarkdownStyle {
    pub base_text_style: TextStyle,
    pub container_style: StyleRefinement,
    pub code_block: StyleRefinement,
    pub code_block_overflow_x_scroll: bool,
    pub inline_code: TextStyleRefinement,
    pub block_quote: TextStyleRefinement,
    pub link: TextStyleRefinement,
    pub link_callback: Option<LinkStyleCallback>,
    pub rule_color: Hsla,
    pub block_quote_border_color: Hsla,
    pub block_quote_kind_colors: BlockQuoteKindColors,
    pub syntax: Arc<SyntaxTheme>,
    pub selection_background_color: Hsla,
    pub heading: StyleRefinement,
    pub heading_level_styles: Option<HeadingLevelStyles>,
    pub heading_border_color: Option<Hsla>,
    pub height_is_multiple_of_line_height: bool,
    pub prevent_mouse_interaction: bool,
    pub table_columns_min_size: bool,
    pub soft_break_as_hard_break: bool,
}

impl Default for MarkdownStyle {
    fn default() -> Self {
        Self {
            base_text_style: Default::default(),
            container_style: Default::default(),
            code_block: Default::default(),
            code_block_overflow_x_scroll: false,
            inline_code: Default::default(),
            block_quote: Default::default(),
            link: Default::default(),
            link_callback: None,
            rule_color: Default::default(),
            block_quote_border_color: Default::default(),
            block_quote_kind_colors: Default::default(),
            syntax: Arc::new(SyntaxTheme::default()),
            selection_background_color: Default::default(),
            heading: Default::default(),
            heading_level_styles: None,
            heading_border_color: None,
            height_is_multiple_of_line_height: false,
            prevent_mouse_interaction: false,
            table_columns_min_size: false,
            soft_break_as_hard_break: false,
        }
    }
}

#[derive(Clone, Copy)]
pub enum MarkdownFont {
    Agent,
    Editor,
    Preview,
}

impl MarkdownStyle {
    pub fn themed(font: MarkdownFont, window: &Window, cx: &App) -> Self {
        let colors = cx.theme().colors();
        let syntax = cx.theme().syntax().clone();
        Self::themed_with_overrides(font, colors, &syntax, window, cx)
    }

    /// Like [`Self::themed`], but takes explicit [`ThemeColors`] and
    /// [`SyntaxTheme`] so callers (e.g. the markdown preview) can render the
    /// markdown using a theme other than the active editor theme.
    pub fn themed_with_overrides(
        font: MarkdownFont,
        colors: &theme::ThemeColors,
        syntax: &Arc<SyntaxTheme>,
        window: &Window,
        cx: &App,
    ) -> Self {
        let theme_settings = ThemeSettings::get_global(cx);
        let is_preview = matches!(font, MarkdownFont::Preview);

        let buffer_font_weight = theme_settings.buffer_font.weight;
        let (buffer_font_size, ui_font_size) = match font {
            MarkdownFont::Agent => (
                theme_settings.agent_buffer_font_size(cx),
                theme_settings.agent_ui_font_size(cx),
            ),
            MarkdownFont::Editor => (
                theme_settings.buffer_font_size(cx),
                theme_settings.ui_font_size(cx),
            ),
            MarkdownFont::Preview => (
                theme_settings.markdown_preview_font_size(cx),
                theme_settings.ui_font_size(cx),
            ),
        };

        let body_font_family = if is_preview {
            theme_settings.markdown_preview_font_family().clone()
        } else {
            theme_settings.ui_font.family.clone()
        };
        let code_font_family = if is_preview {
            theme_settings.markdown_preview_code_font_family().clone()
        } else {
            theme_settings.buffer_font.family.clone()
        };

        let mut text_style = window.text_style();
        let line_height = buffer_font_size * 1.75;

        text_style.refine(&TextStyleRefinement {
            font_family: Some(body_font_family),
            font_fallbacks: theme_settings.ui_font.fallbacks.clone(),
            font_features: Some(theme_settings.ui_font.features.clone()),
            font_size: Some(if is_preview {
                rems(1.0).into()
            } else {
                ui_font_size.into()
            }),
            line_height: Some(line_height.into()),
            color: Some(colors.text),
            ..Default::default()
        });

        let style = MarkdownStyle {
            base_text_style: text_style.clone(),
            syntax: syntax.clone(),
            selection_background_color: colors.element_selection_background,
            rule_color: colors.border,
            block_quote_border_color: colors.border,
            block_quote_kind_colors: {
                let status = cx.theme().status();
                BlockQuoteKindColors {
                    note: status.info,
                    tip: status.success,
                    important: status.info,
                    warning: status.warning,
                    caution: status.error,
                }
            },
            code_block_overflow_x_scroll: true,
            code_block: StyleRefinement {
                padding: EdgesRefinement {
                    top: Some(DefiniteLength::Absolute(AbsoluteLength::Pixels(px(8.)))),
                    left: Some(DefiniteLength::Absolute(AbsoluteLength::Pixels(px(8.)))),
                    right: Some(DefiniteLength::Absolute(AbsoluteLength::Pixels(px(8.)))),
                    bottom: Some(DefiniteLength::Absolute(AbsoluteLength::Pixels(px(8.)))),
                },
                margin: EdgesRefinement {
                    top: Some(Length::Definite(px(8.).into())),
                    left: Some(Length::Definite(px(0.).into())),
                    right: Some(Length::Definite(px(0.).into())),
                    bottom: Some(Length::Definite(px(12.).into())),
                },
                border_style: Some(BorderStyle::Solid),
                border_widths: EdgesRefinement {
                    top: Some(AbsoluteLength::Pixels(px(1.))),
                    left: Some(AbsoluteLength::Pixels(px(1.))),
                    right: Some(AbsoluteLength::Pixels(px(1.))),
                    bottom: Some(AbsoluteLength::Pixels(px(1.))),
                },
                border_color: Some(colors.border_variant),
                background: Some(colors.editor_background.into()),
                text: TextStyleRefinement {
                    font_family: Some(code_font_family.clone()),
                    font_fallbacks: theme_settings.buffer_font.fallbacks.clone(),
                    font_features: Some(theme_settings.buffer_font.features.clone()),
                    font_size: Some(buffer_font_size.into()),
                    font_weight: Some(buffer_font_weight),
                    ..Default::default()
                },
                ..Default::default()
            },
            inline_code: TextStyleRefinement {
                font_family: Some(code_font_family),
                font_fallbacks: theme_settings.buffer_font.fallbacks.clone(),
                font_features: Some(theme_settings.buffer_font.features.clone()),
                font_size: Some(buffer_font_size.into()),
                font_weight: Some(buffer_font_weight),
                background_color: Some(colors.editor_foreground.opacity(0.08)),
                ..Default::default()
            },
            link: TextStyleRefinement {
                background_color: Some(colors.editor_foreground.opacity(0.025)),
                color: Some(colors.text_accent),
                underline: Some(UnderlineStyle {
                    color: Some(colors.text_accent.opacity(0.5)),
                    thickness: px(1.),
                    ..Default::default()
                }),
                ..Default::default()
            },
            soft_break_as_hard_break: matches!(font, MarkdownFont::Agent),
            heading_level_styles: matches!(font, MarkdownFont::Agent).then_some(
                HeadingLevelStyles {
                    h1: Some(TextStyleRefinement {
                        font_size: Some(rems(1.15).into()),
                        ..Default::default()
                    }),
                    h2: Some(TextStyleRefinement {
                        font_size: Some(rems(1.1).into()),
                        ..Default::default()
                    }),
                    h3: Some(TextStyleRefinement {
                        font_size: Some(rems(1.05).into()),
                        ..Default::default()
                    }),
                    h4: Some(TextStyleRefinement {
                        font_size: Some(rems(1.).into()),
                        ..Default::default()
                    }),
                    h5: Some(TextStyleRefinement {
                        font_size: Some(rems(0.95).into()),
                        ..Default::default()
                    }),
                    h6: Some(TextStyleRefinement {
                        font_size: Some(rems(0.875).into()),
                        ..Default::default()
                    }),
                },
            ),
            ..Default::default()
        };

        if is_preview {
            style.with_preview_overrides(colors)
        } else {
            style
        }
    }

    fn with_preview_overrides(mut self, colors: &theme::ThemeColors) -> Self {
        let body_font_size = rems(0.92);
        self.base_text_style.font_size = body_font_size.into();
        self.container_style.text.font_size = Some(body_font_size.into());

        self.base_text_style.color = colors.text_muted.blend(colors.text.opacity(0.25));
        self.inline_code.color = Some(colors.text);
        self.heading.text.color = Some(colors.text);

        self.heading_level_styles = Some(HeadingLevelStyles {
            h1: Some(TextStyleRefinement {
                font_size: Some(rems(1.45).into()),
                ..Default::default()
            }),
            h2: Some(TextStyleRefinement {
                font_size: Some(rems(1.3).into()),
                ..Default::default()
            }),
            h3: Some(TextStyleRefinement {
                font_size: Some(rems(1.1).into()),
                ..Default::default()
            }),
            h4: Some(TextStyleRefinement {
                font_size: Some(rems(1.01).into()),
                ..Default::default()
            }),
            h5: Some(TextStyleRefinement {
                font_size: Some(rems(0.95).into()),
                ..Default::default()
            }),
            h6: Some(TextStyleRefinement {
                font_size: Some(rems(0.85).into()),
                ..Default::default()
            }),
        });

        self.heading_border_color = Some(colors.border_variant);

        self
    }

    pub fn with_buffer_font(mut self, cx: &App) -> Self {
        let theme_settings = ThemeSettings::get_global(cx);
        self.base_text_style.font_family = theme_settings.buffer_font.family.clone();
        self.base_text_style.font_fallbacks = theme_settings.buffer_font.fallbacks.clone();
        self.base_text_style.font_features = theme_settings.buffer_font.features.clone();
        self.base_text_style.font_weight = theme_settings.buffer_font.weight;
        self
    }

    pub fn with_muted_text(mut self, cx: &App) -> Self {
        let colors = cx.theme().colors();
        self.base_text_style.color = colors.text_muted;
        self
    }
}

pub struct Markdown {
    source: SharedString,
    selection: Selection,
    pressed_link: Option<RenderedLink>,
    pressed_footnote_ref: Option<RenderedFootnoteRef>,
    autoscroll_request: Option<usize>,
    active_root_block: Option<usize>,
    parsed_markdown: ParsedMarkdown,
    images_by_source_offset: HashMap<usize, Arc<Image>>,
    should_reparse: bool,
    pending_parse: Option<Task<()>>,
    focus_handle: FocusHandle,
    language_registry: Option<Arc<LanguageRegistry>>,
    fallback_code_block_language: Option<LanguageName>,
    options: MarkdownOptions,
    mermaid_state: MermaidState,
    math_state: MathState,
    _mermaid_theme_subscription: Option<Subscription>,
    mermaid_showing_code: HashSet<usize>,
    copied_code_blocks: HashSet<ElementId>,
    wrapped_code_blocks: HashSet<usize>,
    code_block_scroll_handles: BTreeMap<usize, ScrollHandle>,
    context_menu_link: Option<SharedString>,
    context_menu_selected_text: Option<SharedString>,
    context_menu_selected_markdown: Option<SharedString>,
    search_highlights: Vec<Range<usize>>,
    active_search_highlight: Option<usize>,
}

#[derive(Clone, Copy, Default)]
pub struct MarkdownOptions {
    pub parse_links_only: bool,
    pub parse_html: bool,
    pub render_mermaid_diagrams: bool,
    pub render_math: bool,
    pub parse_heading_slugs: bool,
    pub render_metadata_blocks: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CopyButtonVisibility {
    Hidden,
    AlwaysVisible,
    VisibleOnHover,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WrapButtonVisibility {
    Hidden,
    AlwaysVisible,
    VisibleOnHover,
}

pub enum CodeBlockRenderer {
    Default {
        copy_button_visibility: CopyButtonVisibility,
        wrap_button_visibility: WrapButtonVisibility,
        border: bool,
    },
    Custom {
        render: CodeBlockRenderFn,
        /// A function that can modify the parent container after the code block
        /// content has been appended as a child element.
        transform: Option<CodeBlockTransformFn>,
    },
}

pub type CodeBlockRenderFn = Arc<
    dyn Fn(
        &CodeBlockKind,
        &ParsedMarkdown,
        Range<usize>,
        CodeBlockMetadata,
        &mut Window,
        &App,
    ) -> Div,
>;

pub type CodeBlockTransformFn =
    Arc<dyn Fn(AnyDiv, Range<usize>, CodeBlockMetadata, &mut Window, &App) -> AnyDiv>;

actions!(
    markdown,
    [
        /// Copies the selected text to the clipboard.
        Copy,
        /// Copies the selected text as markdown to the clipboard.
        CopyAsMarkdown
    ]
);

enum EscapeAction {
    PassThrough,
    Nbsp(usize),
    DoubleNewline,
    PrefixBackslash,
}

impl EscapeAction {
    fn output_len(&self, c: char) -> usize {
        match self {
            Self::PassThrough => c.len_utf8(),
            Self::Nbsp(count) => count * '\u{00A0}'.len_utf8(),
            Self::DoubleNewline => 2,
            Self::PrefixBackslash => '\\'.len_utf8() + c.len_utf8(),
        }
    }

    fn write_to(&self, c: char, output: &mut String) {
        match self {
            Self::PassThrough => output.push(c),
            Self::Nbsp(count) => {
                for _ in 0..*count {
                    output.push('\u{00A0}');
                }
            }
            Self::DoubleNewline => {
                output.push('\n');
                output.push('\n');
            }
            Self::PrefixBackslash => {
                // '\\' is a single backslash in Rust, e.g. '|' -> '\|'
                output.push('\\');
                output.push(c);
            }
        }
    }
}

struct MarkdownEscaper {
    in_leading_whitespace: bool,
}

impl MarkdownEscaper {
    const TAB_SIZE: usize = 4;

    fn new() -> Self {
        Self {
            in_leading_whitespace: true,
        }
    }

    fn next(&mut self, c: char) -> EscapeAction {
        let action = if self.in_leading_whitespace && c == '\t' {
            EscapeAction::Nbsp(Self::TAB_SIZE)
        } else if self.in_leading_whitespace && c == ' ' {
            EscapeAction::Nbsp(1)
        } else if c == '\n' {
            EscapeAction::DoubleNewline
        } else if c.is_ascii_punctuation() {
            EscapeAction::PrefixBackslash
        } else {
            EscapeAction::PassThrough
        };

        self.in_leading_whitespace =
            c == '\n' || (self.in_leading_whitespace && (c == ' ' || c == '\t'));
        action
    }
}

impl Markdown {
    pub fn new(
        source: SharedString,
        language_registry: Option<Arc<LanguageRegistry>>,
        fallback_code_block_language: Option<LanguageName>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self::new_with_options(
            source,
            language_registry,
            fallback_code_block_language,
            MarkdownOptions::default(),
            cx,
        )
    }

    pub fn new_with_options(
        source: SharedString,
        language_registry: Option<Arc<LanguageRegistry>>,
        fallback_code_block_language: Option<LanguageName>,
        options: MarkdownOptions,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();

        let theme_subscription = if options.render_mermaid_diagrams || options.render_math {
            Some(
                cx.observe_global::<theme::GlobalTheme>(|this: &mut Self, cx| {
                    this.invalidate_mermaid_cache(cx);
                    this.invalidate_math_cache(cx);
                }),
            )
        } else {
            None
        };
        let mut this = Self {
            source,
            selection: Selection::default(),
            pressed_link: None,
            pressed_footnote_ref: None,
            autoscroll_request: None,
            active_root_block: None,
            should_reparse: false,
            images_by_source_offset: Default::default(),
            parsed_markdown: ParsedMarkdown::default(),
            pending_parse: None,
            focus_handle,
            language_registry,
            fallback_code_block_language,
            options,
            mermaid_state: MermaidState::default(),
            math_state: MathState::default(),
            _mermaid_theme_subscription: theme_subscription,
            mermaid_showing_code: HashSet::default(),
            copied_code_blocks: HashSet::default(),
            wrapped_code_blocks: HashSet::default(),
            code_block_scroll_handles: BTreeMap::default(),
            context_menu_link: None,
            context_menu_selected_text: None,
            context_menu_selected_markdown: None,
            search_highlights: Vec::new(),
            active_search_highlight: None,
        };
        this.parse(cx);
        this
    }

    pub fn new_text(source: SharedString, cx: &mut Context<Self>) -> Self {
        Self::new_with_options(
            source,
            None,
            None,
            MarkdownOptions {
                parse_links_only: true,
                ..Default::default()
            },
            cx,
        )
    }

    fn is_code_block_wrapped(&self, id: usize) -> bool {
        self.wrapped_code_blocks.contains(&id)
    }

    fn toggle_code_block_wrap(&mut self, id: usize) {
        if !self.wrapped_code_blocks.remove(&id) {
            self.wrapped_code_blocks.insert(id);
        }
    }

    fn code_block_scroll_handle(&mut self, id: usize) -> Option<ScrollHandle> {
        (!self.is_code_block_wrapped(id)).then(|| {
            self.code_block_scroll_handles
                .entry(id)
                .or_insert_with(ScrollHandle::new)
                .clone()
        })
    }

    fn retain_code_block_scroll_handles(&mut self, ids: &HashSet<usize>) {
        self.code_block_scroll_handles
            .retain(|id, _| ids.contains(id));
    }

    pub fn invalidate_mermaid_cache(&mut self, cx: &mut Context<Self>) {
        if !self.options.render_mermaid_diagrams || self.parsed_markdown.mermaid_diagrams.is_empty()
        {
            return;
        }

        self.mermaid_state.clear();
        self.mermaid_state.update(&self.parsed_markdown, cx);
        cx.notify();
    }

    pub fn invalidate_math_cache(&mut self, cx: &mut Context<Self>) {
        if !self.options.render_math {
            return;
        }
        // The parsed math `DisplayList` is theme-independent — color is resolved from the theme at
        // paint time (see `resolve_math_color`) — so a theme change only needs a repaint, not a
        // re-parse. Mermaid differs (it bakes color into a rasterized image) and is invalidated
        // separately.
        cx.notify();
    }

    pub(crate) fn is_mermaid_showing_code(&self, source_offset: usize) -> bool {
        self.mermaid_showing_code.contains(&source_offset)
    }

    pub(crate) fn toggle_mermaid_tab(&mut self, source_offset: usize) {
        if !self.mermaid_showing_code.remove(&source_offset) {
            self.mermaid_showing_code.insert(source_offset);
        }
    }

    fn clear_code_block_scroll_handles(&mut self) {
        self.code_block_scroll_handles.clear();
    }

    fn autoscroll_code_block(&self, source_index: usize, cursor_position: Point<Pixels>) {
        let Some((_, scroll_handle)) = self
            .code_block_scroll_handles
            .range(..=source_index)
            .next_back()
        else {
            return;
        };

        let bounds = scroll_handle.bounds();
        if cursor_position.y < bounds.top() || cursor_position.y > bounds.bottom() {
            return;
        }

        let horizontal_delta = if cursor_position.x < bounds.left() {
            bounds.left() - cursor_position.x
        } else if cursor_position.x > bounds.right() {
            bounds.right() - cursor_position.x
        } else {
            return;
        };

        let offset = scroll_handle.offset();
        scroll_handle.set_offset(point(offset.x + horizontal_delta, offset.y));
    }

    pub fn is_parsing(&self) -> bool {
        self.pending_parse.is_some()
    }

    pub fn scroll_to_heading(&mut self, slug: &str, cx: &mut Context<Self>) -> Option<usize> {
        if let Some(source_index) = self.parsed_markdown.heading_slugs.get(slug).copied() {
            self.autoscroll_request = Some(source_index);
            cx.notify();
            Some(source_index)
        } else {
            None
        }
    }

    pub fn source(&self) -> &SharedString {
        &self.source
    }

    pub fn first_code_block_language(&self) -> Option<Arc<Language>> {
        self.parsed_markdown.events.iter().find_map(|(_, event)| {
            let MarkdownEvent::Start(MarkdownTag::CodeBlock { kind, .. }) = event else {
                return None;
            };

            match kind {
                CodeBlockKind::FencedLang(language) => self
                    .parsed_markdown
                    .languages_by_name
                    .get(language)
                    .cloned(),
                CodeBlockKind::FencedSrc(path_range) => self
                    .parsed_markdown
                    .languages_by_path
                    .get(&path_range.path)
                    .cloned(),
                CodeBlockKind::Fenced | CodeBlockKind::Indented => None,
            }
        })
    }

    pub fn append(&mut self, text: &str, cx: &mut Context<Self>) {
        self.source = SharedString::new(self.source.to_string() + text);
        self.parse(cx);
    }

    pub fn replace(&mut self, source: impl Into<SharedString>, cx: &mut Context<Self>) {
        self.source = source.into();
        self.parse(cx);
    }

    pub fn request_autoscroll_to_source_index(
        &mut self,
        source_index: usize,
        cx: &mut Context<Self>,
    ) {
        self.autoscroll_request = Some(source_index);
        cx.refresh_windows();
    }

    fn footnote_definition_content_start(&self, label: &SharedString) -> Option<usize> {
        self.parsed_markdown
            .footnote_definitions
            .get(label)
            .copied()
    }

    pub fn set_active_root_for_source_index(
        &mut self,
        source_index: Option<usize>,
        cx: &mut Context<Self>,
    ) {
        let active_root_block =
            source_index.and_then(|index| self.parsed_markdown.root_block_for_source_index(index));
        if self.active_root_block == active_root_block {
            return;
        }

        self.active_root_block = active_root_block;
        cx.notify();
    }

    pub fn reset(&mut self, source: SharedString, cx: &mut Context<Self>) {
        if &source == self.source() {
            return;
        }
        self.source = source;
        self.selection = Selection::default();
        self.autoscroll_request = None;
        self.pending_parse = None;
        self.should_reparse = false;
        self.search_highlights.clear();
        self.active_search_highlight = None;
        // Don't clear parsed_markdown here - keep existing content visible until new parse completes
        self.parse(cx);
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn parsed_markdown(&self) -> &ParsedMarkdown {
        &self.parsed_markdown
    }

    pub fn escape(s: &str) -> Cow<'_, str> {
        let output_len: usize = {
            let mut escaper = MarkdownEscaper::new();
            s.chars().map(|c| escaper.next(c).output_len(c)).sum()
        };

        if output_len == s.len() {
            return s.into();
        }

        let mut escaper = MarkdownEscaper::new();
        let mut output = String::with_capacity(output_len);
        for c in s.chars() {
            escaper.next(c).write_to(c, &mut output);
        }
        output.into()
    }

    pub fn has_selection(&self) -> bool {
        self.selection.end > self.selection.start
    }

    pub fn selected_source(&self) -> Option<&str> {
        if self.selection.end <= self.selection.start {
            return None;
        }
        self.source.get(self.selection.start..self.selection.end)
    }

    pub fn set_search_highlights(
        &mut self,
        highlights: Vec<Range<usize>>,
        active: Option<usize>,
        cx: &mut Context<Self>,
    ) {
        debug_assert!(
            highlights
                .windows(2)
                .all(|ranges| (ranges[0].start, ranges[0].end) <= (ranges[1].start, ranges[1].end))
        );
        self.search_highlights = highlights;
        self.active_search_highlight =
            active.filter(|active| *active < self.search_highlights.len());
        cx.notify();
    }

    pub fn clear_search_highlights(&mut self, cx: &mut Context<Self>) {
        if !self.search_highlights.is_empty() || self.active_search_highlight.is_some() {
            self.search_highlights.clear();
            self.active_search_highlight = None;
            cx.notify();
        }
    }

    pub fn set_active_search_highlight(&mut self, active: Option<usize>, cx: &mut Context<Self>) {
        let active = active.filter(|active| *active < self.search_highlights.len());
        if self.active_search_highlight != active {
            self.active_search_highlight = active;
            cx.notify();
        }
    }

    pub fn search_highlights(&self) -> &[Range<usize>] {
        &self.search_highlights
    }

    pub fn active_search_highlight(&self) -> Option<usize> {
        self.active_search_highlight
    }

    fn copy(&self, text: &RenderedText, _: &mut Window, cx: &mut Context<Self>) {
        if self.selection.end <= self.selection.start {
            return;
        }
        let range =
            text.expand_range_for_atomic_regions(self.selection.start..self.selection.end);
        let text = text.text_for_range(range);
        cx.write_to_clipboard(ClipboardItem::new_string(text));
    }

    fn copy_as_markdown(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = self.context_menu_selected_markdown.take() {
            cx.write_to_clipboard(ClipboardItem::new_string(text.to_string()));
            return;
        }
        if self.selection.end <= self.selection.start {
            return;
        }
        let text = self
            .parsed_markdown
            .rebalanced_markdown_for_selection(self.selection.start..self.selection.end);
        cx.write_to_clipboard(ClipboardItem::new_string(text));
    }

    fn capture_for_context_menu(
        &mut self,
        link: Option<SharedString>,
        rendered_text: Option<&RenderedText>,
    ) {
        let range = self.selection.start..self.selection.end;
        // Math is atomic for copy: widen a partial range to fully cover any formula it touches.
        let range = rendered_text
            .map(|text| text.expand_range_for_atomic_regions(range.clone()))
            .unwrap_or(range);
        if range.end > range.start {
            self.context_menu_selected_markdown = Some(SharedString::new(
                self.parsed_markdown
                    .rebalanced_markdown_for_selection(range.clone()),
            ));
            self.context_menu_selected_text = rendered_text
                .map(|text| text.text_for_range(range))
                .map(SharedString::new)
                .or_else(|| self.context_menu_selected_markdown.clone());
        } else {
            self.context_menu_selected_markdown = None;
            self.context_menu_selected_text = None;
        }
        self.context_menu_link = link;
    }

    /// Returns the URL of the link that was most recently right-clicked, if any.
    /// This is set during a right-click mouse-down event and can be read by parent
    /// views to include a "Copy Link" item in their context menus.
    pub fn context_menu_link(&self) -> Option<&SharedString> {
        self.context_menu_link.as_ref()
    }

    /// Returns the rendered (plain) text that was selected when the most recent
    /// context menu invocation happened.
    pub fn context_menu_selected_text(&self) -> Option<&SharedString> {
        self.context_menu_selected_text.as_ref()
    }

    /// Returns the markdown that was selected when the most recent context
    /// menu invocation happened, rebalanced via
    /// [`ParsedMarkdown::rebalanced_markdown_for_selection`].
    pub fn context_menu_selected_markdown(&self) -> Option<&SharedString> {
        self.context_menu_selected_markdown.as_ref()
    }

    fn parse(&mut self, cx: &mut Context<Self>) {
        if self.source.is_empty() {
            self.should_reparse = false;
            self.pending_parse.take();
            self.parsed_markdown = ParsedMarkdown {
                source: self.source.clone(),
                ..Default::default()
            };
            self.active_root_block = None;
            self.images_by_source_offset.clear();
            self.mermaid_state.clear();
            cx.notify();
            cx.refresh_windows();
            return;
        }

        if self.pending_parse.is_some() {
            self.should_reparse = true;
            return;
        }
        self.should_reparse = false;
        self.pending_parse = Some(self.start_background_parse(cx));
    }

    fn start_background_parse(&self, cx: &Context<Self>) -> Task<()> {
        let source = self.source.clone();
        let should_parse_links_only = self.options.parse_links_only;
        let should_parse_html = self.options.parse_html;
        let should_render_mermaid_diagrams = self.options.render_mermaid_diagrams;
        let should_render_math = self.options.render_math;
        let should_parse_heading_slugs = self.options.parse_heading_slugs;
        let should_parse_metadata_blocks = self.options.render_metadata_blocks;
        let language_registry = self.language_registry.clone();
        let fallback = self.fallback_code_block_language.clone();

        let parsed = cx.background_spawn(async move {
            if should_parse_links_only {
                return (
                    ParsedMarkdown {
                        events: Arc::from(parse_links_only(source.as_ref())),
                        source,
                        languages_by_name: TreeMap::default(),
                        languages_by_path: TreeMap::default(),
                        root_block_starts: Arc::default(),
                        html_blocks: BTreeMap::default(),
                        metadata_blocks: BTreeMap::default(),
                        mermaid_diagrams: BTreeMap::default(),
                        heading_slugs: HashMap::default(),
                        footnote_definitions: HashMap::default(),
                    },
                    Default::default(),
                );
            }

            let parsed = parse_markdown_inner(
                &source,
                should_parse_html,
                should_parse_heading_slugs,
                should_parse_metadata_blocks,
                should_render_math,
            );
            let events = parsed.events;
            let language_names = parsed.language_names;
            let paths = parsed.language_paths;
            let root_block_starts = parsed.root_block_starts;
            let html_blocks = parsed.html_blocks;
            let metadata_blocks = parsed.metadata_blocks;
            let heading_slugs = parsed.heading_slugs;
            let footnote_definitions = parsed.footnote_definitions;
            let mermaid_diagrams = if should_render_mermaid_diagrams {
                extract_mermaid_diagrams(&source, &events)
            } else {
                BTreeMap::default()
            };
            let mut images_by_source_offset = HashMap::default();
            let mut languages_by_name = TreeMap::default();
            let mut languages_by_path = TreeMap::default();
            if let Some(registry) = language_registry.as_ref() {
                for name in language_names {
                    let language = if !name.is_empty() {
                        registry.language_for_name_or_extension(&name).left_future()
                    } else if let Some(fallback) = &fallback {
                        registry.language_for_name(fallback.as_ref()).right_future()
                    } else {
                        continue;
                    };
                    if let Ok(language) = language.await {
                        languages_by_name.insert(name, language);
                    }
                }

                for path in paths {
                    if let Ok(language) = registry
                        .load_language_for_file_path(Path::new(path.as_ref()))
                        .await
                    {
                        languages_by_path.insert(path, language);
                    }
                }
            }

            for (range, event) in &events {
                if let MarkdownEvent::Start(MarkdownTag::Image { dest_url, .. }) = event
                    && let Some(data_url) = dest_url.strip_prefix("data:")
                {
                    let Some((mime_info, data)) = data_url.split_once(',') else {
                        continue;
                    };
                    let Some((mime_type, encoding)) = mime_info.split_once(';') else {
                        continue;
                    };
                    let Some(format) = ImageFormat::from_mime_type(mime_type) else {
                        continue;
                    };
                    let is_base64 = encoding == "base64";
                    if is_base64
                        && let Some(bytes) = base64::prelude::BASE64_STANDARD
                            .decode(data)
                            .log_with_level(Level::Debug)
                    {
                        let image = Arc::new(Image::from_bytes(format, bytes));
                        images_by_source_offset.insert(range.start, image);
                    }
                }
            }

            (
                ParsedMarkdown {
                    source,
                    events: Arc::from(events),
                    languages_by_name,
                    languages_by_path,
                    root_block_starts: Arc::from(root_block_starts),
                    html_blocks,
                    metadata_blocks,
                    mermaid_diagrams,
                    heading_slugs,
                    footnote_definitions,
                },
                images_by_source_offset,
            )
        });

        cx.spawn(async move |this, cx| {
            let (parsed, images_by_source_offset) = parsed.await;

            this.update(cx, |this, cx| {
                this.parsed_markdown = parsed;
                this.images_by_source_offset = images_by_source_offset;
                if this.active_root_block.is_some_and(|block_index| {
                    block_index >= this.parsed_markdown.root_block_starts.len()
                }) {
                    this.active_root_block = None;
                }
                if this.options.render_mermaid_diagrams {
                    let parsed_markdown = this.parsed_markdown.clone();
                    this.mermaid_state.update(&parsed_markdown, cx);
                    this.mermaid_showing_code
                        .retain(|offset| parsed_markdown.mermaid_diagrams.contains_key(offset));
                } else {
                    this.mermaid_state.clear();
                    this.mermaid_showing_code.clear();
                }
                if this.options.render_math {
                    let parsed_markdown = this.parsed_markdown.clone();
                    this.math_state.update(&parsed_markdown, cx);
                } else {
                    this.math_state.clear();
                }
                this.pending_parse.take();
                if this.should_reparse {
                    this.parse(cx);
                }
                cx.notify();
                cx.refresh_windows();
            })
            .ok();
        })
    }
}

impl Focusable for Markdown {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

#[derive(Debug, Default, Clone)]
enum SelectMode {
    #[default]
    Character,
    Word(Range<usize>),
    Line(Range<usize>),
    All,
}

#[derive(Clone, Default)]
struct Selection {
    start: usize,
    end: usize,
    reversed: bool,
    pending: bool,
    mode: SelectMode,
}

impl Selection {
    fn set_head(&mut self, head: usize, rendered_text: &RenderedText) {
        match &self.mode {
            SelectMode::Character => {
                if head < self.tail() {
                    if !self.reversed {
                        self.end = self.start;
                        self.reversed = true;
                    }
                    self.start = head;
                } else {
                    if self.reversed {
                        self.start = self.end;
                        self.reversed = false;
                    }
                    self.end = head;
                }
            }
            SelectMode::Word(original_range) | SelectMode::Line(original_range) => {
                let head_range = if matches!(self.mode, SelectMode::Word(_)) {
                    rendered_text.surrounding_word_range(head)
                } else {
                    rendered_text.surrounding_line_range(head)
                };

                if head < original_range.start {
                    self.start = head_range.start;
                    self.end = original_range.end;
                    self.reversed = true;
                } else if head >= original_range.end {
                    self.start = original_range.start;
                    self.end = head_range.end;
                    self.reversed = false;
                } else {
                    self.start = original_range.start;
                    self.end = original_range.end;
                    self.reversed = false;
                }
            }
            SelectMode::All => {
                self.start = 0;
                self.end = rendered_text
                    .lines
                    .last()
                    .map(|line| line.source_end)
                    .unwrap_or(0);
                self.reversed = false;
            }
        }
        // Math formulas are atomic: a selection touching any part of a formula covers the whole
        // formula, so highlight and copy stay consistent regardless of the drag end position.
        let expanded = rendered_text.expand_range_for_atomic_regions(self.start..self.end);
        self.start = expanded.start;
        self.end = expanded.end;
    }

    fn tail(&self) -> usize {
        if self.reversed { self.end } else { self.start }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ParsedMarkdown {
    pub source: SharedString,
    pub events: Arc<[(Range<usize>, MarkdownEvent)]>,
    pub languages_by_name: TreeMap<SharedString, Arc<Language>>,
    pub languages_by_path: TreeMap<Arc<str>, Arc<Language>>,
    pub root_block_starts: Arc<[usize]>,
    pub(crate) html_blocks: BTreeMap<usize, html::html_parser::ParsedHtmlBlock>,
    pub(crate) metadata_blocks: BTreeMap<usize, ParsedMetadataBlock>,
    pub(crate) mermaid_diagrams: BTreeMap<usize, ParsedMarkdownMermaidDiagram>,
    pub heading_slugs: HashMap<SharedString, usize>,
    pub footnote_definitions: HashMap<SharedString, usize>,
}

impl ParsedMarkdown {
    pub fn source(&self) -> &SharedString {
        &self.source
    }

    pub fn events(&self) -> &Arc<[(Range<usize>, MarkdownEvent)]> {
        &self.events
    }

    pub fn root_block_starts(&self) -> &Arc<[usize]> {
        &self.root_block_starts
    }

    pub fn root_block_for_source_index(&self, source_index: usize) -> Option<usize> {
        if self.root_block_starts.is_empty() {
            return None;
        }

        let partition = self
            .root_block_starts
            .partition_point(|block_start| *block_start <= source_index);

        Some(partition.saturating_sub(1))
    }

    /// Extracts the markdown source for a selection, rebalancing inline
    /// delimiters (`**`, backticks, link syntax, etc.) so partial selections of
    /// styled spans stay well-formed.
    ///
    /// With an exception of a single inline code span, which is returned as plain
    /// text, since copying a command or identifier is the dominant use case there.
    pub fn rebalanced_markdown_for_selection(&self, selection: Range<usize>) -> String {
        selection::rebalanced_markdown_for_selection(
            &self.source,
            &self.events,
            &self.root_block_starts,
            selection,
        )
    }
}

pub enum AutoscrollBehavior {
    /// Propagate the request up the element tree for the nearest
    /// scrollable ancestor (e.g. `List`) to handle.
    Propagate,
    /// Directly control a specific scroll handle.
    Controlled(ScrollHandle),
}

pub struct MarkdownElement {
    markdown: Entity<Markdown>,
    style: MarkdownStyle,
    code_block_renderer: CodeBlockRenderer,
    on_url_click: Option<Rc<dyn Fn(SharedString, &mut Window, &mut App)>>,
    on_url_hover: Option<UrlHoverCallback>,
    code_span_link: Option<CodeSpanLinkCallback>,
    on_source_click: Option<SourceClickCallback>,
    on_checkbox_toggle: Option<CheckboxToggleCallback>,
    image_resolver: Option<Box<dyn Fn(&str) -> Option<ImageSource>>>,
    show_root_block_markers: bool,
    autoscroll: AutoscrollBehavior,
}

impl MarkdownElement {
    pub fn new(markdown: Entity<Markdown>, style: MarkdownStyle) -> Self {
        Self {
            markdown,
            style,
            code_block_renderer: CodeBlockRenderer::Default {
                copy_button_visibility: CopyButtonVisibility::VisibleOnHover,
                wrap_button_visibility: WrapButtonVisibility::Hidden,
                border: false,
            },
            on_url_click: None,
            on_url_hover: None,
            code_span_link: None,
            on_source_click: None,
            on_checkbox_toggle: None,
            image_resolver: None,
            show_root_block_markers: false,
            autoscroll: AutoscrollBehavior::Propagate,
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn rendered_text(
        markdown: Entity<Markdown>,
        cx: &mut gpui::VisualTestContext,
        style: impl FnOnce(&Window, &App) -> MarkdownStyle,
    ) -> String {
        use gpui::size;

        let (text, _) = cx.draw(
            Default::default(),
            size(px(600.0), px(600.0)),
            |window, cx| Self::new(markdown, style(window, cx)),
        );
        text.text
            .lines
            .iter()
            .map(|line| line.layout.wrapped_text())
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub fn code_block_renderer(mut self, variant: CodeBlockRenderer) -> Self {
        self.code_block_renderer = variant;
        self
    }

    pub fn on_url_click(
        mut self,
        handler: impl Fn(SharedString, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_url_click = Some(Rc::new(handler));
        self
    }

    pub fn on_url_hover(
        mut self,
        handler: impl Fn(Option<SharedString>, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_url_hover = Some(Rc::new(handler));
        self
    }

    pub fn on_code_span_link(
        mut self,
        callback: impl Fn(&str, &App) -> Option<SharedString> + 'static,
    ) -> Self {
        self.code_span_link = Some(Arc::new(callback));
        self
    }

    pub fn on_source_click(
        mut self,
        handler: impl Fn(usize, usize, &mut Window, &mut App) -> bool + 'static,
    ) -> Self {
        self.on_source_click = Some(Box::new(handler));
        self
    }

    pub fn on_checkbox_toggle(
        mut self,
        handler: impl Fn(Range<usize>, bool, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_checkbox_toggle = Some(Rc::new(handler));
        self
    }

    pub fn image_resolver(
        mut self,
        resolver: impl Fn(&str) -> Option<ImageSource> + 'static,
    ) -> Self {
        self.image_resolver = Some(Box::new(resolver));
        self
    }

    pub fn show_root_block_markers(mut self) -> Self {
        self.show_root_block_markers = true;
        self
    }

    pub fn scroll_handle(mut self, scroll_handle: ScrollHandle) -> Self {
        self.autoscroll = AutoscrollBehavior::Controlled(scroll_handle);
        self
    }

    fn push_markdown_code_span(
        &self,
        builder: &mut MarkdownElementBuilder,
        text: &str,
        range: Range<usize>,
        cx: &App,
    ) {
        let link_url = if builder.code_block_stack.is_empty()
            && builder.link_depth == 0
            && !self.style.prevent_mouse_interaction
        {
            self.code_span_link
                .as_ref()
                .and_then(|callback| callback(text, cx))
        } else {
            None
        };

        if let Some(url) = link_url {
            builder.push_link(url.clone(), range.clone());
            let link_style = self
                .style
                .link_callback
                .as_ref()
                .and_then(|callback| callback(url.as_ref(), cx))
                .unwrap_or_else(|| self.style.link.clone());
            builder.push_text_style(self.style.inline_code.clone());
            builder.push_text_style(link_style);
            builder.push_text(text, range);
            builder.pop_text_style();
            builder.pop_text_style();
        } else {
            let mut code_style = self.style.inline_code.clone();
            if builder.link_depth > 0 {
                code_style.color = self.style.link.color.or(code_style.color);
            }
            builder.push_text_style(code_style);
            builder.push_text(text, range);
            builder.pop_text_style();
        }
    }

    fn push_markdown_image(
        &self,
        builder: &mut MarkdownElementBuilder,
        range: &Range<usize>,
        source: ImageSource,
        dest_url: SharedString,
        alt_text: Option<SharedString>,
        width: Option<DefiniteLength>,
        height: Option<DefiniteLength>,
    ) {
        let enclosing_link_url = (builder.link_depth > 0)
            .then(|| builder.rendered_links.last())
            .flatten()
            .map(|link| link.destination_url.clone());
        let fallback_opens_image_url = enclosing_link_url.is_none();

        let image_element = {
            let wrapper = div().id(("markdown-image-link", range.start)).min_w_0();
            let wrapper = if !self.style.prevent_mouse_interaction
                && let Some(url) = enclosing_link_url
            {
                let click_url = url.clone();
                let markdown = self.markdown.clone();
                let url_click = self.on_url_click.clone();
                let bounds = Rc::new(Cell::new(None));
                builder.push_image_link(url.clone(), bounds.clone());
                wrapper
                    .relative()
                    .cursor_pointer()
                    .child(
                        canvas(
                            move |image_bounds, _window, _cx| bounds.set(Some(image_bounds)),
                            |_, _, _, _| {},
                        )
                        .size_full()
                        .absolute()
                        .top_0()
                        .left_0(),
                    )
                    .on_click(move |_, window, cx| {
                        if let Some(ref on_url_click) = url_click {
                            on_url_click(click_url.clone(), window, cx);
                        } else {
                            cx.open_url(&click_url);
                        }
                    })
                    .capture_any_mouse_down(move |event, _window, cx| {
                        if event.button == MouseButton::Right {
                            markdown.update(cx, |md, _| {
                                md.capture_for_context_menu(Some(url.clone()), None)
                            });
                        }
                    })
            } else {
                wrapper
            };
            wrapper.child(
                img(source)
                    .id(("markdown-image", range.start))
                    .min_w_0()
                    .max_w_full()
                    .rounded_md()
                    .mr_1()
                    .mb_1()
                    .when_some(height, |this, height| this.h(height))
                    .when_some(width, |this, width| this.w(width))
                    .with_fallback(move || {
                        image_fallback_element(
                            dest_url.clone(),
                            alt_text.clone(),
                            fallback_opens_image_url,
                        )
                    }),
            )
        };

        builder.push_image_child(image_element);
    }

    fn push_markdown_math(
        &self,
        builder: &mut MarkdownElementBuilder,
        source: &str,
        latex: &SharedString,
        range: Range<usize>,
        display: bool,
        math_state: &MathState,
        error_color: Hsla,
    ) {
        if display {
            match render_math_block(math_state, latex) {
                // Center block (display) math horizontally, like other previewers. The
                // invisible source anchor makes the formula's source copyable.
                MathBlock::Element(mut math) => {
                    builder.flush_text();
                    let slot: MathBoundsSlot = Rc::new(Cell::new(None));
                    math.set_bounds_slot(slot.clone());
                    builder.push_math_region(range.clone(), vec![slot]);
                    let anchor =
                        builder.render_copyable_source_anchor(&source[range.clone()], range);
                    builder.append_child(
                        div()
                            .relative()
                            .child(anchor)
                            .child(div().w_full().flex().justify_center().child(math))
                            .into_any_element(),
                    );
                }
                MathBlock::Error(message) => {
                    self.push_math_error(builder, &message, range, error_color)
                }
                // Not parsed yet: show the raw source until it resolves.
                MathBlock::Pending => builder.push_text(&source[range.clone()], range),
            }
        } else {
            match render_math_inline(math_state, latex) {
                // Embed the formula's fragments into the current line's text as one atomic span so
                // an `InlineFlowElement` flows text and math on shared wrapped rows (the formula
                // sits after the text before it). The span's stand-in text is the verbatim `$...$`
                // source, which makes the formula copyable in source order.
                MathInline::Fragments(fragments) => {
                    builder.push_inline_math_span(&source[range.clone()], range, fragments);
                }
                MathInline::Error(message) => {
                    self.push_math_error(builder, &message, range, error_color)
                }
                MathInline::Pending => builder.push_text(&source[range.clone()], range),
            }
        }
    }

    /// Show a math parse error in place of the formula, in the theme's error color, like
    /// other previewers (e.g. VS Code's KaTeX errors).
    fn push_math_error(
        &self,
        builder: &mut MarkdownElementBuilder,
        message: &str,
        range: Range<usize>,
        error_color: Hsla,
    ) {
        builder.push_text_style(TextStyleRefinement {
            color: Some(error_color),
            ..Default::default()
        });
        builder.push_text(message, range);
        builder.pop_text_style();
    }

    fn push_markdown_paragraph(
        &self,
        builder: &mut MarkdownElementBuilder,
        range: &Range<usize>,
        markdown_end: usize,
        text_align_override: Option<TextAlign>,
    ) {
        let align = text_align_override.unwrap_or(self.style.base_text_style.text_align);
        let mut paragraph = div().when(!self.style.height_is_multiple_of_line_height, |el| {
            el.mb_2().line_height(rems(1.3))
        });

        paragraph = match align {
            TextAlign::Center => paragraph.text_center(),
            TextAlign::Left => paragraph.text_left(),
            TextAlign::Right => paragraph.text_right(),
        };

        builder.push_text_style(TextStyleRefinement {
            text_align: Some(align),
            ..Default::default()
        });
        builder.push_div(paragraph, range, markdown_end);
    }

    fn pop_markdown_paragraph(&self, builder: &mut MarkdownElementBuilder) {
        builder.pop_div();
        builder.pop_text_style();
    }

    fn push_markdown_heading(
        &self,
        builder: &mut MarkdownElementBuilder,
        level: pulldown_cmark::HeadingLevel,
        range: &Range<usize>,
        markdown_end: usize,
        text_align_override: Option<TextAlign>,
    ) {
        let align = text_align_override.unwrap_or(self.style.base_text_style.text_align);
        let mut heading = div().mt_4().mb_2();
        heading = apply_heading_style(
            heading,
            level,
            self.style.heading_level_styles.as_ref(),
            self.style.heading_border_color,
        );

        heading = match align {
            TextAlign::Center => heading.text_center(),
            TextAlign::Left => heading.text_left(),
            TextAlign::Right => heading.text_right(),
        };

        let mut heading_style = self.style.heading.clone();
        let heading_text_style = heading_style.text_style().clone();
        heading.style().refine(&heading_style);

        builder.push_text_style(TextStyleRefinement {
            text_align: Some(align),
            ..heading_text_style
        });
        builder.push_div(heading, range, markdown_end);
    }

    fn pop_markdown_heading(&self, builder: &mut MarkdownElementBuilder) {
        builder.pop_div();
        builder.pop_text_style();
    }

    fn push_markdown_block_quote(
        &self,
        builder: &mut MarkdownElementBuilder,
        kind: Option<pulldown_cmark::BlockQuoteKind>,
        range: &Range<usize>,
        markdown_end: usize,
    ) {
        let border_color = self
            .style
            .block_quote_kind_colors
            .for_kind(kind, self.style.block_quote_border_color);

        let header = kind.map(|kind| {
            let (icon_name, label) = match kind {
                BlockQuoteKind::Note => (IconName::Info, "Note"),
                BlockQuoteKind::Tip => (IconName::Sparkle, "Tip"),
                BlockQuoteKind::Important => (IconName::Chat, "Important"),
                BlockQuoteKind::Warning => (IconName::Warning, "Warning"),
                BlockQuoteKind::Caution => (IconName::Stop, "Caution"),
            };
            h_flex()
                .gap_1()
                .items_center()
                .mb_1()
                .child(
                    Icon::new(icon_name)
                        .size(IconSize::Small)
                        .color(Color::Custom(border_color)),
                )
                .child(
                    Label::new(label)
                        .color(Color::Custom(border_color))
                        .weight(FontWeight::BOLD),
                )
                .into_any_element()
        });

        let block_div = div().pl_4().mb_2().border_l_4().border_color(border_color);
        let block_div = match header {
            Some(header) => block_div.child(header),
            None => block_div,
        };

        builder.push_text_style(self.style.block_quote.clone());
        builder.push_div(block_div, range, markdown_end);
    }

    fn pop_markdown_block_quote(&self, builder: &mut MarkdownElementBuilder) {
        builder.pop_div();
        builder.pop_text_style();
    }

    fn push_metadata_block(
        &self,
        builder: &mut MarkdownElementBuilder,
        source: &str,
        metadata_block: &ParsedMetadataBlock,
        markdown_end: usize,
        cx: &App,
    ) {
        let content_range = &metadata_block.content_range;
        if let Some(rows) = metadata_block.rows.as_deref() {
            builder.push_div(
                div()
                    .grid()
                    .grid_cols(2)
                    .w_full()
                    .mb_2()
                    .border_1()
                    .border_color(cx.theme().colors().border)
                    .rounded_sm()
                    .overflow_hidden(),
                content_range,
                markdown_end,
            );

            for (row_index, row) in rows.iter().enumerate() {
                self.push_metadata_cell(
                    builder,
                    source,
                    row.key.clone(),
                    content_range,
                    markdown_end,
                    MetadataCellStyle {
                        row_index,
                        is_key: true,
                    },
                    cx,
                );
                self.push_metadata_cell(
                    builder,
                    source,
                    row.value.clone(),
                    content_range,
                    markdown_end,
                    MetadataCellStyle {
                        row_index,
                        is_key: false,
                    },
                    cx,
                );
            }

            builder.pop_div();
        } else {
            let mut metadata_block = div().w_full().rounded_md();
            metadata_block.style().refine(&self.style.code_block);
            builder.push_text_style(self.style.code_block.text.to_owned());
            builder.push_code_block(None);
            builder.push_div(metadata_block, content_range, markdown_end);
            builder.push_text(&source[content_range.clone()], content_range.clone());
            builder.trim_trailing_newline();
            builder.pop_div();
            builder.pop_code_block();
            builder.pop_text_style();
        }
    }

    fn push_metadata_cell(
        &self,
        builder: &mut MarkdownElementBuilder,
        source: &str,
        text_range: Range<usize>,
        block_range: &Range<usize>,
        markdown_end: usize,
        cell_style: MetadataCellStyle,
        cx: &App,
    ) {
        builder.push_div(
            div()
                .flex()
                .flex_col()
                .min_w_0()
                .px_2()
                .py_1()
                .border_color(cx.theme().colors().border)
                .when(cell_style.row_index > 0, |this| this.border_t_1())
                .when(!cell_style.is_key, |this| this.border_l_1())
                .when(cell_style.is_key, |this| {
                    this.bg(cx.theme().colors().panel_background)
                }),
            block_range,
            markdown_end,
        );

        let text_style = if cell_style.is_key {
            TextStyleRefinement {
                color: Some(cx.theme().colors().text_muted),
                font_weight: Some(FontWeight::SEMIBOLD),
                ..Default::default()
            }
        } else {
            TextStyleRefinement::default()
        };
        builder.push_text_style(text_style);
        builder.push_text(&source[text_range.clone()], text_range);
        builder.pop_text_style();
        builder.pop_div();
    }

    fn push_markdown_list_item(
        &self,
        builder: &mut MarkdownElementBuilder,
        bullet: AnyElement,
        range: &Range<usize>,
        markdown_end: usize,
    ) {
        builder.push_div(
            div()
                .when(!self.style.height_is_multiple_of_line_height, |el| {
                    el.mb_1().gap_1().line_height(rems(1.3))
                })
                .h_flex()
                .items_start()
                .child(bullet),
            range,
            markdown_end,
        );
        // Without `w_0`, text doesn't wrap to the width of the container.
        builder.push_div(div().flex_1().w_0(), range, markdown_end);
    }

    fn pop_markdown_list_item(&self, builder: &mut MarkdownElementBuilder) {
        builder.pop_div();
        builder.pop_div();
    }

    fn paint_highlight_range(
        start: usize,
        end: usize,
        color: Hsla,
        rendered_text: &RenderedText,
        window: &mut Window,
    ) {
        for bounds in rendered_text.bounds_for_source_range(start..end) {
            window.paint_quad(quad(
                bounds,
                Pixels::ZERO,
                color,
                Edges::default(),
                Hsla::transparent_black(),
                BorderStyle::default(),
            ));
        }
    }

    fn paint_selection(&self, rendered_text: &RenderedText, window: &mut Window, cx: &mut App) {
        let selection = self.markdown.read(cx).selection.clone();
        Self::paint_highlight_range(
            selection.start,
            selection.end,
            self.style.selection_background_color,
            rendered_text,
            window,
        );
    }

    fn paint_search_highlights(
        &self,
        rendered_text: &RenderedText,
        window: &mut Window,
        cx: &mut App,
    ) {
        let markdown = self.markdown.read(cx);
        let active_index = markdown.active_search_highlight;
        let colors = cx.theme().colors();

        let highlight_bounds = rendered_text.bounds_for_sorted_source_ranges(
            markdown
                .search_highlights
                .iter()
                .enumerate()
                .map(|(ix, range)| (ix, range.clone())),
        );
        for (highlight_ix, bounds) in highlight_bounds {
            let color = if Some(highlight_ix) == active_index {
                colors.search_active_match_background
            } else {
                colors.search_match_background
            };
            window.paint_quad(quad(
                bounds,
                Pixels::ZERO,
                color,
                Edges::default(),
                Hsla::transparent_black(),
                BorderStyle::default(),
            ));
        }
    }

    fn paint_mouse_listeners(
        &mut self,
        hitbox: &Hitbox,
        rendered_text: &RenderedText,
        window: &mut Window,
        cx: &mut App,
    ) {
        if self.style.prevent_mouse_interaction {
            return;
        }

        let is_hovering_clickable = hitbox.is_hovered(window)
            && !self.markdown.read(cx).selection.pending
            && (rendered_text
                .image_link_for_position(window.mouse_position())
                .is_some()
                || rendered_text
                    .source_index_for_position(window.mouse_position())
                    .ok()
                    .is_some_and(|source_index| {
                        rendered_text.link_for_source_index(source_index).is_some()
                            || rendered_text
                                .footnote_ref_for_source_index(source_index)
                                .is_some()
                    }));

        if is_hovering_clickable {
            window.set_cursor_style(CursorStyle::PointingHand, hitbox);
        } else {
            window.set_cursor_style(CursorStyle::IBeam, hitbox);
        }

        let on_open_url = self.on_url_click.take();
        let on_url_hover = self.on_url_hover.take();
        let on_source_click = self.on_source_click.take();

        self.on_mouse_event(window, cx, {
            let hitbox = hitbox.clone();
            let rendered_text = rendered_text.clone();
            move |markdown, event: &MouseDownEvent, phase, window, _cx| {
                if phase.capture()
                    && event.button == MouseButton::Right
                    && hitbox.is_hovered(window)
                {
                    let link = rendered_text
                        .source_index_for_position(event.position)
                        .ok()
                        .and_then(|ix| rendered_text.link_for_source_index(ix))
                        .map(|link| link.destination_url.clone());
                    markdown.capture_for_context_menu(link, Some(&rendered_text));
                }
            }
        });

        self.on_mouse_event(window, cx, {
            let rendered_text = rendered_text.clone();
            let hitbox = hitbox.clone();
            move |markdown, event: &MouseDownEvent, phase, window, cx| {
                if hitbox.is_hovered(window) {
                    if phase.bubble() && event.button != MouseButton::Right {
                        let position_result =
                            rendered_text.source_index_for_position(event.position);

                        if let Ok(source_index) = position_result {
                            if let Some(footnote_ref) =
                                rendered_text.footnote_ref_for_source_index(source_index)
                            {
                                markdown.pressed_footnote_ref = Some(footnote_ref.clone());
                            } else if let Some(link) =
                                rendered_text.link_for_source_index(source_index)
                            {
                                markdown.pressed_link = Some(link.clone());
                            }
                        }

                        if markdown.pressed_footnote_ref.is_none()
                            && markdown.pressed_link.is_none()
                        {
                            let source_index = match position_result {
                                Ok(ix) | Err(ix) => ix,
                            };
                            // Double-clicking a drawn formula should select the whole formula
                            // (atomic). Decide "on a formula" by the click POSITION (its painted
                            // bounds), not by the resolved source index: a right-half click resolves
                            // to the formula's end index, which is also the start index of any text
                            // immediately following it, so an index test would select the formula
                            // when the user clicked the adjacent word. The preview binds double-click
                            // to "jump editor to source" (on_source_click returns blocked); let that
                            // run, but for math keep our own formula selection instead of clearing it.
                            let clicked_math_range = rendered_text
                                .math_regions
                                .iter()
                                .find(|region| region.bounds_containing(event.position).is_some())
                                .map(|region| region.source_range.clone());
                            let clicked_math = clicked_math_range.is_some();
                            if let Some(handler) = on_source_click.as_ref() {
                                let blocked = handler(source_index, event.click_count, window, cx);
                                if blocked && !clicked_math {
                                    markdown.selection = Selection::default();
                                    markdown.pressed_link = None;
                                    window.prevent_default();
                                    cx.notify();
                                    return;
                                }
                            }
                            let (range, mode, reversed) = match event.click_count {
                                1 if event.modifiers.shift => {
                                    let tail = markdown.selection.tail();
                                    let reversed = source_index < tail;
                                    let range = if reversed {
                                        source_index..tail
                                    } else {
                                        tail..source_index
                                    };
                                    (range, SelectMode::Character, reversed)
                                }
                                1 => {
                                    let range = source_index..source_index;
                                    (range, SelectMode::Character, false)
                                }
                                2 => {
                                    let range = clicked_math_range.unwrap_or_else(|| {
                                        rendered_text.surrounding_word_range(source_index)
                                    });
                                    (range.clone(), SelectMode::Word(range), false)
                                }
                                3 => {
                                    let range = clicked_math_range.unwrap_or_else(|| {
                                        rendered_text.surrounding_line_range(source_index)
                                    });
                                    (range.clone(), SelectMode::Line(range), false)
                                }
                                _ => {
                                    let range = 0..rendered_text
                                        .lines
                                        .last()
                                        .map(|line| line.source_end)
                                        .unwrap_or(0);
                                    (range, SelectMode::All, false)
                                }
                            };
                            // Keep the selection atomic over formulas from the first click/drag.
                            let range = rendered_text.expand_range_for_atomic_regions(range);
                            markdown.selection = Selection {
                                start: range.start,
                                end: range.end,
                                reversed,
                                pending: true,
                                mode,
                            };
                            window.focus(&markdown.focus_handle, cx);
                        }

                        window.prevent_default();
                        cx.notify();
                    }
                } else if phase.capture() && event.button == MouseButton::Left {
                    markdown.selection = Selection::default();
                    markdown.pressed_link = None;
                    cx.notify();
                }
            }
        });
        self.on_mouse_event(window, cx, {
            let rendered_text = rendered_text.clone();
            let hitbox = hitbox.clone();
            let was_hovering_clickable = is_hovering_clickable;
            move |markdown, event: &MouseMoveEvent, phase, window, cx| {
                if phase.capture() {
                    return;
                }

                if markdown.selection.pending {
                    let source_index = match rendered_text.source_index_for_position(event.position)
                    {
                        Ok(ix) | Err(ix) => ix,
                    };
                    markdown.selection.set_head(source_index, &rendered_text);
                    markdown.autoscroll_code_block(source_index, event.position);
                    markdown.autoscroll_request = Some(source_index);
                    cx.notify();
                } else {
                    let is_hitbox_hovered = hitbox.is_hovered(window);
                    let source_index = is_hitbox_hovered
                        .then(|| rendered_text.source_index_for_position(event.position).ok())
                        .flatten();
                    let hovered_url = is_hitbox_hovered
                        .then(|| rendered_text.image_link_for_position(event.position))
                        .flatten()
                        .map(|image| image.destination_url.clone())
                        .or_else(|| {
                            source_index
                                .and_then(|source_index| {
                                    rendered_text.link_for_source_index(source_index)
                                })
                                .map(|link| link.destination_url.clone())
                        });
                    let is_hovering_clickable = hovered_url.is_some()
                        || source_index.is_some_and(|source_index| {
                            rendered_text
                                .footnote_ref_for_source_index(source_index)
                                .is_some()
                        });
                    if let Some(on_url_hover) = on_url_hover.as_ref() {
                        on_url_hover(hovered_url, window, cx);
                    }
                    if is_hovering_clickable != was_hovering_clickable {
                        cx.notify();
                    }
                }
            }
        });
        self.on_mouse_event(window, cx, {
            let rendered_text = rendered_text.clone();
            move |markdown, event: &MouseUpEvent, phase, window, cx| {
                if phase.bubble() {
                    let source_index = rendered_text.source_index_for_position(event.position).ok();
                    if let Some(pressed_footnote_ref) = markdown.pressed_footnote_ref.take()
                        && source_index
                            .and_then(|ix| rendered_text.footnote_ref_for_source_index(ix))
                            == Some(&pressed_footnote_ref)
                    {
                        if let Some(source_index) =
                            markdown.footnote_definition_content_start(&pressed_footnote_ref.label)
                        {
                            markdown.autoscroll_request = Some(source_index);
                            cx.notify();
                        }
                    } else if let Some(pressed_link) = markdown.pressed_link.take()
                        && source_index.and_then(|ix| rendered_text.link_for_source_index(ix))
                            == Some(&pressed_link)
                    {
                        if let Some(open_url) = on_open_url.as_ref() {
                            open_url(pressed_link.destination_url, window, cx);
                        } else {
                            cx.open_url(&pressed_link.destination_url);
                        }
                    }
                } else if markdown.selection.pending {
                    markdown.selection.pending = false;
                    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
                    {
                        let range = rendered_text.expand_range_for_atomic_regions(
                            markdown.selection.start..markdown.selection.end,
                        );
                        let text = rendered_text.text_for_range(range);
                        cx.write_to_primary(ClipboardItem::new_string(text))
                    }
                    cx.notify();
                }
            }
        });
    }

    fn autoscroll(
        &self,
        rendered_text: &RenderedText,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<()> {
        let autoscroll_index = self
            .markdown
            .update(cx, |markdown, _| markdown.autoscroll_request.take())?;
        let (position, line_height) = rendered_text.position_for_source_index(autoscroll_index)?;

        match &self.autoscroll {
            AutoscrollBehavior::Controlled(scroll_handle) => {
                let viewport = scroll_handle.bounds();
                let margin = line_height * 3.;
                let top_goal = viewport.top() + margin;
                let bottom_goal = viewport.bottom() - margin;
                let current_offset = scroll_handle.offset();

                let new_offset_y = if position.y < top_goal {
                    current_offset.y + (top_goal - position.y)
                } else if position.y + line_height > bottom_goal {
                    current_offset.y + (bottom_goal - (position.y + line_height))
                } else {
                    current_offset.y
                };

                scroll_handle.set_offset(point(
                    current_offset.x,
                    new_offset_y.clamp(-scroll_handle.max_offset().y, Pixels::ZERO),
                ));
            }
            AutoscrollBehavior::Propagate => {
                let text_style = self.style.base_text_style.clone();
                let font_id = window.text_system().resolve_font(&text_style.font());
                let font_size = text_style.font_size.to_pixels(window.rem_size());
                let em_width = window.text_system().em_width(font_id, font_size).unwrap();
                window.request_autoscroll(Bounds::from_corners(
                    point(position.x - 3. * em_width, position.y - 3. * line_height),
                    point(position.x + 3. * em_width, position.y + 3. * line_height),
                ));
            }
        }
        Some(())
    }

    fn on_mouse_event<T: MouseEvent>(
        &self,
        window: &mut Window,
        _cx: &mut App,
        mut f: impl 'static
        + FnMut(&mut Markdown, &T, DispatchPhase, &mut Window, &mut Context<Markdown>),
    ) {
        window.on_mouse_event({
            let markdown = self.markdown.downgrade();
            move |event, phase, window, cx| {
                markdown
                    .update(cx, |markdown, cx| f(markdown, event, phase, window, cx))
                    .log_err();
            }
        });
    }
}

impl Styled for MarkdownElement {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style.container_style
    }
}

impl Element for MarkdownElement {
    type RequestLayoutState = RenderedMarkdown;
    type PrepaintState = Hitbox;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (gpui::LayoutId, Self::RequestLayoutState) {
        let mut builder = MarkdownElementBuilder::new(
            &self.style.container_style,
            self.style.base_text_style.clone(),
            self.style.syntax.clone(),
        );
        let (
            parsed_markdown,
            images,
            active_root_block,
            render_mermaid_diagrams,
            mermaid_state,
            math_state,
        ) = {
            let markdown = self.markdown.read(cx);
            (
                markdown.parsed_markdown.clone(),
                markdown.images_by_source_offset.clone(),
                markdown.active_root_block,
                markdown.options.render_mermaid_diagrams,
                markdown.mermaid_state.clone(),
                markdown.math_state.clone(),
            )
        };
        let math_error_color = cx.theme().status().error;
        let markdown_end = if let Some(last) = parsed_markdown.events.last() {
            last.0.end
        } else {
            0
        };
        let mut code_block_ids = HashSet::default();

        let mut current_img_block_range: Option<Range<usize>> = None;
        let mut handled_html_block = false;
        let mut rendered_mermaid_block = false;
        let mut rendered_metadata_block = false;
        for (index, (range, event)) in parsed_markdown.events.iter().enumerate() {
            // Skip alt text for images that rendered
            if let Some(current_img_block_range) = &current_img_block_range
                && current_img_block_range.end > range.end
            {
                continue;
            }

            if handled_html_block {
                if let MarkdownEvent::End(MarkdownTagEnd::HtmlBlock) = event {
                    handled_html_block = false;
                } else {
                    continue;
                }
            }

            if rendered_mermaid_block {
                if matches!(event, MarkdownEvent::End(MarkdownTagEnd::CodeBlock)) {
                    rendered_mermaid_block = false;
                }
                continue;
            }

            if rendered_metadata_block {
                if matches!(event, MarkdownEvent::End(MarkdownTagEnd::MetadataBlock(_))) {
                    rendered_metadata_block = false;
                }
                continue;
            }

            match event {
                MarkdownEvent::RootStart => {
                    if self.show_root_block_markers {
                        builder.push_root_block(range, markdown_end);
                    }
                }
                MarkdownEvent::RootEnd(root_block_index) => {
                    if self.show_root_block_markers {
                        builder.pop_root_block(
                            active_root_block == Some(*root_block_index),
                            cx.theme().colors().border,
                            cx.theme().colors().border_variant,
                        );
                    }
                }
                MarkdownEvent::Start(tag) => {
                    match tag {
                        MarkdownTag::Image { dest_url, .. } => {
                            let alt_text = collect_image_alt_text(
                                &parsed_markdown.events[index..],
                                &parsed_markdown.source,
                            );
                            if let Some(image) = images.get(&range.start) {
                                current_img_block_range = Some(range.clone());
                                self.push_markdown_image(
                                    &mut builder,
                                    range,
                                    image.clone().into(),
                                    dest_url.clone(),
                                    alt_text,
                                    None,
                                    None,
                                );
                            } else if let Some(source) = self
                                .image_resolver
                                .as_ref()
                                .and_then(|resolve| resolve(dest_url.as_ref()))
                            {
                                current_img_block_range = Some(range.clone());
                                self.push_markdown_image(
                                    &mut builder,
                                    range,
                                    source,
                                    dest_url.clone(),
                                    alt_text,
                                    None,
                                    None,
                                );
                            }
                        }
                        MarkdownTag::Paragraph => {
                            let text_align_override = builder
                                .table
                                .current_cell_alignment()
                                .and_then(alignment_to_text_align);
                            self.push_markdown_paragraph(
                                &mut builder,
                                range,
                                markdown_end,
                                text_align_override,
                            );
                        }
                        MarkdownTag::Heading { level, .. } => {
                            let text_align_override = builder
                                .table
                                .current_cell_alignment()
                                .and_then(alignment_to_text_align);
                            self.push_markdown_heading(
                                &mut builder,
                                *level,
                                range,
                                markdown_end,
                                text_align_override,
                            );
                        }
                        MarkdownTag::BlockQuote(kind) => {
                            self.push_markdown_block_quote(
                                &mut builder,
                                *kind,
                                range,
                                markdown_end,
                            );
                        }
                        MarkdownTag::CodeBlock { kind, .. } => {
                            if render_mermaid_diagrams
                                && let Some(mermaid_diagram) =
                                    parsed_markdown.mermaid_diagrams.get(&range.start)
                            {
                                let showing_code =
                                    self.markdown.read(cx).is_mermaid_showing_code(range.start);
                                let copy_button_visibility = match &self.code_block_renderer {
                                    CodeBlockRenderer::Default {
                                        copy_button_visibility,
                                        ..
                                    } => *copy_button_visibility,
                                    _ => CopyButtonVisibility::VisibleOnHover,
                                };
                                builder.push_sourced_element(
                                    mermaid_diagram.content_range.clone(),
                                    render_mermaid_diagram(
                                        mermaid_diagram,
                                        &mermaid_state,
                                        &self.style,
                                        self.markdown.clone(),
                                        range.start,
                                        showing_code,
                                        copy_button_visibility,
                                    ),
                                );
                                rendered_mermaid_block = true;
                                continue;
                            }

                            let language = match kind {
                                CodeBlockKind::Fenced => None,
                                CodeBlockKind::FencedLang(language) => {
                                    parsed_markdown.languages_by_name.get(language).cloned()
                                }
                                CodeBlockKind::FencedSrc(path_range) => parsed_markdown
                                    .languages_by_path
                                    .get(&path_range.path)
                                    .cloned(),
                                _ => None,
                            };

                            let is_indented = matches!(kind, CodeBlockKind::Indented);
                            let scroll_handle = if self.style.code_block_overflow_x_scroll {
                                self.markdown.update(cx, |markdown, _| {
                                    markdown.code_block_scroll_handle(range.start)
                                })
                            } else {
                                None
                            };
                            if scroll_handle.is_some() {
                                code_block_ids.insert(range.start);
                            }

                            match (&self.code_block_renderer, is_indented) {
                                (CodeBlockRenderer::Default { .. }, _) | (_, true) => {
                                    // This is a parent container that we can position the copy button inside.
                                    let parent_container =
                                        div().group("code_block").relative().w_full();

                                    let mut parent_container: AnyDiv = if let Some(scroll_handle) =
                                        scroll_handle.as_ref()
                                    {
                                        let scrollbars = Scrollbars::new(ScrollAxes::Horizontal)
                                            .id(("markdown-code-block-scrollbar", range.start))
                                            .tracked_scroll_handle(scroll_handle)
                                            .with_track_along(
                                                ScrollAxes::Horizontal,
                                                cx.theme().colors().editor_background,
                                            )
                                            .notify_content();

                                        parent_container
                                            .rounded_lg()
                                            .custom_scrollbars(scrollbars, window, cx)
                                            .into()
                                    } else {
                                        parent_container.into()
                                    };

                                    if let CodeBlockRenderer::Default { border: true, .. } =
                                        &self.code_block_renderer
                                    {
                                        parent_container = parent_container
                                            .rounded_md()
                                            .border_1()
                                            .border_color(cx.theme().colors().border_variant);
                                    }

                                    parent_container.style().refine(&self.style.code_block);
                                    builder.push_div(parent_container, range, markdown_end);

                                    let code_block = div()
                                        .id(("code-block", range.start))
                                        .rounded_lg()
                                        .map(|mut code_block| {
                                            if let Some(scroll_handle) = scroll_handle.as_ref() {
                                                code_block.style().restrict_scroll_to_axis =
                                                    Some(true);
                                                code_block
                                                    .flex()
                                                    .overflow_x_scroll()
                                                    .track_scroll(scroll_handle)
                                            } else {
                                                code_block.w_full()
                                            }
                                        });

                                    builder.push_text_style(self.style.code_block.text.to_owned());
                                    builder.push_code_block(language);
                                    builder.push_div(code_block, range, markdown_end);
                                }
                                (CodeBlockRenderer::Custom { .. }, _) => {}
                            }
                        }
                        MarkdownTag::HtmlBlock => {
                            builder.push_div(div(), range, markdown_end);
                            if let Some(block) = parsed_markdown.html_blocks.get(&range.start) {
                                self.render_html_block(block, &mut builder, markdown_end, cx);
                                handled_html_block = true;
                            }
                        }
                        MarkdownTag::List(bullet_index) => {
                            builder.push_list(*bullet_index);
                            builder.push_div(div().pl_2p5(), range, markdown_end);
                        }
                        MarkdownTag::Item => {
                            let bullet =
                                if let Some((task_range, MarkdownEvent::TaskListMarker(checked))) =
                                    parsed_markdown.events.get(index.saturating_add(1))
                                {
                                    let source = &parsed_markdown.source()[range.clone()];
                                    let checked = *checked;
                                    let toggle_state = if checked {
                                        ToggleState::Selected
                                    } else {
                                        ToggleState::Unselected
                                    };

                                    let checkbox = Checkbox::new(
                                        ElementId::Name(source.to_string().into()),
                                        toggle_state,
                                    )
                                    .fill();

                                    if let Some(on_toggle) = self.on_checkbox_toggle.clone() {
                                        let task_source_range = task_range.clone();
                                        checkbox
                                            .on_click(move |_state, window, cx| {
                                                on_toggle(
                                                    task_source_range.clone(),
                                                    !checked,
                                                    window,
                                                    cx,
                                                );
                                            })
                                            .into_any_element()
                                    } else {
                                        checkbox.visualization_only(true).into_any_element()
                                    }
                                } else if let Some(bullet_index) = builder.next_bullet_index() {
                                    div().child(format!("{}.", bullet_index)).into_any_element()
                                } else {
                                    div().child("•").into_any_element()
                                };
                            self.push_markdown_list_item(&mut builder, bullet, range, markdown_end);
                        }
                        MarkdownTag::Emphasis => builder.push_text_style(TextStyleRefinement {
                            font_style: Some(FontStyle::Italic),
                            ..Default::default()
                        }),
                        MarkdownTag::Strong => builder.push_text_style(TextStyleRefinement {
                            font_weight: Some(FontWeight::BOLD),
                            color: Some(cx.theme().colors().text),
                            ..Default::default()
                        }),
                        MarkdownTag::Strikethrough => {
                            builder.push_text_style(TextStyleRefinement {
                                strikethrough: Some(StrikethroughStyle {
                                    thickness: px(1.),
                                    color: None,
                                }),
                                ..Default::default()
                            })
                        }
                        MarkdownTag::Link { dest_url, .. } => {
                            if builder.code_block_stack.is_empty() {
                                builder.link_depth += 1;
                                builder.push_link(dest_url.clone(), range.clone());
                                let style = self
                                    .style
                                    .link_callback
                                    .as_ref()
                                    .and_then(|callback| callback(dest_url, cx))
                                    .unwrap_or_else(|| self.style.link.clone());
                                builder.push_text_style(style)
                            }
                        }
                        MarkdownTag::FootnoteDefinition(label) => {
                            if !builder.rendered_footnote_separator {
                                builder.rendered_footnote_separator = true;
                                builder.push_div(
                                    div()
                                        .border_t_1()
                                        .mt_2()
                                        .border_color(self.style.rule_color),
                                    range,
                                    markdown_end,
                                );
                                builder.pop_div();
                            }
                            builder.push_div(
                                div()
                                    .pt_1()
                                    .mb_1()
                                    .line_height(rems(1.3))
                                    .text_size(rems(0.85))
                                    .h_flex()
                                    .items_start()
                                    .gap_2()
                                    .child(
                                        div().text_size(rems(0.85)).child(format!("{}.", label)),
                                    ),
                                range,
                                markdown_end,
                            );
                            builder.push_div(div().flex_1().w_0(), range, markdown_end);
                        }
                        MarkdownTag::MetadataBlock(_) => {
                            if let Some(metadata_block) =
                                parsed_markdown.metadata_blocks.get(&range.start)
                            {
                                self.push_metadata_block(
                                    &mut builder,
                                    &parsed_markdown.source,
                                    metadata_block,
                                    markdown_end,
                                    cx,
                                );
                                rendered_metadata_block = true;
                            }
                        }
                        MarkdownTag::Table(alignments) => {
                            builder.table.start(alignments.clone());

                            let column_count = alignments.len();
                            builder.push_div(
                                div()
                                    .id(("table", range.start))
                                    .grid()
                                    .grid_cols(column_count as u16)
                                    .when(self.style.table_columns_min_size, |this| {
                                        this.grid_cols_min_content(column_count as u16)
                                    })
                                    .when(!self.style.table_columns_min_size, |this| {
                                        this.grid_cols(column_count as u16)
                                    })
                                    .w_full()
                                    .mb_2()
                                    .border(px(1.5))
                                    .border_color(cx.theme().colors().border)
                                    .rounded_sm()
                                    .overflow_hidden(),
                                range,
                                markdown_end,
                            );
                        }
                        MarkdownTag::TableHead => {
                            builder.table.start_head();
                            builder.push_text_style(TextStyleRefinement {
                                font_weight: Some(FontWeight::SEMIBOLD),
                                ..Default::default()
                            });
                        }
                        MarkdownTag::TableRow => {
                            builder.table.start_row();
                        }
                        MarkdownTag::TableCell => {
                            let is_header = builder.table.in_head;
                            let row_index = builder.table.row_index;
                            let col_index = builder.table.col_index;
                            let alignment = builder.table.current_cell_alignment();
                            let text_align = alignment
                                .and_then(alignment_to_text_align)
                                .unwrap_or(self.style.base_text_style.text_align);

                            let mut cell_div = div()
                                .flex()
                                .flex_col()
                                .h_full()
                                .when(col_index > 0, |this| this.border_l_1())
                                .when(row_index > 0, |this| this.border_t_1())
                                .border_color(cx.theme().colors().border)
                                .px_1()
                                .py_0p5()
                                .when(is_header, |this| {
                                    this.bg(cx.theme().colors().title_bar_background)
                                })
                                .when(!is_header && row_index % 2 == 1, |this| {
                                    this.bg(cx.theme().colors().panel_background)
                                });

                            cell_div = match alignment {
                                Some(Alignment::Center) => cell_div.items_center(),
                                Some(Alignment::Right) => cell_div.items_end(),
                                _ => cell_div,
                            };

                            builder.push_text_style(TextStyleRefinement {
                                text_align: Some(text_align),
                                ..Default::default()
                            });
                            builder.push_div(cell_div, range, markdown_end);
                            builder.push_div(
                                div()
                                    .flex()
                                    .flex_col()
                                    .flex_1()
                                    .w_full()
                                    .justify_center()
                                    .text_align(text_align),
                                range,
                                markdown_end,
                            );
                            // Plain content div: `push_image_child` rewrites the *current* div
                            // into a wrapping flex row (for inline images/math). Keeping that on
                            // this innermost div preserves the flex_1/justify_center vertical
                            // centering of the div above it.
                            builder.push_div(
                                div().w_full().text_align(text_align),
                                range,
                                markdown_end,
                            );
                        }
                        _ => log::debug!("unsupported markdown tag {:?}", tag),
                    }
                }
                MarkdownEvent::End(tag) => match tag {
                    MarkdownTagEnd::Image => {
                        current_img_block_range.take();
                    }
                    MarkdownTagEnd::Paragraph => {
                        self.pop_markdown_paragraph(&mut builder);
                    }
                    MarkdownTagEnd::Heading(_) => {
                        self.pop_markdown_heading(&mut builder);
                    }
                    MarkdownTagEnd::BlockQuote(_kind) => {
                        self.pop_markdown_block_quote(&mut builder);
                    }
                    MarkdownTagEnd::CodeBlock => {
                        builder.trim_trailing_newline();

                        builder.pop_div();
                        builder.pop_code_block();
                        builder.pop_text_style();

                        if let CodeBlockRenderer::Default {
                            copy_button_visibility,
                            wrap_button_visibility,
                            ..
                        } = &self.code_block_renderer
                            && (*copy_button_visibility != CopyButtonVisibility::Hidden
                                || *wrap_button_visibility != WrapButtonVisibility::Hidden)
                        {
                            let copy_button_visibility = *copy_button_visibility;
                            let wrap_button_visibility = *wrap_button_visibility;
                            builder.modify_current_div(|el| {
                                let content_range = parser::extract_code_block_content_range(
                                    &parsed_markdown.source()[range.clone()],
                                );
                                let content_range = content_range.start + range.start
                                    ..content_range.end + range.start;

                                let code = parsed_markdown.source()[content_range].to_string();

                                let any_hover = copy_button_visibility
                                    == CopyButtonVisibility::VisibleOnHover
                                    || wrap_button_visibility
                                        == WrapButtonVisibility::VisibleOnHover;
                                let any_always = copy_button_visibility
                                    == CopyButtonVisibility::AlwaysVisible
                                    || wrap_button_visibility
                                        == WrapButtonVisibility::AlwaysVisible;
                                let use_hover = any_hover && !any_always;

                                let button_row = h_flex()
                                    .gap_0p5()
                                    .absolute()
                                    .bg(cx.theme().colors().editor_background)
                                    .when_else(
                                        use_hover,
                                        |this| {
                                            this.top_1().right_1().visible_on_hover("code_block")
                                        },
                                        |this| this.top_1p5().right_1p5(),
                                    )
                                    .when(
                                        wrap_button_visibility != WrapButtonVisibility::Hidden,
                                        |this| {
                                            let is_wrapped = self
                                                .markdown
                                                .read(cx)
                                                .is_code_block_wrapped(range.start);

                                            this.child(render_wrap_code_block_button(
                                                range.start,
                                                is_wrapped,
                                                self.markdown.clone(),
                                            ))
                                        },
                                    )
                                    .when(
                                        copy_button_visibility != CopyButtonVisibility::Hidden,
                                        |this| {
                                            this.child(render_copy_code_block_button(
                                                range.end,
                                                code,
                                                self.markdown.clone(),
                                            ))
                                        },
                                    );

                                el.child(button_row)
                            });
                        }

                        // Pop the parent container.
                        builder.pop_div();
                    }
                    MarkdownTagEnd::HtmlBlock => builder.pop_div(),
                    MarkdownTagEnd::List(_) => {
                        builder.pop_list();
                        builder.pop_div();
                    }
                    MarkdownTagEnd::Item => {
                        self.pop_markdown_list_item(&mut builder);
                    }
                    MarkdownTagEnd::Emphasis => builder.pop_text_style(),
                    MarkdownTagEnd::Strong => builder.pop_text_style(),
                    MarkdownTagEnd::Strikethrough => builder.pop_text_style(),
                    MarkdownTagEnd::Link => {
                        if builder.code_block_stack.is_empty() {
                            builder.link_depth = builder.link_depth.saturating_sub(1);
                            builder.pop_text_style()
                        }
                    }
                    MarkdownTagEnd::Table => {
                        builder.pop_div();
                        builder.table.end();
                    }
                    MarkdownTagEnd::TableHead => {
                        builder.pop_text_style();
                        builder.table.end_head();
                    }
                    MarkdownTagEnd::TableRow => {
                        builder.table.end_row();
                    }
                    MarkdownTagEnd::TableCell => {
                        builder.replace_pending_checkbox(self.on_checkbox_toggle.clone());
                        builder.pop_div();
                        builder.pop_div();
                        builder.pop_div();
                        builder.pop_text_style();
                        builder.table.end_cell();
                    }
                    MarkdownTagEnd::FootnoteDefinition => {
                        builder.pop_div();
                        builder.pop_div();
                    }
                    MarkdownTagEnd::MetadataBlock(_) => {}
                    _ => log::debug!("unsupported markdown tag end: {:?}", tag),
                },
                MarkdownEvent::Text => {
                    builder.push_text(&parsed_markdown.source[range.clone()], range.clone());
                }
                MarkdownEvent::SubstitutedText(text) => {
                    builder.push_text(text, range.clone());
                }
                MarkdownEvent::Code => {
                    self.push_markdown_code_span(
                        &mut builder,
                        &parsed_markdown.source[range.clone()],
                        range.clone(),
                        cx,
                    );
                }
                MarkdownEvent::SubstitutedCode(text) => {
                    self.push_markdown_code_span(&mut builder, text, range.clone(), cx);
                }
                MarkdownEvent::Html => {
                    let html = &parsed_markdown.source[range.clone()];
                    if html.starts_with("<!--") {
                        builder.html_comment = true;
                    }
                    if html.trim_end().ends_with("-->") {
                        builder.html_comment = false;
                        continue;
                    }
                    if builder.html_comment {
                        continue;
                    }
                    builder.push_text(html, range.clone());
                }
                MarkdownEvent::InlineHtml => {
                    let html = &parsed_markdown.source[range.clone()];
                    if let Some(code) = html
                        .strip_prefix("<code>")
                        .and_then(|html| html.strip_suffix("</code>"))
                    {
                        let code_start = range.start + "<code>".len();
                        self.push_markdown_code_span(
                            &mut builder,
                            code,
                            code_start..code_start + code.len(),
                            cx,
                        );
                        continue;
                    }
                    if html.starts_with("<code>") {
                        builder.push_text_style(self.style.inline_code.clone());
                        continue;
                    }
                    if html.trim_end().starts_with("</code>") {
                        builder.pop_text_style();
                        continue;
                    }
                    builder.push_text(&parsed_markdown.source[range.clone()], range.clone());
                }
                MarkdownEvent::Rule => {
                    builder.push_div(
                        div()
                            .border_b_1()
                            .my_2()
                            .border_color(self.style.rule_color),
                        range,
                        markdown_end,
                    );
                    builder.pop_div()
                }
                MarkdownEvent::SoftBreak if !self.style.soft_break_as_hard_break => {
                    builder.push_soft_break(range.clone());
                }
                MarkdownEvent::SoftBreak | MarkdownEvent::HardBreak => {
                    builder.push_line_break(range.clone());
                }
                MarkdownEvent::TaskListMarker(_) => {
                    // handled inside the `MarkdownTag::Item` case
                }
                MarkdownEvent::InlineMath(latex) => {
                    self.push_markdown_math(
                        &mut builder,
                        &parsed_markdown.source,
                        latex,
                        range.clone(),
                        false,
                        &math_state,
                        math_error_color,
                    );
                }
                MarkdownEvent::DisplayMath(latex) => {
                    self.push_markdown_math(
                        &mut builder,
                        &parsed_markdown.source,
                        latex,
                        range.clone(),
                        true,
                        &math_state,
                        math_error_color,
                    );
                }
                MarkdownEvent::FootnoteReference(label) => {
                    builder.push_footnote_ref(label.clone(), range.clone());
                    builder.push_text_style(self.style.link.clone());
                    builder.push_text(&format!("[{label}]"), range.clone());
                    builder.pop_text_style();
                }
            }
        }
        if self.style.code_block_overflow_x_scroll {
            let code_block_ids = code_block_ids;
            self.markdown.update(cx, move |markdown, _| {
                markdown.retain_code_block_scroll_handles(&code_block_ids);
            });
        } else {
            self.markdown
                .update(cx, |markdown, _| markdown.clear_code_block_scroll_handles());
        }
        let mut rendered_markdown = builder.build();
        let child_layout_id = rendered_markdown.element.request_layout(window, cx);
        let layout_id = window.request_layout(gpui::Style::default(), [child_layout_id], cx);
        (layout_id, rendered_markdown)
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        rendered_markdown: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let focus_handle = self.markdown.read(cx).focus_handle.clone();
        window.set_focus_handle(&focus_handle, cx);
        window.set_view_id(self.markdown.entity_id());

        let hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);
        rendered_markdown.element.prepaint(window, cx);
        self.autoscroll(&rendered_markdown.text, window, cx);
        hitbox
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        _bounds: Bounds<Pixels>,
        rendered_markdown: &mut Self::RequestLayoutState,
        hitbox: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let mut context = KeyContext::default();
        context.add("Markdown");
        window.set_key_context(context);
        window.on_action(std::any::TypeId::of::<crate::Copy>(), {
            let entity = self.markdown.clone();
            let text = rendered_markdown.text.clone();
            move |_, phase, window, cx| {
                let text = text.clone();
                if phase == DispatchPhase::Bubble {
                    entity.update(cx, move |this, cx| this.copy(&text, window, cx))
                }
            }
        });
        window.on_action(std::any::TypeId::of::<crate::CopyAsMarkdown>(), {
            let entity = self.markdown.clone();
            move |_, phase, window, cx| {
                if phase == DispatchPhase::Bubble {
                    entity.update(cx, move |this, cx| this.copy_as_markdown(window, cx))
                }
            }
        });

        self.paint_mouse_listeners(hitbox, &rendered_markdown.text, window, cx);
        rendered_markdown.element.paint(window, cx);
        self.paint_search_highlights(&rendered_markdown.text, window, cx);
        self.paint_selection(&rendered_markdown.text, window, cx);
    }
}

fn collect_image_alt_text(
    events_from_image_start: &[(Range<usize>, MarkdownEvent)],
    source: &str,
) -> Option<SharedString> {
    let mut alt_text = String::new();
    for (range, event) in events_from_image_start.iter().skip(1) {
        match event {
            MarkdownEvent::End(MarkdownTagEnd::Image) => break,
            MarkdownEvent::Text => alt_text.push_str(&source[range.clone()]),
            _ => {}
        }
    }
    if alt_text.is_empty() {
        None
    } else {
        Some(alt_text.into())
    }
}

fn image_fallback_element(
    dest_url: SharedString,
    alt_text: Option<SharedString>,
    open_image_url_on_click: bool,
) -> AnyElement {
    let link_label = alt_text
        .filter(|alt| !alt.is_empty())
        .unwrap_or_else(|| dest_url.clone());

    let label = format!("Failed to Load: {link_label}");

    div()
        .id("image-fallback")
        .min_w_0()
        .child(Label::new(label).color(Color::Warning).underline())
        .tooltip(Tooltip::text(
            "Image failed to load. Open `zed: log` for more details.",
        ))
        .when(open_image_url_on_click, |this| {
            this.cursor_pointer()
                .on_click(move |_, _, cx| cx.open_url(&dest_url))
        })
        .into_any_element()
}

fn apply_heading_style(
    mut heading: Div,
    level: pulldown_cmark::HeadingLevel,
    custom_styles: Option<&HeadingLevelStyles>,
    border_color: Option<Hsla>,
) -> Div {
    heading = match level {
        pulldown_cmark::HeadingLevel::H1 => heading.text_3xl(),
        pulldown_cmark::HeadingLevel::H2 => heading.text_2xl(),
        pulldown_cmark::HeadingLevel::H3 => heading.text_xl(),
        pulldown_cmark::HeadingLevel::H4 => heading.text_lg(),
        pulldown_cmark::HeadingLevel::H5 => heading.text_base(),
        pulldown_cmark::HeadingLevel::H6 => heading.text_sm(),
    };

    heading = match level {
        pulldown_cmark::HeadingLevel::H1 => heading,
        _ => heading.mt_6(),
    };

    if let Some(border_color) = border_color
        && matches!(
            level,
            pulldown_cmark::HeadingLevel::H1
                | pulldown_cmark::HeadingLevel::H2
                | pulldown_cmark::HeadingLevel::H3
        )
    {
        heading = heading.pb_1().border_b_1().border_color(border_color);
    }

    if let Some(styles) = custom_styles {
        let style_opt = match level {
            pulldown_cmark::HeadingLevel::H1 => &styles.h1,
            pulldown_cmark::HeadingLevel::H2 => &styles.h2,
            pulldown_cmark::HeadingLevel::H3 => &styles.h3,
            pulldown_cmark::HeadingLevel::H4 => &styles.h4,
            pulldown_cmark::HeadingLevel::H5 => &styles.h5,
            pulldown_cmark::HeadingLevel::H6 => &styles.h6,
        };

        if let Some(style) = style_opt {
            heading.style().text = style.clone();
        }
    }

    heading
}

fn render_wrap_code_block_button(
    id: usize,
    is_wrapped: bool,
    markdown: Entity<Markdown>,
) -> impl IntoElement {
    let (icon, tooltip) = if is_wrapped {
        (IconName::TextUnwrap, "Unwrap Content")
    } else {
        (IconName::TextWrap, "Wrap Content")
    };
    let button_id = ElementId::NamedChild(
        Arc::new(ElementId::from(("wrap-code-block", markdown.entity_id()))),
        id.to_string().into(),
    );

    IconButton::new(button_id, icon)
        .icon_size(IconSize::Small)
        .icon_color(Color::Muted)
        .tooltip(Tooltip::text(tooltip))
        .on_click(move |_event, _window, cx| {
            markdown.update(cx, |markdown, cx| {
                markdown.toggle_code_block_wrap(id);
                cx.notify();
            });
        })
}

fn render_copy_code_block_button(
    id: usize,
    code: String,
    markdown: Entity<Markdown>,
) -> impl IntoElement {
    let id = ElementId::NamedChild(
        Arc::new(ElementId::from((
            "copy-markdown-code",
            markdown.entity_id(),
        ))),
        id.to_string().into(),
    );

    CopyButton::new(id.clone(), code.clone()).custom_on_click({
        let markdown = markdown;
        move |_window, cx| {
            let id = id.clone();
            markdown.update(cx, |this, cx| {
                this.copied_code_blocks.insert(id.clone());

                cx.write_to_clipboard(ClipboardItem::new_string(code.clone()));

                cx.spawn(async move |this, cx| {
                    cx.background_executor().timer(Duration::from_secs(2)).await;

                    cx.update(|cx| {
                        this.update(cx, |this, cx| {
                            this.copied_code_blocks.remove(&id);
                            cx.notify();
                        })
                    })
                    .ok();
                })
                .detach();
            });
        }
    })
}

impl IntoElement for MarkdownElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

pub enum AnyDiv {
    Div(Div),
    Stateful(Stateful<Div>),
}

impl AnyDiv {
    fn into_any_element(self) -> AnyElement {
        match self {
            Self::Div(div) => div.into_any_element(),
            Self::Stateful(div) => div.into_any_element(),
        }
    }
}

impl From<Div> for AnyDiv {
    fn from(value: Div) -> Self {
        Self::Div(value)
    }
}

impl From<Stateful<Div>> for AnyDiv {
    fn from(value: Stateful<Div>) -> Self {
        Self::Stateful(value)
    }
}

impl Styled for AnyDiv {
    fn style(&mut self) -> &mut StyleRefinement {
        match self {
            Self::Div(div) => div.style(),
            Self::Stateful(div) => div.style(),
        }
    }
}

impl ParentElement for AnyDiv {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        match self {
            Self::Div(div) => div.extend(elements),
            Self::Stateful(div) => div.extend(elements),
        }
    }
}

#[derive(Default)]
struct TableState {
    alignments: Vec<Alignment>,
    in_head: bool,
    row_index: usize,
    col_index: usize,
}

impl TableState {
    fn start(&mut self, alignments: Vec<Alignment>) {
        self.alignments = alignments;
        self.in_head = false;
        self.row_index = 0;
        self.col_index = 0;
    }

    fn end(&mut self) {
        self.alignments.clear();
        self.in_head = false;
        self.row_index = 0;
        self.col_index = 0;
    }

    fn start_head(&mut self) {
        self.in_head = true;
    }

    fn end_head(&mut self) {
        self.in_head = false;
    }

    fn start_row(&mut self) {
        self.col_index = 0;
    }

    fn end_row(&mut self) {
        self.row_index += 1;
    }

    fn end_cell(&mut self) {
        self.col_index += 1;
    }

    fn current_cell_alignment(&self) -> Option<Alignment> {
        if self.alignments.is_empty() {
            return None;
        }
        if self.in_head {
            return Some(Alignment::Center);
        }
        self.alignments.get(self.col_index).copied()
    }
}

fn alignment_to_text_align(alignment: Alignment) -> Option<TextAlign> {
    match alignment {
        Alignment::Left => Some(TextAlign::Left),
        Alignment::Center => Some(TextAlign::Center),
        Alignment::Right => Some(TextAlign::Right),
        Alignment::None => None,
    }
}

struct MetadataCellStyle {
    row_index: usize,
    is_key: bool,
}

struct MarkdownElementBuilder {
    div_stack: Vec<DivStackEntry>,
    rendered_lines: Vec<RenderedLine>,
    math_regions: Vec<MathRegion>,
    pending_line: PendingLine,
    rendered_links: Vec<RenderedLink>,
    rendered_image_links: Vec<RenderedImageLink>,
    rendered_footnote_refs: Vec<RenderedFootnoteRef>,
    current_source_index: usize,
    html_comment: bool,
    rendered_footnote_separator: bool,
    base_text_style: TextStyle,
    text_style_stack: Vec<TextStyleRefinement>,
    code_block_stack: Vec<Option<Arc<Language>>>,
    link_depth: usize,
    list_stack: Vec<ListStackEntry>,
    table: TableState,
    syntax_theme: Arc<SyntaxTheme>,
}

struct DivStackEntry {
    div: AnyDiv,
    line_break_mode: LineBreakMode,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum LineBreakMode {
    TextLayout,
    FlexWrap,
}

impl DivStackEntry {
    fn new(div: impl Into<AnyDiv>) -> Self {
        Self {
            div: div.into(),
            line_break_mode: LineBreakMode::TextLayout,
        }
    }
}

#[derive(Default)]
struct PendingLine {
    text: String,
    runs: Vec<TextRun>,
    source_mappings: Vec<SourceMapping>,
    /// Drawn inline formulas embedded in this line's text. When non-empty, `flush_text` emits an
    /// [`InlineFlowElement`] (text and math on shared wrapped rows) instead of a `StyledText`.
    math_spans: Vec<MathSpan>,
}

struct ListStackEntry {
    bullet_index: Option<u64>,
}

impl MarkdownElementBuilder {
    fn new(
        container_style: &StyleRefinement,
        base_text_style: TextStyle,
        syntax_theme: Arc<SyntaxTheme>,
    ) -> Self {
        Self {
            div_stack: vec![{
                let mut base_div = div();
                base_div.style().refine(container_style);
                DivStackEntry::new(base_div.debug_selector(|| "inner".into()))
            }],
            rendered_lines: Vec::new(),
            math_regions: Vec::new(),
            pending_line: PendingLine::default(),
            rendered_links: Vec::new(),
            rendered_image_links: Vec::new(),
            rendered_footnote_refs: Vec::new(),
            current_source_index: 0,
            html_comment: false,
            rendered_footnote_separator: false,
            base_text_style,
            text_style_stack: Vec::new(),
            code_block_stack: Vec::new(),
            link_depth: 0,
            list_stack: Vec::new(),
            table: TableState::default(),
            syntax_theme,
        }
    }

    fn push_text_style(&mut self, style: TextStyleRefinement) {
        self.text_style_stack.push(style);
    }

    fn text_style(&self) -> TextStyle {
        let mut style = self.base_text_style.clone();
        for refinement in &self.text_style_stack {
            style.refine(refinement);
        }
        style
    }

    fn pop_text_style(&mut self) {
        self.text_style_stack.pop();
    }

    fn push_div(&mut self, div: impl Into<AnyDiv>, range: &Range<usize>, markdown_end: usize) {
        let mut div = div.into();
        self.flush_text();

        if range.start == 0 {
            // Remove the top margin on the first element.
            div.style().refine(&StyleRefinement {
                margin: gpui::EdgesRefinement {
                    top: Some(Length::Definite(px(0.).into())),
                    left: None,
                    right: None,
                    bottom: None,
                },
                ..Default::default()
            });
        }

        if range.end == markdown_end {
            div.style().refine(&StyleRefinement {
                margin: gpui::EdgesRefinement {
                    top: None,
                    left: None,
                    right: None,
                    bottom: Some(Length::Definite(rems(0.).into())),
                },
                ..Default::default()
            });
        }

        self.div_stack.push(DivStackEntry::new(div));
    }

    fn push_root_block(&mut self, range: &Range<usize>, markdown_end: usize) {
        self.push_div(
            div().group("markdown-root-block").relative(),
            range,
            markdown_end,
        );
        self.push_div(div().pl_4(), range, markdown_end);
    }

    /// Switch the current div into flex-wrap mode (a flex row that wraps) and flush any pending
    /// text first. Setting the mode *before* the flush is what lets the run preceding the child
    /// — e.g. the text before the *first* inline formula or image — be emitted as a wrappable
    /// flex item rather than a bare `StyledText` that overflows the container. Idempotent.
    fn begin_flex_wrap(&mut self) {
        if let Some(entry) = self.div_stack.last_mut() {
            entry.line_break_mode = LineBreakMode::FlexWrap;
        }
        self.modify_current_div(|el| el.flex().flex_row().flex_wrap().items_start());
    }

    fn push_image_child(&mut self, child: impl IntoElement) {
        self.begin_flex_wrap();
        self.append_child(child.into_any_element());
    }

    fn push_line_break(&mut self, source_range: Range<usize>) {
        if self.uses_flex_line_breaks() {
            self.modify_current_div(|el| el.child(div().w_full().h_0()));
        } else {
            self.push_text("\n", source_range);
        }
    }

    fn push_soft_break(&mut self, source_range: Range<usize>) {
        // A soft break right after an item in flex wrap container would otherwise
        // render as a stray leading space before the next wrapped item.
        if self.uses_flex_line_breaks() && self.pending_line.text.is_empty() {
            return;
        }
        self.push_text(" ", source_range);
    }

    fn append_child(&mut self, child: AnyElement) {
        self.div_stack.last_mut().unwrap().div.extend([child]);
    }

    fn uses_flex_line_breaks(&self) -> bool {
        self.div_stack
            .last()
            .is_some_and(|entry| entry.line_break_mode == LineBreakMode::FlexWrap)
    }

    fn modify_current_div(&mut self, f: impl FnOnce(AnyDiv) -> AnyDiv) {
        self.flush_text();
        if let Some(mut entry) = self.div_stack.pop() {
            entry.div = f(entry.div);
            self.div_stack.push(entry);
        }
    }

    fn pop_root_block(
        &mut self,
        is_active: bool,
        active_gutter_color: Hsla,
        hovered_gutter_color: Hsla,
    ) {
        self.pop_div();
        self.modify_current_div(|el| {
            el.child(
                div()
                    .h_full()
                    .w(px(4.0))
                    .when(is_active, |this| this.bg(active_gutter_color))
                    .group_hover("markdown-root-block", |this| {
                        if is_active {
                            this
                        } else {
                            this.bg(hovered_gutter_color)
                        }
                    })
                    .rounded_xs()
                    .absolute()
                    .left_0()
                    .top_0(),
            )
        });
        self.pop_div();
    }

    fn pop_div(&mut self) {
        self.flush_text();
        let div = self.div_stack.pop().unwrap().div.into_any_element();
        self.append_child(div);
    }

    fn push_sourced_element(&mut self, source_range: Range<usize>, element: impl Into<AnyElement>) {
        self.flush_text();
        let anchor = self.render_source_anchor(source_range);
        self.append_child(
            div()
                .relative()
                .child(anchor)
                .child(element.into())
                .into_any_element(),
        );
    }

    fn push_list(&mut self, bullet_index: Option<u64>) {
        self.list_stack.push(ListStackEntry { bullet_index });
    }

    fn next_bullet_index(&mut self) -> Option<u64> {
        self.list_stack.last_mut().and_then(|entry| {
            let item_index = entry.bullet_index.as_mut()?;
            *item_index += 1;
            Some(*item_index - 1)
        })
    }

    fn pop_list(&mut self) {
        self.list_stack.pop();
    }

    fn push_code_block(&mut self, language: Option<Arc<Language>>) {
        self.code_block_stack.push(language);
    }

    fn pop_code_block(&mut self) {
        self.code_block_stack.pop();
    }

    fn push_link(&mut self, destination_url: SharedString, source_range: Range<usize>) {
        self.rendered_links.push(RenderedLink {
            source_range,
            destination_url,
        });
    }

    fn push_image_link(
        &mut self,
        destination_url: SharedString,
        bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    ) {
        self.rendered_image_links.push(RenderedImageLink {
            bounds,
            destination_url,
        });
    }

    fn push_footnote_ref(&mut self, label: SharedString, source_range: Range<usize>) {
        self.rendered_footnote_refs.push(RenderedFootnoteRef {
            source_range,
            label,
        });
    }

    fn push_text(&mut self, text: &str, source_range: Range<usize>) {
        self.pending_line.source_mappings.push(SourceMapping {
            rendered_index: self.pending_line.text.len(),
            source_index: source_range.start,
        });
        self.pending_line.text.push_str(text);
        self.current_source_index = source_range.end;

        // Compute the base text style once
        let text_style = self.text_style();

        if let Some(Some(language)) = self.code_block_stack.last() {
            let mut offset = 0;
            for (range, highlight_id) in language.highlight_text(&Rope::from(text), 0..text.len()) {
                if range.start > offset {
                    self.pending_line
                        .runs
                        .push(text_style.to_run(range.start - offset));
                }

                let run_len = range.len();
                if let Some(highlight) = self.syntax_theme.get(highlight_id).cloned() {
                    self.pending_line
                        .runs
                        .push(text_style.clone().highlight(highlight).to_run(run_len));
                } else {
                    self.pending_line.runs.push(text_style.to_run(run_len));
                }
                offset = range.end;
            }

            if offset < text.len() {
                self.pending_line
                    .runs
                    .push(text_style.to_run(text.len() - offset));
            }
        } else {
            self.pending_line.runs.push(text_style.to_run(text.len()));
        }
    }

    /// Embed a drawn inline formula into the pending line. `source_text` is the verbatim `$...$`
    /// source (its byte length must equal `source_range.len()`); `elements` are the formula's
    /// fragments. The source is appended as the span's stand-in text so selection/copy read it in
    /// order, a placeholder run keeps run coverage contiguous (these bytes are never shaped), and a
    /// `MathRegion` records the atomic source span plus the fragments' painted-bounds slots.
    fn push_inline_math_span(
        &mut self,
        source_text: &str,
        source_range: Range<usize>,
        mut elements: Vec<MathElement>,
    ) {
        let mut bounds_cells = Vec::with_capacity(elements.len());
        for element in &mut elements {
            let slot: MathBoundsSlot = Rc::new(Cell::new(None));
            element.set_bounds_slot(slot.clone());
            bounds_cells.push(slot);
        }
        self.push_math_region(source_range.clone(), bounds_cells);

        let advance_em: f32 = elements.iter().map(MathElement::inline_advance_em).sum();
        // All fragments of one formula share the same baseline extents; `max` collapses to that.
        let (ascent_em, descent_em) = elements
            .iter()
            .filter_map(MathElement::inline_line_extents)
            .fold((0.0_f32, 0.0_f32), |(ascent, depth), (height, deep)| {
                (ascent.max(height), depth.max(deep))
            });

        let byte_start = self.pending_line.text.len();
        self.pending_line.source_mappings.push(SourceMapping {
            rendered_index: byte_start,
            source_index: source_range.start,
        });
        self.pending_line.text.push_str(source_text);
        self.pending_line
            .runs
            .push(self.text_style().to_run(source_text.len()));
        self.current_source_index = source_range.end;
        self.pending_line.math_spans.push(MathSpan {
            byte_range: byte_start..byte_start + source_text.len(),
            elements,
            advance_em,
            ascent_em,
            descent_em,
        });
    }

    fn trim_trailing_newline(&mut self) {
        if self.pending_line.text.ends_with('\n') {
            self.pending_line
                .text
                .truncate(self.pending_line.text.len() - 1);
            self.pending_line.runs.last_mut().unwrap().len -= 1;
            self.current_source_index -= 1;
        }
    }

    fn replace_pending_checkbox(&mut self, on_toggle: Option<CheckboxToggleCallback>) {
        let text = &self.pending_line.text;
        let trimmed = text.trim();
        if trimmed != "[x]" && trimmed != "[X]" && trimmed != "[ ]" {
            return;
        }
        let checked = trimmed != "[ ]";

        let leading_ws = text.len() - text.trim_start().len();
        let marker_rendered = leading_ws..leading_ws + trimmed.len();
        let marker_source = self
            .source_range_for_rendered(&marker_rendered)
            .expect("pending checkbox text must have source mappings");

        self.pending_line = PendingLine::default();

        let toggle_state = if checked {
            ToggleState::Selected
        } else {
            ToggleState::Unselected
        };
        let checkbox = Checkbox::new(
            ElementId::Name(
                format!(
                    "table_checkbox_{}_{}",
                    marker_source.start, marker_source.end
                )
                .into(),
            ),
            toggle_state,
        )
        .fill();

        let checkbox = if let Some(on_toggle) = on_toggle {
            checkbox
                .on_click(move |_state, window, cx| {
                    on_toggle(marker_source.clone(), !checked, window, cx);
                })
                .into_any_element()
        } else {
            checkbox.visualization_only(true).into_any_element()
        };

        let mut checkbox_container = h_flex().w_full();
        checkbox_container = match self.text_style().text_align {
            TextAlign::Left => checkbox_container.justify_start(),
            TextAlign::Center => checkbox_container.justify_center(),
            TextAlign::Right => checkbox_container.justify_end(),
        };

        self.append_child(checkbox_container.child(checkbox).into_any_element());
    }

    fn source_range_for_rendered(&self, rendered: &Range<usize>) -> Option<Range<usize>> {
        source_range_for_rendered(&self.pending_line.source_mappings, rendered)
    }

    fn render_source_anchor(&mut self, source_range: Range<usize>) -> AnyElement {
        let mut text_style = self.base_text_style.clone();
        text_style.color = Hsla::transparent_black();
        let text = "\u{200B}";
        let styled_text = StyledText::new(text).with_runs(vec![text_style.to_run(text.len())]);
        self.rendered_lines.push(RenderedLine {
            layout: LineGeometry::Text(styled_text.layout().clone()),
            source_mappings: vec![SourceMapping {
                rendered_index: 0,
                source_index: source_range.start,
            }],
            source_end: source_range.end,
            language: None,
            text_align: TextAlign::Left,
            is_math_anchor: false,
        });
        div()
            .absolute()
            .top_0()
            .left_0()
            .opacity(0.)
            .child(styled_text)
            .into_any_element()
    }

    /// Like [`render_source_anchor`], but the (invisible) text carries the element's source
    /// span verbatim so selection/copy reproduces it. Used for math, which paints separately
    /// and otherwise contributes no copyable text. `source_text` must equal the source slice
    /// for `source_range` so rendered and source offsets stay aligned.
    fn render_copyable_source_anchor(
        &mut self,
        source_text: &str,
        source_range: Range<usize>,
    ) -> AnyElement {
        let mut text_style = self.base_text_style.clone();
        text_style.color = Hsla::transparent_black();
        let styled_text = StyledText::new(source_text.to_string())
            .with_runs(vec![text_style.to_run(source_text.len())]);
        self.rendered_lines.push(RenderedLine {
            layout: LineGeometry::Text(styled_text.layout().clone()),
            source_mappings: vec![SourceMapping {
                rendered_index: 0,
                source_index: source_range.start,
            }],
            source_end: source_range.end,
            language: None,
            text_align: TextAlign::Left,
            is_math_anchor: true,
        });
        div()
            .absolute()
            .top_0()
            .left_0()
            .opacity(0.)
            .child(styled_text)
            .into_any_element()
    }

    /// Register a drawn formula's source span together with the bounds cells its `MathElement`
    /// piece(s) publish during prepaint, so selection can hit-test/highlight where math is drawn.
    fn push_math_region(&mut self, source_range: Range<usize>, bounds_cells: Vec<MathBoundsSlot>) {
        self.math_regions.push(MathRegion {
            source_range,
            bounds_cells,
        });
    }

    fn flush_text(&mut self) {
        let text_align = self.text_style().text_align;
        let line = mem::take(&mut self.pending_line);
        if line.text.is_empty() {
            return;
        }

        // A line carrying drawn inline formulas flows text and math on shared wrapped rows via a
        // custom element, so a formula sits after the text before it. Plain lines stay on the
        // `StyledText` path unchanged.
        if !line.math_spans.is_empty() {
            let layout = InlineFlowLayout::default();
            self.rendered_lines.push(RenderedLine {
                layout: LineGeometry::InlineFlow(layout.clone()),
                source_mappings: line.source_mappings,
                source_end: self.current_source_index,
                language: self.code_block_stack.last().cloned().flatten(),
                text_align,
                is_math_anchor: false,
            });
            self.append_child(
                InlineFlowElement {
                    text: SharedString::from(line.text),
                    runs: line.runs,
                    math_spans: line.math_spans,
                    text_align,
                    layout,
                    laid_out_math: Vec::new(),
                }
                .into_any_element(),
            );
            return;
        }

        let text = StyledText::new(line.text).with_runs(line.runs);
        self.rendered_lines.push(RenderedLine {
            layout: LineGeometry::Text(text.layout().clone()),
            source_mappings: line.source_mappings,
            source_end: self.current_source_index,
            language: self.code_block_stack.last().cloned().flatten(),
            text_align,
            is_math_anchor: false,
        });
        // In flex-wrap mode each text run is an independent flex item. `StyledText` only
        // wraps when it is handed a definite width (see `TextLayout::layout`), and taffy
        // won't shrink a lone over-wide item on its own, so `max_w_full` clamps the item to
        // the container width and forces the wrap; `min_w_0` lets it shrink further if
        // needed. Short runs stay below the cap and sit inline with the math (same idea as
        // the image path's min_w_0/max_w_full). Plain block layout wraps on its own, so only
        // constrain the flex-item case.
        if self.uses_flex_line_breaks() {
            self.append_child(div().min_w_0().max_w_full().child(text).into_any_element());
        } else {
            self.append_child(text.into_any());
        }
    }

    fn build(mut self) -> RenderedMarkdown {
        debug_assert_eq!(self.div_stack.len(), 1);
        self.flush_text();
        RenderedMarkdown {
            element: self.div_stack.pop().unwrap().div.into_any_element(),
            text: RenderedText {
                lines: self.rendered_lines.into(),
                links: self.rendered_links.into(),
                image_links: self.rendered_image_links.into(),
                footnote_refs: self.rendered_footnote_refs.into(),
                math_regions: self.math_regions.into(),
            },
        }
    }
}

/// Horizontal offset applied to a laid-out segment so it honors the line's text alignment.
fn alignment_offset(text_align: TextAlign, available_width: Pixels, segment_width: Pixels) -> Pixels {
    match text_align {
        TextAlign::Left => px(0.),
        TextAlign::Center => ((available_width - segment_width) / 2.).max(px(0.)),
        TextAlign::Right => (available_width - segment_width).max(px(0.)),
    }
}

/// A drawn inline formula embedded in an [`InlineFlowElement`]'s text. `byte_range` locates its
/// stand-in bytes (the verbatim `$...$` source) within the flow's rendered string; `elements` are
/// the formula's fragments, painted left-to-right; `advance_em`/`ascent_em`/`descent_em` are its
/// extents in em, resolved to pixels against the cascaded text style at layout time.
struct MathSpan {
    byte_range: Range<usize>,
    elements: Vec<MathElement>,
    advance_em: f32,
    /// Shared baseline extents (em) of the formula, used to size a row to the formula's real
    /// laid-out height (see [`math::inline_math_box_height`]).
    ascent_em: f32,
    descent_em: f32,
}

/// `Clone`able geometry-only view of a [`MathSpan`], captured into the measure closure (which
/// cannot hold the non-`Clone` [`MathElement`]s).
#[derive(Clone)]
struct MathSpanSpec {
    byte_range: Range<usize>,
    advance_em: f32,
    ascent_em: f32,
    descent_em: f32,
}

/// Shared, `TextLayout`-like handle for an [`InlineFlowElement`]: filled during measure/prepaint,
/// read by the selection layer during paint. `bounds` is `None` until the element is prepainted.
#[derive(Clone, Default)]
struct InlineFlowLayout(Rc<RefCell<Option<InlineFlowLayoutInner>>>);

struct InlineFlowLayoutInner {
    /// The flow's full rendered string (surrounding text with each formula's `$...$` source
    /// standing in for the drawn glyphs), so selection/copy read the source in order.
    text: SharedString,
    rows: Vec<VisualRow>,
    len: usize,
    line_height: Pixels,
    size: Size<Pixels>,
    /// Absolute bounds, set during prepaint; the rows' `top` is relative to `bounds.origin`.
    bounds: Option<Bounds<Pixels>>,
    wrap_width: Option<Pixels>,
    text_align: TextAlign,
}

/// One visual (wrapped) row of an inline flow. `top` is relative to the element origin.
struct VisualRow {
    top: Pixels,
    height: Pixels,
    /// Content width (sum of piece widths), used to honor text alignment.
    width: Pixels,
    byte_start: usize,
    byte_end: usize,
    pieces: Vec<RowPiece>,
}

/// A run of shaped text or a reserved gap for a drawn formula, within a [`VisualRow`]. `x` is the
/// piece's offset from the row's content start (before the alignment offset is applied).
enum RowPiece {
    Text {
        shaped: ShapedLine,
        byte_start: usize,
        x: Pixels,
    },
    Math {
        byte_start: usize,
        byte_len: usize,
        width: Pixels,
        span_ix: usize,
        x: Pixels,
    },
}

impl InlineFlowLayout {
    fn len(&self) -> usize {
        self.0.borrow().as_ref().map_or(0, |inner| inner.len)
    }

    fn line_height(&self) -> Pixels {
        self.0.borrow().as_ref().map_or(px(0.), |inner| inner.line_height)
    }

    fn bounds(&self) -> Bounds<Pixels> {
        self.0
            .borrow()
            .as_ref()
            .and_then(|inner| inner.bounds)
            .unwrap_or_default()
    }

    fn text(&self) -> String {
        self.0
            .borrow()
            .as_ref()
            .map_or(String::new(), |inner| inner.text.to_string())
    }

    #[cfg(any(test, feature = "test-support"))]
    fn wrapped_text(&self) -> String {
        let borrow = self.0.borrow();
        let Some(inner) = borrow.as_ref() else {
            return String::new();
        };
        let mut out = String::new();
        for (ix, row) in inner.rows.iter().enumerate() {
            if ix > 0 {
                out.push('\n');
            }
            out.push_str(inner.text[row.byte_start..row.byte_end].trim_end_matches('\n'));
        }
        out
    }

    fn position_for_index(&self, index: usize) -> Option<Point<Pixels>> {
        let borrow = self.0.borrow();
        let inner = borrow.as_ref()?;
        let bounds = inner.bounds?;
        for row in &inner.rows {
            if index < row.byte_start || index > row.byte_end {
                continue;
            }
            let row_top = bounds.top() + row.top;
            let align = alignment_offset(inner.text_align, bounds.size.width, row.width);
            for piece in &row.pieces {
                match piece {
                    RowPiece::Text {
                        shaped,
                        byte_start,
                        x,
                    } => {
                        let end = byte_start + shaped.len();
                        if index >= *byte_start && index <= end {
                            let local = index - byte_start;
                            return Some(point(
                                bounds.left() + align + *x + shaped.x_for_index(local),
                                row_top,
                            ));
                        }
                    }
                    RowPiece::Math {
                        byte_start,
                        byte_len,
                        x,
                        ..
                    } => {
                        if index >= *byte_start && index <= byte_start + byte_len {
                            return Some(point(bounds.left() + align + *x, row_top));
                        }
                    }
                }
            }
        }
        None
    }

    /// Rendered-index hit-test for a point, honoring alignment. `Ok` when the point lands on a
    /// piece, `Err` (with the nearest index) when it falls in empty space.
    fn rendered_index_for_position(
        &self,
        position: Point<Pixels>,
        text_align: TextAlign,
    ) -> Result<usize, usize> {
        let borrow = self.0.borrow();
        let Some(inner) = borrow.as_ref() else {
            return Err(0);
        };
        let Some(bounds) = inner.bounds else {
            return Err(0);
        };
        let relative_y = (position.y - bounds.top()).max(px(0.));
        let row = inner
            .rows
            .iter()
            .find(|row| relative_y < row.top + row.height)
            .or_else(|| inner.rows.last());
        let Some(row) = row else {
            return Err(inner.len);
        };
        let align = alignment_offset(text_align, bounds.size.width, row.width);
        let local_x = position.x - bounds.left() - align;
        let mut last_end = row.byte_start;
        for piece in &row.pieces {
            match piece {
                RowPiece::Text {
                    shaped,
                    byte_start,
                    x,
                } => {
                    let piece_end_x = *x + shaped.width();
                    if local_x < piece_end_x {
                        let piece_local_x = (local_x - *x).max(px(0.));
                        let index = shaped
                            .index_for_x(piece_local_x)
                            .unwrap_or_else(|| shaped.closest_index_for_x(piece_local_x));
                        return Ok(byte_start + index);
                    }
                    last_end = byte_start + shaped.len();
                }
                RowPiece::Math {
                    byte_start,
                    byte_len,
                    width,
                    x,
                    ..
                } => {
                    if local_x < *x + *width {
                        // Split the whole reserved math box left/right, mirroring the
                        // painted-glyph-bounds split in `RenderedText::source_index_for_position`,
                        // so a click resolves consistently whether it lands on the glyphs or in the
                        // surrounding line-height / inter-fragment padding.
                        let center = *x + *width / 2.;
                        return Ok(if local_x < center {
                            *byte_start
                        } else {
                            byte_start + byte_len
                        });
                    }
                    last_end = byte_start + byte_len;
                }
            }
        }
        Err(last_end)
    }

    /// Highlight rectangles for a rendered-index range. Skips [`RowPiece::Math`] pieces; the
    /// formula's painted bounds (from its `MathRegion`) provide math highlights instead.
    fn highlight_bounds(&self, range: Range<usize>, text_align: TextAlign) -> Vec<Bounds<Pixels>> {
        let mut out = Vec::new();
        let borrow = self.0.borrow();
        let Some(inner) = borrow.as_ref() else {
            return out;
        };
        let Some(bounds) = inner.bounds else {
            return out;
        };
        for row in &inner.rows {
            let row_top = bounds.top() + row.top;
            let align = alignment_offset(text_align, bounds.size.width, row.width);
            for piece in &row.pieces {
                let RowPiece::Text {
                    shaped,
                    byte_start,
                    x,
                } = piece
                else {
                    continue;
                };
                let piece_end = byte_start + shaped.len();
                let selection_start = range.start.max(*byte_start);
                let selection_end = range.end.min(piece_end);
                if selection_start < selection_end {
                    let x0 = bounds.left()
                        + align
                        + *x
                        + shaped.x_for_index(selection_start - byte_start);
                    let x1 = bounds.left()
                        + align
                        + *x
                        + shaped.x_for_index(selection_end - byte_start);
                    out.push(Bounds::from_corners(
                        point(x0, row_top),
                        point(x1, row_top + inner.line_height),
                    ));
                }
            }
        }
        out
    }
}

/// The rendered geometry of a [`RenderedLine`]: plain text uses GPUI's [`TextLayout`]; a line that
/// mixes text and drawn inline math uses an [`InlineFlowElement`]'s shared [`InlineFlowLayout`].
/// Both answer the same selection/copy queries so the selection layer treats them uniformly.
enum LineGeometry {
    Text(TextLayout),
    InlineFlow(InlineFlowLayout),
}

impl LineGeometry {
    fn len(&self) -> usize {
        match self {
            LineGeometry::Text(layout) => layout.len(),
            LineGeometry::InlineFlow(flow) => flow.len(),
        }
    }

    fn bounds(&self) -> Bounds<Pixels> {
        match self {
            LineGeometry::Text(layout) => layout.bounds(),
            LineGeometry::InlineFlow(flow) => flow.bounds(),
        }
    }

    fn line_height(&self) -> Pixels {
        match self {
            LineGeometry::Text(layout) => layout.line_height(),
            LineGeometry::InlineFlow(flow) => flow.line_height(),
        }
    }

    fn text(&self) -> String {
        match self {
            LineGeometry::Text(layout) => layout.text(),
            LineGeometry::InlineFlow(flow) => flow.text(),
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    fn wrapped_text(&self) -> String {
        match self {
            LineGeometry::Text(layout) => layout.wrapped_text(),
            LineGeometry::InlineFlow(flow) => flow.wrapped_text(),
        }
    }

    fn position_for_index(&self, index: usize) -> Option<Point<Pixels>> {
        match self {
            LineGeometry::Text(layout) => layout.position_for_index(index),
            LineGeometry::InlineFlow(flow) => flow.position_for_index(index),
        }
    }

    /// Rendered-index hit-test for a point, honoring the line's text alignment.
    fn rendered_index_for_position(
        &self,
        position: Point<Pixels>,
        text_align: TextAlign,
    ) -> Result<usize, usize> {
        match self {
            LineGeometry::Text(layout) => text_layout_index_for_position(layout, position, text_align),
            LineGeometry::InlineFlow(flow) => flow.rendered_index_for_position(position, text_align),
        }
    }

    /// Highlight rectangles for a rendered-index range within this line.
    fn highlight_bounds(&self, range: Range<usize>, text_align: TextAlign) -> Vec<Bounds<Pixels>> {
        match self {
            LineGeometry::Text(layout) => {
                text_layout_highlight_bounds(layout, range, text_align)
            }
            LineGeometry::InlineFlow(flow) => flow.highlight_bounds(range, text_align),
        }
    }
}

/// Alignment-adjusted rendered-index hit-test for a plain [`TextLayout`] line (extracted from the
/// former `RenderedLine::source_index_for_position` so both geometry kinds share the call site).
fn text_layout_index_for_position(
    layout: &TextLayout,
    position: Point<Pixels>,
    text_align: TextAlign,
) -> Result<usize, usize> {
    let adjusted_position = maybe!({
        if text_align == TextAlign::Left {
            return None;
        }
        let wrapped_line = layout.line_layout_for_index(0)?;
        let bounds = layout.bounds();
        let line_height = layout.line_height();
        let relative_y = (position.y - bounds.top()).max(px(0.));
        let wrapped_row_ix = (relative_y / line_height) as usize;
        let boundaries = wrapped_line.wrap_boundaries();
        let segment_start_x = if wrapped_row_ix == 0 {
            px(0.)
        } else {
            boundaries
                .get(wrapped_row_ix - 1)
                .map(|b| wrapped_line.unwrapped_layout.runs[b.run_ix].glyphs[b.glyph_ix].position.x)
                .unwrap_or(px(0.))
        };
        let segment_end_x = boundaries
            .get(wrapped_row_ix)
            .map(|b| wrapped_line.unwrapped_layout.runs[b.run_ix].glyphs[b.glyph_ix].position.x)
            .unwrap_or(wrapped_line.unwrapped_layout.width);
        let offset =
            alignment_offset(text_align, bounds.size.width, segment_end_x - segment_start_x);
        Some(point(position.x - offset, position.y))
    })
    .unwrap_or(position);

    layout.index_for_position(adjusted_position)
}

/// Highlight rectangles for a rendered-index range within a plain [`TextLayout`] line (extracted
/// from the former `RenderedText::wrapped_line_segments`/`push_bounds_for_line_source_range`).
fn text_layout_highlight_bounds(
    layout: &TextLayout,
    range: Range<usize>,
    text_align: TextAlign,
) -> Vec<Bounds<Pixels>> {
    let mut out = Vec::new();
    if range.start >= range.end {
        return out;
    }
    let line_bounds = layout.bounds();
    let line_height = layout.line_height();

    let mut wrapped_line_start = 0;
    let mut row_top = line_bounds.top();
    for wrapped_line in layout.line_layouts() {
        let wrapped_line_end = wrapped_line_start + wrapped_line.len();
        let unwrapped_layout = &wrapped_line.unwrapped_layout;

        if wrapped_line_start < range.end && range.start < wrapped_line_end {
            let row_ends = wrapped_line
                .wrap_boundaries()
                .iter()
                .map(|wrap_boundary| {
                    let glyph =
                        &unwrapped_layout.runs[wrap_boundary.run_ix].glyphs[wrap_boundary.glyph_ix];
                    (wrapped_line_start + glyph.index, glyph.position.x)
                })
                .chain([(wrapped_line_end, unwrapped_layout.width)]);

            let mut sub_row_start = wrapped_line_start;
            let mut sub_row_start_x = Pixels::ZERO;
            let mut sub_row_top = row_top;
            for (row_end, row_end_x) in row_ends {
                let selection_start = range.start.max(sub_row_start);
                let selection_end = range.end.min(row_end);
                if selection_start < selection_end {
                    let offset =
                        alignment_offset(text_align, line_bounds.size.width, row_end_x - sub_row_start_x);
                    let x_for_index = |index: usize| {
                        line_bounds.left() + offset
                            + unwrapped_layout.x_for_index(index - wrapped_line_start)
                            - sub_row_start_x
                    };
                    out.push(Bounds::from_corners(
                        point(x_for_index(selection_start), sub_row_top),
                        point(x_for_index(selection_end), sub_row_top + line_height),
                    ));
                }
                sub_row_start = row_end;
                sub_row_start_x = row_end_x;
                sub_row_top += line_height;
            }
        }

        row_top += wrapped_line.size(line_height).height;
        wrapped_line_start = wrapped_line_end + 1;
    }
    out
}

/// Extract the sub-runs of `runs` (which cover the whole flow text) that fall within `range`,
/// splitting any run that straddles a boundary. Used to shape a row's text slice with the right
/// styles.
fn slice_runs(runs: &[TextRun], range: Range<usize>) -> Vec<TextRun> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    for run in runs {
        let run_start = pos;
        let run_end = pos + run.len;
        pos = run_end;
        let start = run_start.max(range.start);
        let end = run_end.min(range.end);
        if start < end {
            let mut sliced = run.clone();
            sliced.len = end - start;
            out.push(sliced);
        }
        if run_end >= range.end {
            break;
        }
    }
    out
}

/// A custom element that flows surrounding text and drawn inline math on the same wrapped lines,
/// so a formula sits right after the text before it (like KaTeX/LaTeX inline math) instead of
/// being pushed to the start of the next line by flexbox item wrapping. Only inline runs that
/// contain at least one drawn formula use this element; plain paragraphs keep the `StyledText`
/// path unchanged.
struct InlineFlowElement {
    text: SharedString,
    runs: Vec<TextRun>,
    math_spans: Vec<MathSpan>,
    text_align: TextAlign,
    layout: InlineFlowLayout,
    /// Formula fragment elements, built in prepaint from `math_spans` and painted in paint.
    laid_out_math: Vec<AnyElement>,
}

impl IntoElement for InlineFlowElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for InlineFlowElement {
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
        _cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let text = self.text.clone();
        let runs = self.runs.clone();
        let text_align = self.text_align;
        let specs: Vec<MathSpanSpec> = self
            .math_spans
            .iter()
            .map(|span| MathSpanSpec {
                byte_range: span.byte_range.clone(),
                advance_em: span.advance_em,
                ascent_em: span.ascent_em,
                descent_em: span.descent_em,
            })
            .collect();
        let layout = self.layout.clone();
        // Capture the cascaded text style now, while the enclosing div's style is active. The
        // measure closure runs later (during compute_layout), when `window.text_style()` would
        // return the default style — so a formula-bearing line would otherwise shape its text at
        // the base font size and fail to scale with zoom (while the math, laid out in prepaint
        // where the cascaded style *is* active, does scale). `StyledText::layout` captures the
        // style for the same reason.
        let text_style = window.text_style();
        let rem_size = window.rem_size();

        let layout_id = window.request_measured_layout(gpui::Style::default(), {
            move |known_dimensions, available_space, window, cx| {
                let em_px = text_style.font_size.to_pixels(rem_size);
                // Match `MathElement::request_layout` exactly (it uses `line_height_in_pixels`,
                // which rounds) so a row's height and text baseline coincide with the formula's
                // laid-out box to the pixel; `line_height.to_pixels` omits that rounding.
                let line_height = window.pixel_snap(text_style.line_height_in_pixels(rem_size));
                let wrap_width = known_dimensions.width.or(match available_space.width {
                    gpui::AvailableSpace::Definite(width) => Some(width),
                    _ => None,
                });

                if let Some(size) = {
                    let borrow = layout.0.borrow();
                    borrow
                        .as_ref()
                        .filter(|inner| inner.wrap_width == wrap_width)
                        .map(|inner| inner.size)
                } {
                    return size;
                }

                // Baseline where this line's text paints, so a row can be sized to the exact
                // height a formula's `MathElement` will occupy (same probe the element uses).
                let text_baseline = inline_text_baseline(window, &text_style, em_px, line_height);
                let rows = layout_inline_flow_rows(
                    &text,
                    &runs,
                    &specs,
                    text_style.font(),
                    em_px,
                    line_height,
                    text_baseline,
                    wrap_width,
                    window,
                    cx,
                );

                let mut size = Size::<Pixels>::default();
                for row in &rows {
                    size.width = size.width.max(row.width);
                    size.height += row.height;
                }
                size.width = size.width.ceil();

                layout.0.borrow_mut().replace(InlineFlowLayoutInner {
                    text: text.clone(),
                    rows,
                    len: text.len(),
                    line_height,
                    size,
                    bounds: None,
                    wrap_width,
                    text_align,
                });
                size
            }
        });
        (layout_id, ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        // Record the absolute bounds so the (relative) row tops resolve to screen coordinates for
        // selection queries, and lay out/position each formula's fragments at their pen origin.
        let mut placements: Vec<(usize, Point<Pixels>)> = Vec::new();
        {
            let borrow = self.layout.0.borrow();
            let Some(inner) = borrow.as_ref() else {
                return;
            };
            for row in &inner.rows {
                let row_top = bounds.top() + row.top;
                let align = alignment_offset(self.text_align, bounds.size.width, row.width);
                for piece in &row.pieces {
                    if let RowPiece::Math { span_ix, x, .. } = piece {
                        placements.push((*span_ix, point(bounds.left() + align + *x, row_top)));
                    }
                }
            }
        }
        self.layout.0.borrow_mut().as_mut().unwrap().bounds = Some(bounds);

        let em_px = window.text_style().font_size.to_pixels(window.rem_size());
        let mut laid_out = Vec::new();
        for (span_ix, origin) in placements {
            let elements = std::mem::take(&mut self.math_spans[span_ix].elements);
            let mut cursor_x = origin.x;
            for element in elements {
                let advance = em_px * element.inline_advance_em();
                let mut any = element.into_any_element();
                any.layout_as_root(gpui::AvailableSpace::min_size(), window, cx);
                any.prepaint_at(point(cursor_x, origin.y), window, cx);
                laid_out.push(any);
                cursor_x += advance;
            }
        }
        self.laid_out_math = laid_out;
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
        {
            let borrow = self.layout.0.borrow();
            if let Some(inner) = borrow.as_ref() {
                for row in &inner.rows {
                    let row_top = bounds.top() + row.top;
                    let align = alignment_offset(self.text_align, bounds.size.width, row.width);
                    for piece in &row.pieces {
                        if let RowPiece::Text { shaped, x, .. } = piece {
                            shaped
                                .paint(
                                    point(bounds.left() + align + *x, row_top),
                                    inner.line_height,
                                    TextAlign::Left,
                                    None,
                                    window,
                                    cx,
                                )
                                .log_err();
                        }
                    }
                }
            }
        }
        for element in &mut self.laid_out_math {
            element.paint(window, cx);
        }
    }
}

/// Wrap the flow's text (with each formula reserving a fixed-width, atomic gap) into visual rows,
/// shaping each row's text slices. Mirrors the editor's `LineWithInvisibles`/`wrap_map` approach:
/// [`gpui::LineWrapper::wrap_line`] computes break points across a mix of text and element
/// fragments, then each row's text is shaped with [`WindowTextSystem::shape_line`].
fn layout_inline_flow_rows(
    text: &SharedString,
    runs: &[TextRun],
    specs: &[MathSpanSpec],
    font: gpui::Font,
    em_px: Pixels,
    line_height: Pixels,
    text_baseline: Pixels,
    wrap_width: Option<Pixels>,
    window: &mut Window,
    cx: &mut App,
) -> Vec<VisualRow> {
    // Segment the text into maximal non-math runs and atomic math spans, in byte order.
    enum Seg {
        Text(Range<usize>),
        Math(usize),
    }
    let mut segments = Vec::new();
    let mut cursor = 0usize;
    for (spec_ix, spec) in specs.iter().enumerate() {
        if spec.byte_range.start > cursor {
            segments.push(Seg::Text(cursor..spec.byte_range.start));
        }
        segments.push(Seg::Math(spec_ix));
        cursor = spec.byte_range.end;
    }
    if cursor < text.len() {
        segments.push(Seg::Text(cursor..text.len()));
    }

    // Compute wrap boundaries (byte indices where a new row starts) over the mixed fragments.
    //
    // Known approximations (both shared with GPUI's plain `StyledText` wrap path):
    //  - `LineWrapper` measures text with one font's per-character widths, whereas rows are painted
    //    with per-run shaped widths. For uniform-font text (the common case) these agree; with
    //    mixed bold/italic/inline-code runs or complex shaping the wrap point can drift slightly.
    //    A fully faithful fix would wrap on shaped widths, a larger rework.
    //  - `Boundary::next_indent` (continuation-line indent the wrapper reserves) is not applied to
    //    the visual rows, matching `StyledText`; it only matters for leading-indented text.
    let boundaries: Vec<usize> = if let Some(wrap_width) = wrap_width {
        let mut fragments: Vec<LineFragment> = Vec::new();
        for segment in &segments {
            match segment {
                Seg::Text(range) => fragments.push(LineFragment::text(&text[range.clone()])),
                Seg::Math(ix) => fragments
                    .push(LineFragment::element(em_px * specs[*ix].advance_em, specs[*ix].byte_range.len())),
            }
        }
        let mut wrapper = cx.text_system().line_wrapper(font, em_px);
        let mut boundaries: Vec<usize> = wrapper
            .wrap_line(&fragments, wrap_width)
            .map(|boundary| boundary.ix)
            .collect();
        // Force a row break right after each hard line break in the source text.
        for (ix, byte) in text.char_indices() {
            if byte == '\n' {
                boundaries.push(ix + 1);
            }
        }
        boundaries.sort_unstable();
        boundaries.dedup();
        boundaries
    } else {
        // No wrapping width: still break at hard line breaks.
        text.char_indices()
            .filter(|(_, ch)| *ch == '\n')
            .map(|(ix, _)| ix + 1)
            .collect()
    };

    // Row byte ranges: [0, b0), [b0, b1), ... [bk, len).
    let mut row_ranges: Vec<Range<usize>> = Vec::new();
    let mut prev = 0usize;
    for &boundary in &boundaries {
        if boundary > prev && boundary <= text.len() {
            row_ranges.push(prev..boundary);
            prev = boundary;
        }
    }
    row_ranges.push(prev..text.len());

    let font_size = em_px;
    let mut rows = Vec::with_capacity(row_ranges.len());
    let mut row_top = px(0.);
    for range in row_ranges {
        let mut pieces = Vec::new();
        let mut x = px(0.);
        let mut row_height = line_height;
        let mut byte = range.start;
        while byte < range.end {
            // A math span starting exactly here (spans are atomic and never split across rows).
            if let Some((span_ix, spec)) = specs
                .iter()
                .enumerate()
                .find(|(_, spec)| spec.byte_range.start == byte)
            {
                let width = em_px * spec.advance_em;
                pieces.push(RowPiece::Math {
                    byte_start: spec.byte_range.start,
                    byte_len: spec.byte_range.len(),
                    width,
                    span_ix,
                    x,
                });
                x += width;
                // Size the row to the formula's real laid-out height (baseline-aligned), not just
                // its natural extent, so a tall formula does not overlap the next row or clip.
                row_height = row_height.max(inline_math_box_height(
                    em_px,
                    line_height,
                    text_baseline,
                    spec.ascent_em,
                    spec.descent_em,
                ));
                byte = spec.byte_range.end.min(range.end);
                continue;
            }

            // Otherwise a text slice up to the next math span (or row end).
            let next_math = specs
                .iter()
                .filter(|spec| spec.byte_range.start > byte && spec.byte_range.start < range.end)
                .map(|spec| spec.byte_range.start)
                .min()
                .unwrap_or(range.end);
            // Strip a trailing hard break and never feed `\n` to `shape_line`.
            let slice = &text[byte..next_math];
            let slice = slice.split('\n').next().unwrap_or("");
            if !slice.is_empty() {
                let sub_runs = slice_runs(runs, byte..byte + slice.len());
                let shaped = window.text_system().shape_line(
                    SharedString::from(slice.to_string()),
                    font_size,
                    &sub_runs,
                    None,
                );
                let width = shaped.width();
                pieces.push(RowPiece::Text {
                    shaped,
                    byte_start: byte,
                    x,
                });
                x += width;
            }
            byte = next_math;
        }
        rows.push(VisualRow {
            top: row_top,
            height: row_height,
            width: x,
            byte_start: range.start,
            byte_end: range.end,
            pieces,
        });
        row_top += row_height;
    }
    rows
}

struct RenderedLine {
    layout: LineGeometry,
    source_mappings: Vec<SourceMapping>,
    source_end: usize,
    language: Option<Arc<Language>>,
    text_align: TextAlign,
    /// True for the invisible copyable source anchor of a drawn formula. Such a line carries the
    /// LaTeX source for copy, but its layout coordinates are unrelated to the painted math, so it
    /// must be excluded from selection-highlight bounds (the math region's bounds are used instead).
    is_math_anchor: bool,
}

impl RenderedLine {
    fn rendered_index_for_source_index(&self, source_index: usize) -> usize {
        if source_index >= self.source_end {
            return self.layout.len();
        }

        let mapping = match self
            .source_mappings
            .binary_search_by_key(&source_index, |probe| probe.source_index)
        {
            Ok(ix) => &self.source_mappings[ix],
            Err(ix) => &self.source_mappings[ix - 1],
        };
        (mapping.rendered_index + (source_index - mapping.source_index)).min(self.layout.len())
    }

    fn source_index_for_rendered_index(&self, rendered_index: usize) -> usize {
        if rendered_index >= self.layout.len() {
            return self.source_end;
        }

        let mapping = match self
            .source_mappings
            .binary_search_by_key(&rendered_index, |probe| probe.rendered_index)
        {
            Ok(ix) => &self.source_mappings[ix],
            Err(ix) => &self.source_mappings[ix - 1],
        };
        mapping.source_index + (rendered_index - mapping.rendered_index)
    }

    /// Returns the source index for use as an exclusive range end at a word/selection boundary.
    /// When the rendered index is exactly at the start of a segment with a gap from the previous
    /// segment (e.g., after stripped markdown syntax like backticks), this returns the end of the
    /// previous segment rather than the start of the current one.
    fn source_index_for_exclusive_rendered_end(&self, rendered_index: usize) -> usize {
        if rendered_index >= self.layout.len() {
            return self.source_end;
        }

        let ix = match self
            .source_mappings
            .binary_search_by_key(&rendered_index, |probe| probe.rendered_index)
        {
            Ok(ix) => ix,
            Err(ix) => {
                return self.source_mappings[ix - 1].source_index
                    + (rendered_index - self.source_mappings[ix - 1].rendered_index);
            }
        };

        // Exact match at the start of a segment. Check if there's a gap from the previous segment.
        if ix > 0 {
            let prev_mapping = &self.source_mappings[ix - 1];
            let mapping = &self.source_mappings[ix];
            let prev_segment_len = mapping.rendered_index - prev_mapping.rendered_index;
            let prev_source_end = prev_mapping.source_index + prev_segment_len;
            if prev_source_end < mapping.source_index {
                return prev_source_end;
            }
        }

        self.source_mappings[ix].source_index
    }

    fn source_index_for_position(&self, position: Point<Pixels>) -> Result<usize, usize> {
        let (line_rendered_index, out_of_bounds) =
            match self.layout.rendered_index_for_position(position, self.text_align) {
                Ok(ix) => (ix, false),
                Err(ix) => (ix, true),
            };
        let source_index = self.source_index_for_rendered_index(line_rendered_index);
        if out_of_bounds {
            Err(source_index)
        } else {
            Ok(source_index)
        }
    }
}

#[derive(Copy, Clone, Debug, Default)]
struct SourceMapping {
    rendered_index: usize,
    source_index: usize,
}

fn source_range_for_rendered(
    mappings: &[SourceMapping],
    rendered: &Range<usize>,
) -> Option<Range<usize>> {
    if rendered.start >= rendered.end {
        return None;
    }
    let start = source_index_for_rendered(mappings, rendered.start)?;
    let end = source_index_for_rendered(mappings, rendered.end - 1)? + 1;
    Some(start..end)
}

fn source_index_for_rendered(mappings: &[SourceMapping], rendered_index: usize) -> Option<usize> {
    let mut last: Option<&SourceMapping> = None;
    for mapping in mappings {
        if mapping.rendered_index <= rendered_index {
            last = Some(mapping);
        } else {
            break;
        }
    }
    last.map(|m| m.source_index + (rendered_index - m.rendered_index))
}

pub struct RenderedMarkdown {
    element: AnyElement,
    text: RenderedText,
}

/// A drawn math formula's source span paired with the painted bounds of its piece(s). Inline
/// formulas can split into several fragments (and wrap across rows), so a region holds one bounds
/// cell per fragment rather than a single union rectangle. Cells are shared with the `MathElement`s,
/// which publish their bounds during prepaint, so reads here see the location the math is drawn at.
struct MathRegion {
    source_range: Range<usize>,
    bounds_cells: Vec<MathBoundsSlot>,
}

impl MathRegion {
    /// Painted bounds of each fragment laid out this frame (empty before the first prepaint).
    fn bounds(&self) -> impl Iterator<Item = Bounds<Pixels>> + '_ {
        self.bounds_cells.iter().filter_map(|cell| cell.get())
    }

    /// The fragment bounds containing `position`, if any.
    fn bounds_containing(&self, position: Point<Pixels>) -> Option<Bounds<Pixels>> {
        self.bounds().find(|bounds| bounds.contains(&position))
    }

    /// Whether `range` overlaps this formula's source span (touching at a boundary does not count).
    fn overlaps(&self, range: &Range<usize>) -> bool {
        self.source_range.start < range.end && range.start < self.source_range.end
    }

    /// Whether `source_index` falls on or within this formula's span. Both boundaries count, so a
    /// hit resolved to `source_range.start` or `source_range.end` is recognized as "this formula"
    /// for word/line selection and caret positioning.
    fn touches_index(&self, source_index: usize) -> bool {
        self.source_range.start <= source_index && source_index <= self.source_range.end
    }
}

#[derive(Clone)]
struct RenderedText {
    lines: Rc<[RenderedLine]>,
    links: Rc<[RenderedLink]>,
    image_links: Rc<[RenderedImageLink]>,
    footnote_refs: Rc<[RenderedFootnoteRef]>,
    math_regions: Rc<[MathRegion]>,
}

#[derive(Debug, Clone, Eq, PartialEq)]
struct RenderedLink {
    source_range: Range<usize>,
    destination_url: SharedString,
}

#[derive(Clone)]
struct RenderedImageLink {
    // Populated once the image's `canvas` overlay is painted; images aren't part of the
    // text layout, so their hit-test region can't be derived from a source range.
    bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    destination_url: SharedString,
}

#[derive(Debug, Clone, Eq, PartialEq)]
struct RenderedFootnoteRef {
    source_range: Range<usize>,
    label: SharedString,
}

impl RenderedText {
    fn bounds_for_source_range(&self, range: Range<usize>) -> Vec<Bounds<Pixels>> {
        self.bounds_for_sorted_source_ranges([(0, range)])
            .into_iter()
            .map(|(_, bounds)| bounds)
            .collect()
    }

    /// Expand `range` so any drawn formula it overlaps is fully covered. Math is atomic for
    /// selection/copy (matching KaTeX's copy-tex), and the copyable LaTeX source for a formula
    /// lives in one anchor line, so a partial range must be widened to copy the whole formula.
    fn expand_range_for_atomic_regions(&self, mut range: Range<usize>) -> Range<usize> {
        for region in self.math_regions.iter() {
            if region.overlaps(&range) {
                range.start = range.start.min(region.source_range.start);
                range.end = range.end.max(region.source_range.end);
            }
        }
        range
    }

    fn bounds_for_sorted_source_ranges(
        &self,
        ranges: impl IntoIterator<Item = (usize, Range<usize>)>,
    ) -> Vec<(usize, Bounds<Pixels>)> {
        let ranges = ranges.into_iter().collect::<Vec<_>>();
        let mut all_bounds = Vec::new();
        let mut first_possible_range_ix = 0;

        for line in self.lines.iter() {
            // The invisible math source anchor's layout coordinates are unrelated to the drawn
            // formula, so it must not contribute highlight bounds. Selection adds the formula's
            // painted bounds separately in `bounds_for_source_range`.
            if line.is_math_anchor {
                continue;
            }
            let line_source_start = line.source_mappings.first().unwrap().source_index;
            while ranges
                .get(first_possible_range_ix)
                .is_some_and(|(_, range)| range.end <= line_source_start)
            {
                first_possible_range_ix += 1;
            }

            let Some((_, first_possible_range)) = ranges.get(first_possible_range_ix) else {
                break;
            };
            if first_possible_range.start >= line.source_end {
                continue;
            }

            let mut range_ix = first_possible_range_ix;
            while let Some((highlight_ix, range)) = ranges.get(range_ix) {
                if range.start >= line.source_end {
                    break;
                }
                let clamped =
                    range.start.max(line_source_start)..range.end.min(line.source_end);
                if clamped.start < clamped.end {
                    let rendered_start = line.rendered_index_for_source_index(clamped.start);
                    let rendered_end = line.rendered_index_for_source_index(clamped.end);
                    all_bounds.extend(
                        line.layout
                            .highlight_bounds(rendered_start..rendered_end, line.text_align)
                            .into_iter()
                            .map(|bounds| (*highlight_ix, bounds)),
                    );
                }
                range_ix += 1;
            }
        }

        // A drawn formula's invisible anchor line is skipped above (its layout coordinates are
        // unrelated to the painted math), so add the formula's painted bounds for any range that
        // touches it. This covers both selection and search highlights of math source spans.
        for (highlight_ix, range) in &ranges {
            for region in self.math_regions.iter() {
                if region.overlaps(range) {
                    all_bounds.extend(region.bounds().map(|bounds| (*highlight_ix, bounds)));
                }
            }
        }

        all_bounds
    }

    fn source_index_for_position(&self, position: Point<Pixels>) -> Result<usize, usize> {
        // Drawn math is atomic: a hit inside a formula's painted bounds maps to its source-span
        // boundary (left half -> start, right half -> end) so selection aligns with the glyphs
        // instead of the invisible source anchor, which sits at unrelated coordinates.
        for region in self.math_regions.iter() {
            if let Some(bounds) = region.bounds_containing(position) {
                let boundary = if position.x < bounds.center().x {
                    region.source_range.start
                } else {
                    region.source_range.end
                };
                return Ok(boundary);
            }
        }

        let mut lines = self.lines.iter().peekable();
        let mut fallback_line: Option<&RenderedLine> = None;

        while let Some(line) = lines.next() {
            // A formula's invisible source anchor sits at unrelated coordinates; hits inside the
            // drawn math are handled by the region check above, so never resolve a pixel position
            // through an anchor line (its hidden text would otherwise capture stray clicks).
            if line.is_math_anchor {
                continue;
            }
            let line_bounds = line.layout.bounds();

            // Exact match: position is within bounds (handles overlapping bounds like table columns)
            if line_bounds.contains(&position) {
                return line.source_index_for_position(position);
            }

            // Track fallback for Y-coordinate based matching
            if position.y <= line_bounds.bottom() && fallback_line.is_none() {
                fallback_line = Some(line);
            }

            // Handle gap between lines
            if position.y > line_bounds.bottom() {
                if let Some(next_line) = lines.peek()
                    && position.y < next_line.layout.bounds().top()
                {
                    return Err(line.source_end);
                }
            }
        }

        // Fall back to Y-coordinate matched line
        if let Some(line) = fallback_line {
            return line.source_index_for_position(position);
        }

        Err(self.lines.last().map_or(0, |line| line.source_end))
    }

    fn position_for_source_index(&self, source_index: usize) -> Option<(Point<Pixels>, Pixels)> {
        // For a drawn formula, use its painted bounds rather than the invisible anchor's location.
        if let Some(region) = self
            .math_regions
            .iter()
            .find(|region| region.touches_index(source_index))
            && let Some(bounds) = region.bounds().next()
        {
            return Some((bounds.origin, bounds.size.height));
        }
        for line in self.lines.iter() {
            if line.is_math_anchor {
                continue;
            }
            let line_source_start = line.source_mappings.first().unwrap().source_index;
            if source_index < line_source_start {
                break;
            } else if source_index > line.source_end {
                continue;
            } else {
                let line_height = line.layout.line_height();
                let rendered_index_within_line = line.rendered_index_for_source_index(source_index);
                let position = line.layout.position_for_index(rendered_index_within_line)?;
                return Some((position, line_height));
            }
        }
        None
    }

    fn surrounding_word_range(&self, source_index: usize) -> Range<usize> {
        // Double-clicking a formula selects the whole formula (atomic), not a word of hidden source.
        if let Some(region) = self
            .math_regions
            .iter()
            .find(|region| region.touches_index(source_index))
        {
            return region.source_range.clone();
        }
        for line in self.lines.iter() {
            if line.is_math_anchor || source_index > line.source_end {
                continue;
            }

            let line_rendered_start = line.source_mappings.first().unwrap().rendered_index;
            let rendered_index_in_line =
                line.rendered_index_for_source_index(source_index) - line_rendered_start;
            let text = line.layout.text();

            let scope = line.language.as_ref().map(|l| l.default_scope());
            let classifier = CharClassifier::new(scope);

            let mut prev_chars = text[..rendered_index_in_line].chars().rev().peekable();
            let mut next_chars = text[rendered_index_in_line..].chars().peekable();

            let word_kind = std::cmp::max(
                prev_chars.peek().map(|&c| classifier.kind(c)),
                next_chars.peek().map(|&c| classifier.kind(c)),
            );

            let mut start = rendered_index_in_line;
            for c in prev_chars {
                if Some(classifier.kind(c)) == word_kind {
                    start -= c.len_utf8();
                } else {
                    break;
                }
            }

            let mut end = rendered_index_in_line;
            for c in next_chars {
                if Some(classifier.kind(c)) == word_kind {
                    end += c.len_utf8();
                } else {
                    break;
                }
            }

            return line.source_index_for_rendered_index(line_rendered_start + start)
                ..line.source_index_for_exclusive_rendered_end(line_rendered_start + end);
        }

        source_index..source_index
    }

    fn surrounding_line_range(&self, source_index: usize) -> Range<usize> {
        // Triple-clicking a formula selects the whole formula atomically rather than routing through
        // the invisible anchor's hidden source line.
        if let Some(region) = self
            .math_regions
            .iter()
            .find(|region| region.touches_index(source_index))
        {
            return region.source_range.clone();
        }
        for line in self.lines.iter() {
            if line.is_math_anchor || source_index > line.source_end {
                continue;
            }
            let line_source_start = line.source_mappings.first().unwrap().source_index;
            return line_source_start..line.source_end;
        }

        source_index..source_index
    }

    fn text_for_range(&self, range: Range<usize>) -> String {
        let mut accumulator = String::new();

        for line in self.lines.iter() {
            if range.start > line.source_end {
                continue;
            }
            let line_source_start = line.source_mappings.first().unwrap().source_index;
            if range.end < line_source_start {
                break;
            }

            let text = line.layout.text();

            let start = if range.start < line_source_start {
                0
            } else {
                line.rendered_index_for_source_index(range.start)
            };
            let end = if range.end > line.source_end {
                line.rendered_index_for_source_index(line.source_end)
            } else {
                line.rendered_index_for_source_index(range.end)
            }
            .min(text.len());

            accumulator.push_str(&text[start..end]);
            accumulator.push('\n');
        }
        // Remove trailing newline
        accumulator.pop();
        accumulator
    }

    fn link_for_source_index(&self, source_index: usize) -> Option<&RenderedLink> {
        self.links
            .iter()
            .find(|link| link.source_range.contains(&source_index))
    }

    fn image_link_for_position(&self, position: Point<Pixels>) -> Option<&RenderedImageLink> {
        self.image_links.iter().find(|image| {
            image
                .bounds
                .get()
                .is_some_and(|bounds| bounds.contains(&position))
        })
    }

    fn footnote_ref_for_source_index(&self, source_index: usize) -> Option<&RenderedFootnoteRef> {
        self.footnote_refs
            .iter()
            .find(|fref| fref.source_range.contains(&source_index))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{RenderImage, TestAppContext, UpdateGlobal, size};
    use language::{Language, LanguageConfig, LanguageMatcher};
    use std::cell::RefCell;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    struct TestWindow;

    impl Render for TestWindow {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
        }
    }

    fn ensure_theme_initialized(cx: &mut TestAppContext) {
        cx.update(|cx| {
            if !cx.has_global::<settings::SettingsStore>() {
                settings::init(cx);
            }
            if !cx.has_global::<theme::GlobalTheme>() {
                theme_settings::init(theme::LoadThemes::JustBase, cx);
            }
        });
    }

    #[gpui::test]
    fn test_code_block_controls_are_unique_across_markdown_entities(cx: &mut TestAppContext) {
        struct TestWindow;

        impl Render for TestWindow {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                div()
            }
        }

        struct TestMarkdowns {
            first_markdown: Entity<Markdown>,
            second_markdown: Entity<Markdown>,
        }

        impl Render for TestMarkdowns {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                div()
                    .child(MarkdownElement::new(
                        self.first_markdown.clone(),
                        MarkdownStyle::default(),
                    ))
                    .child(MarkdownElement::new(
                        self.second_markdown.clone(),
                        MarkdownStyle::default(),
                    ))
            }
        }

        ensure_theme_initialized(cx);

        let (_, cx) = cx.add_window_view(|_, _| TestWindow);
        let markdown = "```sh\necho hello\n```";
        let first_markdown = cx.new(|cx| Markdown::new(markdown.into(), None, None, cx));
        let second_markdown = cx.new(|cx| Markdown::new(markdown.into(), None, None, cx));
        cx.run_until_parked();

        cx.draw(Default::default(), size(px(600.0), px(600.0)), |_, cx| {
            cx.new(|_| TestMarkdowns {
                first_markdown: first_markdown.clone(),
                second_markdown: second_markdown.clone(),
            })
            .into_any_element()
        });
    }

    #[gpui::test]
    fn test_mappings(cx: &mut TestAppContext) {
        // Formatting.
        assert_mappings(
            &render_markdown("He*l*lo", cx),
            vec![vec![(0, 0), (1, 1), (2, 3), (3, 5), (4, 6), (5, 7)]],
        );

        // Multiple lines.
        assert_mappings(
            &render_markdown("Hello\n\nWorld", cx),
            vec![
                vec![(0, 0), (1, 1), (2, 2), (3, 3), (4, 4), (5, 5)],
                vec![(0, 7), (1, 8), (2, 9), (3, 10), (4, 11), (5, 12)],
            ],
        );

        // Multi-byte characters.
        assert_mappings(
            &render_markdown("αβγ\n\nδεζ", cx),
            vec![
                vec![(0, 0), (2, 2), (4, 4), (6, 6)],
                vec![(0, 8), (2, 10), (4, 12), (6, 14)],
            ],
        );

        // Smart quotes.
        assert_mappings(&render_markdown("\"", cx), vec![vec![(0, 0), (3, 1)]]);
        assert_mappings(
            &render_markdown("\"hey\"", cx),
            vec![vec![(0, 0), (3, 1), (4, 2), (5, 3), (6, 4), (9, 5)]],
        );

        // HTML Comments are ignored
        assert_mappings(
            &render_markdown(
                "<!--\nrdoc-file=string.c\n- str.intern   -> symbol\n- str.to_sym   -> symbol\n-->\nReturns",
                cx,
            ),
            vec![vec![
                (0, 78),
                (1, 79),
                (2, 80),
                (3, 81),
                (4, 82),
                (5, 83),
                (6, 84),
            ]],
        );
    }

    fn render_markdown(markdown: &str, cx: &mut TestAppContext) -> RenderedText {
        render_markdown_with_language_registry(markdown, None, cx)
    }

    #[gpui::test]
    fn test_active_search_highlight_uses_match_index(cx: &mut TestAppContext) {
        let markdown = cx.new(|cx| Markdown::new("zero one two".into(), None, None, cx));

        markdown.update(cx, |markdown, cx| {
            markdown.set_search_highlights(vec![0..4, 5..8, 9..12], Some(0), cx);
            assert_eq!(markdown.search_highlights(), &[0..4, 5..8, 9..12]);
            assert_eq!(markdown.active_search_highlight(), Some(0));

            markdown.set_active_search_highlight(Some(1), cx);
            assert_eq!(markdown.active_search_highlight(), Some(1));

            markdown.set_active_search_highlight(Some(2), cx);
            assert_eq!(markdown.active_search_highlight(), Some(2));

            markdown.set_active_search_highlight(Some(3), cx);
            assert_eq!(markdown.active_search_highlight(), None);
        });
    }

    #[gpui::test]
    fn test_wrapped_code_block_has_no_scroll_handle(cx: &mut TestAppContext) {
        let markdown =
            cx.new(|cx| Markdown::new("```rust\nlet value = 1;\n```".into(), None, None, cx));

        markdown.update(cx, |markdown, _| {
            assert!(markdown.code_block_scroll_handle(0).is_some());

            markdown.toggle_code_block_wrap(0);
            assert!(markdown.code_block_scroll_handle(0).is_none());

            markdown.toggle_code_block_wrap(0);
            assert!(markdown.code_block_scroll_handle(0).is_some());
        });
    }

    #[gpui::test]
    fn test_frontmatter_renders_without_delimiters(cx: &mut TestAppContext) {
        let rendered = render_markdown_with_options(
            "---\ntitle: Post\n---\nBody",
            None,
            MarkdownOptions {
                render_metadata_blocks: true,
                ..Default::default()
            },
            cx,
        );
        assert_eq!(rendered.text_for_range(0..24), "title\nPost\nBody");
    }

    #[gpui::test]
    fn test_frontmatter_falls_back_to_code_block_for_nested_yaml(cx: &mut TestAppContext) {
        let rendered = render_markdown_with_options(
            "---\ntags:\n  - zed\n---\nBody",
            None,
            MarkdownOptions {
                render_metadata_blocks: true,
                ..Default::default()
            },
            cx,
        );
        assert_eq!(rendered.text_for_range(0..26), "tags:\n  - zed\nBody");
    }

    fn render_markdown_with_code_span_link(
        markdown: &str,
        callback: impl Fn(&str, &App) -> Option<SharedString> + 'static,
        cx: &mut TestAppContext,
    ) -> RenderedText {
        render_markdown_with_code_span_link_style(markdown, MarkdownStyle::default(), callback, cx)
    }

    fn render_markdown_with_code_span_link_style(
        markdown: &str,
        style: MarkdownStyle,
        callback: impl Fn(&str, &App) -> Option<SharedString> + 'static,
        cx: &mut TestAppContext,
    ) -> RenderedText {
        ensure_theme_initialized(cx);

        let (_, cx) = cx.add_window_view(|_, _| TestWindow);
        let markdown = cx.new(|cx| Markdown::new(markdown.to_string().into(), None, None, cx));
        cx.run_until_parked();
        let (rendered, _) = cx.draw(
            Default::default(),
            size(px(600.0), px(600.0)),
            |_window, _cx| {
                MarkdownElement::new(markdown, style)
                    .on_code_span_link(callback)
                    .code_block_renderer(CodeBlockRenderer::Default {
                        copy_button_visibility: CopyButtonVisibility::Hidden,
                        wrap_button_visibility: WrapButtonVisibility::Hidden,
                        border: false,
                    })
            },
        );
        rendered.text
    }

    fn render_markdown_with_language_registry(
        markdown: &str,
        language_registry: Option<Arc<LanguageRegistry>>,
        cx: &mut TestAppContext,
    ) -> RenderedText {
        render_markdown_with_options(markdown, language_registry, MarkdownOptions::default(), cx)
    }

    fn render_markdown_with_options(
        markdown: &str,
        language_registry: Option<Arc<LanguageRegistry>>,
        options: MarkdownOptions,
        cx: &mut TestAppContext,
    ) -> RenderedText {
        ensure_theme_initialized(cx);

        let (_, cx) = cx.add_window_view(|_, _| TestWindow);
        let markdown = cx.new(|cx| {
            Markdown::new_with_options(
                markdown.to_string().into(),
                language_registry,
                None,
                options,
                cx,
            )
        });
        cx.run_until_parked();
        let (rendered, _) = cx.draw(
            Default::default(),
            size(px(600.0), px(600.0)),
            |_window, _cx| {
                MarkdownElement::new(markdown, MarkdownStyle::default()).code_block_renderer(
                    CodeBlockRenderer::Default {
                        copy_button_visibility: CopyButtonVisibility::Hidden,
                        wrap_button_visibility: WrapButtonVisibility::Hidden,
                        border: false,
                    },
                )
            },
        );
        rendered.text
    }

    fn render_markdown_with_image_resolver(
        markdown: &str,
        options: MarkdownOptions,
        resolver: impl Fn(&str) -> Option<ImageSource> + 'static,
        cx: &mut TestAppContext,
    ) -> RenderedText {
        ensure_theme_initialized(cx);

        let (_, cx) = cx.add_window_view(|_, _| TestWindow);
        let markdown = cx.new(|cx| {
            Markdown::new_with_options(markdown.to_string().into(), None, None, options, cx)
        });
        cx.run_until_parked();
        let (rendered, _) = cx.draw(
            Default::default(),
            size(px(600.0), px(600.0)),
            |_window, _cx| {
                MarkdownElement::new(markdown, MarkdownStyle::default())
                    .image_resolver(resolver)
                    .code_block_renderer(CodeBlockRenderer::Default {
                        copy_button_visibility: CopyButtonVisibility::Hidden,
                        wrap_button_visibility: WrapButtonVisibility::Hidden,
                        border: false,
                    })
            },
        );
        rendered.text
    }

    fn test_image(cx: &mut TestAppContext) -> Arc<RenderImage> {
        cx.update(|cx| {
            cx.svg_renderer()
                .render_single_frame(
                    br#"<svg xmlns="http://www.w3.org/2000/svg" width="1" height="1"></svg>"#,
                    1.0,
                )
                .expect("test svg should render")
        })
    }

    #[gpui::test]
    fn test_soft_break_keeps_space_in_paragraph_with_image(cx: &mut TestAppContext) {
        let image = test_image(cx);
        let rendered = render_markdown_with_image_resolver(
            "Here is an image ![alt](https://example.com/a.png) and more text\nthat continues",
            MarkdownOptions::default(),
            move |_| Some(ImageSource::Render(image.clone())),
            cx,
        );
        let text: String = rendered
            .lines
            .iter()
            .map(|line| line.layout.wrapped_text())
            .collect();
        assert!(
            text.contains("more text that continues"),
            "soft break in an image paragraph should still separate words with a space; got: {text:?}"
        );
        assert!(
            !text.contains("textthat"),
            "soft break between words must not be dropped; got: {text:?}"
        );
    }

    #[gpui::test]
    fn test_soft_break_after_image_does_not_insert_leading_space(cx: &mut TestAppContext) {
        let image = test_image(cx);
        let rendered = render_markdown_with_image_resolver(
            "![alt](https://example.com/a.png)\ncaption",
            MarkdownOptions::default(),
            move |_| Some(ImageSource::Render(image.clone())),
            cx,
        );
        let text: String = rendered
            .lines
            .iter()
            .map(|line| line.layout.wrapped_text())
            .collect();
        assert_eq!(
            text, "caption",
            "a soft break after an image should not insert leading whitespace before caption text; got: {text:?}"
        );
    }

    #[gpui::test]
    fn test_break_between_images_does_not_inject_leading_space(cx: &mut TestAppContext) {
        let image = test_image(cx);
        let rendered = render_markdown_with_image_resolver(
            "![Image 3](https://example.com/3.png)\n<br>\n![Image 4](https://example.com/4.png)",
            MarkdownOptions {
                parse_html: true,
                ..Default::default()
            },
            move |_| Some(ImageSource::Render(image.clone())),
            cx,
        );
        let stray_whitespace_line = rendered.lines.iter().find_map(|line| {
            let text = line.layout.wrapped_text();
            (!text.is_empty() && text.trim().is_empty()).then_some(text)
        });
        assert!(
            stray_whitespace_line.is_none(),
            "soft break after a <br> between images must not render a stray space \
             before the wrapped image; got whitespace-only line: {stray_whitespace_line:?}"
        );
    }

    #[gpui::test]
    fn test_break_in_tight_list_item_after_image_item_is_newline(cx: &mut TestAppContext) {
        let image = test_image(cx);
        let rendered = render_markdown_with_image_resolver(
            "- ![alt](https://example.com/a.png)\n- first<br>second",
            MarkdownOptions {
                parse_html: true,
                ..Default::default()
            },
            move |_| Some(ImageSource::Render(image.clone())),
            cx,
        );
        let text: String = rendered
            .lines
            .iter()
            .map(|line| line.layout.wrapped_text())
            .collect();
        assert!(
            text.contains("first\nsecond"),
            "break in a text-only tight list item should render as a newline; got: {text:?}"
        );
    }

    #[gpui::test]
    fn test_hard_style_soft_break_after_image_moves_caption_to_next_row(cx: &mut TestAppContext) {
        ensure_theme_initialized(cx);

        let image = test_image(cx);
        let markdown_source = "![alt](https://example.com/a.png)\ncaption";
        let caption_range = markdown_source.len() - "caption".len()..markdown_source.len();

        let (_, cx) = cx.add_window_view(|_, _| TestWindow);
        let markdown = cx.new(|cx| {
            Markdown::new_with_options(
                markdown_source.to_string().into(),
                None,
                None,
                MarkdownOptions::default(),
                cx,
            )
        });
        cx.run_until_parked();

        let mut caption_top = |soft_break_as_hard_break: bool| {
            let mut style = MarkdownStyle::default();
            style.soft_break_as_hard_break = soft_break_as_hard_break;
            let image = image.clone();
            let (rendered, _) = cx.draw(
                Default::default(),
                size(px(600.0), px(600.0)),
                |_window, _cx| {
                    MarkdownElement::new(markdown.clone(), style)
                        .image_resolver(move |_| Some(ImageSource::Render(image.clone())))
                        .code_block_renderer(CodeBlockRenderer::Default {
                            copy_button_visibility: CopyButtonVisibility::Hidden,
                            wrap_button_visibility: WrapButtonVisibility::Hidden,
                            border: false,
                        })
                },
            );
            rendered
                .text
                .bounds_for_source_range(caption_range.clone())
                .into_iter()
                .next()
                .expect("caption should have text bounds")
                .top()
        };

        let caption_top_with_break = caption_top(true);
        let caption_top_without_break = caption_top(false);

        assert!(
            caption_top_with_break > caption_top_without_break,
            "caption should render below the image for hard-style soft breaks; \
             top with break: {caption_top_with_break:?}, top without break: {caption_top_without_break:?}"
        );
    }

    #[gpui::test]
    fn test_surrounding_word_range(cx: &mut TestAppContext) {
        let rendered = render_markdown("Hello world tesεζ", cx);

        // Test word selection for "Hello"
        let word_range = rendered.surrounding_word_range(2); // Simulate click on 'l' in "Hello"
        let selected_text = rendered.text_for_range(word_range);
        assert_eq!(selected_text, "Hello");

        // Test word selection for "world"
        let word_range = rendered.surrounding_word_range(7); // Simulate click on 'o' in "world"
        let selected_text = rendered.text_for_range(word_range);
        assert_eq!(selected_text, "world");

        // Test word selection for "tesεζ"
        let word_range = rendered.surrounding_word_range(14); // Simulate click on 's' in "tesεζ"
        let selected_text = rendered.text_for_range(word_range);
        assert_eq!(selected_text, "tesεζ");

        // Test word selection at word boundary (space)
        let word_range = rendered.surrounding_word_range(5); // Simulate click on space between "Hello" and "world", expect highlighting word to the left
        let selected_text = rendered.text_for_range(word_range);
        assert_eq!(selected_text, "Hello");
    }

    #[gpui::test]
    fn test_surrounding_line_range(cx: &mut TestAppContext) {
        let rendered = render_markdown("First line\n\nSecond line\n\nThird lineεζ", cx);

        // Test getting line range for first line
        let line_range = rendered.surrounding_line_range(5); // Simulate click somewhere in first line
        let selected_text = rendered.text_for_range(line_range);
        assert_eq!(selected_text, "First line");

        // Test getting line range for second line
        let line_range = rendered.surrounding_line_range(13); // Simulate click at beginning in second line
        let selected_text = rendered.text_for_range(line_range);
        assert_eq!(selected_text, "Second line");

        // Test getting line range for third line
        let line_range = rendered.surrounding_line_range(37); // Simulate click at end of third line with multi-byte chars
        let selected_text = rendered.text_for_range(line_range);
        assert_eq!(selected_text, "Third lineεζ");
    }

    #[gpui::test]
    fn test_selection_head_movement(cx: &mut TestAppContext) {
        let rendered = render_markdown("Hello world test", cx);

        let mut selection = Selection {
            start: 5,
            end: 5,
            reversed: false,
            pending: false,
            mode: SelectMode::Character,
        };

        // Test forward selection
        selection.set_head(10, &rendered);
        assert_eq!(selection.start, 5);
        assert_eq!(selection.end, 10);
        assert!(!selection.reversed);
        assert_eq!(selection.tail(), 5);

        // Test backward selection
        selection.set_head(2, &rendered);
        assert_eq!(selection.start, 2);
        assert_eq!(selection.end, 5);
        assert!(selection.reversed);
        assert_eq!(selection.tail(), 5);

        // Test forward selection again from reversed state
        selection.set_head(15, &rendered);
        assert_eq!(selection.start, 5);
        assert_eq!(selection.end, 15);
        assert!(!selection.reversed);
        assert_eq!(selection.tail(), 5);
    }

    #[gpui::test]
    fn test_word_selection_drag(cx: &mut TestAppContext) {
        let rendered = render_markdown("Hello world test", cx);

        // Start with a simulated double-click on "world" (index 6-10)
        let word_range = rendered.surrounding_word_range(7); // Click on 'o' in "world"
        let mut selection = Selection {
            start: word_range.start,
            end: word_range.end,
            reversed: false,
            pending: true,
            mode: SelectMode::Word(word_range),
        };

        // Drag forward to "test" - should expand selection to include "test"
        selection.set_head(13, &rendered); // Index in "test"
        assert_eq!(selection.start, 6); // Start of "world"
        assert_eq!(selection.end, 16); // End of "test"
        assert!(!selection.reversed);
        let selected_text = rendered.text_for_range(selection.start..selection.end);
        assert_eq!(selected_text, "world test");

        // Drag backward to "Hello" - should expand selection to include "Hello"
        selection.set_head(2, &rendered); // Index in "Hello"
        assert_eq!(selection.start, 0); // Start of "Hello"
        assert_eq!(selection.end, 11); // End of "world" (original selection)
        assert!(selection.reversed);
        let selected_text = rendered.text_for_range(selection.start..selection.end);
        assert_eq!(selected_text, "Hello world");

        // Drag back within original word - should revert to original selection
        selection.set_head(8, &rendered); // Back within "world"
        assert_eq!(selection.start, 6); // Start of "world"
        assert_eq!(selection.end, 11); // End of "world"
        assert!(!selection.reversed);
        let selected_text = rendered.text_for_range(selection.start..selection.end);
        assert_eq!(selected_text, "world");
    }

    #[gpui::test]
    fn test_selection_with_markdown_formatting(cx: &mut TestAppContext) {
        let rendered = render_markdown(
            "This is **bold** text, this is *italic* text, use `code` here",
            cx,
        );
        let word_range = rendered.surrounding_word_range(10); // Inside "bold"
        let selected_text = rendered.text_for_range(word_range);
        assert_eq!(selected_text, "bold");

        let word_range = rendered.surrounding_word_range(32); // Inside "italic"
        let selected_text = rendered.text_for_range(word_range);
        assert_eq!(selected_text, "italic");

        let word_range = rendered.surrounding_word_range(51); // Inside "code"
        let selected_text = rendered.text_for_range(word_range);
        assert_eq!(selected_text, "code");
    }

    #[gpui::test]
    fn test_table_column_selection(cx: &mut TestAppContext) {
        let rendered = render_markdown("| a | b |\n|---|---|\n| c | d |", cx);

        assert!(rendered.lines.len() >= 2);
        let first_bounds = rendered.lines[0].layout.bounds();
        let second_bounds = rendered.lines[1].layout.bounds();

        let first_index = match rendered.source_index_for_position(first_bounds.center()) {
            Ok(index) | Err(index) => index,
        };
        let second_index = match rendered.source_index_for_position(second_bounds.center()) {
            Ok(index) | Err(index) => index,
        };

        let first_word = rendered.text_for_range(rendered.surrounding_word_range(first_index));
        let second_word = rendered.text_for_range(rendered.surrounding_word_range(second_index));

        assert_eq!(first_word, "a");
        assert_eq!(second_word, "b");
    }

    #[test]
    fn test_table_state_current_cell_alignment_centers_headers() {
        let mut table = TableState::default();
        table.start(vec![Alignment::Left, Alignment::Right, Alignment::None]);

        table.start_head();
        for _ in 0..3 {
            assert_eq!(table.current_cell_alignment(), Some(Alignment::Center));
            table.end_cell();
        }

        table.end_head();
        table.start_row();
        assert_eq!(table.current_cell_alignment(), Some(Alignment::Left));
        table.end_cell();
        assert_eq!(table.current_cell_alignment(), Some(Alignment::Right));
        table.end_cell();
        assert_eq!(table.current_cell_alignment(), Some(Alignment::None));
        table.end_cell();
        table.end_row();

        table.end();
        assert_eq!(table.current_cell_alignment(), None);
    }

    #[test]
    fn test_table_state_current_cell_alignment_outside_table() {
        let table = TableState::default();
        assert_eq!(table.current_cell_alignment(), None);
    }

    #[test]
    fn test_table_checkbox_detection() {
        let md = "| Done |\n|------|\n| [x] |\n| [ ] |";
        let events = crate::parser::parse_markdown_with_options(md, false, false, false).events;

        let mut in_table = false;
        let mut cell_texts: Vec<String> = Vec::new();
        let mut current_cell = String::new();

        for (range, event) in &events {
            match event {
                MarkdownEvent::Start(MarkdownTag::Table(_)) => in_table = true,
                MarkdownEvent::End(MarkdownTagEnd::Table) => in_table = false,
                MarkdownEvent::Start(MarkdownTag::TableCell) => current_cell.clear(),
                MarkdownEvent::End(MarkdownTagEnd::TableCell) => {
                    if in_table {
                        cell_texts.push(current_cell.clone());
                    }
                }
                MarkdownEvent::Text if in_table => {
                    current_cell.push_str(&md[range.clone()]);
                }
                _ => {}
            }
        }

        let checkbox_cells: Vec<&String> = cell_texts
            .iter()
            .filter(|t| {
                let trimmed = t.trim();
                trimmed == "[x]" || trimmed == "[X]" || trimmed == "[ ]"
            })
            .collect();
        assert_eq!(
            checkbox_cells.len(),
            2,
            "Expected 2 checkbox cells, got: {cell_texts:?}"
        );
        assert_eq!(checkbox_cells[0].trim(), "[x]");
        assert_eq!(checkbox_cells[1].trim(), "[ ]");
    }

    #[test]
    fn test_table_checkbox_marker_source_range() {
        let md = "| Done |\n|------|\n|  [x]  |\n| [ ] |";
        let events = crate::parser::parse_markdown_with_options(md, false, false, false).events;

        let mut in_cell = false;
        let mut pending_text = String::new();
        let mut mappings: Vec<SourceMapping> = Vec::new();
        let mut cell_ranges: Vec<Range<usize>> = Vec::new();

        for (range, event) in &events {
            match event {
                MarkdownEvent::Start(MarkdownTag::TableCell) => {
                    in_cell = true;
                    pending_text.clear();
                    mappings.clear();
                }
                MarkdownEvent::End(MarkdownTagEnd::TableCell) => {
                    if in_cell {
                        let trimmed = pending_text.trim();
                        if trimmed == "[x]" || trimmed == "[X]" || trimmed == "[ ]" {
                            let leading = pending_text.len() - pending_text.trim_start().len();
                            let rendered = leading..leading + trimmed.len();
                            let marker_source = source_range_for_rendered(&mappings, &rendered)
                                .expect("marker source range");
                            cell_ranges.push(marker_source);
                        }
                    }
                    in_cell = false;
                }
                MarkdownEvent::Text if in_cell => {
                    mappings.push(SourceMapping {
                        rendered_index: pending_text.len(),
                        source_index: range.start,
                    });
                    pending_text.push_str(&md[range.clone()]);
                }
                _ => {}
            }
        }

        assert_eq!(cell_ranges.len(), 2);
        for marker_range in &cell_ranges {
            let slice = &md[marker_range.clone()];
            assert!(
                slice == "[x]" || slice == "[X]" || slice == "[ ]",
                "expected `[x]`/`[X]`/`[ ]`, got {slice:?} at {marker_range:?}"
            );
        }
    }

    #[gpui::test]
    fn test_escaped_pipes_in_inline_code_inside_tables(cx: &mut TestAppContext) {
        let markdown = "\
| Pattern | What it does |
| --- | --- |
| `^echo(\\s\\|$)` | command pattern |
| `a\\|b` | alternation |
| `(a\\|b)` | grouped alternation |
| `a\\|\\|b` | empty middle alternative |";
        let rendered = render_markdown(markdown, cx);
        let text = rendered.text_for_range(0..markdown.len());

        assert_eq!(
            text,
            "Pattern\n\
             What it does\n\
             ^echo(\\s|$)\n\
             command pattern\n\
             a|b\n\
             alternation\n\
             (a|b)\n\
             grouped alternation\n\
             a||b\n\
             empty middle alternative"
        );
    }

    #[test]
    fn test_source_range_for_rendered_handles_split_chunks() {
        let mappings = vec![
            SourceMapping {
                rendered_index: 0,
                source_index: 20,
            },
            SourceMapping {
                rendered_index: 1,
                source_index: 21,
            },
            SourceMapping {
                rendered_index: 2,
                source_index: 22,
            },
        ];

        let range = source_range_for_rendered(&mappings, &(0..3)).unwrap();
        assert_eq!(range, 20..23);

        let range = source_range_for_rendered(&mappings, &(1..2)).unwrap();
        assert_eq!(range, 21..22);

        assert_eq!(source_range_for_rendered(&mappings, &(2..2)), None);
    }

    #[gpui::test]
    fn test_inline_code_word_selection_excludes_backticks(cx: &mut TestAppContext) {
        // Test that double-clicking on inline code selects just the code content,
        // not the backticks. This verifies the fix for the bug where selecting
        // inline code would include the trailing backtick.
        let rendered = render_markdown("use `blah` here", cx);

        // Source layout: "use `blah` here"
        //                 0123456789...
        // The inline code "blah" is at source positions 5-8 (content range 5..9)

        // Click inside "blah" - should select just "blah", not "blah`"
        let word_range = rendered.surrounding_word_range(6); // 'l' in "blah"

        // text_for_range extracts from the rendered text (without backticks), so it
        // would return "blah" even with a wrong source range. We check it anyway.
        let selected_text = rendered.text_for_range(word_range.clone());
        assert_eq!(selected_text, "blah");

        // The source range is what matters for copy_as_markdown and selected_text,
        // which extract directly from the source. With the bug, this would be 5..10
        // which includes the closing backtick at position 9.
        assert_eq!(word_range, 5..9);
    }

    #[gpui::test]
    fn test_surrounding_word_range_respects_word_characters(cx: &mut TestAppContext) {
        let rendered = render_markdown("foo.bar() baz", cx);

        // Double clicking on 'f' in "foo" - should select just "foo"
        let word_range = rendered.surrounding_word_range(0);
        let selected_text = rendered.text_for_range(word_range);
        assert_eq!(selected_text, "foo");

        // Double clicking on 'b' in "bar" - should select just "bar"
        let word_range = rendered.surrounding_word_range(4);
        let selected_text = rendered.text_for_range(word_range);
        assert_eq!(selected_text, "bar");

        // Double clicking on 'b' in "baz" - should select "baz"
        let word_range = rendered.surrounding_word_range(10);
        let selected_text = rendered.text_for_range(word_range);
        assert_eq!(selected_text, "baz");

        // Double clicking selects word characters in code blocks
        let javascript_language = Arc::new(Language::new(
            LanguageConfig {
                name: "JavaScript".into(),
                matcher: (LanguageMatcher {
                    path_suffixes: vec!["js".to_string()],
                    ..Default::default()
                })
                .into(),
                word_characters: ['$', '#'].into_iter().collect(),
                ..Default::default()
            },
            None,
        ));

        let language_registry = Arc::new(LanguageRegistry::test(cx.executor()));
        language_registry.add(javascript_language);

        let rendered = render_markdown_with_language_registry(
            "```javascript\n$foo #bar\n```",
            Some(language_registry),
            cx,
        );

        let word_range = rendered.surrounding_word_range(14);
        let selected_text = rendered.text_for_range(word_range);
        assert_eq!(selected_text, "$foo");

        let word_range = rendered.surrounding_word_range(19);
        let selected_text = rendered.text_for_range(word_range);
        assert_eq!(selected_text, "#bar");
    }

    #[gpui::test]
    fn test_all_selection(cx: &mut TestAppContext) {
        let rendered = render_markdown("Hello world\n\nThis is a test\n\nwith multiple lines", cx);

        let total_length = rendered
            .lines
            .last()
            .map(|line| line.source_end)
            .unwrap_or(0);

        let mut selection = Selection {
            start: 0,
            end: total_length,
            reversed: false,
            pending: true,
            mode: SelectMode::All,
        };

        selection.set_head(5, &rendered); // Try to set head in middle
        assert_eq!(selection.start, 0);
        assert_eq!(selection.end, total_length);
        assert!(!selection.reversed);

        selection.set_head(25, &rendered); // Try to set head near end
        assert_eq!(selection.start, 0);
        assert_eq!(selection.end, total_length);
        assert!(!selection.reversed);

        let selected_text = rendered.text_for_range(selection.start..selection.end);
        assert_eq!(
            selected_text,
            "Hello world\nThis is a test\nwith multiple lines"
        );
    }

    fn nbsp(n: usize) -> String {
        "\u{00A0}".repeat(n)
    }

    #[test]
    fn test_escape_plain_text() {
        assert_eq!(Markdown::escape("hello world"), "hello world");
        assert_eq!(Markdown::escape(""), "");
        assert_eq!(Markdown::escape("café ☕ naïve"), "café ☕ naïve");
    }

    #[test]
    fn test_escape_punctuation() {
        assert_eq!(Markdown::escape("hello `world`"), r"hello \`world\`");
        assert_eq!(Markdown::escape("a|b"), r"a\|b");
    }

    #[test]
    fn test_escape_leading_spaces() {
        assert_eq!(Markdown::escape("    hello"), [&nbsp(4), "hello"].concat());
        assert_eq!(
            Markdown::escape("    | { a: string }"),
            [&nbsp(4), r"\| \{ a\: string \}"].concat()
        );
        assert_eq!(
            Markdown::escape("  first\n  second"),
            [&nbsp(2), "first\n\n", &nbsp(2), "second"].concat()
        );
        assert_eq!(Markdown::escape("hello   world"), "hello   world");
    }

    #[test]
    fn test_escape_leading_tabs() {
        assert_eq!(Markdown::escape("\thello"), [&nbsp(4), "hello"].concat());
        assert_eq!(
            Markdown::escape("hello\n\t\tindented"),
            ["hello\n\n", &nbsp(8), "indented"].concat()
        );
        assert_eq!(
            Markdown::escape(" \t hello"),
            [&nbsp(1 + 4 + 1), "hello"].concat()
        );
        assert_eq!(Markdown::escape("hello\tworld"), "hello\tworld");
    }

    #[test]
    fn test_escape_newlines() {
        assert_eq!(Markdown::escape("a\nb"), "a\n\nb");
        assert_eq!(Markdown::escape("a\n\nb"), "a\n\n\n\nb");
        assert_eq!(Markdown::escape("\nhello"), "\n\nhello");
    }

    #[test]
    fn test_escape_multiline_diagnostic() {
        assert_eq!(
            Markdown::escape("    | { a: string }\n    | { b: number }"),
            [
                &nbsp(4),
                r"\| \{ a\: string \}",
                "\n\n",
                &nbsp(4),
                r"\| \{ b\: number \}",
            ]
            .concat()
        );
    }

    #[test]
    fn test_escape_non_ascii() {
        // Cyrillic characters should not have backslashes added before them,
        // but ASCII punctuation should still be escaped.
        assert_eq!(Markdown::escape("Привет, мир"), r"Привет\, мир");
        // Test with markdown special characters mixed in
        assert_eq!(Markdown::escape("Привет, *мир*"), r"Привет\, \*мир\*");
        // Test with the exact example from the issue (single quotes are also ASCII punctuation)
        assert_eq!(
            Markdown::escape("Отсутствует пробел справа от ','"),
            r"Отсутствует пробел справа от \'\,\'"
        );
        // Test more non-ASCII scripts
        assert_eq!(
            Markdown::escape("こんにちは *world*"),
            r"こんにちは \*world\*"
        );
        assert_eq!(Markdown::escape("العربيّة [link]"), r"العربيّة \[link\]");
        assert_eq!(Markdown::escape("Ελληνικά _text_"), r"Ελληνικά \_text\_");
        assert_eq!(Markdown::escape("עברית `code`"), r"עברית \`code\`");
        // Non-ASCII followed by ASCII punctuation
        assert_eq!(Markdown::escape("Test: тест"), r"Test\: тест");
    }

    fn has_code_block(markdown: &str) -> bool {
        let parsed_data = crate::parser::parse_markdown_with_options(markdown, false, false, false);
        parsed_data
            .events
            .iter()
            .any(|(_, event)| matches!(event, MarkdownEvent::Start(MarkdownTag::CodeBlock { .. })))
    }

    #[test]
    fn test_escape_output_len_matches_precomputed() {
        let cases = [
            "",
            "hello world",
            "hello `world`",
            "    hello",
            "    | { a: string }",
            "\thello",
            "hello\n\t\tindented",
            " \t hello",
            "hello\tworld",
            "a\nb",
            "a\n\nb",
            "\nhello",
            "    | { a: string }\n    | { b: number }",
            "café ☕ naïve",
        ];
        for input in cases {
            let mut escaper = MarkdownEscaper::new();
            let precomputed: usize = input.chars().map(|c| escaper.next(c).output_len(c)).sum();

            let mut escaper = MarkdownEscaper::new();
            let mut output = String::new();
            for c in input.chars() {
                escaper.next(c).write_to(c, &mut output);
            }

            assert_eq!(precomputed, output.len(), "length mismatch for {:?}", input);
        }
    }

    #[gpui::test]
    fn test_inline_br_renders_as_line_break(cx: &mut TestAppContext) {
        let options = MarkdownOptions {
            parse_html: true,
            ..Default::default()
        };

        for br in ["<br>", "<br/>", "<br />"] {
            let md = format!("first{br}second");
            let rendered = render_markdown_with_options(&md, None, options, cx);
            let text: String = rendered
                .lines
                .iter()
                .map(|line| line.layout.wrapped_text())
                .collect();
            assert!(
                !text.contains(br),
                "{br} should not appear as literal text; got: {text:?}"
            );
            assert!(
                text.contains("first\nsecond"),
                "{br} should produce a newline between 'first' and 'second'; got: {text:?}"
            );
        }
    }

    #[gpui::test]
    fn test_hard_break_in_text_paragraph_after_paragraph(cx: &mut TestAppContext) {
        let options = MarkdownOptions {
            parse_html: true,
            ..Default::default()
        };
        let rendered =
            render_markdown_with_options("para one\n\nfirst<br>second", None, options, cx);
        let all_text: String = rendered
            .lines
            .iter()
            .map(|line| line.layout.wrapped_text())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            all_text.contains("first") && all_text.contains("second"),
            "both sides of <br> should be present; got: {all_text:?}"
        );
        assert!(
            !all_text.contains("<br>"),
            "<br> should not appear as literal text; got: {all_text:?}"
        );
    }

    #[test]
    fn test_escape_prevents_code_block() {
        let diagnostic = "    | { a: string }";
        assert!(has_code_block(diagnostic));
        assert!(!has_code_block(&Markdown::escape(diagnostic)));
    }

    #[gpui::test]
    fn test_link_detected_for_source_index(cx: &mut TestAppContext) {
        let rendered = render_markdown("[Click here](https://example.com)", cx);

        assert_eq!(rendered.links.len(), 1);
        assert_eq!(rendered.links[0].destination_url, "https://example.com");

        // Source index 1 ('C' in "Click") is inside the link's source range
        let link = rendered.link_for_source_index(1);
        assert!(link.is_some());
        assert_eq!(link.unwrap().destination_url, "https://example.com");

        // A source index past the end of the link range returns None
        let past_end = rendered.links[0].source_range.end;
        assert!(rendered.link_for_source_index(past_end).is_none());
    }

    #[gpui::test]
    fn test_link_for_source_index_ignores_plain_text(cx: &mut TestAppContext) {
        let rendered = render_markdown("Hello world", cx);

        assert!(rendered.links.is_empty());
        assert!(rendered.link_for_source_index(0).is_none());
        assert!(rendered.link_for_source_index(5).is_none());
    }

    #[gpui::test]
    fn test_code_span_link_detected_for_source_index(cx: &mut TestAppContext) {
        let source = "see `foo.rs` for details";
        let rendered = render_markdown_with_code_span_link(
            source,
            |text, _cx| (text == "foo.rs").then(|| "file:///tmp/foo.rs".into()),
            cx,
        );

        assert_eq!(rendered.links.len(), 1);
        assert_eq!(rendered.links[0].destination_url, "file:///tmp/foo.rs");

        let code_index = source.find("foo.rs").unwrap();
        let link = rendered.link_for_source_index(code_index);
        assert!(link.is_some());
        assert_eq!(link.unwrap().destination_url, "file:///tmp/foo.rs");

        assert!(
            rendered
                .link_for_source_index(source.find("see").unwrap())
                .is_none()
        );
    }

    #[gpui::test]
    fn test_code_span_link_receives_decoded_inline_code(cx: &mut TestAppContext) {
        let source = r"| Pattern |
| --- |
| `a\|b` |";
        let rendered = render_markdown_with_code_span_link(
            source,
            |text, _cx| (text == "a|b").then(|| "file:///tmp/a-or-b".into()),
            cx,
        );

        assert_eq!(rendered.links.len(), 1);
        assert_eq!(rendered.links[0].destination_url, "file:///tmp/a-or-b");
    }

    #[gpui::test]
    fn test_code_span_link_ignores_code_when_mouse_interaction_is_prevented(
        cx: &mut TestAppContext,
    ) {
        let callback_count = Arc::new(AtomicUsize::new(0));
        let rendered = render_markdown_with_code_span_link_style(
            "see `foo.rs` for details",
            MarkdownStyle {
                prevent_mouse_interaction: true,
                ..MarkdownStyle::default()
            },
            {
                let callback_count = callback_count.clone();
                move |text, _cx| {
                    callback_count.fetch_add(1, Ordering::Relaxed);
                    (text == "foo.rs").then(|| "file:///tmp/foo.rs".into())
                }
            },
            cx,
        );

        assert!(rendered.links.is_empty());
        assert_eq!(callback_count.load(Ordering::Relaxed), 0);
    }

    #[gpui::test]
    fn test_code_span_link_ignores_code_without_callback(cx: &mut TestAppContext) {
        let rendered = render_markdown("see `foo.rs` for details", cx);

        assert!(rendered.links.is_empty());
    }

    #[gpui::test]
    fn test_code_span_link_ignores_code_inside_markdown_link(cx: &mut TestAppContext) {
        let source = "see [`foo.rs`](https://example.com) for details";
        let rendered = render_markdown_with_code_span_link(
            source,
            |text, _cx| (text == "foo.rs").then(|| "file:///tmp/foo.rs".into()),
            cx,
        );

        assert_eq!(rendered.links.len(), 1);
        assert_eq!(rendered.links[0].destination_url, "https://example.com");
    }

    #[gpui::test]
    fn test_context_menu_link_initial_state(cx: &mut TestAppContext) {
        ensure_theme_initialized(cx);
        let (_, cx) = cx.add_window_view(|_, _| TestWindow);
        let markdown =
            cx.new(|cx| Markdown::new("Hello [world](https://example.com)".into(), None, None, cx));
        cx.run_until_parked();

        cx.update(|_window, cx| {
            assert!(markdown.read(cx).context_menu_link().is_none());
        });
    }

    #[gpui::test]
    fn test_url_hover_callback(cx: &mut TestAppContext) {
        struct HoverTestView {
            markdown: Entity<Markdown>,
            hovered_urls: Rc<RefCell<Vec<Option<SharedString>>>>,
        }

        impl Render for HoverTestView {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                let hovered_urls = self.hovered_urls.clone();
                div().size_full().child(
                    MarkdownElement::new(self.markdown.clone(), MarkdownStyle::default())
                        .on_url_hover(move |url, _, _| {
                            hovered_urls.borrow_mut().push(url);
                        }),
                )
            }
        }

        ensure_theme_initialized(cx);
        let hovered_urls = Rc::new(RefCell::new(Vec::new()));
        let (_, cx) = cx.add_window_view({
            let hovered_urls = hovered_urls.clone();
            move |_, cx| HoverTestView {
                markdown: cx
                    .new(|cx| Markdown::new("[link](https://example.com)".into(), None, None, cx)),
                hovered_urls,
            }
        });
        cx.run_until_parked();

        cx.simulate_mouse_move(point(px(8.), px(8.)), None, gpui::Modifiers::default());
        assert_eq!(
            hovered_urls.borrow().last().cloned().flatten().as_deref(),
            Some("https://example.com")
        );

        cx.simulate_mouse_move(point(px(500.), px(500.)), None, gpui::Modifiers::default());
        assert_eq!(hovered_urls.borrow().last(), Some(&None));
    }

    #[gpui::test]
    fn test_url_hover_callback_for_linked_image(cx: &mut TestAppContext) {
        struct HoverTestView {
            markdown: Entity<Markdown>,
            hovered_urls: Rc<RefCell<Vec<Option<SharedString>>>>,
        }

        impl Render for HoverTestView {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                let hovered_urls = self.hovered_urls.clone();
                div().size_full().child(
                    MarkdownElement::new(self.markdown.clone(), MarkdownStyle::default())
                        .image_resolver(|_| Some(loaded_image_source()))
                        .on_url_hover(move |url, _, _| {
                            hovered_urls.borrow_mut().push(url);
                        }),
                )
            }
        }

        ensure_theme_initialized(cx);
        let hovered_urls = Rc::new(RefCell::new(Vec::new()));
        let (_, cx) = cx.add_window_view({
            let hovered_urls = hovered_urls.clone();
            move |_, cx| HoverTestView {
                markdown: cx.new(|cx| {
                    Markdown::new(
                        "[![badge](https://example.com/badge.png)](https://example.com)".into(),
                        None,
                        None,
                        cx,
                    )
                }),
                hovered_urls,
            }
        });
        cx.run_until_parked();

        cx.simulate_mouse_move(point(px(4.), px(4.)), None, gpui::Modifiers::default());
        assert_eq!(
            hovered_urls.borrow().last().cloned().flatten().as_deref(),
            Some("https://example.com")
        );

        cx.simulate_mouse_move(point(px(8.), px(8.)), None, gpui::Modifiers::default());
        assert_eq!(
            hovered_urls.borrow().last().cloned().flatten().as_deref(),
            Some("https://example.com")
        );

        cx.simulate_mouse_move(point(px(500.), px(500.)), None, gpui::Modifiers::default());
        assert_eq!(hovered_urls.borrow().last(), Some(&None));
    }

    #[gpui::test]
    fn test_capture_for_context_menu(cx: &mut TestAppContext) {
        ensure_theme_initialized(cx);
        let (_, cx) = cx.add_window_view(|_, _| TestWindow);
        let markdown = cx.new(|cx| Markdown::new("some text".into(), None, None, cx));
        cx.run_until_parked();

        // Simulates right-clicking on a link, with "text" selected
        let url: SharedString = "https://example.com".into();
        markdown.update(cx, |md, _cx| {
            md.selection.start = 5;
            md.selection.end = 9;
            md.capture_for_context_menu(Some(url.clone()), None);
        });
        cx.update(|_window, cx| {
            let markdown = markdown.read(cx);
            assert_eq!(
                markdown.context_menu_link().map(SharedString::as_ref),
                Some("https://example.com")
            );
            assert_eq!(
                markdown
                    .context_menu_selected_markdown()
                    .map(SharedString::as_ref),
                Some("text")
            );
            assert_eq!(
                markdown
                    .context_menu_selected_text()
                    .map(SharedString::as_ref),
                Some("text")
            );
        });

        // Simulates right-clicking on plain text with no selection — everything is cleared
        markdown.update(cx, |md, _cx| {
            md.selection.start = 0;
            md.selection.end = 0;
            md.capture_for_context_menu(None, None);
        });
        cx.update(|_window, cx| {
            let markdown = markdown.read(cx);
            assert!(markdown.context_menu_link().is_none());
            assert!(markdown.context_menu_selected_markdown().is_none());
            assert!(markdown.context_menu_selected_text().is_none());
        });
    }

    #[gpui::test]
    fn test_preview_body_font_size_is_rem_based(cx: &mut TestAppContext) {
        ensure_theme_initialized(cx);
        let (_, cx) = cx.add_window_view(|_, _| TestWindow);
        cx.update(|window, cx| {
            let style = MarkdownStyle::themed(MarkdownFont::Preview, window, cx);
            assert!(
                matches!(style.base_text_style.font_size, AbsoluteLength::Rems(_)),
                "preview body font size must be rem-based, got {:?}",
                style.base_text_style.font_size
            );
            assert!(
                matches!(
                    style.container_style.text.font_size,
                    Some(AbsoluteLength::Rems(_))
                ),
                "preview container font size must be rem-based, got {:?}",
                style.container_style.text.font_size
            );
        });
    }

    fn failing_image_source() -> ImageSource {
        ImageSource::Custom(Arc::new(|_, _| {
            Some(Err(gpui::ImageCacheError::Asset(
                "failed to load image".into(),
            )))
        }))
    }

    fn loaded_image_source() -> ImageSource {
        let buffer = image::ImageBuffer::from_pixel(16, 16, image::Rgba([0, 0, 0, 255]));
        ImageSource::Render(Arc::new(gpui::RenderImage::new(SmallVec::from_elem(
            image::Frame::new(buffer),
            1,
        ))))
    }

    fn open_markdown_image_test_window<'a>(
        source: &str,
        image_source: ImageSource,
        cx: &'a mut TestAppContext,
    ) -> &'a mut gpui::VisualTestContext {
        struct ImageTestView {
            markdown: Entity<Markdown>,
            image_source: ImageSource,
        }

        impl Render for ImageTestView {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                let image_source = self.image_source.clone();
                div().size_full().child(
                    MarkdownElement::new(self.markdown.clone(), MarkdownStyle::default())
                        .image_resolver(move |_| Some(image_source.clone())),
                )
            }
        }

        ensure_theme_initialized(cx);

        let source = source.to_string();
        let (_, cx) = cx.add_window_view(|_, cx| ImageTestView {
            markdown: cx.new(|cx| Markdown::new(source.into(), None, None, cx)),
            image_source,
        });
        cx.run_until_parked();
        cx
    }

    #[gpui::test]
    fn test_clicking_image_fallback_opens_image_url(cx: &mut TestAppContext) {
        let cx = open_markdown_image_test_window(
            "![alt text](https://example.com/image.png)",
            failing_image_source(),
            cx,
        );

        cx.simulate_click(point(px(8.), px(8.)), gpui::Modifiers::default());
        assert_eq!(
            cx.opened_url(),
            Some("https://example.com/image.png".to_string())
        );
    }

    #[gpui::test]
    fn test_clicking_image_fallback_inside_link_opens_link_url(cx: &mut TestAppContext) {
        let cx = open_markdown_image_test_window(
            "[![alt text](https://example.com/image.png)](https://example.com/link)",
            failing_image_source(),
            cx,
        );

        cx.simulate_click(point(px(8.), px(8.)), gpui::Modifiers::default());
        assert_eq!(
            cx.opened_url(),
            Some("https://example.com/link".to_string())
        );
    }

    #[gpui::test]
    fn test_clicking_loaded_image_inside_link_opens_link_url(cx: &mut TestAppContext) {
        let cx = open_markdown_image_test_window(
            "[![alt text](https://example.com/image.png)](https://example.com/link)",
            loaded_image_source(),
            cx,
        );

        cx.simulate_click(point(px(8.), px(8.)), gpui::Modifiers::default());
        assert_eq!(
            cx.opened_url(),
            Some("https://example.com/link".to_string())
        );
    }

    #[track_caller]
    fn assert_mappings(rendered: &RenderedText, expected: Vec<Vec<(usize, usize)>>) {
        assert_eq!(rendered.lines.len(), expected.len(), "line count mismatch");
        for (line_ix, line_mappings) in expected.into_iter().enumerate() {
            let line = &rendered.lines[line_ix];

            assert!(
                line.source_mappings.windows(2).all(|mappings| {
                    mappings[0].source_index < mappings[1].source_index
                        && mappings[0].rendered_index < mappings[1].rendered_index
                }),
                "line {} has duplicate mappings: {:?}",
                line_ix,
                line.source_mappings
            );

            for (rendered_ix, source_ix) in line_mappings {
                assert_eq!(
                    line.source_index_for_rendered_index(rendered_ix),
                    source_ix,
                    "line {}, rendered_ix {}",
                    line_ix,
                    rendered_ix
                );

                assert_eq!(
                    line.rendered_index_for_source_index(source_ix),
                    rendered_ix,
                    "line {}, source_ix {}",
                    line_ix,
                    source_ix
                );
            }
        }
    }

    #[gpui::test]
    fn test_bounds_for_source_range_skips_gaps_between_rendered_lines(cx: &mut TestAppContext) {
        let source = "First\n\nSecond";
        let rendered = render_markdown(source, cx);
        let highlight_bounds = rendered.bounds_for_source_range(0..source.len());
        assert_eq!(highlight_bounds.len(), rendered.lines.len());

        for (line, highlight_bounds) in rendered.lines.iter().zip(highlight_bounds.iter()) {
            let line_bounds = line.layout.bounds();
            assert_eq!(highlight_bounds.top(), line_bounds.top());
            assert_eq!(
                highlight_bounds.bottom(),
                line_bounds.top() + line.layout.line_height()
            );
        }
    }

    #[gpui::test]
    fn test_bounds_for_source_range_returns_one_bound_per_soft_wrap_row(cx: &mut TestAppContext) {
        let sentence = "Lorem ipsum dolor sit amet, consectetur adipiscing elit, \
            sed do eiusmod tempor incididunt ut labore et dolore magna aliqua.";
        let source = [sentence, sentence, sentence, sentence].join(" ");
        let rendered = render_markdown(&source, cx);
        let line = &rendered.lines[0];
        let line_bounds = line.layout.bounds();
        let line_height = line.layout.line_height();
        let LineGeometry::Text(text_layout) = &line.layout else {
            panic!("a math-free paragraph should render as a plain-text line");
        };
        let wrapped_line = text_layout.line_layout_for_index(0).unwrap();
        let visual_row_count = wrapped_line.wrap_boundaries().len() + 1;

        let highlight_bounds = rendered.bounds_for_source_range(0..source.len());
        assert_eq!(highlight_bounds.len(), visual_row_count);

        let mut row_top = line_bounds.top();
        for (row_index, row_bounds) in highlight_bounds.iter().enumerate() {
            assert_eq!(row_bounds.top(), row_top);
            assert_eq!(row_bounds.bottom(), row_top + line_height);
            assert!(
                row_bounds.size.width > Pixels::ZERO,
                "row {row_index} should have a non-empty highlight"
            );
            row_top += line_height;
        }
    }

    #[gpui::test]
    fn test_heading_font_sizes_are_distinct(cx: &mut TestAppContext) {
        let rendered = render_markdown("# H1\n\n## H2\n\n### H3\n\nBody text", cx);

        assert!(
            rendered.lines.len() >= 4,
            "expected at least 4 rendered lines, got {}",
            rendered.lines.len()
        );

        let h1_line_height = rendered.lines[0].layout.line_height();
        let h2_line_height = rendered.lines[1].layout.line_height();
        let h3_line_height = rendered.lines[2].layout.line_height();
        let body_line_height = rendered.lines[3].layout.line_height();

        assert!(
            h1_line_height > h2_line_height,
            "H1 line height ({h1_line_height:?}) should be greater than H2 ({h2_line_height:?})"
        );
        assert!(
            h2_line_height > h3_line_height,
            "H2 line height ({h2_line_height:?}) should be greater than H3 ({h3_line_height:?})"
        );
        assert!(
            h3_line_height > body_line_height,
            "H3 line height ({h3_line_height:?}) should be greater than body text ({body_line_height:?})"
        );
    }

    #[gpui::test]
    fn test_ui_zoom_does_not_affect_markdown_preview(cx: &mut TestAppContext) {
        ensure_theme_initialized(cx);

        cx.update(|cx| {
            settings::SettingsStore::update_global(cx, |store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.theme.ui_font_size = Some(16.0.into());
                    settings.theme.markdown_preview_font_size = None;
                });
            });
        });
        cx.run_until_parked();

        cx.update(|cx| {
            let before = ThemeSettings::get_global(cx).markdown_preview_font_size(cx);
            assert_eq!(before, px(16.0));

            theme_settings::adjust_ui_font_size(cx, |size| size + px(3.0));

            assert_eq!(ThemeSettings::get_global(cx).ui_font_size(cx), px(19.0));
            assert_eq!(
                ThemeSettings::get_global(cx).markdown_preview_font_size(cx),
                before
            );
        });
    }

    #[gpui::test]
    fn test_markdown_preview_follows_ui_font_size_setting_when_unset(cx: &mut TestAppContext) {
        ensure_theme_initialized(cx);

        cx.update(|cx| {
            settings::SettingsStore::update_global(cx, |store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.theme.ui_font_size = Some(20.0.into());
                    settings.theme.markdown_preview_font_size = None;
                });
            });
        });
        cx.run_until_parked();
        cx.update(|cx| {
            assert_eq!(
                ThemeSettings::get_global(cx).markdown_preview_font_size(cx),
                px(20.0)
            );
        });

        cx.update(|cx| {
            settings::SettingsStore::update_global(cx, |store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.theme.ui_font_size = Some(24.0.into());
                });
            });
        });
        cx.run_until_parked();
        cx.update(|cx| {
            assert_eq!(
                ThemeSettings::get_global(cx).markdown_preview_font_size(cx),
                px(24.0)
            );
        });
    }
}
