use std::ffi::OsStr;
use std::fmt;
use std::fmt::Write;
use std::io;
use std::sync::Arc;

use crate::liteinst_record::RecordFailure;

const CAPACITY: usize = 1024;
const TRUNCATED: &[u8] = b" [truncated]\n";
const FORMAT_FAILED: &[u8] = b" [formatting failed]\n";
const MAX_WRITES: usize = 32;

#[derive(Debug)]
pub(crate) enum Failure {
    Selection,
    MissingBootstrap,
    Bootstrap(io::Error),
    Payload(String),
    MissingLog,
    Transport(io::Error),
    Formatter(io::Error),
    Subscriber(tracing_subscriber::util::TryInitError),
    Record(Arc<RecordFailure>),
    Installation(io::Error),
    #[cfg(feature = "private-crt")]
    PrivateStartup(&'static str, io::Error),
}

impl Failure {
    fn stage(&self) -> &'static str {
        match self {
            Self::Selection => "selection",
            Self::MissingBootstrap | Self::Bootstrap(_) => "bootstrap",
            Self::Payload(error) if error.starts_with("invalid bootstrap log filter: ") => "filter",
            Self::Payload(_) => "payload",
            Self::MissingLog => "log-endpoint",
            Self::Transport(_) => "log-transport",
            Self::Formatter(_) => "formatter",
            Self::Subscriber(_) => "subscriber",
            Self::Record(_) => "record-status",
            Self::Installation(_) => "installation",
            #[cfg(feature = "private-crt")]
            Self::PrivateStartup(stage, _) => stage,
        }
    }
}

struct IoDetail<'error>(&'error io::Error);

impl fmt::Display for IoDetail<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "kind={:?}", self.0.kind())?;
        if let Some(errno) = self.0.raw_os_error() {
            return write!(formatter, " errno={errno}");
        }
        if matches!(
            self.0.kind(),
            io::ErrorKind::InvalidData | io::ErrorKind::Other
        ) {
            return formatter.write_str(" detail=[redacted: may contain bootstrap data]");
        }
        if let Some(source) = self.0.get_ref() {
            write!(formatter, " error={source}")?;
        }
        Ok(())
    }
}

impl fmt::Display for Failure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "hermit-liteinst startup failed: stage={} ",
            self.stage()
        )?;
        match self {
            #[cfg(feature = "private-crt")]
            Self::PrivateStartup(_, error) => write!(formatter, "error={error}"),
            Self::Selection => formatter.write_str("error=unrecognized selector"),
            Self::MissingBootstrap => formatter.write_str("error=sealed bootstrap not found"),
            Self::Bootstrap(error)
            | Self::Transport(error)
            | Self::Formatter(error)
            | Self::Installation(error) => IoDetail(error).fmt(formatter),
            Self::Payload(error) => match error.as_str() {
                "unsupported bootstrap payload version"
                | "bootstrap tool mismatch"
                | "bootstrap config fingerprint mismatch" => write!(formatter, "error={error}"),
                _ => formatter.write_str("error=invalid sealed input detail=[redacted]"),
            },
            Self::MissingLog => formatter.write_str("error=sealed log endpoint missing"),
            Self::Subscriber(error) => write!(formatter, "error={error}"),
            Self::Record(error) => match error.as_ref() {
                RecordFailure::Sink(error) => write!(formatter, "sink {}", IoDetail(error)),
                error => write!(formatter, "error={error}"),
            },
        }
    }
}

pub(crate) fn selected(selection: Option<&OsStr>) -> Result<bool, Failure> {
    match selection {
        None => Ok(false),
        Some(value) if value == crate::BOOTSTRAP_SELECTOR => Ok(true),
        Some(_) => Err(Failure::Selection),
    }
}

struct Diagnostic {
    bytes: [u8; CAPACITY],
    used: usize,
    truncated: bool,
}

impl Diagnostic {
    fn format(arguments: fmt::Arguments<'_>) -> Self {
        let mut result = Self {
            bytes: [0; CAPACITY],
            used: 0,
            truncated: false,
        };
        let failed = result.write_fmt(arguments).is_err();
        let suffix = if result.truncated {
            TRUNCATED
        } else if failed {
            FORMAT_FAILED
        } else {
            b"\n"
        };
        result.bytes[result.used..result.used + suffix.len()].copy_from_slice(suffix);
        result.used += suffix.len();
        result
    }

