//! Byte-based `Path`, `PathBuf` and `Component` with Unix semantics (only
//! `/` separates, there are no prefixes), over the byte-based `OsStr`. Pure
//! string manipulation: the queries that consult a file system (`exists`,
//! `is_dir`, `is_file`, `metadata`, `canonicalize`, `read_link`, `read_dir`)
//! are absent. Comparison, ordering and hashing go by components, as std's
//! do, so `a//b` equals `a/b`.

use core::borrow::Borrow;
use core::cmp;
use core::fmt;
use core::hash::Hash;
use core::hash::Hasher;
use core::ops::Deref;
use core::str::FromStr;

use a::borrow::Cow;
use a::borrow::ToOwned;
use a::boxed::Box;
use a::string::String;
use a::vec::Vec;

use crate::ffi::OsStr;
use crate::ffi::OsString;

/// The separator.
pub const MAIN_SEPARATOR: char = '/';
/// The separator.
pub const MAIN_SEPARATOR_STR: &str = "/";

/// Whether `c` separates components.
pub fn is_separator(c: char) -> bool {
    c == '/'
}

/// std's `Path` on Unix.
#[repr(transparent)]
pub struct Path {
    inner: OsStr,
}

/// std's `PathBuf` on Unix.
#[derive(Clone, Default)]
pub struct PathBuf {
    inner: OsString,
}

/// Never produced on Unix; present so that matches written against std's
/// `Component` still compile.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PrefixComponent<'a> {
    raw: &'a OsStr,
}

impl<'a> PrefixComponent<'a> {
    /// The prefix's text.
    pub fn as_os_str(&self) -> &'a OsStr {
        self.raw
    }
}

/// One component of a path.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Component<'a> {
    /// Never produced on Unix.
    Prefix(PrefixComponent<'a>),
    /// A leading `/`.
    RootDir,
    /// A leading `.`.
    CurDir,
    /// `..`.
    ParentDir,
    /// Anything else.
    Normal(&'a OsStr),
}

impl<'a> Component<'a> {
    /// The component's text.
    pub fn as_os_str(self) -> &'a OsStr {
        match self {
            Component::Prefix(p) => p.as_os_str(),
            Component::RootDir => OsStr::new("/"),
            Component::CurDir => OsStr::new("."),
            Component::ParentDir => OsStr::new(".."),
            Component::Normal(p) => p,
        }
    }
}

impl AsRef<OsStr> for Component<'_> {
    fn as_ref(&self) -> &OsStr {
        self.as_os_str()
    }
}

impl AsRef<Path> for Component<'_> {
    fn as_ref(&self) -> &Path {
        Path::new(self.as_os_str())
    }
}

/// std's `Components`: the components with their byte ranges in the path.
#[derive(Clone)]
pub struct Components<'a> {
    path: &'a [u8],
    items: Vec<(Component<'a>, usize, usize)>,
    front: usize,
    back: usize,
}

impl<'a> Components<'a> {
    fn parse(path: &'a [u8]) -> Components<'a> {
        let mut items = Vec::new();
        if path.first() == Some(&b'/') {
            items.push((Component::RootDir, 0, 1));
        } else if path == b"." || path.starts_with(b"./") {
            items.push((Component::CurDir, 0, 1));
        }
        let mut start = 0;
        for part in path.split(|&b| b == b'/') {
            let end = start + part.len();
            match part {
                b"" | b"." => {}
                b".." => items.push((Component::ParentDir, start, end)),
                p => items.push((Component::Normal(OsStr::from_bytes(p)), start, end)),
            }
            start = end + 1;
        }
        let back = items.len();
        Components {
            path,
            items,
            front: 0,
            back,
        }
    }

    /// The rest of the path.
    pub fn as_path(&self) -> &'a Path {
        if self.front >= self.back {
            return Path::from_bytes(b"");
        }
        let start = self.items[self.front].1;
        let end = self.items[self.back - 1].2;
        Path::from_bytes(&self.path[start..end])
    }
}

impl<'a> Iterator for Components<'a> {
    type Item = Component<'a>;

    fn next(&mut self) -> Option<Component<'a>> {
        if self.front >= self.back {
            return None;
        }
        self.front += 1;
        Some(self.items[self.front - 1].0)
    }
}

impl<'a> DoubleEndedIterator for Components<'a> {
    fn next_back(&mut self) -> Option<Component<'a>> {
        if self.front >= self.back {
            return None;
        }
        self.back -= 1;
        Some(self.items[self.back].0)
    }
}

