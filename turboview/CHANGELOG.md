# Changelog

## 0.2.1

- Restore immediate decoding for the current image instead of queuing it behind a global decode cap; page turns respond without waiting for a prefetch worker.
- Count only prefetch work against the prefetch concurrency limit, decoupled from the current image.
- Keep nearby prefetch entries on navigation rather than clearing the whole pending queue.
- Clear in-flight and active task state on root change and fast-mode toggle; cancellation alone left decode slots occupied.
- Replace serialized ROI decoding with latest-wins requests and a debounced generation check.
- Fix stale completions after a full state reset by adding a real registry clear.

## 0.2.0

- Remove the Windows GPUI jump-list parser's ICU DLL dependency by using the equivalent bounded UTF-16 terminator scan.
- Statically link the MSVC C/C++ runtime across Rust, UnRAR and libjpeg-turbo so the portable package does not require the Visual C++ Redistributable.
- Add a CI import-table audit that rejects ICU and VC++ runtime DLL dependencies in the packaged executable.
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
