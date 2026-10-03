//! `core::ffi` and `alloc::ffi`, plus byte-based `OsStr` and `OsString` with
//! the Unix representation: any byte string.

use core::borrow::Borrow;
pub use core::ffi::*;
use core::fmt;
use core::ops::Deref;
use core::ops::DerefMut;

use a::borrow::Cow;
use a::borrow::ToOwned;
use a::boxed::Box;
pub use a::ffi::CString;
pub use a::ffi::FromVecWithNulError;
pub use a::ffi::IntoStringError;
pub use a::ffi::NulError;
use a::string::String;
use a::vec::Vec;

/// std's `OsStr` on Unix: bytes.
#[repr(transparent)]
#[derive(PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OsStr {
    inner: [u8],
}

/// std's `OsString` on Unix: bytes.
#[derive(Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OsString {
    inner: Vec<u8>,
}

impl OsStr {
    /// `s` as an `OsStr`.
    pub fn new<S: AsRef<OsStr> + ?Sized>(s: &S) -> &OsStr {
        s.as_ref()
    }

    pub(crate) fn from_bytes(b: &[u8]) -> &OsStr {
        // SAFETY: `OsStr` is a repr(transparent) wrapper around `[u8]`.
        unsafe { &*(b as *const [u8] as *const OsStr) }
    }

    pub(crate) fn from_bytes_mut(b: &mut [u8]) -> &mut OsStr {
        // SAFETY: as in `from_bytes`.
        unsafe { &mut *(b as *mut [u8] as *mut OsStr) }
    }

    /// The bytes as `str`, if they are UTF-8.
    pub fn to_str(&self) -> Option<&str> {
        core::str::from_utf8(&self.inner).ok()
    }

    /// The bytes as `str`, invalid sequences replaced.
    pub fn to_string_lossy(&self) -> Cow<'_, str> {
        String::from_utf8_lossy(&self.inner)
    }

    /// An owned copy.
    pub fn to_os_string(&self) -> OsString {
        OsString {
            inner: self.inner.to_vec(),
        }
    }

    /// Whether there are no bytes.
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }

    /// The length in bytes.
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// The bytes.
    pub fn as_encoded_bytes(&self) -> &[u8] {
        &self.inner
    }

    /// `b` as an `OsStr`.
    ///
    /// # Safety
    /// None needed on Unix; unsafe to match std.
    pub unsafe fn from_encoded_bytes_unchecked(b: &[u8]) -> &OsStr {
        OsStr::from_bytes(b)
    }

    /// ASCII-lowercased copy.
    pub fn to_ascii_lowercase(&self) -> OsString {
        OsString {
            inner: self.inner.to_ascii_lowercase(),
        }
    }

    /// ASCII case-insensitive comparison.
    pub fn eq_ignore_ascii_case<S: AsRef<OsStr>>(&self, other: S) -> bool {
        self.inner.eq_ignore_ascii_case(&other.as_ref().inner)
    }

    /// A lossy `Display`.
    pub fn display(&self) -> Display<'_> {
        Display { inner: &self.inner }
    }
}

/// `OsStr::display`'s result.
pub struct Display<'a> {
    inner: &'a [u8],
}

impl fmt::Display for Display<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&String::from_utf8_lossy(self.inner), f)
    }
}

impl fmt::Debug for OsStr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&String::from_utf8_lossy(&self.inner), f)
    }
}

impl fmt::Debug for OsString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}

impl OsString {
    /// An empty string.
    pub fn new() -> OsString {
        OsString { inner: Vec::new() }
    }

    /// An empty string with room for `n` bytes.
    pub fn with_capacity(n: usize) -> OsString {
        OsString {
            inner: Vec::with_capacity(n),
        }
    }

    /// As `&OsStr`.
    pub fn as_os_str(&self) -> &OsStr {
        self
    }

    /// As `String`, if the bytes are UTF-8.
    pub fn into_string(self) -> Result<String, OsString> {
        String::from_utf8(self.inner).map_err(|e| OsString {
            inner: e.into_bytes(),
        })
    }

