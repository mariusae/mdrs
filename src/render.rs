use std::fmt::Write as _;
use std::io::{self, Write};
use std::sync::Arc;

use comrak::nodes::{AstNode, ListType, NodeValue, TableAlignment};
use comrak::{Arena, Options, parse_document};
use thiserror::Error;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::style::RenderStyle;
use crate::{BOLD, DIM, FG_BLUE, ITALIC, OSC8_END, RESET, UNDERLINE, osc8_start};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Heading {
    pub level: u8,
    pub text: String,
    /// Zero-based line in the rendered output, or `None` when extracted without rendering.
    pub line: Option<usize>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CodeBlock {
    pub line: usize,
    pub text: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RenderedLink {
    pub url: String,
    pub line: usize,
    pub start_col: usize,
    pub end_col: usize,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RenderResult {
    pub output: String,
    pub headings: Vec<Heading>,
    pub code_blocks: Vec<CodeBlock>,
    pub links: Vec<RenderedLink>,
    pub(crate) line_mappings: Vec<RenderLineMapping>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LinkKind {
    Markdown,
    Wiki,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LinkRequest {
    pub kind: LinkKind,
    pub target: String,
    pub label: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LinkResolution {
    pub url: String,
    pub label: Option<String>,
}

pub type LinkResolver = Arc<dyn Fn(&LinkRequest) -> Option<LinkResolution> + Send + Sync>;

#[derive(Clone)]
pub struct RenderOptions {
    pub width: usize,
    pub osc8: bool,
    pub style: RenderStyle,
    pub link_resolver: Option<LinkResolver>,
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self {
            width: 0,
            osc8: false,
            style: RenderStyle::default(),
            link_resolver: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct SourceSpan {
    pub(crate) start: usize,
    pub(crate) end: usize,
}

impl SourceSpan {
    pub(crate) fn valid(self) -> bool {
        self.end > self.start
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct RenderLineMapping {
    pub(crate) spans: Vec<SourceSpan>,
}

#[derive(Debug, Error)]
pub enum RenderError {
    #[error("markdown source is not valid UTF-8")]
    InvalidUtf8(#[from] std::str::Utf8Error),
    #[error("writing rendered output failed")]
    Io(#[from] io::Error),
}

pub type Result<T> = std::result::Result<T, RenderError>;

pub fn render(source: &[u8], width: usize, osc8: bool) -> Result<String> {
    render_with_style(source, width, osc8, &RenderStyle::default())
}

pub fn render_with_style(
    source: &[u8],
    width: usize,
    osc8: bool,
    style: &RenderStyle,
) -> Result<String> {
    Ok(render_document_with_style(source, width, osc8, style)?.output)
}

pub fn render_to<W: Write>(source: &[u8], writer: W, width: usize, osc8: bool) -> Result<()> {
    render_to_with_style(source, writer, width, osc8, &RenderStyle::default())
}

pub fn render_to_with_style<W: Write>(
    source: &[u8],
    mut writer: W,
    width: usize,
    osc8: bool,
    style: &RenderStyle,
) -> Result<()> {
    writer.write_all(render_with_style(source, width, osc8, style)?.as_bytes())?;
    Ok(())
}

pub fn render_document(source: &[u8], width: usize, osc8: bool) -> Result<RenderResult> {
    render_document_with_style(source, width, osc8, &RenderStyle::default())
}

pub fn render_document_with_style(
    source: &[u8],
    width: usize,
    osc8: bool,
    style: &RenderStyle,
) -> Result<RenderResult> {
    render_document_with_options(
        source,
        &RenderOptions {
            width,
            osc8,
            style: style.clone(),
            link_resolver: None,
        },
    )
}

pub fn render_document_with_options(
    source: &[u8],
    options: &RenderOptions,
) -> Result<RenderResult> {
    let source = std::str::from_utf8(source)?;
    let (front_matter, body) = split_front_matter(source);
    let body_offset = body.as_ptr() as usize - source.as_ptr() as usize;
    let mut renderer = AnsiRenderer::with_options(options.clone());
    renderer.render_source(body);
    let mut result = renderer.finish();
    offset_mappings(&mut result.line_mappings, body_offset);

    if let Some(front) = front_matter {
        let mut prefix = front.to_owned();
        if !front.is_empty() && !front.ends_with('\n') {
            prefix.push('\n');
        }
        let hr_width = if options.width == 0 {
            40
        } else {
            options.width
        };
        let _ = write!(prefix, "{DIM}{}{RESET}\n\n", "─".repeat(hr_width));
        let line_offset = prefix.matches('\n').count();
        for heading in &mut result.headings {
            if let Some(line) = &mut heading.line {
                *line += line_offset;
            }
        }
        for block in &mut result.code_blocks {
            block.line += line_offset;
        }
        let front_offset = front.as_ptr() as usize - source.as_ptr() as usize;
        let front_mappings = front_matter_mappings(front, front_offset, &prefix);
        result.line_mappings.splice(0..0, front_mappings);
        result.output.insert_str(0, &prefix);
    }
    result.links = extract_rendered_links(&result.output);
    Ok(result)
}

fn offset_mappings(mappings: &mut [RenderLineMapping], offset: usize) {
    for mapping in mappings {
        for span in &mut mapping.spans {
            if span.valid() {
                span.start += offset;
                span.end += offset;
            }
        }
    }
}

fn front_matter_mappings(front: &str, offset: usize, rendered: &str) -> Vec<RenderLineMapping> {
    let mut mappings = vec![RenderLineMapping::default()];
    let mut source_offset = offset;
    let front_end = front.len();
    let mut visible_index = 0;
    let mut index = 0;
    while index < rendered.len() {
        if rendered.as_bytes()[index] == 0x1b {
            index = escape_sequence_end(rendered, index);
            continue;
        }
        let character = rendered[index..].chars().next().unwrap();
        index += character.len_utf8();
        let span = if visible_index < front_end {
            let size = front[visible_index..].chars().next().unwrap().len_utf8();
            let span = SourceSpan {
                start: source_offset,
                end: source_offset + size,
            };
            visible_index += size;
            source_offset += size;
            span
        } else {
            SourceSpan::default()
        };
        if character == '\n' {
            mappings.push(RenderLineMapping::default());
            continue;
        }
        mappings.last_mut().unwrap().spans.push(span);
    }
    mappings
}

pub fn extract_headings(source: &[u8]) -> Result<Vec<Heading>> {
    let source = std::str::from_utf8(source)?;
    let (_, body) = split_front_matter(source);
    let arena = Arena::new();
    let options = parser_options();
    let root = parse_document(&arena, body, &options);
    let mut headings = Vec::new();
    collect_headings(root, &mut headings, false);
    for heading in &mut headings {
        heading.line = None;
    }
    Ok(headings)
}

fn split_front_matter(source: &str) -> (Option<&str>, &str) {
    let source_without_bom = source.strip_prefix('\u{feff}').unwrap_or(source);
    let Some(after_open) = source_without_bom
        .strip_prefix("---\n")
        .or_else(|| source_without_bom.strip_prefix("---\r\n"))
    else {
        return (None, source);
    };
    let content_start = source.len() - after_open.len();
    let mut offset = content_start;
    for line in after_open.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed == "---" || trimmed == "..." {
            let front = &source[content_start..offset];
            let body_start = offset + line.len();
            return (Some(front), &source[body_start..]);
        }
        offset += line.len();
    }
    (None, source)
}

fn parser_options<'a>() -> Options<'a> {
    let mut options = Options::default();
    options.extension.table = true;
    options.extension.strikethrough = true;
    options.extension.autolink = true;
    options.extension.tasklist = true;
    options.extension.highlight = true;
    options
}

#[derive(Clone, Debug, Default)]
struct TextStyle {
    bold: bool,
    italic: bool,
    underline: bool,
    color: String,
    background: String,
}

/// Stateful ANSI renderer. Most callers should use [`render`] or
/// [`render_document`].
pub struct AnsiRenderer {
    output: String,
    styles: Vec<TextStyle>,
    list_depth: usize,
    ordered_indices: Vec<Option<usize>>,
    indent_stack: Vec<usize>,
    line: usize,
    width: usize,
    col: usize,
    indent: usize,
    blockquote_depth: usize,
    osc8: bool,
    headings: Vec<Heading>,
    render_style: RenderStyle,
    link_resolver: Option<LinkResolver>,
    code_blocks: Vec<CodeBlock>,
    source: String,
    line_mappings: Vec<RenderLineMapping>,
    span_stack: Vec<SourceSpan>,
}

impl AnsiRenderer {
    pub fn new(width: usize, osc8: bool, render_style: RenderStyle) -> Self {
        Self::with_options(RenderOptions {
            width,
            osc8,
            style: render_style,
            link_resolver: None,
        })
    }

    pub fn with_options(options: RenderOptions) -> Self {
        Self {
            width: options.width,
            osc8: options.osc8,
            render_style: options.style,
            link_resolver: options.link_resolver,
            ..Self::empty()
        }
    }

    fn empty() -> Self {
        Self {
            output: String::new(),
            styles: Vec::new(),
            list_depth: 0,
            ordered_indices: Vec::new(),
            indent_stack: Vec::new(),
            line: 0,
            width: 0,
            col: 0,
            indent: 0,
            blockquote_depth: 0,
            osc8: false,
            headings: Vec::new(),
            render_style: RenderStyle::default(),
            link_resolver: None,
            code_blocks: Vec::new(),
            source: String::new(),
            line_mappings: Vec::new(),
            span_stack: Vec::new(),
        }
    }

    pub fn render(&mut self, source: &[u8]) -> Result<()> {
        let source = std::str::from_utf8(source)?;
        self.render_source(source);
        Ok(())
    }

    pub fn output(&self) -> &str {
        &self.output
    }
    pub fn headings(&self) -> &[Heading] {
        &self.headings
    }
    pub fn code_blocks(&self) -> &[CodeBlock] {
        &self.code_blocks
    }

    fn render_source(&mut self, source: &str) {
        self.source.clear();
        self.source.push_str(source);
        let arena = Arena::new();
        let options = parser_options();
        let root = parse_document(&arena, source, &options);
        for child in root.children() {
            self.render_node(child, false);
        }
    }

    fn finish(self) -> RenderResult {
        RenderResult {
            output: self.output,
            headings: self.headings,
            code_blocks: self.code_blocks,
            links: Vec::new(),
            line_mappings: self.line_mappings,
        }
    }

    fn render_node<'a>(&mut self, node: &'a AstNode<'a>, tight_list: bool) {
        let value = node.data.borrow().value.clone();
        match value {
            NodeValue::Document => self.render_children(node, tight_list),
            NodeValue::Heading(heading) => {
                self.push_span(self.node_block_span(node));
                let text = extract_text(node).trim().to_owned();
                self.headings.push(Heading {
                    level: heading.level,
                    text,
                    line: Some(self.line),
                });
                self.push_style(TextStyle {
                    bold: true,
                    ..Default::default()
                });
                self.render_children(node, tight_list);
                self.pop_style();
                self.pop_span();
                self.newline();
                self.newline();
            }
            NodeValue::Paragraph => {
                self.render_children(node, tight_list);
                if !tight_list {
                    self.newline();
                    if self.blockquote_depth > 0 && node.next_sibling().is_some() {
                        self.write_indent();
                    }
                    self.newline();
                } else if node.next_sibling().is_some() {
                    self.newline();
                }
            }
            NodeValue::CodeBlock(code) => {
                self.push_span(self.node_block_span(node));
                self.code_blocks.push(CodeBlock {
                    line: self.line,
                    text: code.literal.clone(),
                });
                for line in code.literal.split_inclusive('\n') {
                    let (content, ending) = split_line_ending(line);
                    if self.render_style.code_block_bg.is_empty() {
                        self.write_raw("    ");
                        self.write_raw(content);
                        self.write_raw(ending);
                    } else {
                        let bg = self.render_style.code_block_bg.clone();
                        self.write_raw(&bg);
                        self.write_raw("    ");
                        self.write_raw(content);
                        let padding = self.width.saturating_sub(4 + display_width(content));
                        self.write_raw(&" ".repeat(padding));
                        self.write_raw(RESET);
                        self.write_raw(ending);
                    }
                }
                if !code.literal.ends_with('\n') && !code.literal.is_empty() {
                    self.newline();
                }
                self.newline();
                self.col = 0;
                self.pop_span();
            }
            NodeValue::BlockQuote => {
                self.push_span(self.node_block_span(node));
                self.blockquote_depth += 1;
                self.indent += 2;
                self.render_children(node, tight_list);
                self.blockquote_depth -= 1;
                self.indent -= 2;
                self.pop_span();
            }
            NodeValue::List(list) => {
                self.list_depth += 1;
                self.ordered_indices
                    .push((list.list_type == ListType::Ordered).then_some(list.start));
                self.render_children(node, list.tight);
                self.ordered_indices.pop();
                self.list_depth -= 1;
                if self.list_depth == 0 {
                    self.newline();
                }
            }
            NodeValue::Item(_) => self.render_list_item(node, tight_list),
            NodeValue::ThematicBreak => {
                self.push_span(self.node_block_span(node));
                let width = if self.width == 0 { 40 } else { self.width };
                self.write_raw(&"─".repeat(width));
                self.newline();
                self.newline();
                self.pop_span();
            }
            NodeValue::HtmlBlock(html) => {
                self.push_span(self.node_block_span(node));
                self.write_raw(&html.literal);
                self.pop_span();
            }
            NodeValue::Text(text) => {
                self.render_text(node, &text);
            }
            NodeValue::SoftBreak => {
                self.write_wrapped_mapped(" ", vec![self.node_source_span(node)]);
            }
            NodeValue::LineBreak => {
                self.newline();
                self.write_indent();
            }
            NodeValue::Code(code) => {
                self.push_span(self.node_source_span(node));
                self.push_style(TextStyle {
                    color: FG_BLUE.into(),
                    ..Default::default()
                });
                self.write_wrapped(&code.literal);
                self.pop_style();
                self.pop_span();
            }
            NodeValue::Emph => {
                self.push_span(self.node_source_span(node));
                self.push_style(TextStyle {
                    italic: true,
                    ..Default::default()
                });
                self.render_children(node, tight_list);
                self.pop_style();
                self.pop_span();
            }
            NodeValue::Strong => {
                self.push_span(self.node_source_span(node));
                self.push_style(TextStyle {
                    bold: true,
                    ..Default::default()
                });
                self.render_children(node, tight_list);
                self.pop_style();
                self.pop_span();
            }
            NodeValue::Highlight => {
                self.push_span(self.node_source_span(node));
                let background = self.render_style.highlight_bg.clone();
                self.push_style(TextStyle {
                    background,
                    ..Default::default()
                });
                self.render_children(node, tight_list);
                self.pop_style();
                self.pop_span();
            }
            NodeValue::Strikethrough => {
                self.write_raw("<del>");
                self.render_children(node, tight_list);
                self.write_raw("</del>");
            }
            NodeValue::Link(link) => {
                self.push_span(self.node_source_span(node));
                if self
                    .node_source(node)
                    .is_some_and(|source| !source.starts_with('['))
                {
                    self.render_autolink(&link.url);
                } else {
                    self.render_link(node, &link.url, tight_list);
                }
                self.pop_span();
            }
            NodeValue::Image(_) => {
                self.push_span(self.node_source_span(node));
                self.write_raw("[image: ");
                self.render_children(node, tight_list);
                self.write_raw("]");
                self.pop_span();
            }
            NodeValue::HtmlInline(html) | NodeValue::Raw(html) => self.write_raw(&html),
            NodeValue::Table(table) => self.render_table(node, &table.alignments),
            NodeValue::TableRow(_) | NodeValue::TableCell => self.render_children(node, tight_list),
            NodeValue::TaskItem(task) => {
                self.push_span(self.node_block_span(node));
                self.indent_stack.push(self.indent);
                let prefix = format!("{}    ", "  ".repeat(self.list_depth.saturating_sub(1)));
                self.write_raw(&prefix);
                self.col = display_width(&prefix);
                self.indent = self.col;
                self.write_wrapped(if task.symbol.is_some() {
                    "☑ "
                } else {
                    "☐ "
                });
                self.indent += 2;
                self.render_children(node, tight_list);
                self.indent = self.indent_stack.pop().unwrap_or(0);
                self.newline();
                self.pop_span();
            }
            _ => self.render_children(node, tight_list),
        }
    }

    fn render_children<'a>(&mut self, node: &'a AstNode<'a>, tight_list: bool) {
        for child in node.children() {
            self.render_node(child, tight_list);
        }
    }

    fn render_list_item<'a>(&mut self, node: &'a AstNode<'a>, tight: bool) {
        self.push_span(self.node_block_span(node));
        self.indent_stack.push(self.indent);
        let nested = "  ".repeat(self.list_depth.saturating_sub(1));
        let is_task = node
            .first_child()
            .and_then(|p| p.first_child())
            .is_some_and(|n| matches!(n.data.borrow().value, NodeValue::TaskItem(_)));
        let prefix = match self.ordered_indices.last_mut().and_then(Option::as_mut) {
            Some(index) => {
                let prefix = format!("{nested}  {index}. ");
                *index += 1;
                prefix
            }
            None if is_task => format!("{nested}    "),
            None => format!("{nested}  • "),
        };
        self.write_raw(&prefix);
        self.col = display_width(&prefix);
        self.indent = self.col;
        self.render_children(node, tight);
        self.indent = self.indent_stack.pop().unwrap_or(0);
        self.pop_span();
        self.newline();
    }

    fn render_text(&mut self, node: &AstNode<'_>, text: &str) {
        let Some(resolver) = self.link_resolver.clone() else {
            self.render_plain_text(node, text);
            return;
        };
        let mut rest = text;
        while let Some(start) = rest.find("[[") {
            let before = &rest[..start];
            self.render_plain_text(node, before);
            let candidate = &rest[start + 2..];
            let Some(end) = candidate.find("]]") else {
                self.render_plain_text(node, &rest[start..]);
                return;
            };
            let inner = &candidate[..end];
            let (target, alias) = split_wiki_link(inner);
            if target.trim().is_empty() {
                self.render_plain_text(node, &rest[start..start + end + 4]);
            } else {
                let request = LinkRequest {
                    kind: LinkKind::Wiki,
                    target: target.to_string(),
                    label: alias.map(str::to_string),
                };
                if let Some(resolution) = resolver(&request) {
                    let label = resolution
                        .label
                        .as_deref()
                        .or(request.label.as_deref())
                        .unwrap_or(&request.target)
                        .to_string();
                    self.render_resolved_link_text(&label, &resolution.url);
                } else {
                    self.render_plain_text(node, &rest[start..start + end + 4]);
                }
            }
            rest = &candidate[end + 2..];
        }
        self.render_plain_text(node, rest);
    }

    fn render_plain_text(&mut self, node: &AstNode<'_>, text: &str) {
        if text.is_empty() {
            return;
        }
        if self.current_span().valid() {
            self.write_wrapped(text);
        } else {
            self.write_wrapped_mapped(text, self.text_spans(node, text));
        }
    }

    fn render_resolved_link_text(&mut self, label: &str, url: &str) {
        if self.osc8 {
            self.write_raw(&osc8_start(url));
            self.push_style(TextStyle {
                color: FG_BLUE.into(),
                underline: true,
                ..Default::default()
            });
            self.write_wrapped(label);
            self.pop_style();
            self.write_raw(OSC8_END);
        } else {
            self.push_style(TextStyle {
                color: FG_BLUE.into(),
                ..Default::default()
            });
            self.write_wrapped(label);
            self.write_wrapped(" (");
            self.push_style(TextStyle {
                underline: true,
                ..Default::default()
            });
            self.write_wrapped(url);
            self.pop_style();
            self.write_wrapped(")");
            self.pop_style();
        }
    }

    fn render_link<'a>(&mut self, node: &'a AstNode<'a>, url: &str, tight: bool) {
        let resolved = self.link_resolver.as_ref().and_then(|resolver| {
            resolver(&LinkRequest {
                kind: LinkKind::Markdown,
                target: url.to_string(),
                label: None,
            })
        });
        let href = resolved
            .as_ref()
            .map_or(url, |resolution| resolution.url.as_str());
        if let Some(label) = resolved
            .as_ref()
            .and_then(|resolution| resolution.label.as_deref())
        {
            self.render_resolved_link_text(label, href);
            return;
        }
        if self.osc8 {
            self.write_raw(&osc8_start(href));
            self.push_style(TextStyle {
                color: FG_BLUE.into(),
                underline: true,
                ..Default::default()
            });
            self.render_children(node, tight);
            self.pop_style();
            self.write_raw(OSC8_END);
        } else {
            self.push_style(TextStyle {
                color: FG_BLUE.into(),
                ..Default::default()
            });
            self.render_children(node, tight);
            self.write_wrapped(" (");
            self.push_style(TextStyle {
                underline: true,
                ..Default::default()
            });
            self.write_wrapped(href);
            self.pop_style();
            self.write_wrapped(")");
            self.pop_style();
        }
    }

    fn render_autolink(&mut self, url: &str) {
        if self.osc8 {
            self.write_raw(&osc8_start(url));
            self.push_style(TextStyle {
                color: FG_BLUE.into(),
                underline: true,
                ..Default::default()
            });
            self.write_wrapped(url);
            self.pop_style();
            self.write_raw(OSC8_END);
        } else {
            self.write_wrapped(url);
        }
    }

    fn node_source<'a>(&'a self, node: &AstNode<'_>) -> Option<&'a str> {
        let sourcepos = node.data.borrow().sourcepos;
        if sourcepos.start.line != sourcepos.end.line || sourcepos.start.line == 0 {
            return None;
        }
        let line = self
            .source
            .split_inclusive('\n')
            .nth(sourcepos.start.line - 1)?;
        let start = sourcepos.start.column.checked_sub(1)?;
        let end = sourcepos.end.column.min(line.len());
        line.get(start..end)
    }

    fn node_source_span(&self, node: &AstNode<'_>) -> SourceSpan {
        let position = node.data.borrow().sourcepos;
        let Some(start) = self.source_offset(position.start.line, position.start.column) else {
            return SourceSpan::default();
        };
        let Some(end_start) = self.source_offset(position.end.line, position.end.column) else {
            return SourceSpan::default();
        };
        let end = (end_start + 1).min(self.source.len());
        SourceSpan { start, end }
    }

    fn node_block_span(&self, node: &AstNode<'_>) -> SourceSpan {
        let mut span = self.node_source_span(node);
        while span.start > 0 && self.source.as_bytes()[span.start - 1] != b'\n' {
            span.start -= 1;
        }
        span
    }

    fn source_offset(&self, line: usize, column: usize) -> Option<usize> {
        if line == 0 || column == 0 {
            return None;
        }
        let line_start = self
            .source
            .split_inclusive('\n')
            .take(line - 1)
            .map(str::len)
            .sum::<usize>();
        let offset = line_start + column - 1;
        (offset <= self.source.len()).then_some(offset)
    }

    fn text_spans(&self, node: &AstNode<'_>, text: &str) -> Vec<SourceSpan> {
        let span = self.node_source_span(node);
        let mut offset = span.start;
        text.chars()
            .map(|character| {
                let size = character.len_utf8();
                let result = SourceSpan {
                    start: offset,
                    end: (offset + size).min(span.end),
                };
                offset += size;
                result
            })
            .collect()
    }

    fn push_span(&mut self, span: SourceSpan) {
        self.span_stack.push(span);
    }

    fn pop_span(&mut self) {
        self.span_stack.pop();
    }

    fn current_span(&self) -> SourceSpan {
        self.span_stack
            .iter()
            .rev()
            .copied()
            .find(|span| span.valid())
            .unwrap_or_default()
    }

    fn render_table<'a>(&mut self, node: &'a AstNode<'a>, alignments: &[TableAlignment]) {
        let rows: Vec<Vec<String>> = node
            .children()
            .map(|row| row.children().map(|cell| extract_text(cell)).collect())
            .collect();
        if rows.is_empty() {
            return;
        }
        let columns = if alignments.is_empty() {
            rows.iter().map(Vec::len).max().unwrap_or(0)
        } else {
            alignments.len()
        };
        let mut widths = vec![3; columns];
        for row in &rows {
            for (i, cell) in row.iter().enumerate().take(columns) {
                widths[i] = widths[i].max(display_width(cell));
            }
        }
        self.table_separator(&widths);
        for (row_index, row) in rows.iter().enumerate() {
            self.write_raw("|");
            for (i, width) in widths.iter().copied().enumerate() {
                let cell = row.get(i).map(String::as_str).unwrap_or("");
                let padding = width.saturating_sub(display_width(cell));
                let aligned = match alignments.get(i).copied().unwrap_or(TableAlignment::None) {
                    TableAlignment::Right => format!("{}{cell}", " ".repeat(padding)),
                    TableAlignment::Center => format!(
                        "{}{cell}{}",
                        " ".repeat(padding / 2),
                        " ".repeat(padding - padding / 2)
                    ),
                    _ => format!("{cell}{}", " ".repeat(padding)),
                };
                self.write_raw(&format!(" {aligned} |"));
            }
            self.newline();
            if row_index == 0 || row_index + 1 == rows.len() {
                self.table_separator(&widths);
            }
        }
        self.newline();
        self.col = 0;
    }

    fn table_separator(&mut self, widths: &[usize]) {
        self.write_raw("+");
        for width in widths {
            self.write_raw(&"-".repeat(width + 2));
            self.write_raw("+");
        }
        self.newline();
    }

    fn push_style(&mut self, style: TextStyle) {
        self.styles.push(style);
        self.apply_current_style();
    }
    fn pop_style(&mut self) {
        self.styles.pop();
        self.write_raw(RESET);
        self.apply_current_style();
    }
    fn apply_current_style(&mut self) {
        let mut current = TextStyle::default();
        for style in &self.styles {
            current.bold |= style.bold;
            current.italic |= style.italic;
            current.underline |= style.underline;
            if !style.color.is_empty() {
                current.color.clone_from(&style.color);
            }
            if !style.background.is_empty() {
                current.background.clone_from(&style.background);
            }
        }
        if current.bold {
            self.output.push_str(BOLD);
        }
        if current.italic {
            self.output.push_str(ITALIC);
        }
        if current.underline {
            self.output.push_str(UNDERLINE);
        }
        self.output.push_str(&current.color);
        self.output.push_str(&current.background);
    }
    fn write_raw(&mut self, text: &str) {
        self.output.push_str(text);
        self.record_output(text, None);
    }
    fn newline(&mut self) {
        self.write_raw("\n");
        self.col = 0;
    }
    fn write_indent(&mut self) {
        if self.blockquote_depth > 0 {
            for _ in 0..self.blockquote_depth {
                if self.render_style.blockquote_bg.is_empty() {
                    self.write_raw(" ");
                } else {
                    let bg = self.render_style.blockquote_bg.clone();
                    self.write_raw(&bg);
                    self.write_raw(" ");
                    self.write_raw(RESET);
                }
            }
            self.write_raw(&" ".repeat(self.indent.saturating_sub(self.blockquote_depth)));
        } else {
            self.write_raw(&" ".repeat(self.indent));
        }
        self.col = self.indent;
    }
    fn write_wrapped(&mut self, text: &str) {
        if self.width == 0 {
            self.write_raw(text);
            return;
        }
        for token in split_words(text) {
            let token_width = display_width(token);
            if token_width == 0 {
                continue;
            }
            let space = token.chars().next().is_some_and(char::is_whitespace);
            if self.col == 0 && self.indent > 0 {
                self.write_indent();
                self.apply_current_style();
            }
            if self.col > self.indent && self.col + token_width > self.width {
                self.write_raw(RESET);
                self.newline();
                self.write_indent();
                self.apply_current_style();
                if space {
                    continue;
                }
            }
            if space && self.col == self.indent {
                continue;
            }
            self.write_raw(token);
            self.col += token_width;
        }
    }

    fn write_wrapped_mapped(&mut self, text: &str, spans: Vec<SourceSpan>) {
        if self.width == 0 {
            self.output.push_str(text);
            self.record_output(text, Some(&spans));
            return;
        }
        let mut span_index = 0;
        for token in split_words(text) {
            let token_width = display_width(token);
            let token_chars = token.chars().count();
            let token_spans =
                &spans[span_index.min(spans.len())..(span_index + token_chars).min(spans.len())];
            span_index += token_chars;
            if token_width == 0 {
                continue;
            }
            let space = token.chars().next().is_some_and(char::is_whitespace);
            if self.col == 0 && self.indent > 0 {
                self.write_indent();
                self.apply_current_style();
            }
            if self.col > self.indent && self.col + token_width > self.width {
                self.write_raw(RESET);
                self.newline();
                self.write_indent();
                self.apply_current_style();
                if space {
                    continue;
                }
            }
            if space && self.col == self.indent {
                continue;
            }
            self.output.push_str(token);
            self.record_output(token, Some(token_spans));
            self.col += token_width;
        }
    }

    fn record_output(&mut self, text: &str, spans: Option<&[SourceSpan]>) {
        if self.line_mappings.is_empty() {
            self.line_mappings.push(RenderLineMapping::default());
        }
        let default_span = self.current_span();
        let mut span_index = 0;
        let mut index = 0;
        while index < text.len() {
            if text.as_bytes()[index] == 0x1b {
                index = escape_sequence_end(text, index);
                continue;
            }
            let character = text[index..].chars().next().unwrap();
            index += character.len_utf8();
            if character == '\n' {
                self.line += 1;
                self.line_mappings.push(RenderLineMapping::default());
                continue;
            }
            let span = spans
                .and_then(|values| values.get(span_index))
                .copied()
                .unwrap_or(default_span);
            self.line_mappings.last_mut().unwrap().spans.push(span);
            span_index += 1;
        }
    }
}

fn escape_sequence_end(text: &str, start: usize) -> usize {
    let bytes = text.as_bytes();
    if bytes.get(start) != Some(&0x1b) {
        return start + 1;
    }
    match bytes.get(start + 1) {
        Some(b'[') => {
            let mut index = start + 2;
            while index < bytes.len() {
                if (0x40..=0x7e).contains(&bytes[index]) {
                    return index + 1;
                }
                index += 1;
            }
            bytes.len()
        }
        Some(b']') => {
            let mut index = start + 2;
            while index < bytes.len() {
                if bytes[index] == 0x07 {
                    return index + 1;
                }
                if bytes[index] == 0x1b && bytes.get(index + 1) == Some(&b'\\') {
                    return index + 2;
                }
                index += 1;
            }
            bytes.len()
        }
        _ => (start + 2).min(bytes.len()),
    }
}

fn split_wiki_link(inner: &str) -> (&str, Option<&str>) {
    match inner.split_once('|') {
        Some((target, label)) => (target, Some(label)),
        None => (inner, None),
    }
}

fn extract_rendered_links(text: &str) -> Vec<RenderedLink> {
    let mut links = Vec::new();
    let mut active_url: Option<String> = None;
    let mut current: Option<RenderedLink> = None;
    let mut line = 0;
    let mut col = 0;
    let mut index = 0;
    while index < text.len() {
        if text.as_bytes()[index] == 0x1b {
            if let Some((url, end)) = osc8_url(text, index) {
                flush_link(&mut links, &mut current);
                active_url = (!url.is_empty()).then_some(url.to_string());
                index = end;
            } else {
                index = escape_sequence_end(text, index);
            }
            continue;
        }
        let character = text[index..].chars().next().unwrap();
        index += character.len_utf8();
        if character == '\n' {
            flush_link(&mut links, &mut current);
            line += 1;
            col = 0;
            continue;
        }
        let width = character.width().unwrap_or(0);
        if width == 0 {
            continue;
        }
        if let Some(url) = active_url.as_deref() {
            match &mut current {
                Some(range) if range.url == url && range.line == line && range.end_col == col => {
                    range.end_col += width;
                }
                _ => {
                    flush_link(&mut links, &mut current);
                    current = Some(RenderedLink {
                        url: url.to_string(),
                        line,
                        start_col: col,
                        end_col: col + width,
                    });
                }
            }
        }
        col += width;
    }
    flush_link(&mut links, &mut current);
    links
}

fn osc8_url(text: &str, start: usize) -> Option<(&str, usize)> {
    let rest = text.get(start..)?;
    let content = rest.strip_prefix("\x1b]8;;")?;
    if let Some(end) = content.find('\x07') {
        return Some((&content[..end], start + 5 + end + 1));
    }
    let end = content.find("\x1b\\")?;
    Some((&content[..end], start + 5 + end + 2))
}

fn flush_link(links: &mut Vec<RenderedLink>, current: &mut Option<RenderedLink>) {
    if let Some(link) = current.take()
        && link.start_col < link.end_col
    {
        links.push(link);
    }
}

fn collect_headings<'a>(node: &'a AstNode<'a>, headings: &mut Vec<Heading>, rendered: bool) {
    if let NodeValue::Heading(heading) = node.data.borrow().value {
        headings.push(Heading {
            level: heading.level,
            text: extract_text(node).trim().to_owned(),
            line: rendered.then_some(0),
        });
    }
    for child in node.children() {
        collect_headings(child, headings, rendered);
    }
}

fn extract_text<'a>(node: &'a AstNode<'a>) -> String {
    let mut output = String::new();
    extract_text_into(node, &mut output);
    output
}

fn extract_text_into<'a>(node: &'a AstNode<'a>, output: &mut String) {
    match &node.data.borrow().value {
        NodeValue::Text(text) => output.push_str(text),
        NodeValue::Code(code) => output.push_str(&code.literal),
        _ => {
            for child in node.children() {
                extract_text_into(child, output);
            }
        }
    }
}

fn split_line_ending(line: &str) -> (&str, &str) {
    if let Some(content) = line.strip_suffix("\r\n") {
        (content, "\r\n")
    } else if let Some(content) = line.strip_suffix('\n') {
        (content, "\n")
    } else {
        (line, "")
    }
}

fn display_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

fn split_words(text: &str) -> Vec<&str> {
    let mut tokens = Vec::new();
    let mut start = 0;
    let mut last_space = None;
    for (index, ch) in text.char_indices() {
        let space = ch.is_whitespace();
        if let Some(previous) = last_space
            && previous != space
        {
            tokens.push(&text[start..index]);
            start = index;
        }
        last_space = Some(space);
    }
    if start < text.len() {
        tokens.push(&text[start..]);
    }
    tokens
}

#[allow(dead_code)]
fn char_width(ch: char) -> usize {
    ch.width().unwrap_or(0)
}
