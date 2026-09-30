/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The parts of `std` that Detcore and detcore-model use, for building them
//! without `std` (the Narf kernel target, `x86_64-unknown-none`).
//!
//! On `target_os = "none"`, Detcore and detcore-model are `#![no_std]` and
//! bind this crate as `std`, so their `std::` paths resolve here and the code
//! they share with the host build keeps its `std::` paths. On every other
//! target this crate is empty and has no dependencies: the host build never
//! compiles it.
//!
//! It provides:
//!   - the parts of `std` that `core` and `alloc` already have, under `std`'s
//!     own paths;
//!   - `HashMap`, `HashSet` and their entry types (hashbrown);
//!   - `DefaultHasher`: core's SipHash-1-3 with keys (0, 0), the algorithm and
//!     keys std's `DefaultHasher::new` uses, so hashes are unchanged;
//!   - `Mutex`, `MutexGuard`, `LazyLock`, `OnceLock` (spin; never poisoned,
//!     so `.lock().unwrap()` keeps compiling and never panics), with serde's
//!     `Serialize`/`Deserialize` for `Mutex` as serde has them for std's;
//!   - `io::Error`, `io::ErrorKind`, `io::Result`, `io::Read`, `io::Write`;
//!   - byte-based `Path`, `PathBuf`, `Component`, `OsStr`, `OsString` with Unix
//!     semantics;
//!   - `RawFd` (`c_int`) and the `OsStrExt`/`OsStringExt` byte conversions;
//!   - `Instant`, `SystemTime` and `UNIX_EPOCH` as values;
//!   - `f64::floor`, `ceil`, `round` and `powf`, which core lacks, as the
//!     prelude trait `FloatMath` over libm (see its note on `powf`);
//!   - `eprint!` and `eprintln!`, formatting as std's do and handing the text
//!     to a sink the embedding kernel registers with `io::set_stderr_sink`, or
//!     dropping it if none is registered.
//!
//! Anything that asks an operating system for something is absent, so each
//! use is a compile error that Detcore must route through its backend or keep
//! to the host build: `Instant::now`, `SystemTime::now`, `io::stderr`,
//! `io::Error::last_os_error`, `fs`, `env`, `process`, `thread`,
//! `OwnedFd`/`BorrowedFd`/`AsRawFd`/`FromRawFd`, the file-system queries on
//! `Path` (`exists`, `is_dir`, `metadata`, `canonicalize`, `read_link`), and
//! `println!`.
#![no_std]
#![cfg(target_os = "none")]
#![feature(hashmap_internals)]
#![allow(internal_features)]

extern crate alloc as a;

pub use core::any;
pub use core::array;
pub use core::ascii;
pub use core::cell;
pub use core::char;
pub use core::clone;
pub use core::cmp;
pub use core::convert;
pub use core::default;
pub use core::error;
pub use core::f32;
pub use core::f64;
pub use core::future;
pub use core::hint;
pub use core::i8;
pub use core::i16;
pub use core::i32;
pub use core::i64;
pub use core::i128;
pub use core::isize;
pub use core::iter;
pub use core::marker;
pub use core::mem;
pub use core::net;
pub use core::num;
pub use core::ops;
pub use core::option;
pub use core::panic;
pub use core::pin;
pub use core::primitive;
pub use core::ptr;
pub use core::result;
pub use core::task;
pub use core::u8;
pub use core::u16;
pub use core::u32;
pub use core::u64;
pub use core::u128;
pub use core::usize;

pub use a::borrow;
pub use a::boxed;
pub use a::fmt;
pub use a::format;
pub use a::rc;
pub use a::slice;
pub use a::str;
pub use a::string;
pub use a::vec;

pub mod collections;
pub mod ffi;
pub mod io;
pub mod os;
pub mod path;
pub mod sync;
pub mod time;

/// `core::hash` plus std's `DefaultHasher` and `RandomState`.
pub mod hash {
    pub use core::hash::*;

    pub use crate::collections::hash_map::DefaultHasher;
    pub use crate::collections::hash_map::RandomState;
}

/// `alloc::alloc`.
pub mod alloc {
    pub use a::alloc::*;
}

/// std's `f64` methods Detcore calls that `core` lacks, over libm. `floor`,
/// `ceil` and `round` are exact operations, so libm returns what std returns.
/// `powf` is libm's `pow`; std's calls the host C library's `pow`, and the two
/// are not guaranteed to agree in the last place. Where std is present its
/// inherent methods take precedence over this trait.
pub trait FloatMath {
    /// Largest integer not above `self`.
    fn floor(self) -> Self;
    /// Smallest integer not below `self`.
    fn ceil(self) -> Self;
    /// Nearest integer, halfway cases away from zero.
    fn round(self) -> Self;
    /// `self` raised to the power `n`.
    fn powf(self, n: Self) -> Self;
}

impl FloatMath for f64 {
    fn floor(self) -> f64 {
        libm::floor(self)
    }

    fn ceil(self) -> f64 {
        libm::ceil(self)
    }

    fn round(self) -> f64 {
        libm::round(self)
    }

    fn powf(self, n: f64) -> f64 {
        libm::pow(self, n)
    }
}

/// The 2024 prelude: core's, plus the alloc names std's prelude adds.
pub mod prelude {
    /// Edition 2024.
    pub mod rust_2024 {
        pub use core::prelude::rust_2024::*;

        pub use a::borrow::ToOwned;
        pub use a::boxed::Box;
        pub use a::format;
        pub use a::string::String;
        pub use a::string::ToString;
        pub use a::vec;
        pub use a::vec::Vec;

        pub use crate::FloatMath;
        pub use crate::eprint;
        pub use crate::eprintln;
    }
}

/// std's `eprint!`: formats the arguments and passes them to the sink
/// registered with `io::set_stderr_sink`. Without a sink they are dropped.
#[macro_export]
macro_rules! eprint {
    ($($arg:tt)*) => {
        $crate::io::_eprint(format_args!($($arg)*))
    };
}

/// std's `eprintln!`: formats the arguments and a newline, then passes them to
/// the sink registered with `io::set_stderr_sink`. Without a sink the line is
/// dropped.
#[macro_export]
macro_rules! eprintln {
    () => {
        $crate::io::_eprint(format_args!("\n"))
    };
    ($($arg:tt)*) => {
        $crate::io::_eprint(format_args!("{}\n", format_args!($($arg)*)))
    };
}
