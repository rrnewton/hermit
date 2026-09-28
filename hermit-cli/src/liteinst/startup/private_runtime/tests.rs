use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;

use sha2::Digest;
use sha2::Sha256;

use super::*;

const CHILD: &str = "HERMIT_PRIVATE_IMAGE_HOST_CONTROL";
const TEST: &str =
    "liteinst::startup::private_runtime::tests::transfer_uses_real_spawn_and_same_sealed_object";
const SEALS: i32 = libc::F_SEAL_SEAL | libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_WRITE;

fn put(bytes: &mut [u8], offset: usize, value: u64, width: usize) {
    bytes[offset..offset + width].copy_from_slice(&value.to_le_bytes()[..width]);
}

pub(in crate::liteinst::startup) fn image_bytes() -> Vec<u8> {
    let mut bytes = vec![0; 768];
    bytes[..7].copy_from_slice(b"\x7fELF\x02\x01\x01");
    for (offset, value, width) in [
        (16, 3, 2),
        (18, 62, 2),
        (20, 1, 4),
        (32, 64, 8),
        (40, 512, 8),
        (52, 64, 2),
        (54, 56, 2),
        (56, 1, 2),
        (58, 64, 2),
        (60, 4, 2),
    ] {
        put(&mut bytes, offset, value, width);
    }
    for (offset, value, width) in [
        (64, 1, 4),
        (68, 5, 4),
        (96, 768, 8),
        (104, 768, 8),
        (112, 4096, 8),
        (580, 1, 4),
        (584, 6, 8),
        (592, 256, 8),
        (600, 256, 8),
        (608, 2, 8),
        (644, 2, 4),
        (664, 288, 8),
        (672, 48, 8),
        (680, 3, 4),
        (684, 1, 4),
        (696, 24, 8),
        (708, 3, 4),
        (728, 352, 8),
        (736, 22, 8),
    ] {
        put(&mut bytes, offset, value, width);
    }
    bytes[256..258].copy_from_slice(&[0x0f, 0x0b]);
    bytes[352..374].copy_from_slice(b"\0pe_private_crt_entry\0");
    put(&mut bytes, 312, 1, 4);
    bytes[316] = 0x12;
    bytes[317] = 2;
    put(&mut bytes, 318, 1, 2);
    put(&mut bytes, 320, 256, 8);
    put(&mut bytes, 328, 2, 8);
    bytes
}

fn prepare(path: &Path) -> PrivateRuntimeImage {
    let bytes = std::fs::read(path).unwrap();
    prepare_in_current_filesystem(
        &Command::new("/bin/true"),
        path,
        Sha256::digest(bytes).into(),
        4096,
    )
    .unwrap()
}

fn write_image(path: &Path, bytes: &[u8]) {
    std::fs::write(path, bytes).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn closed(descriptor: i32) {
    assert_eq!(unsafe { libc::fcntl(descriptor, libc::F_GETFD) }, -1);
    assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::EBADF));
}

fn check_child(mode: &str) {
    if mode == "unrelated" {
        assert!(std::env::var_os(IMAGE_FD_ENV).is_none());
        let descriptor: i32 = std::env::var("HERMIT_PRIVATE_RESERVED_FD")
            .unwrap()
            .parse()
            .unwrap();
        let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
        if unsafe { libc::fstat(descriptor, metadata.as_mut_ptr()) } == 0 {
            let metadata = unsafe { metadata.assume_init() };
            assert_ne!(
                format!("{}:{}", metadata.st_dev, metadata.st_ino),
                std::env::var("HERMIT_PRIVATE_IMAGE_ID").unwrap()
            );
        } else {
            assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::EBADF));
        }
        return;
    }
    if mode == "absent" {
        assert!(std::env::var_os(IMAGE_FD_ENV).is_none());
        return;
    }
    let descriptor: i32 = std::env::var(IMAGE_FD_ENV).unwrap().parse().unwrap();
    assert!(descriptor > 2);
    assert_eq!(
        unsafe { libc::fcntl(descriptor, libc::F_GETFD) } & libc::FD_CLOEXEC,
        0
    );
    assert_eq!(unsafe { libc::fcntl(descriptor, libc::F_GET_SEALS) }, SEALS);
    let borrowed = unsafe { std::os::fd::BorrowedFd::borrow_raw(descriptor) };
    let mut file = File::from(borrowed.try_clone_to_owned().unwrap());
    let metadata = file.metadata().unwrap();
    assert_eq!(
        format!("{}:{}", metadata.dev(), metadata.ino()),
        std::env::var("HERMIT_PRIVATE_IMAGE_ID").unwrap()
    );
    let mut observed = vec![0; image_bytes().len()];
    use std::os::unix::fs::FileExt;
    file.read_exact_at(&mut observed, 0).unwrap();
    assert_eq!(observed, image_bytes());
    use std::io::Write;
    assert_eq!(
        file.write(b"X").unwrap_err().raw_os_error(),
        Some(libc::EPERM)
    );
    for key in ["HERMIT_PROTECTED_RPC_FD", "HERMIT_PROTECTED_LOG_FD"] {
        let protected: i32 = std::env::var(key).unwrap().parse().unwrap();
        assert!(protected > 2 && protected != descriptor);
        assert_eq!(
            unsafe { libc::write(protected, b"K".as_ptr().cast(), 1) },
            1
        );
    }
    assert_eq!(unsafe { libc::close(descriptor) }, 0);
    closed(descriptor);
    println!(
        "sealed-image-host-control: same-object bytes seals protected-fds child-close verified"
    );
}

