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


## Repeatable benchmark

Run from `turboview/` on the target Windows machine (release mode):

```powershell
cargo run --release --bin pathbench -- 100000
cargo run --release --bin pathbench -- 1000000
```

The benchmark reports construction, sorting, indexed navigation/materialization,
and an approximate owned-heap estimate for `Vec<PathBuf>`, `Vec<Arc<Path>`,
and `PathStore + PathId`. Run all three on the same local SSD/library and
repeat at least three times. The synthetic dataset uses 4,096 shared directories;
it is intended for representation comparisons, not to predict every real folder
distribution. Record the Windows build, CPU model, RAM, storage type, and median
of the runs alongside the output.

The Windows insertion path now appends UTF-16 filename units directly into the
arena (no temporary per-filename `Vec<u16>`). The storage estimate includes the
directory hash table and directory string payload, but excludes allocator
metadata/alignment; treat it as an estimate rather than RSS. A Windows-only
test covers unpaired UTF-16 code units.


## Current benchmark caveat

The current `PathStore::sort_ids` comparator reconstructs two owned `PathBuf`
values per comparison to guarantee exact `Path::cmp` semantics. Its sort timing
is therefore a deliberately conservative correctness baseline and is expected
to be much slower than sorting borrowed `PathBuf`/`Arc<Path>` values. This is
not an acceptable production implementation. Do not interpret the current
prototype's sort time as the best possible compact-store performance, and do
not integrate it into the viewer. The next optimization must provide a
component-aware borrowed comparator with ordering-parity tests, or another
precomputed sort-key design whose memory cost is measured honestly.
