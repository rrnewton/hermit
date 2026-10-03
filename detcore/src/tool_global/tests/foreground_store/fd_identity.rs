//! Descriptor-release checks for fixtures sharing a process with parallel tests.

use std::io;
use std::os::fd::AsRawFd;
use std::os::fd::OwnedFd;
use std::os::fd::RawFd;

fn stat(fd: RawFd) -> io::Result<libc::stat> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe { libc::fstat(fd, stat.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful fstat initialized the complete stat object.
    Ok(unsafe { stat.assume_init() })
}

pub(super) fn check_original_socket_released(original: RawFd, audit: &OwnedFd) -> io::Result<()> {
    // This owned alias keeps the original socket inode alive throughout both
    // observations, so its identity cannot be recycled for another object.
    let held = stat(audit.as_raw_fd())?;
    if held.st_mode & libc::S_IFMT != libc::S_IFSOCK {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "audit is not a socket",
        ));
    }
    match stat(original) {
        Err(error) if error.raw_os_error() == Some(libc::EBADF) => Ok(()),
        Err(error) => Err(error),
        Ok(current) if (current.st_dev, current.st_ino) != (held.st_dev, held.st_ino) => Ok(()),
        Ok(_) => Err(io::Error::other("original socket alias remains open")),
    }
}

#[test]
fn original_socket_release_refuses_still_open_alias() {
    use std::io::Read;
    use std::os::unix::net::UnixStream;

    let (original, mut peer) = UnixStream::pair().unwrap();
    let audit: OwnedFd = original.try_clone().unwrap().into();
    let slot = original.as_raw_fd();
    let error = check_original_socket_released(slot, &audit).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::Other);
    assert_eq!(error.to_string(), "original socket alias remains open");
    drop(original);
    check_original_socket_released(slot, &audit).unwrap();
    drop(audit);
    assert_eq!(peer.read(&mut [0u8; 1]).unwrap(), 0);
}

#[test]
fn original_socket_release_accepts_recycled_slot_with_different_object() {
    use std::io::Read;
    use std::io::Write;
    use std::os::unix::net::UnixStream;

    let (mut original, mut original_peer) = UnixStream::pair().unwrap();
    let audit: OwnedFd = original.try_clone().unwrap().into();
    let slot = original.as_raw_fd();
    let (replacement, mut replacement_peer) = UnixStream::pair().unwrap();
    assert_ne!(replacement.as_raw_fd(), slot);
    assert_eq!(
        unsafe { libc::dup3(replacement.as_raw_fd(), slot, libc::O_CLOEXEC) },
        slot
    );
    drop(replacement);
    check_original_socket_released(slot, &audit).unwrap();

    // Both identities remain real: the reused slot receives the replacement's
    // bytes, and the retained audit alias still receives the original's bytes.
    replacement_peer.write_all(b"replacement").unwrap();
    let mut replaced_bytes = [0u8; 11];
    original.read_exact(&mut replaced_bytes).unwrap();
    assert_eq!(&replaced_bytes, b"replacement");
    original_peer.write_all(b"original").unwrap();
    let mut audit = UnixStream::from(audit);
    let mut original_bytes = [0u8; 8];
    audit.read_exact(&mut original_bytes).unwrap();
    assert_eq!(&original_bytes, b"original");
}
