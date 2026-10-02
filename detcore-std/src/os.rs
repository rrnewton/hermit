//! `std::os`: the raw C types, the descriptor number type, and the byte
//! conversions of `OsStr`/`OsString`. The descriptor handle types and traits
//! (`OwnedFd`, `BorrowedFd`, `AsRawFd`, `FromRawFd`) own, borrow or close a
//! descriptor of the calling process and are absent, as is
//! `unix::fs::MetadataExt` (there is no `fs::Metadata`).

/// C types.
pub mod raw {
    pub use core::ffi::c_char;
    pub use core::ffi::c_double;
    pub use core::ffi::c_float;
    pub use core::ffi::c_int;
    pub use core::ffi::c_long;
    pub use core::ffi::c_longlong;
    pub use core::ffi::c_schar;
    pub use core::ffi::c_short;
    pub use core::ffi::c_uchar;
    pub use core::ffi::c_uint;
    pub use core::ffi::c_ulong;
    pub use core::ffi::c_ulonglong;
    pub use core::ffi::c_ushort;
    pub use core::ffi::c_void;
}

/// Descriptor numbers.
pub mod fd {
    /// A descriptor number.
    pub type RawFd = core::ffi::c_int;
}

/// Unix.
pub mod unix {
    /// `std::os::unix::io`.
    pub mod io {
        pub use crate::os::fd::RawFd;
    }

    /// `std::os::unix::ffi`.
    pub mod ffi {
        use a::vec::Vec;

        use crate::ffi::OsStr;
        use crate::ffi::OsString;

        /// Byte access to `OsStr`.
        pub trait OsStrExt {
            /// `slice` as `Self`.
            fn from_bytes(slice: &[u8]) -> &Self;
            /// The bytes.
            fn as_bytes(&self) -> &[u8];
        }

        impl OsStrExt for OsStr {
            fn from_bytes(slice: &[u8]) -> &OsStr {
                OsStr::from_bytes(slice)
            }

            fn as_bytes(&self) -> &[u8] {
                self.as_encoded_bytes()
            }
        }

        /// Byte access to `OsString`.
        pub trait OsStringExt {
            /// `vec` as `Self`.
            fn from_vec(vec: Vec<u8>) -> Self;
            /// The bytes.
            fn into_vec(self) -> Vec<u8>;
        }

        impl OsStringExt for OsString {
            fn from_vec(vec: Vec<u8>) -> OsString {
                OsString::from_vec(vec)
            }

            fn into_vec(self) -> Vec<u8> {
                OsString::into_vec(self)
            }
        }
    }
}
