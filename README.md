# mdrs

Rust library for rendering GitHub-flavored Markdown as ANSI terminal text.
Its behavior is compatible with the Go package at
`github.com/mariusae/md`; this crate intentionally does not provide a CLI.

```rust
let rendered = mdrs::render(b"# Hello\n", 80, false)?;
assert_eq!(rendered, "\x1b[1mHello\x1b[0m\n\n");
# Ok::<(), mdrs::RenderError>(())
```

The crate also exposes heading extraction, structured render results (including
rendered heading and code-block locations), optional OSC-8 links, custom tint
styles, writer-oriented rendering, and a library pager entry point.
