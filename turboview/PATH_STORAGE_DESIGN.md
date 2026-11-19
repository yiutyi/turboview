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
cargo run --release --bin pathbench -- 100000 flat
cargo run --release --bin pathbench -- 100000 deep
cargo run --release --bin pathbench -- 1000000 flat
cargo run --release --bin pathbench -- 1000000 deep
```

The benchmark reports construction, sorting, sampled indexed access, forward and
reverse sequential ID traversal, sampled path materialization, full forward/reverse path materialization, and an approximate owned-heap
estimate for `Vec<PathBuf>`, `Vec<Arc<Path>`, and `PathStore + PathId`. Run all three on the same local SSD/library and
repeat at least three times. The `flat` profile uses 4,096 shallow shared directories. The `deep` profile
uses six nested directory components and up to one leaf directory per eight
entries. Both are deterministic representation comparisons, not substitutes for
a real library's path distribution. Record the Windows build, CPU model, RAM, storage type, and median
of the runs alongside the output.

The implementation currently appends encoded filename bytes directly into the
arena. `PathStore::cmp` compares precomputed component-aware keys and does not
materialize `PathBuf` values during sorting. The Windows-only round-trip test
covers unpaired UTF-16 code units. `storage_bytes()` is an owned-heap estimate,
not RSS: allocator metadata, alignment, fragmentation, and exact hash-table
layout are not measured.


## Generation lifecycle prototype

`path_generation.rs` adds an isolated lifecycle layer around v1:

- Each scan calls `PathIndexManager::begin_generation` and builds a fresh
  `PathStore` without mutating the active index.
- A completed scan publishes in O(1) by swapping an `Arc<Generation>` under a
  short `RwLock` write lock. Readers clone a `GenerationSnapshot` and do not
  hold the lock while browsing.
- `PathHandle` contains both a `GenerationId` and the dense `PathId`. A handle
  from an older scan is rejected by a newer snapshot instead of accidentally
  resolving to a different image at the same numeric index.
- The most recently started scan wins. A slower earlier scan is rejected if a
  newer scan has started; scan start and publication decisions are serialized
  by the manager lock.
- Replacing the active generation drops the manager's reference to the old one.
  Existing readers may finish safely; the old allocation is reclaimed when the
  last snapshot is dropped.
- A failed or cancelled scan should simply drop its unpublished builder. It must
  not publish a partial index.

This is a lifecycle proof-of-concept, not production integration. It currently
models a complete replacement scan, not incremental splice/merge. Callers must
keep a snapshot alive for the duration of any operation that uses its handles.
A long-lived UI component retaining old snapshots can intentionally delay memory
reclamation, so production integration must define snapshot ownership boundaries.

### Generation lifecycle tests

The module tests cover complete publication, generation-scoped handles,
out-of-order scan completion, old-generation reader safety, reclamation after the
last snapshot drops, and empty generations. These tests still require Windows CI
validation; they do not establish peak RSS or scan throughput.

## Current limitations before integration

- `PathStore` remains append-only within a generation. Removal is implemented by
  building and publishing a replacement generation, not by mutating individual
  records.
- Rebuilding a generation temporarily keeps old and new stores alive together.
  Peak RSS during refresh must be measured at 1M/10M entries; atomic publication
  improves reader latency, not peak-memory usage.
- A cancelled/failed scan must drop its builder. Do not publish partial results.
- Archive virtual paths need an explicit record kind and separate ordering/
  refresh tests before the generation layer can back the complete viewer.
- The prototype does not yet account for concurrent directory watchers or
  incremental filesystem change events.
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
