# gpui-wgpu mobile snapshot

This crate is vendored from Zed's `gpui_wgpu`, as published in the Apache-2.0
licensed `gpui-pre-wgpu` 0.3.3 snapshot. It carries only the mobile integration
patches needed by tcode: renderer scale-factor plumbing, Android HAL-label
suppression, Metal selection on Apple mobile, Android font-family aliases and
fallback preservation, configurable platform font fallbacks, and host-assisted
color-emoji rasterization.