impl fmt::Debug for Components<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.clone()).finish()
    }
}

/// std's `Iter`: the components as `OsStr`.
#[derive(Clone, Debug)]
pub struct Iter<'a> {
    inner: Components<'a>,
}

impl<'a> Iter<'a> {
    /// The rest of the path.
    pub fn as_path(&self) -> &'a Path {
        self.inner.as_path()
    }
}

impl<'a> Iterator for Iter<'a> {
    type Item = &'a OsStr;

    fn next(&mut self) -> Option<&'a OsStr> {
        self.inner.next().map(Component::as_os_str)
    }
}

impl<'a> DoubleEndedIterator for Iter<'a> {
    fn next_back(&mut self) -> Option<&'a OsStr> {
        self.inner.next_back().map(Component::as_os_str)
    }
}

/// std's `Ancestors`.
#[derive(Clone, Debug)]
pub struct Ancestors<'a> {
    next: Option<&'a Path>,
}

impl<'a> Iterator for Ancestors<'a> {
    type Item = &'a Path;

    fn next(&mut self) -> Option<&'a Path> {
        let next = self.next;
        self.next = next.and_then(Path::parent);
        next
    }
}

/// `Path::strip_prefix`'s error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StripPrefixError(());

impl fmt::Display for StripPrefixError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("prefix not found")
    }
}

impl core::error::Error for StripPrefixError {}

fn iter_after<'a, 'b, I, J>(mut iter: I, mut prefix: J) -> Option<I>
where
    I: Iterator<Item = Component<'a>> + Clone,
    J: Iterator<Item = Component<'b>>,
{
    loop {
        let mut iter_next = iter.clone();
        match (iter_next.next(), prefix.next()) {
            (Some(ref x), Some(ref y)) if x.as_os_str() == y.as_os_str() => (),
            (Some(_), Some(_)) => return None,
            (Some(_), None) => return Some(iter),
            (None, None) => return Some(iter),
            (None, Some(_)) => return None,
        }
        iter = iter_next;
    }
}

impl Path {
    /// `s` as a `Path`.
    pub fn new<S: AsRef<OsStr> + ?Sized>(s: &S) -> &Path {
        let s: &OsStr = s.as_ref();
        // SAFETY: `Path` is a repr(transparent) wrapper around `OsStr`.
        unsafe { &*(s as *const OsStr as *const Path) }
    }

    fn from_bytes(b: &[u8]) -> &Path {
        Path::new(OsStr::from_bytes(b))
    }

    fn bytes(&self) -> &[u8] {
        self.inner.as_encoded_bytes()
    }

    /// As `OsStr`.
    pub fn as_os_str(&self) -> &OsStr {
        &self.inner
    }

    /// As `str`, if UTF-8.
    pub fn to_str(&self) -> Option<&str> {
        self.inner.to_str()
    }

