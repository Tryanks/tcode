`syntaxes.bin` is the compiled Sublime-syntax set from [bat v0.26.1](https://github.com/sharkdp/bat) (MIT/Apache-2.0), aggregating community syntax definitions documented in bat's own acknowledgements.

bat pre-tests its set against the Oniguruma regex backend, while this crate
compiles syntect with `regex-fancy`. The dump is therefore post-processed with
`examples/sanitize_syntaxes.rs`, which replaces the handful of patterns that
only Oniguruma can compile with a never-matching placeholder — syntect compiles
grammar regexes lazily and panics at runtime on the first use of a pattern the
active backend rejects. After swapping in a new upstream dump, rerun:

```
cargo run -p tcode-ui --example sanitize_syntaxes -- <upstream.bin> crates/ui/assets/syntaxes.bin
```
