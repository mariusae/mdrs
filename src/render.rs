use std::fmt::Write as _;
use std::io::{self, Write};

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

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RenderResult {
    pub output: String,
    pub headings: Vec<Heading>,
    pub code_blocks: Vec<CodeBlock>,
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
    let source = std::str::from_utf8(source)?;
    let (front_matter, body) = split_front_matter(source);
    let mut renderer = AnsiRenderer::new(width, osc8, style.clone());
    renderer.render_source(body);
    let mut result = renderer.finish();

    if let Some(front) = front_matter {
        let mut prefix = front.to_owned();
        if !front.is_empty() && !front.ends_with('\n') {
            prefix.push('\n');
        }
        let hr_width = if width == 0 { 40 } else { width };
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
        result.output.insert_str(0, &prefix);
    }
    Ok(result)
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
    code_blocks: Vec<CodeBlock>,
    source: String,
}

impl AnsiRenderer {
    pub fn new(width: usize, osc8: bool, render_style: RenderStyle) -> Self {
        Self {
            width,
            osc8,
            render_style,
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
            code_blocks: Vec::new(),
            source: String::new(),
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
        }
    }

    fn render_node<'a>(&mut self, node: &'a AstNode<'a>, tight_list: bool) {
        let value = node.data.borrow().value.clone();
        match value {
            NodeValue::Document => self.render_children(node, tight_list),
            NodeValue::Heading(heading) => {
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
            }
            NodeValue::BlockQuote => {
                self.blockquote_depth += 1;
                self.indent += 2;
                self.render_children(node, tight_list);
                self.blockquote_depth -= 1;
                self.indent -= 2;
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
                let width = if self.width == 0 { 40 } else { self.width };
                self.write_raw(&"─".repeat(width));
                self.newline();
                self.newline();
            }
            NodeValue::HtmlBlock(html) => self.write_raw(&html.literal),
            NodeValue::Text(text) => self.write_wrapped(&text),
            NodeValue::SoftBreak => self.write_wrapped(" "),
            NodeValue::LineBreak => {
                self.newline();
                self.write_indent();
            }
            NodeValue::Code(code) => {
                self.push_style(TextStyle {
                    color: FG_BLUE.into(),
                    ..Default::default()
                });
                self.write_wrapped(&code.literal);
                self.pop_style();
            }
            NodeValue::Emph => {
                self.push_style(TextStyle {
                    italic: true,
                    ..Default::default()
                });
                self.render_children(node, tight_list);
                self.pop_style();
            }
            NodeValue::Strong => {
                self.push_style(TextStyle {
                    bold: true,
                    ..Default::default()
                });
                self.render_children(node, tight_list);
                self.pop_style();
            }
            NodeValue::Highlight => {
                let background = self.render_style.highlight_bg.clone();
                self.push_style(TextStyle {
                    background,
                    ..Default::default()
                });
                self.render_children(node, tight_list);
                self.pop_style();
            }
            NodeValue::Strikethrough => {
                self.write_raw("<del>");
                self.render_children(node, tight_list);
                self.write_raw("</del>");
            }
            NodeValue::Link(link) => {
                if self
                    .node_source(node)
                    .is_some_and(|source| !source.starts_with('['))
                {
                    self.render_autolink(&link.url);
                } else {
                    self.render_link(node, &link.url, tight_list);
                }
            }
            NodeValue::Image(_) => {
                self.write_raw("[image: ");
                self.render_children(node, tight_list);
                self.write_raw("]");
            }
            NodeValue::HtmlInline(html) | NodeValue::Raw(html) => self.write_raw(&html),
            NodeValue::Table(table) => self.render_table(node, &table.alignments),
            NodeValue::TableRow(_) | NodeValue::TableCell => self.render_children(node, tight_list),
            NodeValue::TaskItem(task) => {
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
        self.newline();
    }

    fn render_link<'a>(&mut self, node: &'a AstNode<'a>, url: &str, tight: bool) {
        if self.osc8 {
            self.write_raw(&osc8_start(url));
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
            self.write_wrapped(url);
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
                let _ = write!(self.output, " {aligned} |");
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
        self.line += text.matches('\n').count();
        self.output.push_str(text);
    }
    fn newline(&mut self) {
        self.output.push('\n');
        self.line += 1;
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
