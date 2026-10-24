# Changes to Smithay 0.7.0

This is Smithay 0.7.0 from crates.io with a few small additions wayland-time needs that
upstream doesn't have yet. Every change is marked with a `wayland-time:` comment.

- `src/lib.rs`: upstream warnings are silenced (`#![allow(warnings)]`), since a path
  dependency's warnings show up in our build.
- Tearing page flips (for `wp_tearing_control_v1`):
  - `DrmSurface::page_flip_with`, `AtomicDrmSurface::page_flip_with` and
    `LegacyDrmSurface::page_flip_with` take a `tearing` flag that adds
    `DRM_MODE_PAGE_FLIP_ASYNC`.
  - `DrmCompositor::set_tearing` / `last_flip_torn`: with tearing on, a frame is first
    flipped asynchronously; if the kernel refuses (anything besides the primary plane's
    buffer changed, e.g. the cursor moved) it falls back to a normal vblank-synced flip.
- `DrmOutputManager::set_color_formats`: pick buffer formats per output (10-bit for HDR).

To update Smithay: replace this directory with the new release and re-apply these changes,
or drop the ones upstream has gained.