    fn bytes(&self) -> &[u8] {
        &self.bytes[..self.used]
    }
}

impl fmt::Write for Diagnostic {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        for character in text.chars() {
            let mut encoded = [0; 4];
            let text = match character {
                '\n' => "\\n",
                '\r' => "\\r",
                '\t' => "\\t",
                character if character.is_control() => "?",
                character => character.encode_utf8(&mut encoded),
            };
            if text.len() > CAPACITY - FORMAT_FAILED.len() - self.used {
                self.truncated = true;
                return Err(fmt::Error);
            }
            self.bytes[self.used..self.used + text.len()].copy_from_slice(text.as_bytes());
            self.used += text.len();
        }
        Ok(())
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum WriteFailure {
    SignalMask(i64),
    Errno(i32),
    Zero,
    InvalidResult(i64),
    AttemptLimit,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Delivery {
    written: usize,
    failure: Option<WriteFailure>,
}

fn deliver(bytes: &[u8], mut write: impl FnMut(&[u8]) -> i64) -> Delivery {
    let mut written = 0;
    for _ in 0..MAX_WRITES {
        if written == bytes.len() {
            return Delivery {
                written,
                failure: None,
            };
        }
        let remaining = &bytes[written..];
        let result = write(remaining);
        let failure = match result {
            result if result == -(libc::EINTR as i64) => continue,
            -4095..=-1 => WriteFailure::Errno((-result) as i32),
            0 => WriteFailure::Zero,
            result if result > 0 && result as u64 <= remaining.len() as u64 => {
                written += result as usize;
                continue;
            }
            result => WriteFailure::InvalidResult(result),
        };
        return Delivery {
            written,
            failure: Some(failure),
        };
    }
    Delivery {
        written,
        failure: (written != bytes.len()).then_some(WriteFailure::AttemptLimit),
    }
}

pub(crate) fn finish(
    result: Result<i32, Failure>,
    write: impl FnMut(&[u8]) -> i64,
) -> (i32, Option<Delivery>) {
    match result {
        Ok(status) => (status, None),
        Err(error) => {
            let diagnostic = Diagnostic::format(format_args!("{error}"));
            (127, Some(deliver(diagnostic.bytes(), write)))
        }
    }
}

fn finish_terminal(
    result: Result<i32, Failure>,
    block_sigpipe: impl FnOnce() -> i64,
    write: impl FnMut(&[u8]) -> i64,
) -> (i32, Option<Delivery>) {
    match result {
        Ok(status) => (status, None),
        Err(error) => {
            let masked = block_sigpipe();
            if masked != 0 {
                return (
                    127,
                    Some(Delivery {
                        written: 0,
                        failure: Some(WriteFailure::SignalMask(masked)),
                    }),
                );
            }
            finish(Err(error), write)
        }
    }
}

/// Finish startup with terminal-only diagnostic output on failure.
///
/// # Safety
/// After a failure return the caller must terminate the process with status 127,
/// without resuming guest execution or restoring the signal mask: SIGPIPE may
/// remain blocked on this thread. The production initializer must return its
/// status directly to `clocked_initializer`'s assembly tail.
pub(crate) unsafe fn finish_initializer(result: Result<i32, Failure>) -> i32 {
    finish_terminal(result, block_sigpipe, stderr).0
}

fn block_sigpipe() -> i64 {
    let mask = 1u64 << (libc::SIGPIPE - 1);
    unsafe {
        reverie_preload::trap::raw_syscall6(
            libc::SYS_rt_sigprocmask,
            [libc::SIG_BLOCK as u64, (&raw const mask) as u64, 0, 8, 0, 0],
        )
    }
}

fn stderr(bytes: &[u8]) -> i64 {
    unsafe {
        reverie_preload::trap::raw_syscall6(
            libc::SYS_write,
            [
                libc::STDERR_FILENO as u64,
                bytes.as_ptr() as u64,
                bytes.len() as u64,
                0,
                0,
                0,
            ],
        )
    }
}

#[cfg(test)]
mod tests;
