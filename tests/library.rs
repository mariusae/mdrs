use pretty_assertions::assert_eq;

use mdrs::{BOLD, DIM, FG_BLUE, OSC8_END, RESET, RenderStyle, UNDERLINE};

#[test]
fn structured_result_tracks_headings_and_code_blocks() {
    let source = b"---\ntitle: Example\n---\n# Body\n\n```go\nfoo()\nbar()\n```\n";
    let result = mdrs::render_document(source, 12, false).unwrap();
    assert_eq!(result.headings.len(), 1);
    assert_eq!(result.headings[0].level, 1);
    assert_eq!(result.headings[0].text, "Body");
    assert_eq!(result.headings[0].line, Some(3));
    assert_eq!(result.code_blocks.len(), 1);
    assert_eq!(result.code_blocks[0].line, 5);
    assert_eq!(result.code_blocks[0].text, "foo()\nbar()\n");
}

#[test]
fn extracts_headings_without_front_matter() {
    let headings =
        mdrs::extract_headings(b"---\ntitle: '# no'\n---\n# One\n## Two `code`\n").unwrap();
    assert_eq!(headings.len(), 2);
    assert_eq!(headings[0].text, "One");
    assert_eq!(headings[0].line, None);
    assert_eq!(headings[1].text, "Two code");
}

#[test]
fn styled_blocks_match_go_layout() {
    let style = RenderStyle {
        blockquote_bg: "\x1b[48;5;238m".into(),
        code_block_bg: "\x1b[48;5;237m".into(),
        highlight_bg: "\x1b[48;5;250m".into(),
    };
    assert_eq!(
        mdrs::render_with_style(b"> hello\n", 12, false, &style).unwrap(),
        "\x1b[48;5;238m \x1b[0m hello\n\n"
    );
    assert_eq!(
        mdrs::render_with_style(b"```\nfoo\n```\n", 12, false, &style).unwrap(),
        "\x1b[48;5;237m    foo     \x1b[0m\n\n"
    );
    assert_eq!(
        mdrs::render_with_style(b"some ==important== text\n", 80, false, &style).unwrap(),
        "some \x1b[48;5;250mimportant\x1b[0m text\n\n"
    );
}

#[test]
fn osc8_links_hide_the_destination_suffix() {
    let output = mdrs::render(b"[click](https://example.com)\n", 80, true).unwrap();
    assert_eq!(
        output,
        format!(
            "{}{}{}click{}{}\n\n",
            mdrs::osc8_start("https://example.com"),
            UNDERLINE,
            FG_BLUE,
            RESET,
            OSC8_END,
        )
    );
}

#[test]
fn constants_match_the_go_package() {
    assert_eq!(BOLD, "\x1b[1m");
    assert_eq!(DIM, "\x1b[2m");
    assert_eq!(UNDERLINE, "\x1b[4m");
    assert_eq!(RESET, "\x1b[0m");
}
