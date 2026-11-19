# Compact path storage prototype

Status: isolated prototype branch; not wired into the viewer.

## Why this exists

PR #8 reduced the global navigation slot to a shared `Arc<Path>`, but each image
still owns one allocation containing its complete absolute path. For a large
library, sibling images repeat the same directory prefix many times.

The next candidate representation is:

- a directory table: one shared path allocation per unique parent directory;
- a contiguous filename-byte arena using `OsStr::as_encoded_bytes()` (on Windows this preserves the platform's lossless encoded form, including unpaired UTF-16 units);
- a compact image record containing a directory id and filename-byte range;
- precomputed component-aware sort keys for exact `Path::cmp` ordering without constructing `PathBuf` values in the comparator;
- conversion to `PathBuf` only at filesystem/decode boundaries.

The prototype must preserve Windows non-UTF-8 paths and the exact ordering
semantics of `PathBuf::cmp`. It must also handle archive virtual paths, which
are not ordinary filesystem children, without silently changing their order.

## Important correctness constraints

1. Do not compare concatenated filename strings as a substitute for
   `PathBuf::cmp`: component ordering differs for cases such as
   `C:\\foo\\bar.jpg` and `C:\\foo.txt`. The current implementation encodes
   path components with tags and terminators, then compares the virtual
   concatenation of directory and filename keys.
2. Preserve arbitrary Windows names; never round-trip through
   `to_string_lossy()`. The current implementation stores `OsStr` encoded bytes
   and reconstructs with `from_encoded_bytes_unchecked` only for bytes it encoded.
3. Treat archive virtual paths as a separate record kind until their ordering
   and refresh semantics are explicitly tested.
4. Keep the global list sorted so `partition_point` and binary search remain
   valid.
5. Measure allocations, resident memory, sort time, and navigation latency.
   A smaller record is not a win if comparisons reconstruct paths on every
   comparison.

## Acceptance tests before integration

- Exact round-trip for Unicode and non-Unicode Windows filenames.
- Exact ordering parity with `PathBuf::cmp`, including prefix edge cases.
- Duplicate directory interning and stable ids.
- 1M and 10M synthetic sibling-file datasets.
- Mixed directories, refresh insertion/removal, and archive virtual paths.
- Compare memory against `Vec<Arc<Path>>` and `Vec<PathBuf>`.
- Benchmark sort, binary search, next/previous navigation, and path materialization.

No main-viewer integration should happen until the prototype demonstrates a
material memory reduction without a meaningful navigation regression.


## Repeatable benchmark

Run from `turboview/` on the target Windows machine (release mode):

```powershell
cargo run --release --bin pathbench -- 100000
cargo run --release --bin pathbench -- 1000000
```

The benchmark reports construction, sorting, sampled indexed access, forward and
reverse sequential traversal, path materialization, and an approximate owned-heap
estimate for `Vec<PathBuf>`, `Vec<Arc<Path>`, and `PathStore + PathId`. Run all three on the same local SSD/library and
repeat at least three times. The synthetic dataset uses 4,096 shared directories;
it is intended for representation comparisons, not to predict every real folder
distribution. Record the Windows build, CPU model, RAM, storage type, and median
of the runs alongside the output.

The implementation currently appends encoded filename bytes directly into the
arena. `PathStore::cmp` compares precomputed component-aware keys and does not
materialize `PathBuf` values during sorting. The Windows-only round-trip test
covers unpaired UTF-16 code units. `storage_bytes()` is an owned-heap estimate,
not RSS: allocator metadata, alignment, fragmentation, and exact hash-table
layout are not measured.


## Current limitations before integration

- `PathStore` is append-only. It has no per-record removal or filename-arena
  reclamation. A production integration must either build a fresh store for a
  completed scan and atomically swap generations, or implement and test a
  reclaiming strategy. Repeated refreshes must not retain deleted paths forever.
- `pathbench` now samples the sorted `ids[i]` array and measures forward and
  reverse sequential traversal of that sorted ID list. Its PathStore traversal
  metric measures ID access only; path materialization is measured separately.
  The Vec baselines traverse their sorted paths in both directions.
- The current synthetic dataset has 4,096 shared directories and regular short
  filenames. Add a second profile with deep paths and many unique directories.
- The current benchmark should be run at 100K, 1M, and 10M entries where machine
  memory permits, with repeated medians. Its estimates must not be presented as
  process RSS. The synthetic profile still uses 4,096 shared directories and
  does not yet model deep paths or high-cardinality directory layouts.

The current sort-key comparator is an optimization already present in the code;
the remaining acceptance work is broader path-distribution validation, large-scale
and real-library measurements, and refresh-generation lifetime testing. Keep the prototype
isolated from `ImageViewer` until those checks pass.
