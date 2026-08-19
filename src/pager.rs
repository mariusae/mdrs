use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufReader, IsTerminal, Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant, SystemTime};

use base64::Engine;
use notify::{EventKind, RecursiveMode, Watcher};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::render::{CodeBlock, RenderLineMapping, SourceSpan};
use crate::{BOLD, DIM, Heading, RESET, REVERSE, RenderStyle, render_document_with_style};

const ENTER_ALT: &str = "\x1b[?1049h";
const EXIT_ALT: &str = "\x1b[?1049l";
const HIDE_CURSOR: &str = "\x1b[?25l";
const SHOW_CURSOR: &str = "\x1b[?25h";
const ENABLE_FOCUS: &str = "\x1b[?1004h";
const DISABLE_FOCUS: &str = "\x1b[?1004l";
const ENABLE_MOUSE: &str = "\x1b[?1000h\x1b[?1002h\x1b[?1006h";
const DISABLE_MOUSE: &str = "\x1b[?1006l\x1b[?1002l\x1b[?1000l";
const QUERY_BACKGROUND: &str = "\x1b]11;?\x1b\\";
const FLASH_DURATION: Duration = Duration::from_secs(2);

#[derive(Clone, Debug, Default)]
pub struct PagerConfig {
    pub paths: Vec<PathBuf>,
    pub initial_source: Vec<u8>,
    pub label: String,
    pub width: usize,
}

