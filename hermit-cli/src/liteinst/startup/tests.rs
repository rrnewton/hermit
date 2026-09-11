use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::fs::symlink;

use super::*;

pub(super) fn elf(interpreter: Option<&Path>) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    let mut bytes = vec![0; 4096];
    bytes[..7].copy_from_slice(b"\x7fELF\x02\x01\x01");
    bytes[16..18].copy_from_slice(&3u16.to_le_bytes());
    bytes[18..20].copy_from_slice(&62u16.to_le_bytes());
    bytes[20..24].copy_from_slice(&1u32.to_le_bytes());
    bytes[24..32].copy_from_slice(&256u64.to_le_bytes());
    bytes[32..40].copy_from_slice(&64u64.to_le_bytes());
    bytes[52..54].copy_from_slice(&64u16.to_le_bytes());
    bytes[54..56].copy_from_slice(&56u16.to_le_bytes());
    bytes[56..58].copy_from_slice(&(if interpreter.is_some() { 2u16 } else { 1u16 }).to_le_bytes());
    bytes[64..68].copy_from_slice(&1u32.to_le_bytes());
    bytes[68..72].copy_from_slice(&5u32.to_le_bytes());
    bytes[96..104].copy_from_slice(&4096u64.to_le_bytes());
    bytes[104..112].copy_from_slice(&4096u64.to_le_bytes());
    bytes[112..120].copy_from_slice(&4096u64.to_le_bytes());
    if let Some(path) = interpreter {
        let path = path.as_os_str().as_bytes();
        bytes[120..124].copy_from_slice(&3u32.to_le_bytes());
        bytes[128..136].copy_from_slice(&512u64.to_le_bytes());
        bytes[152..160].copy_from_slice(&((path.len() + 1) as u64).to_le_bytes());
        bytes[512..512 + path.len()].copy_from_slice(path);
    }
    bytes
}

fn executable(path: &Path, bytes: &[u8]) {
    fs::write(path, bytes).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn inputs() -> (tempfile::TempDir, Command, PathBuf) {
    let directory = tempfile::tempdir().unwrap();
    let loader = directory.path().join("ld.so");
    let program = directory.path().join("app");
    executable(&loader, &elf(None));
    executable(&program, &elf(Some(&loader)));
    (directory, Command::new(program), loader)
}

#[test]
fn sealed_original_interpreter_uses_held_bytes_after_path_replacement() {
    use std::os::fd::AsRawFd;

    use super::original_interpreter::OriginalInterpreterImage;
    let (directory, command, loader) = inputs();
    let selected = prepare_in_current_filesystem(&command, 4096).unwrap();
    let original = OriginalInterpreterImage::from_held_inputs(selected).unwrap();
    fs::rename(&loader, directory.path().join("original")).unwrap();
    executable(&loader, b"replacement must not be reopened");
    let mut bytes = vec![0; original.inputs().interpreter().bytes().len()];
    original.file().read_exact_at(&mut bytes, 0).unwrap();
    assert_eq!(bytes, elf(None));
    assert_ne!(
        original.file().metadata().unwrap().ino(),
        fs::metadata(loader).unwrap().ino()
    );
    let required = libc::F_SEAL_SEAL | libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_WRITE;
    assert_eq!(
        unsafe { libc::fcntl(original.file().as_raw_fd(), libc::F_GET_SEALS) },
        required
    );
    assert!(original.file().write_at(b"x", 0).is_err());
}

#[test]
fn sealed_original_interpreter_refuses_modified_held_inode() {
    use super::original_interpreter::OriginalInterpreterImage;
    let (_directory, command, loader) = inputs();
    let selected = prepare_in_current_filesystem(&command, 4096).unwrap();
    fs::write(loader, b"changed original inode").unwrap();
    assert!(OriginalInterpreterImage::from_held_inputs(selected).is_err());
}

#[test]
fn original_interpreter_selection_change_before_sealing_is_refused() {
    use super::original_interpreter::OriginalInterpreterImage;
    let (directory, command, loader) = inputs();
    let selected = prepare_in_current_filesystem(&command, 4096).unwrap();
    fs::rename(&loader, directory.path().join("original")).unwrap();
    executable(&loader, b"new loader path");
    assert!(OriginalInterpreterImage::from_held_inputs(selected).is_err());
}

#[test]
fn original_interpreter_descriptor_uses_real_command_child_inheritance() {
    use super::original_interpreter::OriginalInterpreterImage;
    let (_directory, command, _) = inputs();
    let selected = prepare_in_current_filesystem(&command, 4096).unwrap();
    let original = OriginalInterpreterImage::from_held_inputs(selected).unwrap();
    let mut child = Command::new("/bin/cat");
    child.stdout(crate::Stdio::piped());
    child.stderr(crate::Stdio::piped());
    let descriptor = original.inherit(&mut child).unwrap();
    assert!(descriptor >= 3);
    assert_eq!(
        unsafe { libc::fcntl(descriptor, libc::F_GETFD) },
        libc::FD_CLOEXEC
    );
    child.arg(format!("/proc/self/fd/{descriptor}"));
    let mut child = child.try_into_std().unwrap();
    let output = child.output().unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, original.inputs().interpreter().bytes());
    assert!(output.stderr.is_empty());
    drop(child);
    assert_eq!(unsafe { libc::fcntl(descriptor, libc::F_GETFD) }, -1);
    assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::EBADF));
}