    /// As `str`, invalid sequences replaced.
    pub fn to_string_lossy(&self) -> Cow<'_, str> {
        self.inner.to_string_lossy()
    }

    /// An owned copy.
    pub fn to_path_buf(&self) -> PathBuf {
        PathBuf {
            inner: self.inner.to_os_string(),
        }
    }

    /// Whether it starts with `/`.
    pub fn is_absolute(&self) -> bool {
        self.has_root()
    }

    /// Whether it does not start with `/`.
    pub fn is_relative(&self) -> bool {
        !self.is_absolute()
    }

    /// Whether it starts with `/`.
    pub fn has_root(&self) -> bool {
        self.bytes().first() == Some(&b'/')
    }

    /// The path without its last component.
    pub fn parent(&self) -> Option<&Path> {
        let mut comps = self.components();
        match comps.next_back() {
            Some(Component::Normal(_) | Component::CurDir | Component::ParentDir) => {
                Some(comps.as_path())
            }
            _ => None,
        }
    }

    /// The path and each of its parents.
    pub fn ancestors(&self) -> Ancestors<'_> {
        Ancestors { next: Some(self) }
    }

    /// The last component, if it is a normal one.
    pub fn file_name(&self) -> Option<&OsStr> {
        match self.components().next_back() {
            Some(Component::Normal(p)) => Some(p),
            _ => None,
        }
    }

    /// The path without the leading `base`.
    pub fn strip_prefix<P: AsRef<Path>>(&self, base: P) -> Result<&Path, StripPrefixError> {
        iter_after(self.components(), base.as_ref().components())
            .map(|c| c.as_path())
            .ok_or(StripPrefixError(()))
    }

    /// Whether `base`'s components lead this path's.
    pub fn starts_with<P: AsRef<Path>>(&self, base: P) -> bool {
        iter_after(self.components(), base.as_ref().components()).is_some()
    }

    /// Whether `child`'s components end this path's.
    pub fn ends_with<P: AsRef<Path>>(&self, child: P) -> bool {
        iter_after(self.components().rev(), child.as_ref().components().rev()).is_some()
    }

    fn split_file_at_dot(&self) -> Option<(&[u8], Option<&[u8]>)> {
        let name = self.file_name()?.as_encoded_bytes();
        if name == b".." {
            return Some((name, None));
        }
        match name.iter().rposition(|&b| b == b'.') {
            None | Some(0) => Some((name, None)),
            Some(i) => Some((&name[..i], Some(&name[i + 1..]))),
        }
    }

    /// The file name without its extension.
    pub fn file_stem(&self) -> Option<&OsStr> {
        self.split_file_at_dot()
            .map(|(stem, _)| OsStr::from_bytes(stem))
    }

    /// The file name's extension.
    pub fn extension(&self) -> Option<&OsStr> {
        self.split_file_at_dot()
            .and_then(|(_, ext)| ext)
            .map(OsStr::from_bytes)
    }

    /// `self` with `path` pushed onto it.
    pub fn join<P: AsRef<Path>>(&self, path: P) -> PathBuf {
        let mut buf = self.to_path_buf();
        buf.push(path);
        buf
    }

    /// `self` with its file name replaced.
    pub fn with_file_name<S: AsRef<OsStr>>(&self, file_name: S) -> PathBuf {
        let mut buf = self.to_path_buf();
        buf.set_file_name(file_name);
        buf
    }

    /// `self` with its extension replaced.
    pub fn with_extension<S: AsRef<OsStr>>(&self, extension: S) -> PathBuf {
        let mut buf = self.to_path_buf();
        buf.set_extension(extension);
        buf
    }

    /// The components.
    pub fn components(&self) -> Components<'_> {
        Components::parse(self.bytes())
    }

    /// The components as `OsStr`.
    pub fn iter(&self) -> Iter<'_> {
        Iter {
            inner: self.components(),
        }
    }

    /// A lossy `Display`.
    pub fn display(&self) -> Display<'_> {
        Display { path: self }
    }

    /// As `PathBuf`.
    pub fn into_path_buf(self: Box<Path>) -> PathBuf {
        let raw = Box::into_raw(self) as *mut [u8];
        // SAFETY: `Path` wraps `OsStr`, which wraps `[u8]`, both transparently.
        let bytes = unsafe { Box::from_raw(raw) };
        PathBuf {
            inner: OsString::from_vec(bytes.into_vec()),
        }
    }
}

/// `Path::display`'s result.
pub struct Display<'a> {
    path: &'a Path,
}

impl fmt::Display for Display<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.path.to_string_lossy(), f)
    }
}

impl fmt::Debug for Display<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.path, f)
    }
}

impl PathBuf {
    /// An empty path.
    pub fn new() -> PathBuf {
        PathBuf {
            inner: OsString::new(),
        }
    }

    /// An empty path with room for `n` bytes.
    pub fn with_capacity(n: usize) -> PathBuf {
        PathBuf {
            inner: OsString::with_capacity(n),
        }
    }

    /// As `&Path`.
    pub fn as_path(&self) -> &Path {
        self
    }

    /// Appends `path`; an absolute `path` replaces `self`.
    pub fn push<P: AsRef<Path>>(&mut self, path: P) {
        let path = path.as_ref();
        let need_sep = self
            .inner
            .as_encoded_bytes()
            .last()
            .is_some_and(|c| *c != b'/');
        if path.is_absolute() {
            self.inner.clear();
        } else if need_sep {
            self.inner.push("/");
        }
        self.inner.push(path.as_os_str());
    }

    /// Drops the last component; false if there is no parent.
    pub fn pop(&mut self) -> bool {
        match self.parent().map(|p| p.as_os_str().len()) {
            Some(len) => {
                self.inner.truncate(len);
                true
            }
            None => false,
        }
    }

    /// Replaces the file name.
    pub fn set_file_name<S: AsRef<OsStr>>(&mut self, file_name: S) {
        if self.file_name().is_some() {
            self.pop();
        }
        self.push(Path::new(file_name.as_ref()));
    }

