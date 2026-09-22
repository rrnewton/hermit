/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

mod replay_cursor;
pub use replay_cursor::ReplayCursor;

/// Associative containers whose iteration order does not vary between runs.
///
/// `std::collections::HashMap` defaults to `RandomState`, whose keys are drawn
/// from the OS via `getrandom` once per process. For the ptrace backend that is
/// harmless, because Detcore runs in its own process. For a backend that shares
/// the guest's address space -- SaBRe loads the Detcore plugin into the guest --
/// those key bytes are written into memory Detcore itself measures, so Detcore
/// becomes a source of the nondeterminism it exists to remove.
///
/// These aliases use a fixed-key hasher instead, so no OS entropy is consulted.
pub type DetBuildHasher =
    std::hash::BuildHasherDefault<std::collections::hash_map::DefaultHasher>;

/// A `HashMap` that does not seed itself from the OS. Construct with
/// `DetHashMap::default()`; `new()` exists only for `RandomState` maps.
pub type DetHashMap<K, V> = std::collections::HashMap<K, V, DetBuildHasher>;

/// A `HashSet` that does not seed itself from the OS. Construct with
/// `DetHashSet::default()`.
pub type DetHashSet<T> = std::collections::HashSet<T, DetBuildHasher>;