#[test]
fn original_interpreter_duplicate_discovery_is_not_overwritten() {
    use super::original_interpreter::IMAGE_FD_ENV;
    use super::original_interpreter::OriginalInterpreterImage;
    let (_directory, command, _) = inputs();
    let selected = prepare_in_current_filesystem(&command, 4096).unwrap();
    let original = OriginalInterpreterImage::from_held_inputs(selected).unwrap();
    let mut child = Command::new("/bin/cat");
    child.env(IMAGE_FD_ENV, "existing");
    assert_eq!(
        original.inherit(&mut child).unwrap_err().kind(),
        io::ErrorKind::AlreadyExists
    );
    assert_eq!(
        &*child.get_env(IMAGE_FD_ENV).unwrap(),
        std::ffi::OsStr::new("existing")
    );
}

#[test]
fn actual_leaf_consumer_reads_same_sealed_original_object_and_borrows_fd() {
    use std::os::fd::AsFd;
    use std::os::fd::AsRawFd;

    use reverie_liteinst::startup::original_interpreter::OriginalInterpreter;

    use super::original_interpreter::OriginalInterpreterImage;
    let (_directory, command, _) = inputs();
    let selected = prepare_in_current_filesystem(&command, 4096).unwrap();
    let original = OriginalInterpreterImage::from_held_inputs(selected).unwrap();
    let received = OriginalInterpreter::acquire(original.file().as_fd()).unwrap();
    assert_eq!(received.bytes(), original.inputs().interpreter().bytes());
    let metadata = original.file().metadata().unwrap();
    assert_eq!(received.file_identity(), (metadata.dev(), metadata.ino()));
    drop(received);
    assert!(unsafe { libc::fcntl(original.file().as_raw_fd(), libc::F_GETFD) } >= 0);
    assert_eq!(
        super::original_interpreter::IMAGE_FD_ENV,
        reverie_liteinst::startup::original_interpreter::IMAGE_FD_ENV
    );
}

#[test]
fn actual_leaf_consumer_refuses_unsealed_original_file() {
    use std::os::fd::AsFd;

    use reverie_liteinst::startup::original_interpreter::OriginalInterpreter;
    let (_directory, command, _) = inputs();
    let selected = prepare_in_current_filesystem(&command, 4096).unwrap();
    assert!(OriginalInterpreter::acquire(selected.interpreter().file().as_fd()).is_err());
}

#[test]
fn command_path_and_absolute_symlink_use_current_filesystem() {
    let (directory, _, loader) = inputs();
    let alias = directory.path().join("loader-alias");
    symlink(&loader, &alias).unwrap();
    let program = directory.path().join("app");
    executable(&program, &elf(Some(&alias)));
    let mut command = Command::new("app");
    command.env_clear().env("PATH", directory.path());
    let prepared = prepare_in_current_filesystem(&command, 4096).unwrap();
    assert_eq!(prepared.program().lookup_path(), program);
    assert_eq!(prepared.interpreter_path(), alias);
    assert_eq!(
        prepared.interpreter().identity().inode,
        fs::metadata(loader).unwrap().ino()
    );
    assert_eq!(prepared.interpreter().bytes(), elf(None));
    assert_eq!(
        prepared.interpreter().identity().sha256,
        <[u8; 32]>::from(Sha256::digest(elf(None)))
    );
    assert_eq!(prepared.required_span(), 4096);
    assert_eq!(prepared.required_alignment(), 4096);
    prepared.program().revalidate().unwrap();
    prepared.interpreter().revalidate().unwrap();
}

