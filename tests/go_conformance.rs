use std::io::Write;
use std::process::{Command, Stdio};

use pretty_assertions::assert_eq;

const DEFAULT_GO_REFERENCE: &str = "/home/meriksen/src/tries/2026-05-11-mariusae-md";

fn go_reference() -> std::path::PathBuf {
    std::env::var_os("MDRS_GO_REFERENCE")
        .map(Into::into)
        .unwrap_or_else(|| DEFAULT_GO_REFERENCE.into())
}

fn go_render(markdown: &str, width: usize) -> Option<String> {
    let reference = go_reference();
    if !reference.join("go.mod").exists() {
        return None;
    }
    let mut child = Command::new("go")
        .args(["run", "./cmd/md", "-w", &width.to_string()])
        .current_dir(reference)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("start Go conformance renderer");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(markdown.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success());
    Some(String::from_utf8(output.stdout).unwrap())
}

#[test]
fn matches_go_renderer() {
    let cases = [
        "# Hello *world*\n",
        "**bold** and *italic* and `code`\n",
        "one two three four five six seven eight\n",
        "- one\n- two\n  - nested\n",
        "1. first\n2. second\n",
        "- [ ] todo\n- [x] done\n",
        "> quote\n>\n> second paragraph\n",
        "[click](https://example.com) and <https://example.org>\n",
        "![butterfly](image.png)\n",
        "~~deleted~~ and ==marked==\n",
        "```go\nfoo()\nbar()\n```\n",
        "| Name | Age |\n| :--- | ---: |\n| Alice | 30 |\n",
        "before  \nafter\n",
        "<span>raw</span>\n",
        "---\ntitle: **Raw**\n---\n# Body\n",
        "Monarch 🦋\n",
    ];
    for width in [9, 20, 80] {
        for markdown in cases {
            let Some(go) = go_render(markdown, width) else {
                return;
            };
            let rust = mdrs::render(markdown.as_bytes(), width, false).unwrap();
            assert_eq!(rust, go, "markdown={markdown:?}, width={width}");
        }
    }
}

#[test]
fn matches_go_renderer_for_reference_document() {
    let path = go_reference().join("test.md");
    let Ok(markdown) = std::fs::read_to_string(path) else {
        return;
    };
    for width in [40, 80, 120] {
        let go = go_render(&markdown, width).unwrap();
        let rust = mdrs::render(markdown.as_bytes(), width, false).unwrap();
        assert_eq!(rust, go, "reference document at width={width}");
    }
}