    /// Replaces the extension; false if there is no file name.
    pub fn set_extension<S: AsRef<OsStr>>(&mut self, extension: S) -> bool {
        let Some(stem) = self.file_stem() else {
            return false;
        };
        let stem_end = stem.as_encoded_bytes().as_ptr() as usize
            - self.inner.as_encoded_bytes().as_ptr() as usize
            + stem.len();
        self.inner.truncate(stem_end);
        let ext = extension.as_ref();
        if !ext.is_empty() {
            self.inner.push(".");
            self.inner.push(ext);
        }
        true
    }

    /// As `OsString`.
    pub fn into_os_string(self) -> OsString {
        self.inner
    }

    /// As `Box<Path>`.
    pub fn into_boxed_path(self) -> Box<Path> {
        let raw = Box::into_raw(self.inner.into_boxed_os_str()) as *mut Path;
        // SAFETY: `Path` is a repr(transparent) wrapper around `OsStr`.
        unsafe { Box::from_raw(raw) }
    }

    /// Capacity in bytes.
    pub fn capacity(&self) -> usize {
        self.inner.capacity()
    }

    /// Empties the path.
    pub fn clear(&mut self) {
        self.inner.clear()
    }

    /// Reserves room for `n` more bytes.
    pub fn reserve(&mut self, n: usize) {
        self.inner.reserve(n)
    }

    /// The underlying `OsString`.
    pub fn as_mut_os_string(&mut self) -> &mut OsString {
        &mut self.inner
    }
}

impl Deref for PathBuf {
    type Target = Path;

    fn deref(&self) -> &Path {
        Path::new(&self.inner)
    }
}

impl Borrow<Path> for PathBuf {
    fn borrow(&self) -> &Path {
        self
    }
}

impl ToOwned for Path {
    type Owned = PathBuf;

    fn to_owned(&self) -> PathBuf {
        self.to_path_buf()
    }
}

impl AsRef<Path> for Path {
    fn as_ref(&self) -> &Path {
        self
    }
}

impl AsRef<Path> for PathBuf {
    fn as_ref(&self) -> &Path {
        self
    }
}

impl AsRef<Path> for OsStr {
    fn as_ref(&self) -> &Path {
        Path::new(self)
    }
}

impl AsRef<Path> for OsString {
    fn as_ref(&self) -> &Path {
        Path::new(self)
    }
}

impl AsRef<Path> for str {
    fn as_ref(&self) -> &Path {
        Path::new(self)
    }
}

impl AsRef<Path> for String {
    fn as_ref(&self) -> &Path {
        Path::new(self)
    }
}

impl AsRef<Path> for Cow<'_, OsStr> {
    fn as_ref(&self) -> &Path {
        Path::new(&**self)
    }
}

impl AsRef<OsStr> for Path {
    fn as_ref(&self) -> &OsStr {
        &self.inner
    }
}

impl AsRef<OsStr> for PathBuf {
    fn as_ref(&self) -> &OsStr {
        &self.inner
    }
}

impl From<String> for PathBuf {
    fn from(s: String) -> PathBuf {
        PathBuf {
            inner: OsString::from(s),
        }
    }
}

impl From<OsString> for PathBuf {
    fn from(inner: OsString) -> PathBuf {
        PathBuf { inner }
    }
}

impl<T: ?Sized + AsRef<OsStr>> From<&T> for PathBuf {
    fn from(s: &T) -> PathBuf {
        PathBuf {
            inner: s.as_ref().to_os_string(),
        }
    }
}

impl From<Box<Path>> for PathBuf {
    fn from(b: Box<Path>) -> PathBuf {
        b.into_path_buf()
    }
}

impl From<Cow<'_, Path>> for PathBuf {
    fn from(c: Cow<'_, Path>) -> PathBuf {
        c.into_owned()
    }
}

impl From<PathBuf> for OsString {
    fn from(p: PathBuf) -> OsString {
        p.inner
    }
}

impl From<PathBuf> for Box<Path> {
    fn from(p: PathBuf) -> Box<Path> {
        p.into_boxed_path()
    }
}

impl From<&Path> for Box<Path> {
    fn from(p: &Path) -> Box<Path> {
        p.to_path_buf().into_boxed_path()
    }
}

impl<'a> From<&'a Path> for Cow<'a, Path> {
    fn from(p: &'a Path) -> Cow<'a, Path> {
        Cow::Borrowed(p)
    }
}

impl<'a> From<&'a PathBuf> for Cow<'a, Path> {
    fn from(p: &'a PathBuf) -> Cow<'a, Path> {
        Cow::Borrowed(p.as_path())
    }
}

