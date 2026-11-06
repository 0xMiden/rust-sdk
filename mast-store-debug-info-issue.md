# Avoid repeated debug-info decoding and cloning in TransactionMastStore construction

Suggested repository: `0xMiden/protocol`.

## Problem

`TransactionMastStore::new()` loads the same built-in packages for each new store. The executable forests already use `Arc`, but their debug metadata is decoded and copied again.

This affects short operations such as note screening. A client that creates one store per screened note pays the package-loading cost before each trial execution.

The path exists in `miden-tx` and `miden-protocol` 0.17.1 with `miden-processor` 0.35.0. Repeated debug-info decoding also reproduces independently with the released VM 0.36.0 packages. The protocol bridge is still present on the upstream `next` branch inspected for this report.

## Cause

1. [`TransactionMastStore::new()`](https://github.com/0xMiden/protocol/blob/673ed0c7d080d82dfdaed3d36d69ce33c894fbbb/crates/miden-tx/src/prover/mast_store.rs#L38-L63) registers the kernel, core, protocol, standards, and AggLayer packages.
2. [`loaded_mast_forest_from_package()`](https://github.com/0xMiden/protocol/blob/673ed0c7d080d82dfdaed3d36d69ce33c894fbbb/crates/miden-protocol/src/package.rs#L13-L37) calls `Package::debug_info()`, which decodes and validates the debug section on each call.
3. The bridge wraps the result in `Arc<PackageDebugInfo>`, then deep-clones it with `(*package_debug_info).clone()`.
4. [`LoadedMastForest::with_package_debug_info()`](https://github.com/0xMiden/miden-vm/blob/7d2866da412cebc97d4cc3847ba0dae812baa407/processor/src/host/mast_forest_store.rs#L19-L30) takes owned metadata and creates another `Arc`.

The copies occur when each package is registered. Cloning the resulting `LoadedMastForest` for its procedure-map entries shares the metadata through `Arc`.

## Measurements

An isolated release-build probe compared the published `miden-core-lib` and `miden-processor` 0.35.0 and 0.36.0 packages. Both versions used Rust 1.98.1. Times are medians of 50 warm constructions, excluding drop. Heap measurements used a counting allocator in a separate pass.

| Core-library operation | VM 0.35.0 | VM 0.36.0 |
| --- | --- | --- |
| `Package::debug_info()` | 3.11 ms | 4.95 ms |
| VM host package load | 2.96 ms | 4.59 ms |
| Protocol bridge decode and clone | 3.32 ms | 5.83 ms |
| Retained heap after bridge construction | 5.23 MiB | 9.16 MiB |
| Retained heap after VM host package load | 5.32 MiB | 9.67 MiB |

The VM host measurement uses `DefaultHost::default().with_library(HostLibrary::from(core.package()))`. It reproduces repeated decoding without the protocol bridge's extra clone. The bridge measurement implements the existing helper's decode, `Arc` wrapping, and deep clone against each VM version.

Cloning the executable forest or a cached `LoadedMastForest` allocated no heap. Building a procedure map from the cached loaded forest retained about 45–47 KiB for either version.

These measurements cover only the core library, not a complete `TransactionMastStore`. The core package grew from 5.19 MB serialized in 0.35.0 to 8.96 MB in 0.36.0, so the comparison does not isolate a decoder regression. The heap figures are requested allocation sizes, not process RSS.

VM 0.36.0 still [decodes and validates on each `Package::debug_info()` call](https://github.com/0xMiden/miden-vm/blob/b9e97144864296e7cabc2b5a7d72b7cb3c1b860e/crates/mast-package/src/package/mod.rs#L426-L439), and its [loaded-forest constructor takes owned debug metadata](https://github.com/0xMiden/miden-vm/blob/b9e97144864296e7cabc2b5a7d72b7cb3c1b860e/processor/src/host/mast_forest_store.rs#L19-L30).

To reproduce package loading directly in VM 0.36.0, use `miden-core-lib = "=0.36.0"` and `miden-processor = "=0.36.0"` in a small binary:

```rust
use std::{hint::black_box, time::Instant};
use miden_core_lib::CoreLibrary;
use miden_processor::{DefaultHost, HostLibrary};

fn main() {
    let core = CoreLibrary::default();
    let load = || {
        DefaultHost::default()
            .with_library(HostLibrary::from(core.package()))
            .unwrap()
    };
    drop(black_box(load()));

    for _ in 0..100 {
        let started = Instant::now();
        let host = black_box(load());
        let construction = started.elapsed();
        drop(host);
        eprintln!("construction={construction:?}");
    }
}
```

## Expected behavior

Repeated store construction should reuse immutable debug metadata for the built-in packages. It should not decode or deep-clone that metadata for each store.

A possible implementation is to cache each built-in package's `LoadedMastForest`, or cache its decoded metadata and let the VM constructor accept the existing `Arc`. Removing only the bridge's deep clone would still leave repeated decoding.

Keep package validation and trust checks. Preserve source diagnostics in ordinary execution. If screening needs an explicit mode without debug metadata, treat that as a separate option and verify equivalent execution results.

Benchmark cold and warm construction, retained heap, and note screening. Check that library error locations still resolve. Keep account-state and witness caches local to each execution.

## Related work

- [miden-vm #3321](https://github.com/0xMiden/miden-vm/issues/3321) tracks a package-first debug execution API. This issue identifies a repeated-loading cost that API should avoid.
- [rust-sdk #977](https://github.com/0xMiden/rust-sdk/issues/977) tracks SQLite-backed MAST lookup. Persistent lookup does not remove this built-in package decoding cost.

Changing `MastForestStore::get` to async is outside this issue. The VM's [`Host::get_mast_forest`](https://github.com/0xMiden/miden-vm/blob/7d2866da412cebc97d4cc3847ba0dae812baa407/processor/src/host/mod.rs#L152-L165) already returns a future. Supporting database or network misses through the transaction executor would require separate resolver plumbing.
