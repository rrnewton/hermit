//! alloc's collections plus hashbrown's `HashMap` and `HashSet` under std's
//! paths. hashbrown's default hasher is foldhash with a per-process seed, so,
//! as with std's `RandomState`, iteration order is not reproducible and code
//! that depends on it is already a determinism bug.

pub use a::collections::*;
pub use hashbrown::HashMap;
pub use hashbrown::HashSet;

/// hashbrown's `hash_map`, plus std's `DefaultHasher`.
pub mod hash_map {
    pub use hashbrown::hash_map::*;

    /// std's `DefaultHasher`: SipHash-1-3 with keys (0, 0). core has the
    /// hasher (unstable, deprecated in favour of this very type), so the
    /// output is bit-identical to a host build's.
    #[allow(deprecated)]
    #[derive(Clone, Debug)]
    pub struct DefaultHasher(core::hash::SipHasher13);

    impl DefaultHasher {
        /// A hasher with std's fixed keys.
        #[allow(deprecated)]
        #[must_use]
        pub fn new() -> DefaultHasher {
            DefaultHasher(core::hash::SipHasher13::new_with_keys(0, 0))
        }
    }

    impl Default for DefaultHasher {
        fn default() -> DefaultHasher {
            DefaultHasher::new()
        }
    }

    impl core::hash::Hasher for DefaultHasher {
        fn write(&mut self, msg: &[u8]) {
            self.0.write(msg)
        }

        fn finish(&self) -> u64 {
            self.0.finish()
        }
    }

    /// hashbrown's default `BuildHasher` in place of std's `RandomState`.
    pub type RandomState = hashbrown::DefaultHashBuilder;
}

/// hashbrown's `hash_set`.
pub mod hash_set {
    pub use hashbrown::hash_set::*;
}
