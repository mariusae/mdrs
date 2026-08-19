use std::fs::{File, OpenOptions};
use std::io::{self, IsTerminal, Read, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::time::{Duration, Instant};

/// Background escape sequences used for tinted Markdown elements.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RenderStyle {
    pub blockquote_bg: String,
    pub code_block_bg: String,
    pub highlight_bg: String,
}

/// Detects terminal-derived rendering tints.
///
/// A terminal background query is intentionally not attempted when the process
/// does not own an interactive terminal, matching the Go package's behavior in
/// normal library and redirected-output use.
pub fn detect_render_style() -> io::Result<RenderStyle> {
    let mut tty = match OpenOptions::new().read(true).write(true).open("/dev/tty") {
        Ok(tty) => tty,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(RenderStyle::default()),
        Err(error) => return Err(error),
    };
    if !tty.is_terminal() {
        return Ok(RenderStyle::default());
    }

    let _raw_mode = RawMode::enable(&tty)?;
    tty.write_all(b"\x1b]11;?\x1b\\")?;
    tty.flush()?;

    let deadline = Instant::now() + Duration::from_secs(2);
    let mut response = Vec::new();
    while Instant::now() < deadline {
        let timeout = deadline.saturating_duration_since(Instant::now());
        if !wait_for_input(&tty, timeout)? {
            break;
        }
        let mut chunk = [0_u8; 256];
        let count = tty.read(&mut chunk)?;
        response.extend_from_slice(&chunk[..count]);
        if let Some(background) = extract_osc11_color(&response) {
            return Ok(derive_render_style(background));
        }
    }
    Ok(RenderStyle::default())
}

#[derive(Clone, Copy)]
struct Rgb {
    r: u8,
    g: u8,
    b: u8,
}

struct RawMode {
    fd: RawFd,
    original: libc::termios,
}