#[derive(Clone, Debug, Default)]
pub struct EmbeddedPagerConfig {
    pub source: Vec<u8>,
    pub width: usize,
    pub height: usize,
    /// One-based source line to center when possible.
    pub center_source_line: Option<usize>,
    pub scroll: isize,
    pub highlight_terms: Vec<String>,
    pub render_style: RenderStyle,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct EmbeddedCodeBlock {
    /// Zero-based row within the embedded viewport.
    pub row: usize,
    /// Zero-based column where the copy target is rendered.
    pub col: usize,
    pub text: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct EmbeddedPagerView {
    pub lines: Vec<String>,
    pub top_line: usize,
    pub code_blocks: Vec<EmbeddedCodeBlock>,
}

pub fn render_embedded_pager(config: &EmbeddedPagerConfig) -> io::Result<EmbeddedPagerView> {
    if config.height == 0 {
        return Ok(EmbeddedPagerView::default());
    }
    let width = config.width.max(1);
    let result = render_document_with_style(&config.source, width, true, &config.render_style)
        .map_err(io::Error::other)?;
    let rendered = result.output.trim_end_matches('\n');
    if rendered.is_empty() {
        return Ok(EmbeddedPagerView::default());
    }

    let lines = rendered.split('\n').map(str::to_owned).collect::<Vec<_>>();
    let plain = lines
        .iter()
        .map(|line| strip_ansi(line))
        .collect::<Vec<_>>();
    let center_line = config
        .center_source_line
        .and_then(|line| rendered_line_for_source_line(&config.source, line, &result.line_mappings))
        .unwrap_or(0)
        .min(lines.len().saturating_sub(1));
    let max_top = lines.len().saturating_sub(config.height);
    let top = center_line
        .saturating_sub(config.height / 2)
        .saturating_add_signed(config.scroll)
        .min(max_top);
    let highlight = embedded_highlight(&config.render_style);
    let mut code_blocks = Vec::new();
    let visible = lines
        .iter()
        .zip(&plain)
        .enumerate()
        .skip(top)
        .take(config.height)
        .map(|(line_index, (line, plain))| {
            let mut line = line.clone();
            if let Some(block) = result
                .code_blocks
                .iter()
                .find(|block| block.line == line_index)
            {
                let row = line_index - top;
                line = replace_visible(
                    &line,
                    0,
                    &format!("{DIM}⎘{RESET}{}", config.render_style.code_block_bg),
                );
                code_blocks.push(EmbeddedCodeBlock {
                    row,
                    col: 0,
                    text: block.text.clone(),
                });
            }
            highlight_all_matches(&line, plain, &config.highlight_terms, &highlight)
        })
        .collect();
    Ok(EmbeddedPagerView {
        lines: visible,
        top_line: top,
        code_blocks,
    })
}

pub fn copy_osc52_to(mut writer: impl Write, text: &[u8]) -> io::Result<()> {
    if text.is_empty() {
        return Ok(());
    }
    write!(
        writer,
        "\x1b]52;c;{}\x07",
        base64::engine::general_purpose::STANDARD.encode(text)
    )
}

fn embedded_highlight(render_style: &RenderStyle) -> String {
    if render_style.highlight_bg.is_empty() {
        format!("{REVERSE}{BOLD}")
    } else {
        format!("{}{BOLD}", render_style.highlight_bg)
    }
}

pub fn run_pager(config: &PagerConfig) -> io::Result<()> {
    let mut tty = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .map_err(|error| io::Error::new(error.kind(), format!("opening tty: {error}")))?;
    if !tty.is_terminal() {
        return Err(io::Error::other("pager requires a terminal"));
    }
    let _raw = RawMode::enable(&tty)?;
    let _screen = ScreenGuard::enter(tty.try_clone()?)?;
    let (width, height) = terminal_size(&tty)?;
    let mut pager = Pager::new(config.clone(), width, height);
    pager.reload(true)?;
    write!(tty, "{QUERY_BACKGROUND}")?;

    let (input_tx, input_rx) = mpsc::channel();
    let input = tty.try_clone()?;
    std::thread::spawn(move || read_events(input, input_tx));
    let (watch_tx, watch_rx) = mpsc::channel();
    let mut watcher = watch_paths(&config.paths, watch_tx)?;
    let resized = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGWINCH, Arc::clone(&resized))?;

    loop {
        if resized.swap(false, Ordering::Relaxed) {
            let (width, height) = terminal_size(&tty)?;
            pager.resize(width, height);
            pager.rebuild()?;
        }
        while watch_rx.try_recv().is_ok() {
            pager.reload(false)?;
        }
        pager.clear_expired_flash(Instant::now());
        pager.draw(&mut tty)?;
        pager.selection.flash = false;
        match input_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(Ok(event)) => {
                if pager.handle_event(event, &mut tty)? {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Ok(Err(error)) => return Err(error),
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    drop(watcher.take());
    Ok(())
}

fn rendered_line_for_source_line(
    source: &[u8],
    source_line: usize,
    mappings: &[RenderLineMapping],
) -> Option<usize> {
    let line = source_line.checked_sub(1)?;
    let source = std::str::from_utf8(source).ok()?;
    let mut start = 0;
    for (index, text_line) in source.split_inclusive('\n').enumerate() {
        let end = start + text_line.len();
        if index == line {
            return mappings.iter().position(|mapping| {
                mapping
                    .spans
                    .iter()
                    .any(|span| span.start < end && span.end > start)
            });
        }
        start = end;
    }
    (line == source.lines().count()).then(|| {
        mappings
            .iter()
            .position(|mapping| mapping.spans.iter().any(|span| span.start >= source.len()))
            .unwrap_or_else(|| mappings.len().saturating_sub(1))
    })
}

#[derive(Clone, Debug, Default)]
struct Theme {
    status: String,
    prompt: String,
    highlight: String,
    blockquote: String,
    code: String,
    mark: String,
}
impl Theme {
    fn render_style(&self) -> RenderStyle {
        RenderStyle {
            blockquote_bg: self.blockquote.clone(),
            code_block_bg: self.code.clone(),
            highlight_bg: self.mark.clone(),
        }
    }
}

#[derive(Clone, Debug, Default)]
struct Outline {
    active: bool,
    filter: String,
    cursor: usize,
    filtered: Vec<usize>,
    selected: Option<usize>,
    scroll: usize,
}
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Ord, PartialOrd)]
struct Cell {
    line: usize,
    col: usize,
}
#[derive(Clone, Copy, Debug, Default)]
struct Selection {
    active: bool,
    selecting: bool,
    dragged: bool,
    flash: bool,
    quoted: bool,
    anchor: Cell,
    current: Cell,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Range {
    start: usize,
    end: usize,
}

struct Pager {
    cfg: PagerConfig,
    width: usize,
    height: usize,
    source: Vec<u8>,
    modified: Option<SystemTime>,
    headings: Vec<Heading>,
    lines: Vec<String>,
    mappings: Vec<RenderLineMapping>,
    blocks: Vec<CodeBlock>,
    plain: Vec<String>,
    top: usize,
    query: String,
    matches: Vec<usize>,
    match_index: Option<usize>,
    prompt: bool,
    prompt_value: String,
    prompt_cursor: usize,
    notice: String,
    notice_error: bool,
    theme: Theme,
    outline: Outline,
    selection: Selection,
    changed: HashMap<usize, Vec<Range>>,
    flash_until: Option<Instant>,
    help: bool,
    flow: bool,
}

impl Pager {
    fn new(mut cfg: PagerConfig, width: usize, height: usize) -> Self {
        if cfg.label.is_empty() {
            cfg.label = pager_label(&cfg.paths);
        }
        Self {
            cfg,
            width: width.max(1),
            height: height.max(1),
            source: vec![],
            modified: None,
            headings: vec![],
            lines: vec![],
            mappings: vec![],
            blocks: vec![],
            plain: vec![],
            top: 0,
            query: String::new(),
            matches: vec![],
            match_index: None,
            prompt: false,
            prompt_value: String::new(),
            prompt_cursor: 0,
            notice: String::new(),
            notice_error: false,
            theme: Theme::default(),
            outline: Outline::default(),
            selection: Selection::default(),
            changed: HashMap::new(),
            flash_until: None,
            help: false,
            flow: false,
        }
    }
    fn resize(&mut self, width: usize, height: usize) {
        self.width = width.max(1);
        self.height = height.max(1);
    }
    fn render_width(&self) -> usize {
        let width = if self.cfg.width > 0 {
            self.cfg.width
        } else {
            self.width
        };
        if self.flow && self.width > 100 {
            width.min(100)
        } else {
            width
        }
    }
    fn content_left(&self) -> usize {
        if !self.flow || self.width <= 100 {
            1
        } else {
            ((self.width - self.render_width()) / 2 + 1).max(1)
        }
    }
    fn view_height(&self) -> usize {
        self.height.saturating_sub(1).max(1)
    }
    fn max_top(&self) -> usize {
        self.lines.len().saturating_sub(self.view_height())
    }
    fn scroll(&mut self, delta: isize) {
        self.top = self.top.saturating_add_signed(delta).min(self.max_top());
    }

    fn load(&self) -> io::Result<(Vec<u8>, Option<SystemTime>)> {
        if self.cfg.paths.is_empty() {
            return Ok((self.cfg.initial_source.clone(), None));
        }
        let mut all = vec![];
        let mut latest = None;
        for path in &self.cfg.paths {
            all.extend(fs::read(path).map_err(|e| {
                io::Error::new(e.kind(), format!("reading {}: {e}", path.display()))
            })?);
            let time = fs::metadata(path)?.modified()?;
            if latest.is_none_or(|old| time > old) {
                latest = Some(time);
            }
        }
        Ok((all, latest))
    }
    fn reload(&mut self, initial: bool) -> io::Result<()> {
        let (source, modified) = match self.load() {
            Ok(value) => value,
            Err(error) if !initial => {
                self.set_notice(error.to_string(), true);
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        let old = self.plain.clone();
        let changed = !initial && source != self.source;
        self.source = source;
        self.modified = modified;
        if let Err(error) = self.rebuild() {
            if initial {
                return Err(error);
            }
            self.set_notice(error.to_string(), true);
            return Ok(());
        }
        if self.notice_error {
            self.clear_notice();
        }
        if changed {
            self.changed = changed_ranges(&old, &self.plain);
            self.flash_until = (!self.changed.is_empty()).then(|| Instant::now() + FLASH_DURATION);
        }
        Ok(())
    }
    fn rebuild(&mut self) -> io::Result<()> {
        let anchor = self
            .match_index
            .and_then(|index| self.matches.get(index))
            .copied()
            .unwrap_or(self.top);
        let result = render_document_with_style(
            &self.source,
            self.render_width(),
            true,
            &self.theme.render_style(),
        )
        .map_err(io::Error::other)?;
        self.headings = result.headings;
        let text = result.output.trim_end_matches('\n');
        if text.is_empty() {
            self.lines.clear();
            self.mappings.clear();
            self.blocks.clear();
            self.plain.clear();
            self.headings.clear();
            self.top = 0;
        } else {
            self.lines = text.split('\n').map(str::to_owned).collect();
            self.mappings = result.line_mappings;
            self.mappings.truncate(self.lines.len());
            self.blocks = result.code_blocks;
            self.plain = self.lines.iter().map(|line| strip_ansi(line)).collect();
            self.top = self.top.min(self.max_top());
        }
        self.refresh_search(anchor);
        self.refresh_outline();
        Ok(())
    }

    fn handle_event(&mut self, event: Event, tty: &mut File) -> io::Result<bool> {
        match event {
            Event::Key(key) => Ok(self.handle_key(key)),
            Event::Focus(true) => {
                write!(tty, "{QUERY_BACKGROUND}")?;
                Ok(false)
            }
            Event::Focus(false) => Ok(false),
            Event::Color(rgb) => {
                let theme = theme_for(rgb);
                if theme.status != self.theme.status {
                    self.theme = theme;
                    self.rebuild()?;
                }
                Ok(false)
            }
            Event::Mouse(mouse) => {
                self.handle_mouse(mouse, tty)?;
                Ok(false)
            }
            Event::Ignore => Ok(false),
        }
    }
    fn handle_key(&mut self, key: Key) -> bool {
        if self.help {
            if matches!(key, Key::Ctrl('c')) {
                return true;
            }
            if matches!(key, Key::Escape | Key::Char('?') | Key::Char('q')) {
                self.help = false;
            }
            return false;
        }
        if matches!(key, Key::Char('?')) {
            self.help = true;
            return false;
        }
        if self.outline.active {
            return self.handle_outline_key(key);
        }
        if self.prompt {
            return self.handle_prompt_key(key);
        }
        match key {
            Key::Enter | Key::Down | Key::Char('j') | Key::Ctrl('n') => self.scroll(1),
            Key::Up | Key::Char('k') | Key::Ctrl('p') => self.scroll(-1),
            Key::Char('d') => self.scroll((self.view_height() / 2).max(1) as isize),
            Key::Char('u') => self.scroll(-((self.view_height() / 2).max(1) as isize)),
            Key::Char(' ') | Key::Ctrl('v') | Key::PageDown => {
                self.scroll(self.view_height() as isize)
            }
            Key::Char('b') | Key::Alt('v') | Key::PageUp => {
                self.scroll(-(self.view_height() as isize))
            }
            Key::Char('g') | Key::Home | Key::Alt('<') => self.top = 0,
            Key::Char('G') | Key::End | Key::Alt('>') => self.top = self.max_top(),
            Key::Char('/') => {
                self.prompt = true;
                self.prompt_value.clone_from(&self.query);
                self.prompt_cursor = self.prompt_value.chars().count();
            }
            Key::Ctrl('r') => self.open_outline(),
            Key::Char('n') => self.search_next(),
            Key::Char('N') => self.search_prev(),
            Key::Char('r') | Key::Ctrl('l') => {
                let _ = self.reload(false);
            }
            Key::Char('f') => {
                self.flow = !self.flow;
                let _ = self.rebuild();
            }
            Key::Char('q') | Key::Ctrl('c') => return true,
            _ => {}
        }
        false
    }
    fn handle_prompt_key(&mut self, key: Key) -> bool {
        match key {
            Key::Enter => {
                self.prompt = false;
                self.query.clone_from(&self.prompt_value);
                self.refresh_search(self.top);
                if self.query.is_empty() {
                    self.clear_notice();
                } else if !self.jump_to_match(self.top) {
                    self.set_notice(format!("pattern not found: /{}", self.query), true);
                } else {
                    self.clear_notice();
                }
            }
            Key::Backspace | Key::Ctrl('h') => {
                edit_delete_before(&mut self.prompt_value, &mut self.prompt_cursor)
            }
            Key::Delete | Key::Ctrl('d') => {
                edit_delete_at(&mut self.prompt_value, self.prompt_cursor)
            }
            Key::Left | Key::Ctrl('b') => self.prompt_cursor = self.prompt_cursor.saturating_sub(1),
            Key::Right | Key::Ctrl('f') => {
                self.prompt_cursor = (self.prompt_cursor + 1).min(self.prompt_value.chars().count())
            }
            Key::Home | Key::Ctrl('a') => self.prompt_cursor = 0,
            Key::End | Key::Ctrl('e') => self.prompt_cursor = self.prompt_value.chars().count(),
            Key::Ctrl('u') => edit_kill_start(&mut self.prompt_value, &mut self.prompt_cursor),
            Key::Ctrl('k') => edit_kill_end(&mut self.prompt_value, self.prompt_cursor),
            Key::Ctrl('w') => edit_delete_word(&mut self.prompt_value, &mut self.prompt_cursor),
            Key::Alt('b') => self.prompt_cursor = word_back(&self.prompt_value, self.prompt_cursor),
            Key::Alt('f') => {
                self.prompt_cursor = word_forward(&self.prompt_value, self.prompt_cursor)
            }
            Key::Ctrl('g') => {
                self.prompt = false;
                self.prompt_value.clone_from(&self.query);
                self.prompt_cursor = self.prompt_value.chars().count();
            }
            Key::Ctrl('c') => return true,
            Key::Char(ch) if !ch.is_control() => {
                edit_insert(&mut self.prompt_value, &mut self.prompt_cursor, ch)
            }
            _ => {}
        }
        false
    }
    fn handle_outline_key(&mut self, key: Key) -> bool {
        match key {
            Key::Escape | Key::Ctrl('r') | Key::Enter | Key::Ctrl('g') => self.close_outline(),
            Key::Ctrl('c') => return true,
            Key::Down | Key::Ctrl('n') | Key::Char('j') => self.move_outline(1),
            Key::Up | Key::Ctrl('p') | Key::Char('k') => self.move_outline(-1),
            Key::PageDown | Key::Ctrl('v') => {
                self.move_outline(self.outline_rows().saturating_sub(1).max(1) as isize)
            }
            Key::PageUp | Key::Char('b') | Key::Alt('v') => {
                self.move_outline(-(self.outline_rows().saturating_sub(1).max(1) as isize))
            }
            Key::Home | Key::Char('g') | Key::Ctrl('a') => self.move_outline_to(0),
            Key::End | Key::Char('G') | Key::Ctrl('e') => {
                self.move_outline_to(self.outline.filtered.len().saturating_sub(1))
            }
            Key::Backspace | Key::Ctrl('h') => {
                edit_delete_before(&mut self.outline.filter, &mut self.outline.cursor);
                self.refresh_outline();
            }
            Key::Delete | Key::Ctrl('d') => {
                edit_delete_at(&mut self.outline.filter, self.outline.cursor);
                self.refresh_outline();
            }
            Key::Left | Key::Ctrl('b') => {
                self.outline.cursor = self.outline.cursor.saturating_sub(1)
            }
            Key::Right | Key::Ctrl('f') => {
                self.outline.cursor =
                    (self.outline.cursor + 1).min(self.outline.filter.chars().count())
            }
            Key::Ctrl('u') => {
                edit_kill_start(&mut self.outline.filter, &mut self.outline.cursor);
                self.refresh_outline();
            }
            Key::Ctrl('k') => {
                edit_kill_end(&mut self.outline.filter, self.outline.cursor);
                self.refresh_outline();
            }
            Key::Ctrl('w') => {
                edit_delete_word(&mut self.outline.filter, &mut self.outline.cursor);
                self.refresh_outline();
            }
            Key::Alt('b') => {
                self.outline.cursor = word_back(&self.outline.filter, self.outline.cursor)
            }
            Key::Alt('f') => {
                self.outline.cursor = word_forward(&self.outline.filter, self.outline.cursor)
            }
            Key::Char(ch) if !ch.is_control() => {
                let mut candidate = self.outline.filter.clone();
                let mut cursor = self.outline.cursor;
                edit_insert(&mut candidate, &mut cursor, ch);
                if !self.matching_headings(&candidate).is_empty() {
                    self.outline.filter = candidate;
                    self.outline.cursor = cursor;
                    self.refresh_outline();
                }
            }
            _ => {}
        }
        false
    }

    fn refresh_search(&mut self, anchor: usize) {
        self.matches.clear();
        self.match_index = None;
        if self.query.is_empty() {
            return;
        }
        let needle = self.query.to_lowercase();
        self.matches.extend(
            self.plain
                .iter()
                .enumerate()
                .filter_map(|(i, line)| line.to_lowercase().contains(&needle).then_some(i)),
        );
        if !self.matches.is_empty() {
            let at = self.matches.partition_point(|line| *line < anchor);
            self.match_index = Some(if self.matches.get(at) == Some(&anchor) {
                at
            } else {
                at.saturating_sub(1)
            });
        }
    }
    fn jump_to_match(&mut self, anchor: usize) -> bool {
        let index = self.matches.partition_point(|line| *line < anchor);
        if index >= self.matches.len() {
            return false;
        }
        self.match_index = Some(index);
        self.top = self.matches[index].min(self.max_top());
        true
    }
    fn search_next(&mut self) {
        if self.matches.is_empty() {
            if !self.query.is_empty() {
                self.set_notice(format!("pattern not found: /{}", self.query), true)
            };
            return;
        }
        let start = self
            .match_index
            .and_then(|i| self.matches.get(i))
            .copied()
            .unwrap_or(self.top.saturating_sub(1));
        let index = self.matches.partition_point(|line| *line <= start);
        if index >= self.matches.len() {
            self.set_notice(format!("no later match for /{}", self.query), true)
        } else {
            self.match_index = Some(index);
            self.top = self.matches[index].min(self.max_top());
            self.clear_notice()
        }
    }
    fn search_prev(&mut self) {
        if self.matches.is_empty() {
            if !self.query.is_empty() {
                self.set_notice(format!("pattern not found: /{}", self.query), true)
            };
            return;
        }
        let start = self
            .match_index
            .and_then(|i| self.matches.get(i))
            .copied()
            .unwrap_or(self.top);
        let index = self
            .matches
            .partition_point(|line| *line < start)
            .checked_sub(1);
        if let Some(index) = index {
            self.match_index = Some(index);
            self.top = self.matches[index].min(self.max_top());
            self.clear_notice()
        } else {
            self.set_notice(format!("no earlier match for /{}", self.query), true)
        }
    }
    fn set_notice(&mut self, text: String, error: bool) {
        self.notice = text;
        self.notice_error = error
    }
    fn clear_notice(&mut self) {
        self.notice.clear();
        self.notice_error = false
    }

    fn matching_headings(&self, filter: &str) -> Vec<usize> {
        let filter = filter.to_lowercase();
        self.headings
            .iter()
            .enumerate()
            .filter_map(|(i, h)| {
                (filter.is_empty() || h.text.to_lowercase().contains(&filter)).then_some(i)
            })
            .collect()
    }
    fn current_heading(&self) -> Option<usize> {
        if self.headings.is_empty() {
            None
        } else {
            Some(
                self.headings
                    .partition_point(|h| h.line.unwrap_or(0) <= self.top)
                    .saturating_sub(1),
            )
        }
    }
    fn heading_path(&self) -> Vec<Heading> {
        let Some(current) = self.current_heading() else {
            return vec![];
        };
        let mut stack = vec![];
        for heading in self.headings.iter().take(current + 1) {
            while stack
                .last()
                .is_some_and(|h: &Heading| h.level >= heading.level)
            {
                stack.pop();
            }
            stack.push(heading.clone());
        }
        stack
    }
    fn open_outline(&mut self) {
        self.outline.active = true;
        self.outline.filter.clear();
        self.outline.cursor = 0;
        self.outline.selected = None;
        self.outline.scroll = 0;
        self.refresh_outline()
    }
    fn close_outline(&mut self) {
        self.outline = Outline::default()
    }
    fn refresh_outline(&mut self) {
        self.outline.filtered = self.matching_headings(&self.outline.filter);
        if self.outline.filtered.is_empty() {
            self.outline.selected = None;
            self.outline.scroll = 0;
            return;
        }
        let current = self.current_heading();
        if self
            .outline
            .selected
            .is_some_and(|s| self.outline.filtered.contains(&s))
        {
        } else if current.is_some_and(|c| self.outline.filtered.contains(&c)) {
            self.outline.selected = current
        } else {
            self.outline.selected = self.outline.filtered.first().copied()
        }
        self.sync_outline()
    }
    fn outline_position(&self) -> Option<usize> {
        self.outline.selected.and_then(|selected| {
            self.outline
                .filtered
                .iter()
                .position(|index| *index == selected)
        })
    }
    fn move_outline(&mut self, delta: isize) {
        let position = self
            .outline_position()
            .unwrap_or(0)
            .saturating_add_signed(delta)
            .min(self.outline.filtered.len().saturating_sub(1));
        self.move_outline_to(position)
    }
    fn move_outline_to(&mut self, position: usize) {
        if self.outline.filtered.is_empty() {
            self.outline.selected = None
        } else {
            self.outline.selected =
                Some(self.outline.filtered[position.min(self.outline.filtered.len() - 1)]);
            self.sync_outline()
        }
    }
    fn sync_outline(&mut self) {
        if let Some(index) = self.outline.selected {
            self.top = self.headings[index].line.unwrap_or(0).min(self.max_top())
        }
    }
    fn outline_rows(&self) -> usize {
        if self.height <= 2 {
            1
        } else {
            (self.height / 3).max(3).min(self.height - 2).max(1)
        }
    }
    fn outline_width(&self) -> usize {
        let max = self.width.min((self.width * 2 / 3).max(24));
        let headings = self
            .outline
            .filtered
            .iter()
            .map(|i| {
                4 + (self.headings[*i].level as usize - 1) * 2
                    + display_width(&self.headings[*i].text)
            })
            .max()
            .unwrap_or(24);
        max.min(
            headings
                .max(4 + display_width(&self.outline.filter))
                .max(24),
        )
    }

    fn handle_mouse(&mut self, mouse: Mouse, tty: &mut File) -> io::Result<()> {
        if self.help {
            return Ok(());
        }
        if self.outline.active && self.mouse_in_outline(mouse.row, mouse.col) {
            if let Some(direction) = mouse.wheel() {
                self.move_outline(direction)
            }
            return Ok(());
        }
        if let Some(direction) = mouse.wheel() {
            self.scroll(3 * direction);
            return Ok(());
        }
        if mouse.pressed
            && mouse.base() == 0
            && let Some(block) = self.code_button(mouse.row, mouse.col).cloned()
        {
            self.selection = Selection::default();
            if let Err(error) = copy_osc52(tty, block.text.as_bytes()) {
                self.set_notice(error.to_string(), true);
            } else {
                self.set_notice("Copied code block".into(), false);
            }
            return Ok(());
        }
        self.selection_mouse(mouse, tty)
    }
    fn code_button(&self, row: usize, col: usize) -> Option<&CodeBlock> {
        if col != self.content_left() || row < 1 || row > self.view_height() {
            return None;
        }
        let line = self.top + row - 1;
        self.blocks.iter().find(|block| block.line == line)
    }
    fn mouse_in_outline(&self, row: usize, col: usize) -> bool {
        let width = self.outline_width();
        let rows = self.outline_rows() + 1;
        let top = self.height.saturating_sub(rows).max(1);
        row >= top && row < top + rows && col >= 1 && col <= width
    }
    fn mouse_cell(&self, row: usize, col: usize) -> Option<Cell> {
        if row < 1 || row > self.view_height() || self.plain.is_empty() {
            return None;
        }
        let line = self.top + row - 1;
        if line >= self.plain.len() {
            return None;
        }
        let width = self.plain[line].chars().count();
        Some(Cell {
            line,
            col: col
                .saturating_sub(self.content_left())
                .saturating_add(1)
                .clamp(1, width + 1),
        })
    }
    fn selection_mouse(&mut self, mouse: Mouse, tty: &mut File) -> io::Result<()> {
        let Some(cell) = self.mouse_cell(mouse.row, mouse.col) else {
            return Ok(());
        };
        if mouse.motion() {
            if !self.selection.selecting {
                return Ok(());
            }
            self.selection.quoted |= mouse.option();
            self.selection.dragged |= cell != self.selection.current;
            self.selection.active = true;
            self.selection.current = cell;
            if mouse.no_buttons() {
                self.finish_selection(tty)?
            }
            return Ok(());
        }
        if mouse.base() != 0 {
            return Ok(());
        }
        if mouse.pressed {
            self.selection = Selection {
                active: true,
                selecting: true,
                quoted: mouse.option(),
                anchor: cell,
                current: cell,
                ..Default::default()
            }
        } else if self.selection.selecting {
            self.selection.dragged |= cell != self.selection.current;
            self.selection.quoted |= mouse.option();
            self.selection.current = cell;
            self.finish_selection(tty)?
        }
        Ok(())
    }
    fn selection_bounds(&self) -> Option<(Cell, Cell)> {
        if !self.selection.active {
            return None;
        }
        let (a, b) = if self.selection.anchor <= self.selection.current {
            (
                Cell {
                    line: self.selection.anchor.line,
                    col: self.selection.anchor.col.saturating_sub(1),
                },
                Cell {
                    line: self.selection.current.line,
                    col: self.selection.current.col,
                },
            )
        } else {
            (
                Cell {
                    line: self.selection.current.line,
                    col: self.selection.current.col.saturating_sub(1),
                },
                Cell {
                    line: self.selection.anchor.line,
                    col: self.selection.anchor.col,
                },
            )
        };
        (a < b).then_some((a, b))
    }
    fn selection_range(&self, line: usize) -> Option<Range> {
        let (start, end) = self.selection_bounds()?;
        if line < start.line || line > end.line {
            return None;
        }
        let width = self.mappings.get(line).map_or_else(
            || self.plain.get(line).map_or(0, |s| s.chars().count()),
            |m| m.spans.len(),
        );
        let from = if line == start.line {
            start.col.min(width)
        } else {
            0
        };
        let to = if line == end.line {
            end.col.min(width)
        } else {
            width
        };
        (from < to).then_some(Range {
            start: from,
            end: to,
        })
    }
    fn selection_markdown(&self) -> Vec<u8> {
        let Some((start, end)) = self.selection_bounds() else {
            return vec![];
        };
        let mut spans = vec![];
        for line in start.line..=end.line {
            let Some(mapping) = self.mappings.get(line) else {
                continue;
            };
            let from = if line == start.line {
                start.col.min(mapping.spans.len())
            } else {
                0
            };
            let to = if line == end.line {
                end.col.min(mapping.spans.len())
            } else {
                mapping.spans.len()
            };
            spans.extend(
                mapping.spans[from..to]
                    .iter()
                    .copied()
                    .filter(|span| span.valid()),
            );
        }
        let spans = merge_spans(spans);
        if spans.is_empty() {
            return self.selection_plain(start, end).into_bytes();
        }
        let mut out = vec![];
        let mut previous = None;
        for span in spans {
            if let Some(end) = previous
                && span.start > end
            {
                out.extend(&self.source[end..span.start])
            }
            out.extend(&self.source[span.start..span.end]);
            previous = Some(span.end)
        }
        out
    }
    fn selection_plain(&self, start: Cell, end: Cell) -> String {
        let mut out = String::new();
        for line in start.line..=end.line {
            let chars = self
                .plain
                .get(line)
                .map_or(vec![], |s| s.chars().collect::<Vec<_>>());
            let from = if line == start.line {
                start.col.min(chars.len())
            } else {
                0
            };
            let to = if line == end.line {
                end.col.min(chars.len())
            } else {
                chars.len()
            };
            out.extend(&chars[from..to]);
            if line < end.line {
                out.push('\n')
            }
        }
        out
    }
    fn finish_selection(&mut self, tty: &mut File) -> io::Result<()> {
        self.selection.selecting = false;
        if !self.selection.dragged {
            self.selection.active = false;
            return Ok(());
        }
        let mut text = self.selection_markdown();
        if text.is_empty() {
            self.selection.active = false;
            return Ok(());
        }
        if self.selection.quoted {
            text = quote_markdown(&text)
        }
        if let Err(error) = copy_osc52(tty, &text) {
            self.set_notice(error.to_string(), true);
            return Ok(());
        }
        self.clear_notice();
        self.selection.flash = true;
        Ok(())
    }

    fn render_line(&self, index: usize) -> String {
        let mut line = self.lines[index].clone();
        if self.blocks.iter().any(|block| block.line == index) {
            line = replace_visible(&line, 0, &format!("{DIM}⎘{RESET}{}", self.theme.code))
        }
        if !self.query.is_empty() {
            line = highlight_matches(&line, &self.plain[index], &self.query, &self.highlight())
        }
        if !self.selection.flash
            && let Some(range) = self.selection_range(index)
        {
            line = highlight_ranges(&line, &[range], &self.selection_highlight())
        }
        if let Some(ranges) = self.changed.get(&index) {
            line = highlight_ranges(&line, ranges, &self.highlight())
        }
        line
    }
    fn highlight(&self) -> String {
        if self.theme.highlight.is_empty() {
            format!("{REVERSE}{BOLD}")
        } else {
            format!("{}{BOLD}", self.theme.highlight)
        }
    }
    fn selection_highlight(&self) -> String {
        if self.theme.highlight.is_empty() {
            REVERSE.into()
        } else {
            self.theme.highlight.clone()
        }
    }
    fn clear_expired_flash(&mut self, now: Instant) {
        if self.flash_until.is_some_and(|until| now >= until) {
            self.changed.clear();
            self.flash_until = None
        }
    }

    fn draw(&mut self, tty: &mut File) -> io::Result<()> {
        if self.help {
            return self.draw_help(tty);
        }
        let mut out = String::new();
        for row in 0..self.view_height() {
            out += &cursor(row + 1, 1);
            out += "\x1b[2K";
            let index = self.top + row;
            if index < self.lines.len() {
                out += &cursor(row + 1, self.content_left());
                out += &self.render_line(index)
            }
        }
        if self.outline.active {
            out += &self.draw_outline()
        }
        let status_row = self.height.max(1);
        out += &cursor(status_row, 1);
        out += "\x1b[2K";
        if self.prompt {
            let (display, col) = self.prompt_display();
            out += &self.render_bar(&display, true);
            out += &cursor(status_row, col.max(1));
            out += SHOW_CURSOR
        } else {
            out += HIDE_CURSOR;
            out += &self.status_bar();
            if self.outline.active {
                let (row, col) = self.outline_cursor();
                out += &cursor(row, col);
                out += SHOW_CURSOR
            }
        }
        tty.write_all(out.as_bytes())?;
        tty.flush()
    }
    fn draw_help(&self, tty: &mut File) -> io::Result<()> {
        let lines = help_lines();
        let width = lines
            .iter()
            .map(|line| visible_width(line))
            .max()
            .unwrap_or(0)
            .min(self.width);
        let top = (self.height.saturating_sub(lines.len()) / 2 + 1).max(1);
        let left = (self.width.saturating_sub(width) / 2 + 1).max(1);
        let mut out = HIDE_CURSOR.to_owned();
        for row in 1..=self.height {
            out += &cursor(row, 1);
            out += "\x1b[2K"
        }
        for (index, line) in lines.iter().enumerate() {
            if top + index <= self.height {
                out += &cursor(top + index, left);
                out += &fit_width(line, width)
            }
        }
        tty.write_all(out.as_bytes())
    }
    fn render_bar(&self, text: &str, prompt: bool) -> String {
        let text = fit_width(text, self.width);
        tinted(
            &text,
            if prompt {
                &self.theme.prompt
            } else {
                &self.theme.status
            },
            self.width,
        )
    }
    fn status_bar(&self) -> String {
        let right = fit_width(&self.status_right(), self.width);
        let right_width = visible_width(&right);
        if right_width >= self.width {
            return tinted(&right, &self.theme.status, self.width);
        }
        let mut available = self.width - right_width;
        if !self.status_left().is_empty() && !right.is_empty() {
            available = available.saturating_sub(2)
        }
        let left = self.status_left_fitted(available);
        let mut gap = self
            .width
            .saturating_sub(visible_width(&left) + right_width);
        let joined = if !left.is_empty() && !right.is_empty() && gap >= 2 {
            gap -= 2;
            format!("{left}  {}{right}", " ".repeat(gap))
        } else {
            format!("{left}{}{right}", " ".repeat(gap))
        };
        tinted(&joined, &self.theme.status, self.width)
    }
    fn status_parts(&self) -> (String, Vec<String>) {
        let mut extra = vec![];
        if self.flow {
            extra.push("flow".into())
        }
        if !self.query.is_empty() {
            extra.push(if self.matches.is_empty() {
                format!("/{} 0", self.query)
            } else {
                format!(
                    "/{} {}/{}",
                    self.query,
                    self.match_index.unwrap_or(0) + 1,
                    self.matches.len()
                )
            })
        }
        if !self.notice.is_empty() {
            extra.push(self.notice.clone())
        }
        (self.section_path(), extra)
    }
    fn status_left(&self) -> String {
        let (section, extra) = self.status_parts();
        std::iter::once(section)
            .chain(extra)
            .collect::<Vec<_>>()
            .join("  ")
    }
    fn status_left_fitted(&self, width: usize) -> String {
        if width == 0 {
            return String::new();
        }
        let (section, extra) = self.status_parts();
        if extra.is_empty() {
            return self.section_fitted(width);
        }
        let text = extra.join("  ");
        if visible_width(&text) >= width {
            return fit_width(&text, width);
        }
        let gap = usize::from(!section.is_empty()) * 2;
        let section = self.section_fitted(width.saturating_sub(visible_width(&text) + gap));
        if section.is_empty() {
            text
        } else {
            format!("{section}{}{text}", " ".repeat(gap))
        }
    }
    fn status_right(&self) -> String {
        let percent = if self.lines.is_empty() {
            0
        } else if self.max_top() > 0 {
            ((self.top as f64 / self.max_top() as f64) * 100.0).round() as usize
        } else {
            100
        };
        let mut parts = vec![];
        if let Some(time) = self.modified {
            parts.push(relative_time(time, SystemTime::now()))
        }
        parts.push(format!("{percent}%"));
        parts.join("  ")
    }
    fn section_path(&self) -> String {
        let path = self.heading_path();
        if path.is_empty() {
            return self.cfg.label.clone();
        }
        let parts = path
            .iter()
            .enumerate()
            .map(|(i, h)| {
                if i + 1 == path.len() {
                    format!("{BOLD}{}{RESET}", h.text)
                } else {
                    h.text.clone()
                }
            })
            .collect::<Vec<_>>();
        format!("{}: {}", self.cfg.label, parts.join(" › "))
    }
    fn section_fitted(&self, width: usize) -> String {
        let path = self.heading_path();
        if path.is_empty() {
            return fit_left(&self.cfg.label, width).0;
        }
        let plain = format!(
            "{}: {}",
            self.cfg.label,
            path.iter()
                .map(|h| h.text.as_str())
                .collect::<Vec<_>>()
                .join(" › ")
        );
        let (display, offset) = fit_left(&plain, width);
        let Some(last) = path.last() else {
            return display;
        };
        let plain_chars = plain.chars().count();
        let start = plain_chars - last.text.chars().count();
        let display_prefix = display
            .chars()
            .count()
            .saturating_sub(plain_chars.saturating_sub(offset));
        let from = display_prefix + start.saturating_sub(offset);
        let to = (display_prefix + plain_chars.saturating_sub(offset)).min(display.chars().count());
        let chars = display.chars().collect::<Vec<_>>();
        if from >= to {
            return display;
        }
        format!(
            "{}{BOLD}{}{RESET}{}",
            chars[..from].iter().collect::<String>(),
            chars[from..to].iter().collect::<String>(),
            chars[to..].iter().collect::<String>()
        )
    }
    fn prompt_display(&self) -> (String, usize) {
        scrolled_prompt("/", &self.prompt_value, self.prompt_cursor, self.width)
    }
    fn draw_outline(&mut self) -> String {
        if self.outline.filtered.is_empty() {
            return String::new();
        }
        let width = self.outline_width();
        let rows = self.outline_rows();
        let top = self.height.saturating_sub(rows + 1).max(1);
        let selected = self.outline_position();
        let max_scroll = self.outline.filtered.len().saturating_sub(rows);
        self.outline.scroll = self.outline.scroll.min(max_scroll);
        if let Some(position) = selected {
            if position < self.outline.scroll {
                self.outline.scroll = position
            } else if position >= self.outline.scroll + rows {
                self.outline.scroll = position - rows + 1
            }
        }
        let mut out = String::new();
        for row in 0..rows {
            out += &cursor(top + row, 1);
            let index = self.outline.scroll + row;
            let mut bg = &self.theme.status;
            let text = if let Some(heading_index) = self.outline.filtered.get(index) {
                let heading = &self.headings[*heading_index];
                let label = fit_width(
                    &format!(
                        "{}{}",
                        "  ".repeat(heading.level.saturating_sub(1) as usize),
                        heading.text
                    ),
                    width,
                );
                if Some(index) == selected {
                    if !self.theme.highlight.is_empty() {
                        bg = &self.theme.highlight;
                        format!("{BOLD}{label}{RESET}")
                    } else {
                        format!("{REVERSE}{BOLD}{label}{RESET}")
                    }
                } else {
                    format!("{BOLD}{label}{RESET}")
                }
            } else {
                String::new()
            };
            out += &tinted(&text, bg, width)
        }
        out += &cursor(top + rows, 1);
        let (prompt, _) = scrolled_prompt("› ", &self.outline.filter, self.outline.cursor, width);
        out += &tinted(
            &prompt,
            if self.theme.prompt.is_empty() {
                &self.theme.status
            } else {
                &self.theme.prompt
            },
            width,
        );
        out
    }
    fn outline_cursor(&self) -> (usize, usize) {
        let width = self.outline_width();
        let rows = self.outline_rows();
        let (_, col) = scrolled_prompt("› ", &self.outline.filter, self.outline.cursor, width);
        (self.height.saturating_sub(rows + 1).max(1) + rows, col)
    }
}

#[derive(Clone, Copy, Debug)]
enum Key {
    Char(char),
    Ctrl(char),
    Alt(char),
    Up,
    Down,
    Left,
    Right,
    PageUp,
    PageDown,
    Home,
    End,
    Escape,
    Enter,
    Backspace,
    Delete,
}
#[derive(Clone, Copy, Debug)]
struct Mouse {
    button: usize,
    row: usize,
    col: usize,
    pressed: bool,
}
impl Mouse {
    fn motion(self) -> bool {
        (32..64).contains(&self.button)
    }
    fn wheel(self) -> Option<isize> {
        if !self.motion() && self.button & 64 != 0 {
            match self.base() {
                0 => Some(-1),
                1 => Some(1),
                _ => None,
            }
        } else {
            None
        }
    }
    fn base(self) -> usize {
        self.button & 3
    }
    fn option(self) -> bool {
        self.button & 8 != 0
    }
    fn no_buttons(self) -> bool {
        self.motion() && self.base() == 3
    }
}
enum Event {
    Key(Key),
    Focus(bool),
    Color((u8, u8, u8)),
    Mouse(Mouse),
    Ignore,
}

fn read_events(file: File, tx: mpsc::Sender<io::Result<Event>>) {
    let mut reader = BufReader::new(file);
    loop {
        match read_event(&mut reader) {
            Ok(event) => {
                if tx.send(Ok(event)).is_err() {
                    break;
                }
            }
            Err(error) => {
                let _ = tx.send(Err(error));
                break;
            }
        }
    }
}
fn read_event(reader: &mut BufReader<File>) -> io::Result<Event> {
    let byte = read_byte(reader)?;
    match byte {
        b'\r' | b'\n' => Ok(Event::Key(Key::Enter)),
        0x7f => Ok(Event::Key(Key::Backspace)),
        0x1b => read_escape(reader),
        1..=26 => Ok(Event::Key(Key::Ctrl((b'a' + byte - 1) as char))),
        _ => {
            let mut bytes = vec![byte];
            let width = utf8_width(byte);
            for _ in 1..width {
                bytes.push(read_byte(reader)?)
            }
            let ch = std::str::from_utf8(&bytes)
                .ok()
                .and_then(|s| s.chars().next())
                .unwrap_or('\u{fffd}');
            Ok(Event::Key(Key::Char(ch)))
        }
    }
}
fn read_escape(reader: &mut BufReader<File>) -> io::Result<Event> {
    if reader.buffer().is_empty() && !wait_for_input(reader.get_ref(), Duration::from_millis(35))? {
        return Ok(Event::Key(Key::Escape));
    }
    let next = read_byte(reader)?;
    match next {
        b'[' => read_csi(reader),
        b']' => read_osc(reader),
        byte => {
            let ch = byte as char;
            Ok(Event::Key(Key::Alt(ch.to_ascii_lowercase())))
        }
    }
}
fn read_csi(reader: &mut BufReader<File>) -> io::Result<Event> {
    let mut params = String::new();
    loop {
        let byte = read_byte(reader)?;
        if (0x40..=0x7e).contains(&byte) {
            return if params.starts_with('<') && (byte == b'M' || byte == b'm') {
                parse_mouse(&params, byte)
            } else {
                Ok(Event::Key(
                    match (byte, params.split(';').next().unwrap_or("")) {
                        (b'A', _) => Key::Up,
                        (b'B', _) => Key::Down,
                        (b'C', _) => Key::Right,
                        (b'D', _) => Key::Left,
                        (b'H', _) => Key::Home,
                        (b'F', _) => Key::End,
                        (b'~', "1" | "7") => Key::Home,
                        (b'~', "4" | "8") => Key::End,
                        (b'~', "3") => Key::Delete,
                        (b'~', "5") => Key::PageUp,
                        (b'~', "6") => Key::PageDown,
                        (b'I', _) => return Ok(Event::Focus(true)),
                        (b'O', _) => return Ok(Event::Focus(false)),
                        _ => Key::Escape,
                    },
                ))
            };
        }
        params.push(byte as char)
    }
}
fn parse_mouse(params: &str, final_byte: u8) -> io::Result<Event> {
    let values = params
        .trim_start_matches('<')
        .split(';')
        .map(str::parse::<usize>)
        .collect::<Result<Vec<_>, _>>()
        .map_err(io::Error::other)?;
    if values.len() != 3 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid SGR mouse event",
        ));
    }
    Ok(Event::Mouse(Mouse {
        button: values[0],
        col: values[1],
        row: values[2],
        pressed: final_byte == b'M',
    }))
}
fn read_osc(reader: &mut BufReader<File>) -> io::Result<Event> {
    let mut data = vec![];
    loop {
        let byte = read_byte(reader)?;
        if byte == 7 {
            break;
        }
        if byte == 0x1b && read_byte(reader)? == b'\\' {
            break;
        }
        data.push(byte)
    }
    let text = String::from_utf8_lossy(&data);
    Ok(text
        .strip_prefix("11;")
        .and_then(parse_color)
        .map_or(Event::Ignore, Event::Color))
}
fn read_byte(reader: &mut impl Read) -> io::Result<u8> {
    let mut byte = [0];
    reader.read_exact(&mut byte)?;
    Ok(byte[0])
}
fn utf8_width(byte: u8) -> usize {
    if byte < 0x80 {
        1
    } else if byte < 0xe0 {
        2
    } else if byte < 0xf0 {
        3
    } else {
        4
    }
}

fn parse_color(text: &str) -> Option<(u8, u8, u8)> {
    let values = text
        .strip_prefix("rgb:")
        .or_else(|| text.strip_prefix("rgba:"))?
        .split('/')
        .map(color_component)
        .collect::<Option<Vec<_>>>()?;
    (values.len() >= 3).then_some((values[0], values[1], values[2]))
}
fn color_component(text: &str) -> Option<u8> {
    match text.len() {
        2 => u8::from_str_radix(text, 16).ok(),
        4 => u16::from_str_radix(text, 16)
            .ok()
            .map(|v| ((u32::from(v) + 128) / 257) as u8),
        _ => None,
    }
}
fn theme_for(rgb: (u8, u8, u8)) -> Theme {
    let subtle = if light(rgb) { 0.04 } else { 0.12 };
    let prompt = if light(rgb) { 0.10 } else { 0.20 };
    let status = tint(rgb, subtle);
    let prompt_bg = tint(rgb, prompt);
    Theme {
        status: status.clone(),
        prompt: prompt_bg.clone(),
        highlight: prompt_bg.clone(),
        blockquote: tint(rgb, 0.16),
        code: status,
        mark: prompt_bg,
    }
}
fn light((r, g, b): (u8, u8, u8)) -> bool {
    0.299 * f64::from(r) + 0.587 * f64::from(g) + 0.114 * f64::from(b) > 128.0
}
fn tint(bg: (u8, u8, u8), alpha: f64) -> String {
    let overlay = if light(bg) {
        (0, 0, 0)
    } else {
        (255, 255, 255)
    };
    let blend = |a: u8, b: u8| (f64::from(b) * alpha + f64::from(a) * (1. - alpha)).floor() as u8;
    let color = (
        blend(bg.0, overlay.0),
        blend(bg.1, overlay.1),
        blend(bg.2, overlay.2),
    );
    let colorterm = std::env::var("COLORTERM")
        .unwrap_or_default()
        .to_lowercase();
    if colorterm.contains("truecolor") || colorterm.contains("24bit") {
        format!("\x1b[48;2;{};{};{}m", color.0, color.1, color.2)
    } else if std::env::var("TERM")
        .unwrap_or_default()
        .to_lowercase()
        .contains("256color")
    {
        format!("\x1b[48;5;{}m", nearest_256(color))
    } else {
        String::new()
    }
}
fn nearest_256(color: (u8, u8, u8)) -> usize {
    palette()
        .into_iter()
        .enumerate()
        .min_by_key(|(_, c)| {
            let dr = i32::from(color.0) - i32::from(c.0);
            let dg = i32::from(color.1) - i32::from(c.1);
            let db = i32::from(color.2) - i32::from(c.2);
            299 * dr * dr + 587 * dg * dg + 114 * db * db
        })
        .map_or(0, |(i, _)| i)
}
fn palette() -> Vec<(u8, u8, u8)> {
    let mut p = vec![(0, 0, 0); 256];
    let base = [
        (0, 0, 0),
        (128, 0, 0),
        (0, 128, 0),
        (128, 128, 0),
        (0, 0, 128),
        (128, 0, 128),
        (0, 128, 128),
        (192, 192, 192),
        (128, 128, 128),
        (255, 0, 0),
        (0, 255, 0),
        (255, 255, 0),
        (0, 0, 255),
        (255, 0, 255),
        (0, 255, 255),
        (255, 255, 255),
    ];
    p[..16].copy_from_slice(&base);
    let steps = [0, 95, 135, 175, 215, 255];
    let mut i = 16;
    for r in steps {
        for g in steps {
            for b in steps {
                p[i] = (r, g, b);
                i += 1
            }
        }
    }
    for i in 0..24 {
        let v = 8 + i as u8 * 10;
        p[232 + i] = (v, v, v)
    }
    p
}

fn watch_paths(
    paths: &[PathBuf],
    tx: mpsc::Sender<()>,
) -> io::Result<Option<notify::RecommendedWatcher>> {
    if paths.is_empty() {
        return Ok(None);
    }
    let watched = paths
        .iter()
        .filter_map(|p| fs::canonicalize(p).ok())
        .collect::<Vec<_>>();
    let mut watcher = notify::recommended_watcher(move |result: notify::Result<notify::Event>| {
        if let Ok(event) = result
            && matches!(
                event.kind,
                EventKind::Modify(_) | EventKind::Create(_) | EventKind::Remove(_)
            )
            && event
                .paths
                .iter()
                .filter_map(|p| fs::canonicalize(p).ok().or_else(|| Some(p.clone())))
                .any(|p| watched.contains(&p))
        {
            let _ = tx.send(());
        }
    })
    .map_err(io::Error::other)?;
    let mut dirs = vec![];
    for path in paths {
        let dir = path.parent().unwrap_or(Path::new("."));
        if !dirs.iter().any(|d: &PathBuf| d == dir) {
            watcher
                .watch(dir, RecursiveMode::NonRecursive)
                .map_err(io::Error::other)?;
            dirs.push(dir.to_owned())
        }
    }
    Ok(Some(watcher))
}

fn quote_markdown(text: &[u8]) -> Vec<u8> {
    if text.is_empty() {
        return vec![];
    }
    let mut out = b"> ".to_vec();
    for (byte_index, line) in text.split(|b| *b == b'\n').enumerate() {
        if byte_index > 0 {
            out.extend(b"\n> ")
        }
        out.extend(line)
    }
    out
}
fn copy_osc52(tty: &mut File, text: &[u8]) -> io::Result<()> {
    copy_osc52_to(tty, text)
}
fn merge_spans(spans: Vec<SourceSpan>) -> Vec<SourceSpan> {
    let mut merged: Vec<SourceSpan> = vec![];
    for span in spans.into_iter().filter(|s| s.valid()) {
        if let Some(last) = merged.last_mut()
            && span.start <= last.end
        {
            last.start = last.start.min(span.start);
            last.end = last.end.max(span.end);
            continue;
        }
        merged.push(span)
    }
    merged
}

fn changed_ranges(old: &[String], new: &[String]) -> HashMap<usize, Vec<Range>> {
    if old == new {
        return HashMap::new();
    }
    let pairs = line_pairs(old, new);
    let mut changed = HashMap::new();
    let (mut old_at, mut new_at) = (0, 0);
    for (old_match, new_match) in pairs
        .into_iter()
        .chain(std::iter::once((old.len(), new.len())))
    {
        add_changed(&mut changed, old, new, old_at, old_match, new_at, new_match);
        old_at = old_match + 1;
        new_at = new_match + 1
    }
    changed
}
fn add_changed(
    changed: &mut HashMap<usize, Vec<Range>>,
    old: &[String],
    new: &[String],
    old_start: usize,
    old_end: usize,
    new_start: usize,
    new_end: usize,
) {
    let old_count = old_end - old_start;
    let new_count = new_end - new_start;
    let paired = old_count.min(new_count);
    for i in 0..paired {
        if let Some(range) = changed_runes(&old[old_start + i], &new[new_start + i]) {
            changed.insert(new_start + i, vec![range]);
        }
    }
    for i in paired..new_count {
        let width = new[new_start + i].chars().count();
        if width > 0 {
            changed.insert(
                new_start + i,
                vec![Range {
                    start: 0,
                    end: width,
                }],
            );
        }
    }
    if old_count > new_count && !new.is_empty() {
        let line = (new_start + paired).min(new.len() - 1);
        let width = new[line].chars().count();
        if width > 0 {
            changed.insert(
                line,
                vec![Range {
                    start: 0,
                    end: width,
                }],
            );
        }
    }
}
fn changed_runes(old: &str, new: &str) -> Option<Range> {
    let old = old.chars().collect::<Vec<_>>();
    let new = new.chars().collect::<Vec<_>>();
    let mut prefix = 0;
    while prefix < old.len() && prefix < new.len() && old[prefix] == new[prefix] {
        prefix += 1
    }
    let (mut a, mut b) = (old.len(), new.len());
    while a > prefix && b > prefix && old[a - 1] == new[b - 1] {
        a -= 1;
        b -= 1
    }
    if prefix < b {
        Some(Range {
            start: prefix,
            end: b,
        })
    } else if new.is_empty() {
        None
    } else {
        let start = prefix.min(new.len() - 1);
        Some(Range {
            start,
            end: start + 1,
        })
    }
}
fn line_pairs(old: &[String], new: &[String]) -> Vec<(usize, usize)> {
    const MAX: usize = 4_000_000;
    if !old.is_empty() && new.len() <= MAX / old.len() {
        let cols = new.len() + 1;
        let mut dp = vec![0u32; (old.len() + 1) * cols];
        for i in (0..old.len()).rev() {
            for j in (0..new.len()).rev() {
                dp[i * cols + j] = if old[i] == new[j] {
                    dp[(i + 1) * cols + j + 1] + 1
                } else {
                    dp[(i + 1) * cols + j].max(dp[i * cols + j + 1])
                }
            }
        }
        let (mut i, mut j) = (0, 0);
        let mut pairs = vec![];
        while i < old.len() && j < new.len() {
            if old[i] == new[j] {
                pairs.push((i, j));
                i += 1;
                j += 1
            } else if dp[(i + 1) * cols + j] >= dp[i * cols + j + 1] {
                i += 1
            } else {
                j += 1
            }
        }
        return pairs;
    }
    let mut pairs = vec![];
    let mut prefix = 0;
    while prefix < old.len() && prefix < new.len() && old[prefix] == new[prefix] {
        pairs.push((prefix, prefix));
        prefix += 1
    }
    let (mut i, mut j) = (old.len(), new.len());
    let mut suffix = vec![];
    while i > prefix && j > prefix && old[i - 1] == new[j - 1] {
        i -= 1;
        j -= 1;
        suffix.push((i, j))
    }
    suffix.reverse();
    pairs.extend(suffix);
    pairs
}

fn find_matches(plain: &str, query: &str) -> Vec<Range> {
    if query.is_empty() {
        return vec![];
    }
    let hay = plain.to_lowercase().chars().collect::<Vec<_>>();
    let needle = query.to_lowercase().chars().collect::<Vec<_>>();
    if needle.is_empty() || hay.len() < needle.len() {
        return vec![];
    }
    let mut out = vec![];
    let mut i = 0;
    while i + needle.len() <= hay.len() {
        if hay[i..i + needle.len()] == needle {
            out.push(Range {
                start: i,
                end: i + needle.len(),
            });
            i += needle.len()
        } else {
            i += 1
        }
    }
    out
}
fn highlight_matches(rendered: &str, plain: &str, query: &str, start: &str) -> String {
    highlight_ranges(rendered, &find_matches(plain, query), start)
}
fn highlight_all_matches(rendered: &str, plain: &str, queries: &[String], start: &str) -> String {
    let mut ranges = queries
        .iter()
        .flat_map(|query| find_matches(plain, query))
        .collect::<Vec<_>>();
    ranges.sort_by_key(|range| (range.start, range.end));
    let mut merged: Vec<Range> = Vec::new();
    for range in ranges {
        if range.start == range.end {
            continue;
        }
        if let Some(last) = merged.last_mut()
            && range.start <= last.end
        {
            last.end = last.end.max(range.end);
            continue;
        }
        merged.push(range);
    }
    highlight_ranges(rendered, &merged, start)
}
fn highlight_ranges(rendered: &str, ranges: &[Range], start: &str) -> String {
    if start.is_empty() || ranges.is_empty() {
        return rendered.into();
    }
    let mut out = String::new();
    let (mut pos, mut index, mut active, mut end) = (0, 0, false, 0);
    let mut current = String::new();
    let mut byte = 0;
    while byte < rendered.len() {
        if !active && ranges.get(index).is_some_and(|r| r.start == pos) {
            out += start;
            active = true;
            end = ranges[index].end
        }
        if active && pos == end {
            out += RESET;
            out += &current;
            active = false;
            index += 1;
            continue;
        }
        if rendered.as_bytes()[byte] == 0x1b {
            let (next, sgr) = escape_end(rendered, byte);
            let seq = &rendered[byte..next];
            out += seq;
            if sgr {
                current = update_sgr(&current, seq);
                if active {
                    out += start
                }
            }
            byte = next;
            continue;
        }
        let ch = rendered[byte..].chars().next().unwrap();
        out.push(ch);
        byte += ch.len_utf8();
        pos += 1
    }
    if active {
        out += RESET;
        out += &current
    }
    out
}
fn replace_visible(rendered: &str, position: usize, replacement: &str) -> String {
    let mut out = String::new();
    let (mut pos, mut byte) = (0, 0);
    while byte < rendered.len() {
        if rendered.as_bytes()[byte] == 0x1b {
            let (next, _) = escape_end(rendered, byte);
            out += &rendered[byte..next];
            byte = next;
            continue;
        }
        let ch = rendered[byte..].chars().next().unwrap();
        if pos == position {
            out += replacement
        } else {
            out.push(ch)
        }
        byte += ch.len_utf8();
        pos += 1
    }
    out
}
fn escape_end(text: &str, start: usize) -> (usize, bool) {
    let bytes = text.as_bytes();
    match bytes.get(start + 1) {
        Some(b'[') => {
            for (i, byte) in bytes.iter().enumerate().skip(start + 2) {
                if (0x40..=0x7e).contains(byte) {
                    return (i + 1, *byte == b'm');
                }
            }
            (bytes.len(), false)
        }
        Some(b']') => {
            let mut i = start + 2;
            while i < bytes.len() {
                if bytes[i] == 7 {
                    return (i + 1, false);
                }
                if bytes[i] == 0x1b && bytes.get(i + 1) == Some(&b'\\') {
                    return (i + 2, false);
                }
                i += 1
            }
            (bytes.len(), false)
        }
        _ => (start + 2.min(bytes.len() - start), false),
    }
}
fn update_sgr(current: &str, sequence: &str) -> String {
    let params = sequence
        .strip_prefix("\x1b[")
        .and_then(|s| s.strip_suffix('m'))
        .unwrap_or("");
    if params.is_empty() || params.split(';').any(|p| p.is_empty() || p == "0") {
        String::new()
    } else {
        format!("{current}{sequence}")
    }
}
fn strip_ansi(text: &str) -> String {
    let mut out = String::new();
    let mut byte = 0;
    while byte < text.len() {
        if text.as_bytes()[byte] == 0x1b {
            byte = escape_end(text, byte).0
        } else {
            let ch = text[byte..].chars().next().unwrap();
            out.push(ch);
            byte += ch.len_utf8()
        }
    }
    out
}
fn visible_width(text: &str) -> usize {
    display_width(&strip_ansi(text))
}
fn display_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}
fn truncate_visible(text: &str, width: usize) -> String {
    let mut out = String::new();
    let (mut used, mut byte) = (0, 0);
    let mut sgr = String::new();
    while byte < text.len() && used < width {
        if text.as_bytes()[byte] == 0x1b {
            let (next, is_sgr) = escape_end(text, byte);
            let seq = &text[byte..next];
            out += seq;
            if is_sgr {
                sgr = update_sgr(&sgr, seq)
            }
            byte = next;
            continue;
        }
        let ch = text[byte..].chars().next().unwrap();
        let cw = ch.width().unwrap_or(0);
        if used + cw > width {
            break;
        }
        out.push(ch);
        used += cw;
        byte += ch.len_utf8()
    }
    if !sgr.is_empty() {
        out += RESET
    }
    out
}
fn fit_width(text: &str, width: usize) -> String {
    if width == 0 {
        String::new()
    } else if visible_width(text) <= width {
        text.into()
    } else if width <= 3 {
        truncate_visible(text, width)
    } else {
        format!("{}...", truncate_visible(text, width - 3))
    }
}
fn fit_left(text: &str, width: usize) -> (String, usize) {
    if width == 0 {
        return (String::new(), 0);
    }
    if display_width(text) <= width {
        return (text.into(), 0);
    }
    let (prefix, keep) = if width <= 3 {
        ("", width)
    } else {
        ("...", width - 3)
    };
    let chars = text.chars().collect::<Vec<_>>();
    let (mut used, mut start) = (0, chars.len());
    while start > 0 {
        let w = chars[start - 1].width().unwrap_or(0);
        if used + w > keep {
            break;
        }
        used += w;
        start -= 1
    }
    (
        format!("{prefix}{}", chars[start..].iter().collect::<String>()),
        start,
    )
}
fn tinted(text: &str, bg: &str, width: usize) -> String {
    let text = fit_width(text, width);
    let padding = " ".repeat(width.saturating_sub(visible_width(&text)));
    if bg.is_empty() {
        format!("{text}{padding}")
    } else {
        format!(
            "{bg}{}{padding}{RESET}",
            text.replace(RESET, &format!("{RESET}{bg}"))
        )
    }
}
fn scrolled_prompt(prefix: &str, value: &str, cursor: usize, width: usize) -> (String, usize) {
    if width == 0 {
        return (String::new(), 1);
    }
    let chars = value.chars().collect::<Vec<_>>();
    let cursor = cursor.min(chars.len());
    let available = width.saturating_sub(prefix.chars().count());
    if available == 0 {
        return (fit_width(prefix, width), 1);
    }
    let mut start = cursor.saturating_sub(available);
    let end = (start + available).min(chars.len());
    if end - start < available {
        start = end.saturating_sub(available)
    }
    (
        format!("{prefix}{}", chars[start..end].iter().collect::<String>()),
        (prefix.chars().count() + 1 + cursor - start).min(width),
    )
}
fn edit_insert(value: &mut String, cursor: &mut usize, ch: char) {
    let mut chars = value.chars().collect::<Vec<_>>();
    let at = (*cursor).min(chars.len());
    chars.insert(at, ch);
    *value = chars.into_iter().collect();
    *cursor = at + 1
}
fn edit_delete_before(value: &mut String, cursor: &mut usize) {
    if *cursor == 0 {
        return;
    }
    let mut chars = value.chars().collect::<Vec<_>>();
    let at = (*cursor).min(chars.len());
    chars.remove(at - 1);
    *value = chars.into_iter().collect();
    *cursor = at - 1
}
fn edit_delete_at(value: &mut String, cursor: usize) {
    let mut chars = value.chars().collect::<Vec<_>>();
    if cursor < chars.len() {
        chars.remove(cursor);
        *value = chars.into_iter().collect()
    }
}
fn edit_kill_start(value: &mut String, cursor: &mut usize) {
    *value = value
        .chars()
        .skip((*cursor).min(value.chars().count()))
        .collect();
    *cursor = 0
}
fn edit_kill_end(value: &mut String, cursor: usize) {
    *value = value.chars().take(cursor).collect()
}
fn edit_delete_word(value: &mut String, cursor: &mut usize) {
    let start = word_back(value, *cursor);
    let chars = value.chars().collect::<Vec<_>>();
    *value = chars[..start]
        .iter()
        .chain(&chars[(*cursor).min(chars.len())..])
        .collect();
    *cursor = start
}
fn word_back(value: &str, mut cursor: usize) -> usize {
    let chars = value.chars().collect::<Vec<_>>();
    cursor = cursor.min(chars.len());
    while cursor > 0 && chars[cursor - 1].is_whitespace() {
        cursor -= 1
    }
    while cursor > 0 && !chars[cursor - 1].is_whitespace() {
        cursor -= 1
    }
    cursor
}
fn word_forward(value: &str, mut cursor: usize) -> usize {
    let chars = value.chars().collect::<Vec<_>>();
    cursor = cursor.min(chars.len());
    while cursor < chars.len() && chars[cursor].is_whitespace() {
        cursor += 1
    }
    while cursor < chars.len() && !chars[cursor].is_whitespace() {
        cursor += 1
    }
    cursor
}
fn relative_time(then: SystemTime, now: SystemTime) -> String {
    let delta = now.duration_since(then).unwrap_or_default();
    let seconds = delta.as_secs();
    match seconds {
        0..=29 => "just now".into(),
        30..=3599 => format!("{}m", seconds / 60),
        3600..=86399 => format!("{}h", seconds / 3600),
        86400..=172799 => "yesterday".into(),
        172800..=604799 => format!("{}d", seconds / 86400),
        604800..=2591999 => format!("{}w", seconds / 604800),
        2592000..=31535999 => {
            let m = seconds / 2592000;
            if m <= 1 {
                "last month".into()
            } else {
                format!("{m}mo")
            }
        }
        31536000..=63071999 => "last year".into(),
        _ => format!("{}y", seconds / 31536000),
    }
}
fn pager_label(paths: &[PathBuf]) -> String {
    match paths {
        [] => "stdin".into(),
        [one] => one.display().to_string(),
        [first, ..] => format!("{} +{} more", first.display(), paths.len() - 1),
    }
}
fn help_lines() -> Vec<String> {
    vec![
        format!("{BOLD}md keyboard shortcuts{RESET}"),
        format!("{BOLD}Navigation{RESET}"),
        "j / ↓ / Enter / Ctrl-N     down one line".into(),
        "k / ↑ / Ctrl-P             up one line".into(),
        "d / u half screen; Space / Ctrl-V / PgDn page down".into(),
        "b / Alt-V / PgUp           up one screen".into(),
        "g / Home / Alt-< first; G / End / Alt-> last".into(),
        String::new(),
        format!("{BOLD}Document{RESET}"),
        "f flow mode; Ctrl-R open outline".into(),
        "r / Ctrl-L reload; ? help".into(),
        "/                          search; n / N next / previous".into(),
        "q / Ctrl-C                 quit".into(),
        String::new(),
        format!("{BOLD}Search and outline editing{RESET}"),
        "←/→ or Ctrl-B/F move; Home/End or Ctrl-A/E to ends".into(),
        "Alt-B/F by word; Backspace; Delete/Ctrl-D".into(),
        "Ctrl-W delete word; Ctrl-U/K delete to start/end".into(),
        "Enter accept; Ctrl-G cancel".into(),
        format!("{BOLD}Outline navigation{RESET}"),
        "j/k or Ctrl-N/P; PgDn/Ctrl-V; PgUp/b/Alt-V".into(),
        "g/G or Home/End; type to filter".into(),
        "Enter/Esc/Ctrl-G/Ctrl-R close outline; Ctrl-C quits".into(),
        "? / Esc / q closes help".into(),
    ]
}
fn cursor(row: usize, col: usize) -> String {
    format!("\x1b[{row};{col}H")
}

fn terminal_size(file: &File) -> io::Result<(usize, usize)> {
    let mut size = unsafe { std::mem::zeroed::<libc::winsize>() };
    if unsafe { libc::ioctl(file.as_raw_fd(), libc::TIOCGWINSZ, &mut size) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((
        usize::from(size.ws_col).max(1),
        usize::from(size.ws_row).max(1),
    ))
}
fn wait_for_input(file: &File, timeout: Duration) -> io::Result<bool> {
    let mut descriptor = libc::pollfd {
        fd: file.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let milliseconds = timeout.as_millis().min(i32::MAX as u128) as i32;
    // SAFETY: `descriptor` is valid for one element for the duration of poll.
    let result = unsafe { libc::poll(&mut descriptor, 1, milliseconds) };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(result > 0 && descriptor.revents & libc::POLLIN != 0)
}
struct RawMode {
    fd: RawFd,
    original: libc::termios,
}
impl RawMode {
    fn enable(file: &File) -> io::Result<Self> {
        let fd = file.as_raw_fd();
        let mut original = unsafe { std::mem::zeroed::<libc::termios>() };
        if unsafe { libc::tcgetattr(fd, &mut original) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let mut raw = original;
        unsafe { libc::cfmakeraw(&mut raw) };
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { fd, original })
    }
}
impl Drop for RawMode {
    fn drop(&mut self) {
        unsafe { libc::tcsetattr(self.fd, libc::TCSANOW, &self.original) };
    }
}
struct ScreenGuard {
    tty: File,
}
impl ScreenGuard {
    fn enter(mut tty: File) -> io::Result<Self> {
        write!(
            tty,
            "{ENTER_ALT}\x1b[2J\x1b[H{HIDE_CURSOR}{ENABLE_FOCUS}{ENABLE_MOUSE}"
        )?;
        Ok(Self { tty })
    }
}
impl Drop for ScreenGuard {
    fn drop(&mut self) {
        let _ = write!(
            self.tty,
            "{RESET}{SHOW_CURSOR}{DISABLE_MOUSE}{DISABLE_FOCUS}{EXIT_ALT}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pager(source: &str) -> Pager {
        let mut pager = Pager::new(
            PagerConfig {
                initial_source: source.as_bytes().to_vec(),
                label: "test.md".into(),
                ..Default::default()
            },
            80,
            24,
        );
        pager.reload(true).unwrap();
        pager
    }

    #[test]
    fn color_parsing() {
        assert_eq!(parse_color("rgb:ffff/8000/0000"), Some((255, 128, 0)));
        assert_eq!(parse_color("rgba:0000/0000/ffff/ffff"), Some((0, 0, 255)));
    }
    #[test]
    fn strips_ansi_and_links() {
        let text = format!("{BOLD}hello{RESET} \x1b]8;;https://x\x1b\\world\x1b]8;;\x1b\\");
        assert_eq!(strip_ansi(&text), "hello world");
    }
    #[test]
    fn flow_caps_and_centers() {
        let mut p = pager("hello\n");
        p.width = 140;
        p.flow = true;
        assert_eq!(p.render_width(), 100);
        assert_eq!(p.content_left(), 21);
    }
    #[test]
    fn help_fits_terminal() {
        assert!(help_lines().len() <= 24);
    }
    #[test]
    fn changed_text_ranges() {
        let got = changed_ranges(
            &["# Title".into(), "hello world".into(), "tail".into()],
            &[
                "# Title".into(),
                "hello brave world".into(),
                "new line".into(),
                "tail".into(),
            ],
        );
        assert_eq!(got[&1], vec![Range { start: 6, end: 12 }]);
        assert_eq!(got[&2], vec![Range { start: 0, end: 8 }]);
        assert!(!got.contains_key(&3));
    }
    #[test]
    fn deletion_flashes_neighbor() {
        let got = changed_ranges(
            &["before".into(), "removed".into(), "after".into()],
            &["before".into(), "after".into()],
        );
        assert_eq!(got[&1], vec![Range { start: 0, end: 5 }]);
    }
    #[test]
    fn prompt_editing() {
        let mut value = "alpha beta gamma".to_owned();
        let mut cursor = "alpha beta ".chars().count();
        edit_delete_word(&mut value, &mut cursor);
        assert_eq!(value, "alpha gamma");
        assert_eq!(cursor, 6);
        edit_kill_end(&mut value, cursor);
        assert_eq!(value, "alpha ");
        edit_kill_start(&mut value, &mut cursor);
        assert_eq!(value, "");
        assert_eq!(cursor, 0);
    }
    #[test]
    fn search_matches_case_insensitively() {
        assert_eq!(
            find_matches("Alpha alpha ALPHA", "alpha"),
            vec![
                Range { start: 0, end: 5 },
                Range { start: 6, end: 11 },
                Range { start: 12, end: 17 }
            ]
        );
    }
    #[test]
    fn highlights_preserve_styles_and_links() {
        let rendered = format!("{BOLD}hello{RESET} \x1b]8;;x\x1b\\world\x1b]8;;\x1b\\");
        let highlighted = highlight_matches(&rendered, "hello world", "world", REVERSE);
        assert!(highlighted.contains("\x1b]8;;x"));
        assert!(highlighted.contains(REVERSE));
    }
    #[test]
    fn embedded_pager_centers_source_line() {
        let view = render_embedded_pager(&EmbeddedPagerConfig {
            source: b"one\n\ntwo\n\nthree needle\n\nfour\n".to_vec(),
            width: 80,
            height: 3,
            center_source_line: Some(5),
            ..Default::default()
        })
        .unwrap();
        let plain = view
            .lines
            .iter()
            .map(|line| strip_ansi(line))
            .collect::<Vec<_>>();
        assert!(plain.iter().any(|line| line.contains("three needle")));
    }
    #[test]
    fn embedded_pager_highlights_terms_without_status_bar() {
        let view = render_embedded_pager(&EmbeddedPagerConfig {
            source: b"# Title\n\nalpha beta gamma\n".to_vec(),
            width: 80,
            height: 4,
            highlight_terms: vec!["alpha".into(), "gamma".into()],
            ..Default::default()
        })
        .unwrap();
        let rendered = view.lines.join("\n");
        assert!(rendered.contains(REVERSE));
        assert!(!strip_ansi(&rendered).contains("100%"));
    }
    #[test]
    fn embedded_pager_uses_render_style_for_code_blocks_and_highlights() {
        let view = render_embedded_pager(&EmbeddedPagerConfig {
            source: b"```rust\nlet alpha = 1;\n```\n".to_vec(),
            width: 80,
            height: 4,
            highlight_terms: vec!["alpha".into()],
            render_style: RenderStyle {
                code_block_bg: "\x1b[48;5;250m".into(),
                highlight_bg: "\x1b[48;5;240m".into(),
                ..Default::default()
            },
            ..Default::default()
        })
        .unwrap();
        let rendered = view.lines.join("\n");
        assert!(rendered.contains("⎘"));
        assert!(rendered.contains("\x1b[48;5;250m"));
        assert!(rendered.contains("\x1b[48;5;240m"));
    }
    #[test]
    fn embedded_pager_returns_visible_code_block_targets() {
        let view = render_embedded_pager(&EmbeddedPagerConfig {
            source: b"intro\n\n```\ncopy me\n```\n".to_vec(),
            width: 80,
            height: 8,
            ..Default::default()
        })
        .unwrap();
        assert_eq!(
            view.code_blocks,
            vec![EmbeddedCodeBlock {
                row: 2,
                col: 0,
                text: "copy me\n".into(),
            }]
        );
    }
    #[test]
    fn copy_osc52_to_writes_base64_payload() {
        let mut output = Vec::new();
        copy_osc52_to(&mut output, b"hello").unwrap();
        assert_eq!(String::from_utf8(output).unwrap(), "\x1b]52;c;aGVsbG8=\x07");

        let mut empty = Vec::new();
        copy_osc52_to(&mut empty, b"").unwrap();
        assert!(empty.is_empty());
    }
    #[test]
    fn relative_times() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(400 * 86400);
        assert_eq!(
            relative_time(now - Duration::from_secs(10), now),
            "just now"
        );
        assert_eq!(relative_time(now - Duration::from_secs(3600), now), "1h");
        assert_eq!(
            relative_time(now - Duration::from_secs(86400), now),
            "yesterday"
        );
    }
    #[test]
    fn outline_tracks_current_heading() {
        let mut p = pager("# One\n\ntext\n\n## Two\n\ntext\n");
        p.top = p.headings[1].line.unwrap();
        p.open_outline();
        assert_eq!(p.outline.selected, Some(1));
    }
    #[test]
    fn outline_filter_rejects_zero_matches() {
        let mut p = pager("# Alpha\n# Beta\n");
        p.open_outline();
        let _ = p.handle_outline_key(Key::Char('z'));
        assert_eq!(p.outline.filter, "");
    }
    #[test]
    fn mouse_parser() {
        let Event::Mouse(mouse) = parse_mouse("<64;10;5", b'M').unwrap() else {
            panic!()
        };
        assert_eq!(mouse.wheel(), Some(-1));
        assert_eq!((mouse.col, mouse.row), (10, 5));
    }
    #[test]
    fn quote_prefixes_every_line() {
        assert_eq!(quote_markdown(b"one\ntwo\nthree"), b"> one\n> two\n> three");
    }
    #[test]
    fn selection_copies_link_markdown() {
        let mut p = pager("[alpha](https://example.com)\n");
        p.selection = Selection {
            active: true,
            dragged: true,
            anchor: Cell { line: 0, col: 2 },
            current: Cell { line: 0, col: 4 },
            ..Default::default()
        };
        assert_eq!(
            String::from_utf8(p.selection_markdown()).unwrap(),
            "[alpha](https://example.com)"
        );
    }
    #[test]
    fn selection_copies_list_markdown_and_newlines() {
        let mut p = pager("- alpha\n- beta\n");
        p.selection = Selection {
            active: true,
            anchor: Cell { line: 0, col: 1 },
            current: Cell { line: 1, col: 9 },
            ..Default::default()
        };
        assert_eq!(
            String::from_utf8(p.selection_markdown()).unwrap(),
            "- alpha\n- beta"
        );
    }
    #[test]
    fn search_state_uses_nearest_match() {
        let mut p = pager("");
        p.plain = vec!["alpha".into(), "beta alpha".into(), "gamma".into()];
        p.query = "alpha".into();
        p.refresh_search(1);
        assert_eq!(p.matches, vec![0, 1]);
        assert_eq!(p.match_index, Some(1));
    }
    #[test]
    fn question_mark_toggles_help() {
        let mut p = pager("# Title\n");
        p.open_outline();
        assert!(!p.handle_key(Key::Char('?')));
        assert!(p.help);
        assert!(p.outline.filter.is_empty());
        p.handle_key(Key::Char('?'));
        assert!(!p.help);
    }
    #[test]
    fn code_block_line_has_copy_icon() {
        let p = pager("```\none\ntwo\n```\n");
        assert!(p.render_line(0).contains('⎘'));
    }
    #[test]
    fn status_preserves_innermost_heading() {
        let mut p = pager("# Very long outer heading\n\n## Inner\n");
        p.top = p.headings[1].line.unwrap();
        let status = p.section_fitted(14);
        assert!(strip_ansi(&status).contains("Inner"));
    }
    #[test]
    fn heading_breadcrumbs() {
        let mut p = pager("# One\n\n## Two\n\n### Three\n");
        p.top = p.headings[2].line.unwrap();
        assert_eq!(
            p.heading_path()
                .iter()
                .map(|h| h.text.as_str())
                .collect::<Vec<_>>(),
            vec!["One", "Two", "Three"]
        );
    }
    #[test]
    fn width_counts_wide_glyphs() {
        assert_eq!(visible_width("🦋"), 2);
        assert_eq!(fit_width("ab🦋cd", 4), "a...");
    }
    #[test]
    fn tinted_block_reapplies_after_reset() {
        let result = tinted(&format!("a{RESET}b"), "\x1b[48;5;2m", 4);
        assert!(result.contains(&format!("{RESET}\x1b[48;5;2m")));
        assert_eq!(visible_width(&result), 4);
    }
}
