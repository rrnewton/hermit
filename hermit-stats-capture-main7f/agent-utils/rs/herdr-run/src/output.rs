//! Writing to stdout and stderr with every failure accounted for.
//!
//! `print!` panics when stdout cannot be written (exit 101, a panic message instead of a
//! diagnostic) and the standard `Stdout` handle silently swallows a closed descriptor. herdr-run
//! promises one behaviour in both editions instead: a stdout write that fails is reported as
//! `herdr-run: cannot write stdout: <reason>` with exit status 1, whether the reason is a closed
//! descriptor, a full disk, or a reader that went away. Diagnostics on stderr are best effort,
//! because there is nowhere left to report a failure to write them.

use std::fs::File;
use std::io::Write;
use std::mem::ManuallyDrop;
use std::os::unix::io::FromRawFd;

use crate::error::{ErrorKind, HerdrRunError, Result};

fn write_descriptor(descriptor: i32, bytes: &[u8]) -> std::io::Result<()> {
    // SAFETY: the descriptor is borrowed, never closed: `ManuallyDrop` keeps `File`'s destructor
    // from running. Writing to a descriptor that is not open simply returns EBADF.
    let mut file = ManuallyDrop::new(unsafe { File::from_raw_fd(descriptor) });
    file.write_all(bytes)?;
    file.flush()
}

/// Write bytes to descriptor 1 or 2, failing with `cannot write <label>: <reason>`.
pub fn write_labelled(descriptor: i32, bytes: &[u8], label: &str) -> Result<()> {
    write_descriptor(descriptor, bytes).map_err(|error| {
        HerdrRunError::new(ErrorKind::Other, format!("cannot write {label}: {error}"))
    })
}

/// Write bytes to stdout, failing with `cannot write stdout: <reason>`.
pub fn stdout_bytes(bytes: &[u8]) -> Result<()> {
    write_labelled(1, bytes, "stdout")
}

/// Write text to stdout, failing with `cannot write stdout: <reason>`.
pub fn stdout(text: &str) -> Result<()> {
    stdout_bytes(text.as_bytes())
}

/// Write bytes to stderr, failing with `cannot write stderr: <reason>`.
///
/// Used for a command's own captured stderr, which is part of the result rather than a
/// diagnostic.
pub fn stderr_bytes(bytes: &[u8]) -> Result<()> {
    write_labelled(2, bytes, "stderr")
}

/// Write one diagnostic line to stderr, ignoring failure.
pub fn diagnostic(text: &str) {
    let mut line = String::with_capacity(text.len() + 1);
    line.push_str(text);
    line.push('\n');
    let _ = write_descriptor(2, line.as_bytes());
}
