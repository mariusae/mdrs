# mdrs

Rust library for rendering GitHub-flavored Markdown as ANSI terminal text.
Its behavior is compatible with the Go package at `github.com/mariusae/md`.

```rust
let rendered = mdrs::render(b"# Hello\n", 80, false)?;
assert_eq!(rendered, "\x1b[1mHello\x1b[0m\n\n");
# Ok::<(), mdrs::RenderError>(())
```

The crate also exposes heading extraction, structured render results (including
rendered heading and code-block locations), optional OSC-8 links, custom tint
styles, writer-oriented rendering, and a library pager entry point.

```rust,no_run
mdrs::run_pager(&mdrs::PagerConfig {
    paths: vec!["README.md".into()],
    ..Default::default()
})?;
# Ok::<(), std::io::Error>(())
```

The pager includes the Go implementation's navigation, flow layout, search,
filtered heading outline, help overlay, live file reload and change flashes,
mouse scrolling and selection, source-preserving Markdown copy, code-block
copy buttons, OSC-52 clipboard support, status breadcrumbs, terminal tinting,
focus handling, and resize handling.

The included CLI is intentionally thin and delegates display behavior to the
library pager:

```sh
cargo run -- README.md
```

It also accepts multiple files, stdin, and an optional fixed width:

```sh
cargo run -- --width 100 README.md CHANGELOG.md
printf '# Hello\n' | cargo run
```