impl RawMode {
    fn enable(file: &File) -> io::Result<Self> {
        let fd = file.as_raw_fd();
        // SAFETY: `termios` is a plain C data structure and `fd` remains open
        // for the lifetime of the returned guard.
        let mut original = unsafe { std::mem::zeroed::<libc::termios>() };
        // SAFETY: `original` points to writable storage and `fd` is a TTY.
        if unsafe { libc::tcgetattr(fd, &mut original) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let mut raw = original;
        // SAFETY: `raw` is an initialized termios structure.
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
        // SAFETY: the guard retains the original settings for the still-open
        // file descriptor. There is nothing actionable if restoration fails.
        unsafe { libc::tcsetattr(self.fd, libc::TCSANOW, &self.original) };
    }
}

fn wait_for_input(file: &File, timeout: Duration) -> io::Result<bool> {
    let mut descriptor = libc::pollfd {
        fd: file.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let millis = timeout.as_millis().min(i32::MAX as u128) as i32;
    // SAFETY: `descriptor` is valid for one element for the duration of poll.
    let result = unsafe { libc::poll(&mut descriptor, 1, millis) };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(result > 0 && descriptor.revents & libc::POLLIN != 0)
}

fn extract_osc11_color(data: &[u8]) -> Option<Rgb> {
    for start in 0..data.len().saturating_sub(3) {
        if data.get(start..start + 2) != Some(b"\x1b]") {
            continue;
        }
        let payload = &data[start + 2..];
        for end in 0..payload.len() {
            if payload[end] == 0x07 {
                return parse_osc11_payload(&payload[..end]);
            }
            if payload[end] == 0x1b && payload.get(end + 1) == Some(&b'\\') {
                return parse_osc11_payload(&payload[..end]);
            }
        }
    }
    None
}

fn parse_osc11_payload(payload: &[u8]) -> Option<Rgb> {
    let text = std::str::from_utf8(payload).ok()?.strip_prefix("11;")?;
    parse_osc_color(text)
}

fn parse_osc_color(value: &str) -> Option<Rgb> {
    let components = value
        .strip_prefix("rgb:")
        .or_else(|| value.strip_prefix("rgba:"))?
        .split('/')
        .collect::<Vec<_>>();
    if components.len() != 3 && components.len() != 4 {
        return None;
    }
    if components.len() == 4 {
        parse_color_component(components[3])?;
    }
    Some(Rgb {
        r: parse_color_component(components[0])?,
        g: parse_color_component(components[1])?,
        b: parse_color_component(components[2])?,
    })
}

fn parse_color_component(value: &str) -> Option<u8> {
    match value.len() {
        2 => u8::from_str_radix(value, 16).ok(),
        4 => u16::from_str_radix(value, 16)
            .ok()
            .map(|component| ((u32::from(component) + 128) / 257) as u8),
        _ => None,
    }
}

fn derive_render_style(background: Rgb) -> RenderStyle {
    RenderStyle {
        blockquote_bg: tinted_background(background, 0.16),
        code_block_bg: tinted_background(background, subtle_tint_alpha(background)),
        highlight_bg: tinted_background(background, prompt_tint_alpha(background)),
    }
}

fn subtle_tint_alpha(background: Rgb) -> f64 {
    if is_light(background) { 0.04 } else { 0.12 }
}

fn prompt_tint_alpha(background: Rgb) -> f64 {
    if is_light(background) { 0.10 } else { 0.20 }
}

fn is_light(color: Rgb) -> bool {
    0.299 * f64::from(color.r) + 0.587 * f64::from(color.g) + 0.114 * f64::from(color.b) > 128.0
}

fn tinted_background(background: Rgb, alpha: f64) -> String {
    let overlay = if is_light(background) {
        Rgb { r: 0, g: 0, b: 0 }
    } else {
        Rgb {
            r: 255,
            g: 255,
            b: 255,
        }
    };
    let blended = blend(background, overlay, alpha);
    let colorterm = std::env::var("COLORTERM")
        .unwrap_or_default()
        .to_lowercase();
    if colorterm.contains("truecolor") || colorterm.contains("24bit") {
        return format!("\x1b[48;2;{};{};{}m", blended.r, blended.g, blended.b);
    }
    if std::env::var("TERM")
        .unwrap_or_default()
        .to_lowercase()
        .contains("256color")
    {
        return format!("\x1b[48;5;{}m", nearest_ansi256(blended));
    }
    String::new()
}

fn blend(background: Rgb, overlay: Rgb, alpha: f64) -> Rgb {
    let channel = |base: u8, top: u8| {
        (f64::from(top) * alpha + f64::from(base) * (1.0 - alpha)).floor() as u8
    };
    Rgb {
        r: channel(background.r, overlay.r),
        g: channel(background.g, overlay.g),
        b: channel(background.b, overlay.b),
    }
}

fn nearest_ansi256(color: Rgb) -> usize {
    ansi256_palette()
        .into_iter()
        .enumerate()
        .min_by(|(_, left), (_, right)| {
            color_distance(color, *left).total_cmp(&color_distance(color, *right))
        })
        .map_or(0, |(index, _)| index)
}

fn color_distance(left: Rgb, right: Rgb) -> f64 {
    let r = f64::from(left.r) - f64::from(right.r);
    let g = f64::from(left.g) - f64::from(right.g);
    let b = f64::from(left.b) - f64::from(right.b);
    0.299 * r * r + 0.587 * g * g + 0.114 * b * b
}

fn ansi256_palette() -> Vec<Rgb> {
    let mut palette = vec![Rgb { r: 0, g: 0, b: 0 }; 256];
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
    for (index, (r, g, b)) in base.into_iter().enumerate() {
        palette[index] = Rgb { r, g, b };
    }
    let steps = [0, 95, 135, 175, 215, 255];
    let mut index = 16;
    for r in steps {
        for g in steps {
            for b in steps {
                palette[index] = Rgb { r, g, b };
                index += 1;
            }
        }
    }
    for index in 0..24 {
        let value = 8 + index as u8 * 10;
        palette[232 + index] = Rgb {
            r: value,
            g: value,
            b: value,
        };
    }
    palette
}