impl From<PathBuf> for Cow<'_, Path> {
    fn from(p: PathBuf) -> Self {
        Cow::Owned(p)
    }
}

impl Clone for Box<Path> {
    fn clone(&self) -> Self {
        self.to_path_buf().into_boxed_path()
    }
}

impl FromStr for PathBuf {
    type Err = core::convert::Infallible;

    fn from_str(s: &str) -> Result<PathBuf, Self::Err> {
        Ok(PathBuf::from(s))
    }
}

impl<P: AsRef<Path>> Extend<P> for PathBuf {
    fn extend<I: IntoIterator<Item = P>>(&mut self, iter: I) {
        for p in iter {
            self.push(p.as_ref());
        }
    }
}

impl<P: AsRef<Path>> FromIterator<P> for PathBuf {
    fn from_iter<I: IntoIterator<Item = P>>(iter: I) -> PathBuf {
        let mut buf = PathBuf::new();
        buf.extend(iter);
        buf
    }
}

impl<'a> IntoIterator for &'a Path {
    type Item = &'a OsStr;
    type IntoIter = Iter<'a>;

    fn into_iter(self) -> Iter<'a> {
        self.iter()
    }
}

impl<'a> IntoIterator for &'a PathBuf {
    type Item = &'a OsStr;
    type IntoIter = Iter<'a>;

    fn into_iter(self) -> Iter<'a> {
        self.iter()
    }
}

impl fmt::Debug for Path {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.inner, f)
    }
}

impl fmt::Debug for PathBuf {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}

impl PartialEq for Path {
    fn eq(&self, other: &Path) -> bool {
        self.components().eq(other.components())
    }
}

impl Eq for Path {}

impl PartialOrd for Path {
    fn partial_cmp(&self, other: &Path) -> Option<cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Path {
    fn cmp(&self, other: &Path) -> cmp::Ordering {
        self.components().cmp(other.components())
    }
}

impl Hash for Path {
    fn hash<H: Hasher>(&self, h: &mut H) {
        for c in self.components() {
            c.as_os_str().as_encoded_bytes().hash(h);
        }
    }
}

impl PartialEq for PathBuf {
    fn eq(&self, other: &PathBuf) -> bool {
        **self == **other
    }
}

impl Eq for PathBuf {}

impl PartialOrd for PathBuf {
    fn partial_cmp(&self, other: &PathBuf) -> Option<cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for PathBuf {
    fn cmp(&self, other: &PathBuf) -> cmp::Ordering {
        (**self).cmp(&**other)
    }
}

impl Hash for PathBuf {
    fn hash<H: Hasher>(&self, h: &mut H) {
        (**self).hash(h)
    }
}

impl PartialEq<PathBuf> for Path {
    fn eq(&self, other: &PathBuf) -> bool {
        *self == **other
    }
}

impl PartialEq<Path> for PathBuf {
    fn eq(&self, other: &Path) -> bool {
        **self == *other
    }
}

impl PartialEq<&Path> for PathBuf {
    fn eq(&self, other: &&Path) -> bool {
        **self == **other
    }
}

impl PartialEq<PathBuf> for &Path {
    fn eq(&self, other: &PathBuf) -> bool {
        **self == **other
    }
}

impl serde::Serialize for Path {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self.to_str() {
            Some(s) => serializer.serialize_str(s),
            None => Err(serde::ser::Error::custom(
                "path contains invalid UTF-8 characters",
            )),
        }
    }
}

impl serde::Serialize for PathBuf {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        (**self).serialize(serializer)
    }
}

impl<'de> serde::Deserialize<'de> for PathBuf {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<PathBuf, D::Error> {
        struct V;
        impl serde::de::Visitor<'_> for V {
            type Value = PathBuf;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("path string")
            }

            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<PathBuf, E> {
                Ok(PathBuf::from(v))
            }

            fn visit_string<E: serde::de::Error>(self, v: String) -> Result<PathBuf, E> {
                Ok(PathBuf::from(v))
            }

            fn visit_bytes<E: serde::de::Error>(self, v: &[u8]) -> Result<PathBuf, E> {
                Ok(PathBuf::from(OsStr::from_bytes(v)))
            }

            fn visit_byte_buf<E: serde::de::Error>(self, v: Vec<u8>) -> Result<PathBuf, E> {
                Ok(PathBuf::from(OsString::from_vec(v)))
            }
        }
        deserializer.deserialize_string(V)
    }
}