#[tokio::test]
async fn transfer_uses_real_spawn_and_same_sealed_object() {
    if let Ok(mode) = std::env::var(CHILD) {
        check_child(&mode);
        return;
    }
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("runtime.elf");
    for standard in [false, true] {
        write_image(&path, &image_bytes());
        let image = prepare(&path);
        let metadata = image.file().metadata().unwrap();
        let sealed_fd = image.file().as_raw_fd();
        let source_fd = image.source().file().as_raw_fd();
        let mut command = Command::new(std::env::current_exe().unwrap());
        command.stdout(crate::Stdio::piped());
        command.stderr(crate::Stdio::piped());
        command.args([TEST, "--exact", "--nocapture", "--test-threads=1"]);
        command.env(CHILD, "sealed");
        command.env(
            "HERMIT_PRIVATE_IMAGE_ID",
            format!("{}:{}", metadata.dev(), metadata.ino()),
        );
        let mut peers = Vec::new();
        for key in ["HERMIT_PROTECTED_RPC_FD", "HERMIT_PROTECTED_LOG_FD"] {
            let (host, guest) = UnixStream::pair().unwrap();
            let descriptor = command.inherit_fd(guest.as_fd()).unwrap();
            command.env(key, descriptor.to_string());
            peers.push(host);
        }
        let mut launch = image.attach(command).unwrap();
        let inherited = launch.inherited_fd();
        assert!(inherited > 2 && inherited != sealed_fd && inherited != source_fd);
        assert_eq!(
            unsafe { libc::fcntl(inherited, libc::F_GETFD) } & libc::FD_CLOEXEC,
            libc::FD_CLOEXEC
        );
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, b"different object at the same name").unwrap();
        let unrelated = std::process::Command::new(std::env::current_exe().unwrap())
            .args([TEST, "--exact", "--test-threads=1"])
            .env(CHILD, "unrelated")
            .env("HERMIT_PRIVATE_RESERVED_FD", inherited.to_string())
            .env(
                "HERMIT_PRIVATE_IMAGE_ID",
                format!("{}:{}", metadata.dev(), metadata.ino()),
            )
            .output()
            .unwrap();
        assert!(unrelated.status.success(), "{unrelated:?}");
        if standard {
            let mut converted = launch.try_into_std().unwrap();
            let output = converted.command_mut().output().unwrap();
            assert!(output.status.success(), "{output:?}");
            assert!(String::from_utf8_lossy(&output.stdout).contains("same-object bytes seals"));
            assert_eq!(
                unsafe { libc::fcntl(inherited, libc::F_GETFD) } & libc::FD_CLOEXEC,
                libc::FD_CLOEXEC
            );
            drop(converted);
        } else {
            let output = launch.command_mut().output().await.unwrap();
            assert!(output.status.success(), "{output:?}");
            assert!(String::from_utf8_lossy(&output.stdout).contains("same-object bytes seals"));
            assert_eq!(
                unsafe { libc::fcntl(inherited, libc::F_GETFD) } & libc::FD_CLOEXEC,
                libc::FD_CLOEXEC
            );
            drop(launch);
        }
        closed(inherited);
        closed(sealed_fd);
        closed(source_fd);
        for mut peer in peers {
            let mut received = [0];
            peer.read_exact(&mut received).unwrap();
            assert_eq!(received, *b"K");
        }
    }
    let mut default = Command::new(std::env::current_exe().unwrap());
    default.args([TEST, "--exact", "--test-threads=1"]);
    default.env(CHILD, "absent");
    assert!(default.get_env(IMAGE_FD_ENV).is_none());
    assert!(default.output().await.unwrap().status.success());
}

