# Windows compatibility policy

TurboView should minimize direct Windows API usage. Portable application logic
(file discovery, path handling, sorting, caching, image metadata and decoding)
belongs in Rust or cross-platform crates. Windows-specific calls should remain
inside a small platform adapter and must have a documented reason.

## Compatibility targets

- **Windows 10 version 1511 (build 10586):** immediate minimum compatibility
  target. A missing optional API must never prevent process startup.
- **Windows 7 SP1 x64:** aspirational target. Do not claim support until the
  renderer, Rust runtime and complete executable import set have been audited
  and the app has been launched on a real Windows 7 system.
- Newer Windows APIs must be resolved dynamically when a fallback is possible.
  The fallback must preserve safe behavior even if advanced DPI or power
  management behavior is reduced.

## Current known legacy blockers

The current Windows portable executable still imports several APIs introduced
after Windows 7. The import table inspection has identified:

- `DCompositionCreateDevice` (DirectComposition; Windows 8-era API)
- `GetDpiForMonitor` (Shcore; Windows 8.1-era API)
- `CreateDXGIFactory2` (DXGI 1.3; Windows 8.1-era API)
- `WaitOnAddress`, `WakeByAddressAll`, `WakeByAddressSingle`
  (Windows 8-era synchronization APIs)
- `GetSystemTimePreciseAsFileTime` and `GetCurrentPackageFullName`
  (Windows 8-era APIs)
- `RegisterSuspendResumeNotification` and
  `UnregisterSuspendResumeNotification` (newer power-notification APIs)

This list is an import-table observation, not a complete compatibility audit.
Some imports may originate in Rust's standard library or third-party crates,
not application code. Identify their owning object/crate before changing them.

## Development rules

1. Prefer Rust standard library and cross-platform dependencies for portable
   logic. Avoid adding Win32 calls to core image, indexing or storage modules.
2. Keep unavoidable Windows calls in platform-specific modules.
3. Do not statically import APIs newer than the minimum OS target when an
   optional runtime lookup and fallback can be used.
4. Treat renderer dependencies separately: removing a newer API import is not
   sufficient if the required renderer itself is unavailable on an older OS.
5. CI must inspect the final packaged EXE import table. Windows 10 1511
   forbidden imports fail the build; Windows 7 blockers are reported until a
   dedicated Windows 7-compatible renderer/runtime plan is implemented.
6. Never claim OS compatibility from a successful modern Windows CI build
   alone. Require a real launch test on the oldest supported OS.

## Current status

The two known Windows 10 1607 DPI imports,
`GetDpiForWindow` and `GetSystemMetricsForDpi`, have been changed to runtime
resolution with fallback behavior. CI now rejects those imports in the final
EXE. Windows 10 1511 compatibility is not yet verified on the actual OS.
Windows 7 is not currently supported or verified.
