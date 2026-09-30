//! std's `io::Error`, `io::ErrorKind`, `io::Result`, `io::Read` and
//! `io::Write` as data and traits, and the sink behind `eprintln!`.
//! `stdin`, `stdout`, `stderr` and `Error::last_os_error` read or write the
//! calling process's state and are absent.

use core::fmt;

use a::boxed::Box;
use a::string::String;
use a::vec::Vec;

/// std's `ErrorKind` (the stable variants Detcore can name).
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[non_exhaustive]
pub enum ErrorKind {
    NotFound,
    PermissionDenied,
    ConnectionRefused,
    ConnectionReset,
    ConnectionAborted,
    NotConnected,
    AddrInUse,
    AddrNotAvailable,
    BrokenPipe,
    AlreadyExists,
    WouldBlock,
    InvalidInput,
    InvalidData,
    TimedOut,
    WriteZero,
    Interrupted,
    Unsupported,
    UnexpectedEof,
    OutOfMemory,
    Other,
}

impl ErrorKind {
    fn as_str(self) -> &'static str {
        match self {
            ErrorKind::NotFound => "entity not found",
            ErrorKind::PermissionDenied => "permission denied",
            ErrorKind::ConnectionRefused => "connection refused",
            ErrorKind::ConnectionReset => "connection reset",
            ErrorKind::ConnectionAborted => "connection aborted",
            ErrorKind::NotConnected => "not connected",
            ErrorKind::AddrInUse => "address in use",
            ErrorKind::AddrNotAvailable => "address not available",
            ErrorKind::BrokenPipe => "broken pipe",
            ErrorKind::AlreadyExists => "entity already exists",
            ErrorKind::WouldBlock => "operation would block",
            ErrorKind::InvalidInput => "invalid input parameter",
            ErrorKind::InvalidData => "invalid data",
            ErrorKind::TimedOut => "timed out",
            ErrorKind::WriteZero => "write zero",
            ErrorKind::Interrupted => "operation interrupted",
            ErrorKind::Unsupported => "unsupported",
            ErrorKind::UnexpectedEof => "unexpected end of file",
            ErrorKind::OutOfMemory => "out of memory",
            ErrorKind::Other => "other error",
        }
    }

    /// std's decoding of the Linux errno values it names.
    fn from_errno(code: i32) -> ErrorKind {
        match code {
            1 | 13 => ErrorKind::PermissionDenied, // EPERM, EACCES
            2 => ErrorKind::NotFound,              // ENOENT
            4 => ErrorKind::Interrupted,           // EINTR
            11 => ErrorKind::WouldBlock,           // EAGAIN
            12 => ErrorKind::OutOfMemory,          // ENOMEM
            17 => ErrorKind::AlreadyExists,        // EEXIST
            22 => ErrorKind::InvalidInput,         // EINVAL
            32 => ErrorKind::BrokenPipe,           // EPIPE
            38 | 95 => ErrorKind::Unsupported,     // ENOSYS, EOPNOTSUPP
            98 => ErrorKind::AddrInUse,            // EADDRINUSE
            99 => ErrorKind::AddrNotAvailable,     // EADDRNOTAVAIL
            103 => ErrorKind::ConnectionAborted,   // ECONNABORTED
            104 => ErrorKind::ConnectionReset,     // ECONNRESET
            107 => ErrorKind::NotConnected,        // ENOTCONN
            110 => ErrorKind::TimedOut,            // ETIMEDOUT
            111 => ErrorKind::ConnectionRefused,   // ECONNREFUSED
            _ => ErrorKind::Other,
        }
    }
}

impl fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

enum Repr {
    Os(i32),
    Simple(ErrorKind),
    Custom(ErrorKind, Box<dyn core::error::Error + Send + Sync>),
}

/// std's `io::Error`.
pub struct Error {
    repr: Repr,
}

/// std's `io::Result`.
pub type Result<T> = core::result::Result<T, Error>;

