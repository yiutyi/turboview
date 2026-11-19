# Changelog

## 0.2.0

- Consolidate the previously merged viewer UX, large-directory-tree memory, remote-navigation, WebP fallback, PNG decode-cap and centered-fit improvements from the stable main line.
- Add a bounded priority-aware pending decode queue and deduplication/promotion semantics.
- Add task identity tracking and cooperative cancellation tokens to prevent stale task completions from publishing results or clearing newer task state.
- Keep JPEG SIMD decoding, preview loading, viewport-sized decoding and ROI detail loading on the stable production path.
- Keep WIC native-scale decoding opt-in and experimental pending format, EXIF, alpha, color-management and representative performance validation.
- Produce a Windows x64 portable ZIP from the exact workflow commit.

### Known limitations

- Third-party decoder calls already executing cannot be interrupted; cancellation is checked before and after decode and before retry/publication.
- WIC is compiled in CI but is not enabled as the production decoder.
- GPU resource accounting and representative low-end Windows / SMB-NAS profiling remain acceptance work; no unsupported memory or speed claim is made.
- PathStore / PrefixPathStore experiments are excluded from this release.
