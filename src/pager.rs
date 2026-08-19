use std::fs::{self, File, OpenOptions};
use std::io::{self, IsTerminal, Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::path::PathBuf;

use unicode_width::UnicodeWidthStr;

use crate::{RESET, REVERSE, detect_render_style, render_document_with_style};

const ENTER_ALT_SCREEN: &str = "\x1b[?1049h";
const EXIT_ALT_SCREEN: &str = "\x1b[?1049l";
const HIDE_CURSOR: &str = "\x1b[?25l";
const SHOW_CURSOR: &str = "\x1b[?25h";
const CLEAR_SCREEN: &str = "\x1b[2J";
const CURSOR_HOME: &str = "\x1b[H";

/// Input and layout settings for the built-in pager.
#[derive(Clone, Debug, Default)]
pub struct PagerConfig {
    pub paths: Vec<PathBuf>,
    pub initial_source: Vec<u8>,
    pub label: String,
    pub width: usize,
}

/// Opens a full-screen terminal pager for a rendered Markdown document.
pub fn run_pager(config: &PagerConfig) -> io::Result<()> {
    let mut tty = OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .map_err(|error| io::Error::new(error.kind(), format!("opening tty: {error}")))?;
    if !tty.is_terminal() {
        return Err(io::Error::other("pager requires a terminal"));
    }

    let style = detect_render_style()?;
    let _raw = RawMode::enable(&tty)?;
    let mut screen = ScreenGuard::enter(&mut tty)?;
    let mut source = load_source(config)?;
    let mut top_line = 0_usize;
    let mut flow = false;

    loop {
        let (terminal_width, terminal_height) = terminal_size(screen.file())?;
        let render_width = if config.width > 0 {
            config.width
        } else if flow {
            terminal_width.min(100)
        } else {
            terminal_width
        };
        let result = render_document_with_style(&source, render_width, true, &style)
            .map_err(io::Error::other)?;
        let lines = result.output.split('\n').collect::<Vec<_>>();
        let view_height = terminal_height.saturating_sub(1).max(1);
        let max_top = lines.len().saturating_sub(view_height);
        top_line = top_line.min(max_top);
        draw(
            screen.file(),
            &lines,
            top_line,
            terminal_width,
            view_height,
            config,
            flow,
        )?;

        let mut input = [0_u8; 16];
        let count = screen.file().read(&mut input)?;
        if count == 0 {
            return Ok(());
        }
        match parse_key(&input[..count]) {
            Key::Quit => return Ok(()),
            Key::Down => top_line = (top_line + 1).min(max_top),
            Key::Up => top_line = top_line.saturating_sub(1),
            Key::HalfDown => top_line = (top_line + view_height / 2).min(max_top),
            Key::HalfUp => top_line = top_line.saturating_sub(view_height / 2),
            Key::PageDown => top_line = (top_line + view_height).min(max_top),
            Key::PageUp => top_line = top_line.saturating_sub(view_height),
            Key::Home => top_line = 0,
            Key::End => top_line = max_top,
            Key::Flow => flow = !flow,
            Key::Reload => source = load_source(config)?,
            Key::Ignore => {}
        }
    }
}

fn load_source(config: &PagerConfig) -> io::Result<Vec<u8>> {
    if config.paths.is_empty() {
        return Ok(config.initial_source.clone());
    }
    let mut source = Vec::new();
    for path in &config.paths {
        source.extend(fs::read(path).map_err(|error| {
            io::Error::new(error.kind(), format!("reading {}: {error}", path.display()))
        })?);
    }
    Ok(source)
}

fn draw(
    tty: &mut File,
    lines: &[&str],
    top_line: usize,
    width: usize,
    height: usize,
    config: &PagerConfig,
    flow: bool,
) -> io::Result<()> {
    write!(tty, "{CURSOR_HOME}")?;
    let content_width = if flow { width.min(100) } else { width };
    let left = if flow && width > content_width {
        (width - content_width) / 2
    } else {
        0
    };
    for row in 0..height {
        write!(tty, "\x1b[2K")?;
        if let Some(line) = lines.get(top_line + row) {
            write!(tty, "{}{line}", " ".repeat(left))?;
        }
        write!(tty, "{RESET}\r\n")?;
    }
    let label = if config.label.is_empty() {
        config
            .paths
            .iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(" ")
    } else {
        config.label.clone()
    };
    let mut left_status = label;
    if flow {
        left_status.push_str("  flow");
    }
    let right_status = format!("{}%", scroll_percent(top_line, lines.len(), height));
    let available = width.saturating_sub(UnicodeWidthStr::width(right_status.as_str()) + 1);
    left_status = truncate_visible(&left_status, available);
    let padding = width.saturating_sub(
        UnicodeWidthStr::width(left_status.as_str())
            + UnicodeWidthStr::width(right_status.as_str()),
    );
    write!(
        tty,
        "\x1b[2K{REVERSE}{left_status}{}{right_status}{RESET}",
        " ".repeat(padding)
    )?;
    tty.flush()
}

fn scroll_percent(top: usize, line_count: usize, height: usize) -> usize {
    let max_top = line_count.saturating_sub(height);
    (top * 100).checked_div(max_top).unwrap_or(100)
}

fn truncate_visible(text: &str, width: usize) -> String {
    let mut result = String::new();
    let mut used = 0;
    for character in text.chars() {
        let character_width = unicode_width::UnicodeWidthChar::width(character).unwrap_or(0);
        if used + character_width > width {
            break;
        }
        result.push(character);
        used += character_width;
    }
    result
}

enum Key {
    Quit,
    Down,
    Up,
    HalfDown,
    HalfUp,
    PageDown,
    PageUp,
    Home,
    End,
    Flow,
    Reload,
    Ignore,
}

fn parse_key(input: &[u8]) -> Key {
    match input {
        b"q" | b"\x03" => Key::Quit,
        b"j" | b"\x0e" | b"\r" | b"\n" | b"\x1b[B" => Key::Down,
        b"k" | b"\x10" | b"\x1b[A" => Key::Up,
        b"d" => Key::HalfDown,
        b"u" => Key::HalfUp,
        b" " | b"\x16" | b"\x1b[6~" => Key::PageDown,
        b"b" | b"\x1b[5~" => Key::PageUp,
        b"g" | b"\x1b[H" | b"\x1b[1~" => Key::Home,
        b"G" | b"\x1b[F" | b"\x1b[4~" => Key::End,
        b"f" => Key::Flow,
        b"r" | b"\x0c" => Key::Reload,
        _ => Key::Ignore,
    }
}

fn terminal_size(file: &File) -> io::Result<(usize, usize)> {
    // SAFETY: `winsize` is a plain C data structure and `file` is an open TTY.
    let mut size = unsafe { std::mem::zeroed::<libc::winsize>() };
    // SAFETY: `size` points to writable storage for `TIOCGWINSZ`.
    if unsafe { libc::ioctl(file.as_raw_fd(), libc::TIOCGWINSZ, &mut size) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok((usize::from(size.ws_col), usize::from(size.ws_row)))
}

struct RawMode {
    fd: RawFd,
    original: libc::termios,
}

impl RawMode {
    fn enable(file: &File) -> io::Result<Self> {
        let fd = file.as_raw_fd();
        // SAFETY: `termios` is plain data and the file descriptor stays open.
        let mut original = unsafe { std::mem::zeroed::<libc::termios>() };
        // SAFETY: `original` is writable and `fd` is a TTY.
        if unsafe { libc::tcgetattr(fd, &mut original) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let mut raw = original;
        // SAFETY: `raw` is initialized termios data.
        unsafe { libc::cfmakeraw(&mut raw) };
        // SAFETY: `raw` is valid and `fd` remains open.
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { fd, original })
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        // SAFETY: the original settings belong to this still-open descriptor.
        unsafe { libc::tcsetattr(self.fd, libc::TCSANOW, &self.original) };
    }
}

struct ScreenGuard<'a> {
    tty: &'a mut File,
}

impl<'a> ScreenGuard<'a> {
    fn enter(tty: &'a mut File) -> io::Result<Self> {
        write!(
            tty,
            "{ENTER_ALT_SCREEN}{CLEAR_SCREEN}{CURSOR_HOME}{HIDE_CURSOR}"
        )?;
        tty.flush()?;
        Ok(Self { tty })
    }

    fn file(&mut self) -> &mut File {
        self.tty
    }
}

impl Drop for ScreenGuard<'_> {
    fn drop(&mut self) {
        let _ = write!(self.tty, "{RESET}{SHOW_CURSOR}{EXIT_ALT_SCREEN}");
        let _ = self.tty.flush();
    }
}
