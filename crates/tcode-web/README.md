# tcode web

This is the read-only browser shell for the tcode sync protocol. It uses
`gpui_platform::application()`, which selects GPUI's multithreaded WebPlatform
for wasm.

## Toolchain and serving requirements

The pinned GPUI revision needs nightly, a locally rebuilt standard library, and
the wasm atomics, bulk-memory, and mutable-globals target features:

```sh
CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUSTFLAGS="-C target-feature=+atomics,+bulk-memory,+mutable-globals" \
RUSTC_BOOTSTRAP=1 \
cargo +nightly -Zbuild-std=std,panic_abort check \
  -p tcode-web --target wasm32-unknown-unknown
```

Linking an actual browser module additionally needs shared memory and its
thread-local-storage exports. Trunk does not forward Cargo's `-Z` option, so
the corresponding unstable setting is supplied through Cargo's environment:

```sh
CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUSTFLAGS="-C target-feature=+atomics,+bulk-memory,+mutable-globals -C link-arg=--shared-memory -C link-arg=--max-memory=1073741824 -C link-arg=--import-memory -C link-arg=--export=__heap_base -C link-arg=--export=__wasm_init_tls -C link-arg=--export=__tls_size -C link-arg=--export=__tls_align -C link-arg=--export=__tls_base" \
CARGO_UNSTABLE_BUILD_STD=std,panic_abort \
RUSTC_BOOTSTRAP=1 \
RUSTUP_TOOLCHAIN=nightly \
trunk build --release --locked
```

WebAssembly threads require `SharedArrayBuffer`. Browsers expose it only when
the page is cross-origin isolated, so `Trunk.toml` makes `trunk serve` send:

```text
Cross-Origin-Embedder-Policy: require-corp
Cross-Origin-Opener-Policy: same-origin
```

Production hosting must send the same headers. Without them the dispatcher
cannot start its worker threads.

Serve with the same environment and `trunk serve --release --locked`, then
supply the host endpoint and token in the page URL:

```text
http://127.0.0.1:8080/?url=ws%3A%2F%2F127.0.0.1%3APORT%2Fsync&token=TOKEN
```

The token remains in the browser URL/history in this minimal proof. A useful
client should replace this bootstrap mechanism with pairing and credential
storage before deployment.

## Upstream contribution candidate

At Zed revision `1a246efd`, `crates/gpui_web/src/dispatcher.rs` defines
`shared_memory_supported()` only under `#[cfg(feature = "multithreaded")]` but
calls it unconditionally from `MainThreadMailbox::run_waker_loop`.
Consequently, `gpui_web` does not compile with `default-features = false`:

```text
error[E0425]: cannot find function `shared_memory_supported` in this scope
```

That prevents `gpui_platform::single_threaded_web()` from being built without
also unifying in the default multithreaded feature. The fix belongs upstream;
tcode intentionally carries no GPUI fork or local patch.