#[test]
fn selection_is_context_correct_and_digest_pinned() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("runtime.elf");
    let bytes = image_bytes();
    write_image(&path, &bytes);
    let mut command = Command::new("/bin/true");
    command.current_dir(directory.path());
    let prepared = prepare_in_current_filesystem(
        &command,
        Path::new("runtime.elf"),
        Sha256::digest(&bytes).into(),
        4096,
    )
    .unwrap();
    assert_eq!(prepared.source().lookup_path(), path);
    assert_eq!(prepared.crt_symbol().virtual_address, 256);
    assert_eq!(prepared.crt_symbol().file_offset, 256);
    assert_eq!(prepared.crt_symbol().size, 2);
    assert_eq!(prepared.source().bytes(), bytes);
    assert_eq!(
        prepare_in_current_filesystem(&command, Path::new("runtime.elf"), [0; 32], 4096)
            .err()
            .unwrap()
            .kind(),
        io::ErrorKind::InvalidData
    );
    assert!(
        prepare_in_current_filesystem(
            &command,
            &path,
            Sha256::digest(&bytes).into(),
            bytes.len() - 1
        )
        .is_err()
    );
    assert!(
        prepare_in_current_filesystem(&command, &path, Sha256::digest(&bytes).into(), 0).is_err()
    );
}

#[test]
fn missing_or_preemptible_or_misplaced_crt_is_not_elf_entry_admission() {
    let original = image_bytes();
    assert!(validate_crt(&original).is_ok());
    for (offset, value, width) in [
        (316, 0x1a, 1),
        (317, 0, 1),
        (318, 0, 2),
        (320, 257, 8),
        (328, 3, 8),
        (68, 7, 4),
        (580, 8, 4),
        (584, 7, 8),
        (644, 0, 4),
    ] {
        let mut damaged = original.clone();
        put(&mut damaged, offset, value, width);
        assert!(validate_crt(&damaged).is_err(), "{offset}");
    }
    let mut entry_only = original;
    put(&mut entry_only, 24, 256, 8);
    entry_only[353] = b'x';
    assert_eq!(
        validate_crt(&entry_only).unwrap_err().kind(),
        io::ErrorKind::Unsupported
    );
    let actual = std::env::var_os("HERMIT_UNBOUND_RUNTIME_CONTROL")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::env::current_exe().unwrap());
    let bytes = std::fs::read(actual).unwrap();
    assert_eq!(
        validate_crt(&bytes).unwrap_err().kind(),
        io::ErrorKind::Unsupported
    );
}

#[test]
fn failure_and_command_drop_release_only_the_owned_parent_descriptors() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("runtime.elf");
    write_image(&path, &image_bytes());
    for failure in ["drop", "exec", "callback", "conversion", "collision"] {
        let image = prepare(&path);
        let sealed = image.file().as_raw_fd();
        let source = image.source().file().as_raw_fd();
        let mut command = Command::new("/nonexistent/private-runtime-host-control");
        if failure == "collision" {
            command.env(IMAGE_FD_ENV, "existing-selection");
            assert_eq!(
                image.attach(command).err().unwrap().kind(),
                io::ErrorKind::AlreadyExists
            );
        } else {
            if failure == "conversion" {
                command.chroot("/");
            }
            let launch = image.attach(command).unwrap();
            let descriptor = launch.inherited_fd();
            if failure == "conversion" {
                assert!(launch.try_into_std().is_err());
            } else {
                let mut launch = launch.try_into_std().unwrap();
                if failure == "exec" {
                    assert!(launch.command_mut().spawn().is_err());
                }
                if failure == "callback" {
                    unsafe {
                        launch
                            .command_mut()
                            .pre_exec(|| Err(io::Error::from_raw_os_error(libc::EPERM)));
                    }
                    assert_eq!(
                        launch.command_mut().spawn().unwrap_err().raw_os_error(),
                        Some(libc::EPERM)
                    );
                }
                assert_eq!(
                    unsafe { libc::fcntl(descriptor, libc::F_GETFD) } & libc::FD_CLOEXEC,
                    libc::FD_CLOEXEC
                );
                drop(launch);
            }
            closed(descriptor);
        }
        closed(sealed);
        closed(source);
    }
}
