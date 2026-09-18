# `block` 0.1.6 compatibility patch

This is the MIT-licensed `block` 0.1.6 crate by Steven Sheldon from crates.io
(SHA-256 `0d8c1fef690941d3e7788d328517591fecc684c084084702d6ff1641e993699a`).
Its public API is required by the current GPUI, Cocoa and Metal dependencies.
The workspace patch selects this copy without changing those APIs.

Upstream declares `_NSConcreteStackBlock` with a private empty enum type.
That type is uninhabited: accessing the extern static is invalid, and Rust's
`uninhabited_static` lint will become a hard error. The patch replaces it with
an inhabited, opaque `repr(C)` struct. Only its address is taken; no class data
is read, written or allocated by Rust. `BlockBase::isa` remains one pointer, as
required by the [Apple Blocks ABI](https://clang.llvm.org/docs/Block-ABI-Apple.html).
Explicit `extern "C"` spelling preserves the previous default ABI and fixes
the compiler's `missing_abi` warnings. Both manifests explicitly retain edition
2015. No warning is suppressed.

The published crate omits the `test_utils` path dependency needed to run its
existing interoperability tests. Those four files are restored from upstream
commit [`642ea4a4a5853a21b55b05c34832a5f1bb1af61c`](https://github.com/SSheldon/rust-block/tree/642ea4a4a5853a21b55b05c34832a5f1bb1af61c/test_utils).
The helper uses the maintained `cc` crate in place of its predecessor `gcc`;
the C implementation and the six original runtime tests are unchanged.

The vendored crate is excluded from the application workspace. On macOS, run
its native C/Rust invocation and block-copy tests, plus its documentation tests:

```sh
RUSTFLAGS='-D warnings' cargo test --manifest-path vendor/block/Cargo.toml --locked
```

Remove this patch when the consuming dependencies migrate to a maintained
Blocks implementation or an upstream compatible release fixes the declaration.