impl Error {
    /// An error of `kind` carrying `error`.
    pub fn new<E>(kind: ErrorKind, error: E) -> Error
    where
        E: Into<Box<dyn core::error::Error + Send + Sync>>,
    {
        Error {
            repr: Repr::Custom(kind, error.into()),
        }
    }

    /// `Error::new(ErrorKind::Other, error)`.
    pub fn other<E>(error: E) -> Error
    where
        E: Into<Box<dyn core::error::Error + Send + Sync>>,
    {
        Error::new(ErrorKind::Other, error)
    }

    /// An error from a raw errno value.
    pub fn from_raw_os_error(code: i32) -> Error {
        Error {
            repr: Repr::Os(code),
        }
    }

    /// The raw errno value, if this error has one.
    pub fn raw_os_error(&self) -> Option<i32> {
        match self.repr {
            Repr::Os(code) => Some(code),
            _ => None,
        }
    }

    /// The error's kind.
    pub fn kind(&self) -> ErrorKind {
        match &self.repr {
            Repr::Os(code) => ErrorKind::from_errno(*code),
            Repr::Simple(kind) | Repr::Custom(kind, _) => *kind,
        }
    }

    /// The wrapped error, if any.
    pub fn get_ref(&self) -> Option<&(dyn core::error::Error + Send + Sync + 'static)> {
        match &self.repr {
            Repr::Custom(_, e) => Some(&**e),
            _ => None,
        }
    }

    /// The wrapped error, if any.
    pub fn into_inner(self) -> Option<Box<dyn core::error::Error + Send + Sync>> {
        match self.repr {
            Repr::Custom(_, e) => Some(e),
            _ => None,
        }
    }
}

impl From<ErrorKind> for Error {
    fn from(kind: ErrorKind) -> Error {
        Error {
            repr: Repr::Simple(kind),
        }
    }
}

impl From<a::collections::TryReserveError> for Error {
    fn from(_: a::collections::TryReserveError) -> Error {
        ErrorKind::OutOfMemory.into()
    }
}

impl fmt::Debug for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.repr {
            Repr::Os(code) => f
                .debug_struct("Os")
                .field("code", code)
                .field("kind", &self.kind())
                .finish(),
            Repr::Simple(kind) => f.debug_tuple("Kind").field(kind).finish(),
            Repr::Custom(kind, e) => f
                .debug_struct("Custom")
                .field("kind", kind)
                .field("error", e)
                .finish(),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.repr {
            Repr::Os(code) => write!(f, "{} (os error {})", self.kind(), code),
            Repr::Simple(kind) => write!(f, "{}", kind),
            Repr::Custom(_, e) => write!(f, "{}", e),
        }
    }
}

impl core::error::Error for Error {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match &self.repr {
            Repr::Custom(_, e) => e.source(),
            _ => None,
        }
    }
}

/// std's `io::Write`.
pub trait Write {
    /// Writes some of `buf`.
    fn write(&mut self, buf: &[u8]) -> Result<usize>;

    /// Flushes buffered output.
    fn flush(&mut self) -> Result<()>;

    /// Writes all of `buf`.
    fn write_all(&mut self, mut buf: &[u8]) -> Result<()> {
        while !buf.is_empty() {
            match self.write(buf) {
                Ok(0) => {
                    return Err(Error::new(
                        ErrorKind::WriteZero,
                        "failed to write whole buffer",
                    ));
                }
                Ok(n) => buf = &buf[n..],
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// Writes formatted output.
    fn write_fmt(&mut self, args: fmt::Arguments<'_>) -> Result<()> {
        struct Adapter<'a, T: ?Sized> {
            inner: &'a mut T,
            error: Result<()>,
        }
        impl<T: Write + ?Sized> fmt::Write for Adapter<'_, T> {
            fn write_str(&mut self, s: &str) -> fmt::Result {
                match self.inner.write_all(s.as_bytes()) {
                    Ok(()) => Ok(()),
                    Err(e) => {
                        self.error = Err(e);
                        Err(fmt::Error)
                    }
                }
            }
        }
        let mut output = Adapter {
            inner: self,
            error: Ok(()),
        };
        match fmt::write(&mut output, args) {
            Ok(()) => Ok(()),
            Err(..) => match output.error {
                Err(e) => Err(e),
                Ok(()) => Err(Error::new(ErrorKind::Other, "formatter error")),
            },
        }
    }

    /// `self`, by mutable reference.
    fn by_ref(&mut self) -> &mut Self
    where
        Self: Sized,
    {
        self
    }
}

impl Write for Vec<u8> {
    fn write(&mut self, buf: &[u8]) -> Result<usize> {
        self.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> Result<()> {
        Ok(())
    }
}

impl<W: Write + ?Sized> Write for &mut W {
    fn write(&mut self, buf: &[u8]) -> Result<usize> {
        (**self).write(buf)
    }

