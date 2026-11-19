# Compact path storage prototype

Status: isolated prototype branch; not wired into the viewer.

## Why this exists

PR #8 reduced the global navigation slot to a shared `Arc<Path>`, but each image
still owns one allocation containing its complete absolute path. For a large
library, sibling images repeat the same directory prefix many times.

The next candidate representation is:

- a directory table: one shared path allocation per unique parent directory;
- a contiguous UTF-16 filename arena;
- a compact image record containing a directory id and filename range;
- conversion to `PathBuf` only at filesystem/decode boundaries.

The prototype must preserve Windows non-UTF-8 paths and the exact ordering
semantics of `PathBuf::cmp`. It must also handle archive virtual paths, which
are not ordinary filesystem children, without silently changing their order.

## Important correctness constraints

1. Do not compare concatenated UTF-16 strings as a substitute for
   `PathBuf::cmp`: component ordering differs for cases such as
   `C:\\foo\\bar.jpg` and `C:\\foo.txt`.
2. Preserve arbitrary Windows UTF-16 names; never round-trip through
   `to_string_lossy()`.
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
