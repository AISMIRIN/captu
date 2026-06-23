# aribcaption

Safe Rust wrappers for [libaribcaption](https://github.com/xqq/libaribcaption) — a portable ARIB STD-B24 caption decoder/renderer.

Provides `Context`, `Decoder`, `Renderer`, `AribCaption`, and `RenderedImage` as ergonomic, RAII-safe types over the raw FFI bindings in [`aribcaption-sys`](../aribcaption-sys).

## Example

```rust
let ctx = aribcaption::Context::new().expect("context");
let mut decoder = aribcaption::Decoder::new(&ctx).expect("decoder");
// Preserve full-width ー (U+30FC) in MSZ mode (library default replaces it).
decoder.set_replace_msz_fullwidth_japanese(false);

// Feed PES packets in presentation order.
if let Some(cap) = decoder.decode(&pes_payload, pts_ms) {
    println!("{}", cap.text());
}
```

## License

MIT — see the workspace root `LICENSE`.
