# TurboView 0.2.0

TurboView is a Windows x64 image viewer built with Rust, GPUI and libjpeg-turbo.

## Portable use

1. Extract the ZIP to a writable folder.
2. Run `turboview.exe`.
3. You do not need Rust or Cargo installed.

The package includes the TurboView application documentation and the applicable libjpeg-turbo license notices. This software is based in part on the work of the Independent JPEG Group.

## Included capabilities

- Fast JPEG decoding through the libjpeg-turbo SIMD path.
- PNG, BMP, GIF, WebP, TIFF, TGA, PNM and ICO image support through the image decoding stack.
- ZIP, 7z and RAR archive browsing.
- Directory-tree navigation, lazy directory scanning and large-library path sharing.
- Adjacent-image prefetch, bounded pending work, decode-result generation checks and cooperative task cancellation.
- Preview, viewport-sized decode and ROI detail loading.
- Fullscreen, system-theme initialization, image dimensions and Windows application icon.

## Important implementation boundaries

- The Windows Imaging Component (WIC) decoder is experimental and is not enabled in the production image-loading path.
- Cancellation is cooperative at task boundaries. Third-party decoder calls already in progress cannot currently be interrupted.
- Performance and memory use vary with image formats, storage devices, network shares and available GPU memory.

## Keyboard shortcuts

- Left / Right: previous / next image
- Space: next image
- B: show / hide the directory sidebar
- F: toggle fast JPEG mode
- Shift+F: toggle fullscreen
- 0: fit image to viewport
- 1: original-size view

## Third-party notices

This software is based in part on the work of the Independent JPEG Group. See `LIBJPEG-TURBO-LICENSE.md` and `README.ijg` for the applicable notices and license terms.