    /// Appends `s`.
    pub fn push<T: AsRef<OsStr>>(&mut self, s: T) {
        self.inner.extend_from_slice(&s.as_ref().inner)
    }

    /// Empties the string.
    pub fn clear(&mut self) {
        self.inner.clear()
    }

    /// Capacity in bytes.
    pub fn capacity(&self) -> usize {
        self.inner.capacity()
    }

    /// Reserves room for `n` more bytes.
    pub fn reserve(&mut self, n: usize) {
        self.inner.reserve(n)
    }

    /// The bytes.
    pub fn into_encoded_bytes(self) -> Vec<u8> {
        self.inner
    }

    /// `b` as an `OsString`.
    ///
    /// # Safety
    /// None needed on Unix; unsafe to match std.
    pub unsafe fn from_encoded_bytes_unchecked(b: Vec<u8>) -> OsString {
        OsString { inner: b }
    }

    /// As `Box<OsStr>`.
    pub fn into_boxed_os_str(self) -> Box<OsStr> {
        let raw = Box::into_raw(self.inner.into_boxed_slice()) as *mut OsStr;
        // SAFETY: `OsStr` is a repr(transparent) wrapper around `[u8]`.
        unsafe { Box::from_raw(raw) }
    }

    pub(crate) fn from_vec(inner: Vec<u8>) -> OsString {
        OsString { inner }
    }

    pub(crate) fn into_vec(self) -> Vec<u8> {
        self.inner
    }

    pub(crate) fn truncate(&mut self, len: usize) {
        self.inner.truncate(len)
    }
}

impl Deref for OsString {
    type Target = OsStr;

    fn deref(&self) -> &OsStr {
        OsStr::from_bytes(&self.inner)
    }
}

impl DerefMut for OsString {
    fn deref_mut(&mut self) -> &mut OsStr {
        OsStr::from_bytes_mut(&mut self.inner)
    }
}

impl Borrow<OsStr> for OsString {
    fn borrow(&self) -> &OsStr {
        self
    }
}

impl ToOwned for OsStr {
    type Owned = OsString;

    fn to_owned(&self) -> OsString {
        self.to_os_string()
    }
}

impl AsRef<OsStr> for OsStr {
    fn as_ref(&self) -> &OsStr {
        self
    }
}

impl AsRef<OsStr> for OsString {
    fn as_ref(&self) -> &OsStr {
        self
    }
}

impl AsRef<OsStr> for str {
    fn as_ref(&self) -> &OsStr {
        OsStr::from_bytes(self.as_bytes())
    }
}

impl AsRef<OsStr> for String {
    fn as_ref(&self) -> &OsStr {
        (**self).as_ref()
    }
}

impl From<String> for OsString {
    fn from(s: String) -> OsString {
        OsString {
            inner: s.into_bytes(),
        }
    }
}

impl<T: ?Sized + AsRef<OsStr>> From<&T> for OsString {
    fn from(s: &T) -> OsString {
        s.as_ref().to_os_string()
    }
}

impl<'a> From<&'a OsStr> for Cow<'a, OsStr> {
    fn from(s: &'a OsStr) -> Cow<'a, OsStr> {
        Cow::Borrowed(s)
    }
}

impl From<OsString> for Cow<'_, OsStr> {
    fn from(s: OsString) -> Self {
        Cow::Owned(s)
    }
}

impl PartialEq<str> for OsStr {
    fn eq(&self, other: &str) -> bool {
        self.inner == *other.as_bytes()
    }
}

impl PartialEq<str> for OsString {
    fn eq(&self, other: &str) -> bool {
        **self == *other
    }
}

impl PartialEq<&str> for OsString {
    fn eq(&self, other: &&str) -> bool {
        **self == **other
    }
}

impl core::str::FromStr for OsString {
    type Err = core::convert::Infallible;

    fn from_str(s: &str) -> Result<OsString, Self::Err> {
        Ok(OsString::from(s))
    }
}