#[test]
fn relative_interpreter_uses_command_working_directory() {
    let (directory, mut command, loader) = inputs();
    executable(
        &directory.path().join("app"),
        &elf(Some(Path::new("ld.so"))),
    );
    command.current_dir(directory.path());
    let prepared = prepare_in_current_filesystem(&command, 4096).unwrap();
    assert_eq!(prepared.interpreter().lookup_path(), loader);
    assert_eq!(prepared.interpreter_path(), Path::new("ld.so"));
}

#[test]
fn command_path_miss_does_not_use_ambient_path() {
    let (directory, _, _) = inputs();
    let mut command = Command::new("app");
    command
        .env_clear()
        .env("PATH", directory.path().join("missing"));
    assert!(prepare_in_current_filesystem(&command, 4096).is_err());
}

#[test]
fn descriptor_survives_path_replacement_without_reopening() {
    let (directory, command, loader) = inputs();
    let prepared = prepare_in_current_filesystem(&command, 4096).unwrap();
    fs::rename(&loader, directory.path().join("original")).unwrap();
    executable(&loader, b"replacement");
    let retained = prepared.interpreter();
    assert_eq!(
        retained.file().metadata().unwrap().ino(),
        retained.identity().inode
    );
    assert_ne!(
        fs::metadata(loader).unwrap().ino(),
        retained.identity().inode
    );
    let mut actual = vec![0; retained.bytes().len()];
    retained.file().read_exact_at(&mut actual, 0).unwrap();
    assert_eq!(actual, retained.bytes());
}

#[test]
fn same_inode_change_does_not_change_snapshot_and_fails_revalidation() {
    let (_directory, command, loader) = inputs();
    let prepared = prepare_in_current_filesystem(&command, 4096).unwrap();
    let mut changed = elf(None);
    changed[1024] = 42;
    fs::write(&loader, changed).unwrap();
    assert_eq!(prepared.interpreter().bytes(), elf(None));
    assert_eq!(
        prepared.interpreter().revalidate().unwrap_err().kind(),
        io::ErrorKind::InvalidData
    );
}

#[test]
fn checked_reverie_layout_rejects_writable_executable_loader() {
    let (_directory, command, loader) = inputs();
    let mut bytes = elf(None);
    bytes[68..72].copy_from_slice(&7u32.to_le_bytes());
    executable(&loader, &bytes);
    let error = prepare_in_current_filesystem(&command, 4096).err().unwrap();
    assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    assert_eq!(error.to_string(), "unsupported load permissions");
    assert!(error.get_ref().unwrap().is::<PlanError>());
}

#[test]
fn missing_interp_and_malformed_input_are_distinct_errors() {
    let (directory, command, _) = inputs();
    let program = directory.path().join("app");
    executable(&program, &elf(None));
    let error = prepare_in_current_filesystem(&command, 4096).err().unwrap();
    assert_eq!(error.kind(), io::ErrorKind::Unsupported);
    assert_eq!(error.to_string(), "startup preparation requires PT_INTERP");
    executable(&program, b"not an ELF");
    assert_eq!(
        prepare_in_current_filesystem(&command, 4096)
            .err()
            .unwrap()
            .kind(),
        io::ErrorKind::InvalidData
    );
}

#[test]
fn image_limit_and_nonexecutable_interpreter_refuse() {
    let (_directory, command, loader) = inputs();
    assert_eq!(
        prepare_in_current_filesystem(&command, 4095)
            .err()
            .unwrap()
            .kind(),
        io::ErrorKind::InvalidData
    );
    fs::set_permissions(&loader, fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(
        prepare_in_current_filesystem(&command, 4096)
            .err()
            .unwrap()
            .kind(),
        io::ErrorKind::InvalidInput
    );
}

#[test]
fn duplicate_interpreter_is_rejected_by_reader_and_startup() {
    let (directory, command, loader) = inputs();
    let program = directory.path().join("app");
    let mut bytes = elf(Some(&loader));
    bytes[56..58].copy_from_slice(&3u16.to_le_bytes());
    bytes.copy_within(120..176, 176);
    executable(&program, &bytes);
    assert_eq!(crate::interp::elf_get_interp(&program), None);
    let error = prepare_in_current_filesystem(&command, 4096).err().unwrap();
    assert_eq!(error.to_string(), "duplicate PT_INTERP");
}

#[test]
fn truncated_interpreter_segment_retains_io_error() {
    let (directory, command, loader) = inputs();
    let mut bytes = elf(Some(&loader));
    bytes[128..136].copy_from_slice(&4096u64.to_le_bytes());
    executable(&directory.path().join("app"), &bytes);
    assert_eq!(
        prepare_in_current_filesystem(&command, 4096)
            .err()
            .unwrap()
            .kind(),
        io::ErrorKind::UnexpectedEof
    );
}