    fn flush(&mut self) -> Result<()> {
        (**self).flush()
    }
}

impl<W: Write + ?Sized> Write for Box<W> {
    fn write(&mut self, buf: &[u8]) -> Result<usize> {
        (**self).write(buf)
    }

    fn flush(&mut self) -> Result<()> {
        (**self).flush()
    }
}

/// std's `io::Read`.
pub trait Read {
    /// Reads some bytes into `buf`.
    fn read(&mut self, buf: &mut [u8]) -> Result<usize>;

    /// Fills `buf` exactly.
    fn read_exact(&mut self, mut buf: &mut [u8]) -> Result<()> {
        while !buf.is_empty() {
            match self.read(buf) {
                Ok(0) => {
                    return Err(Error::new(
                        ErrorKind::UnexpectedEof,
                        "failed to fill whole buffer",
                    ));
                }
                Ok(n) => buf = &mut buf[n..],
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// Reads to the end into `buf`.
    fn read_to_end(&mut self, buf: &mut Vec<u8>) -> Result<usize> {
        let start = buf.len();
        let mut chunk = [0u8; 256];
        loop {
            match self.read(&mut chunk) {
                Ok(0) => return Ok(buf.len() - start),
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
    }

    /// Reads to the end into `buf`, which must stay UTF-8.
    fn read_to_string(&mut self, buf: &mut String) -> Result<usize> {
        let mut bytes = Vec::new();
        let n = self.read_to_end(&mut bytes)?;
        let s = core::str::from_utf8(&bytes).map_err(|_| {
            Error::new(ErrorKind::InvalidData, "stream did not contain valid UTF-8")
        })?;
        buf.push_str(s);
        Ok(n)
    }

    /// `self`, by mutable reference.
    fn by_ref(&mut self) -> &mut Self
    where
        Self: Sized,
    {
        self
    }
}

impl Read for &[u8] {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        let n = core::cmp::min(buf.len(), self.len());
        let (head, tail) = self.split_at(n);
        buf[..n].copy_from_slice(head);
        *self = tail;
        Ok(n)
    }
}

impl<R: Read + ?Sized> Read for &mut R {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        (**self).read(buf)
    }
}

impl<R: Read + ?Sized> Read for Box<R> {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        (**self).read(buf)
    }
}

/// The `eprintln!` sink: a `fn(fmt::Arguments<'_>)` stored as a pointer, null
/// until the host registers one.
static STDERR_SINK: core::sync::atomic::AtomicPtr<()> =
    core::sync::atomic::AtomicPtr::new(core::ptr::null_mut());

/// Registers the function `eprintln!` writes through, replacing any earlier one.
pub fn set_stderr_sink(sink: fn(fmt::Arguments<'_>)) {
    STDERR_SINK.store(sink as *mut (), core::sync::atomic::Ordering::Release);
}

/// `eprintln!`'s back end; not for direct use.
#[doc(hidden)]
pub fn _eprint(args: fmt::Arguments<'_>) {
    let sink = STDERR_SINK.load(core::sync::atomic::Ordering::Acquire);
    if !sink.is_null() {
        // SAFETY: the only non-null value ever stored is a `fn(fmt::Arguments<'_>)`
        // (set_stderr_sink), and function pointers round-trip through `*mut ()`.
        let sink: fn(fmt::Arguments<'_>) = unsafe { core::mem::transmute(sink) };
        sink(args);
    }
}
