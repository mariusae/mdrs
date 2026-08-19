//! Terminal Markdown rendering compatible with the Go `github.com/mariusae/md`
//! package.
//!
//! The primary Rust API returns owned values. Writer-oriented variants are
//! available when output should be streamed into an existing buffer.

mod pager;
mod render;
mod style;

pub use pager::{PagerConfig, run_pager};
pub use render::{
    AnsiRenderer, CodeBlock, Heading, RenderError, RenderResult, Result, extract_headings, render,
    render_document, render_document_with_style, render_to, render_to_with_style,
    render_with_style,
};
pub use style::{RenderStyle, detect_render_style};

pub const RESET: &str = "\x1b[0m";
pub const BOLD: &str = "\x1b[1m";
pub const ITALIC: &str = "\x1b[3m";
pub const UNDERLINE: &str = "\x1b[4m";
pub const DIM: &str = "\x1b[2m";
pub const REVERSE: &str = "\x1b[7m";
pub const FG_BLUE: &str = "\x1b[34m";
pub const OSC8_END: &str = "\x1b]8;;\x1b\\";

/// Returns an OSC-8 escape sequence that begins a hyperlink to `url`.
pub fn osc8_start(url: &str) -> String {
    format!("\x1b]8;;{url}\x1b\\")
}
