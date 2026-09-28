use std::ffi::OsStr;
use std::fs;
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::Duration;

use super::*;

fn put(bytes: &mut [u8], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn runtime_bytes() -> Vec<u8> {
    let mut bytes = private_runtime::tests::image_bytes();
    put(&mut bytes, 24, 258);
    put(&mut bytes, 608, 4);
    put(&mut bytes, 672, 72);
    let name = b"\0pe_private_crt_entry\0pe_kernel_entry\0";
    put(&mut bytes, 728, 400);
    put(&mut bytes, 736, name.len() as u64);
    bytes[400..400 + name.len()].copy_from_slice(name);
    bytes[336..360].fill(0);
    bytes[336..340].copy_from_slice(&22u32.to_le_bytes());
    bytes[340] = 0x12;
    bytes[341] = 2;
    bytes[342..344].copy_from_slice(&1u16.to_le_bytes());
    put(&mut bytes, 344, 258);
    put(&mut bytes, 352, 2);
    bytes
}

fn write(path: &Path, bytes: &[u8], mode: u32) {
    fs::write(path, bytes).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

fn closed(descriptor: i32) {
    assert_eq!(unsafe { libc::fcntl(descriptor, libc::F_GETFD) }, -1);
    assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::EBADF));
}

fn loaded_libc() -> PathBuf {
    fs::read_to_string("/proc/self/maps")
        .unwrap()
        .lines()
        .filter_map(|line| line.split_whitespace().last())
        .map(PathBuf::from)
        .find(|path| {
            path.is_absolute()
                && path
                    .file_name()
                    .is_some_and(|name| name.as_encoded_bytes().starts_with(b"libc.so"))
        })
        .expect("the supported glibc host must map libc")
}

fn mapping_has_identity(output: &[u8], metadata: &fs::Metadata) -> bool {
    String::from_utf8_lossy(output).lines().any(|line| {
        let mut fields = line.split_whitespace();
        let _range = fields.next();
        let _permissions = fields.next();
        let _offset = fields.next();
        let device = fields.next();
        let inode = fields.next();
        let Some((major, minor)) = device.and_then(|device| device.split_once(':')) else {
            return false;
        };
        let Some(device) = u32::from_str_radix(major, 16)
            .ok()
            .zip(u32::from_str_radix(minor, 16).ok())
            .map(|(major, minor)| libc::makedev(major, minor))
        else {
            return false;
        };
        device == metadata.dev()
            && inode.and_then(|inode| inode.parse::<u64>().ok()) == Some(metadata.ino())
    })
}

fn fixture(bytes: &[u8]) -> (tempfile::TempDir, Command, PinnedImage, PathBuf) {
    let directory = tempfile::tempdir().unwrap();
    let loader = directory.path().join("loader");
    write(&loader, &super::super::tests::elf(None), 0o755);
    let program = directory.path().join("guest");
    write(&program, &super::super::tests::elf(Some(&loader)), 0o755);
    let runtime = directory.path().join("runtime");
    write(&runtime, bytes, 0o755);
    let image = PinnedImage::open(runtime, 4096).unwrap();
    let mut command = Command::new(&program);
    command
        .arg0("original-argv-zero")
        .args(["first", "two words"]);
    command
        .env_clear()
        .env("ORIGINAL", "value")
        .env("LD_PRELOAD", "guest.so");
    command.current_dir(directory.path());
    (directory, command, image, program)
}

#[test]
fn private_selection_keeps_command_and_both_sealed_owners() {
    let (directory, command, image, program) = fixture(&runtime_bytes());
    let RuntimeLaunch::Private(mut launch) = select(command, image).unwrap() else {
        panic!("private executable reached preload route");
    };
    let runtime_fd = launch.runtime().inherited_fd();
    let interpreter_fd = launch.inherited_fd();
    assert_ne!(runtime_fd, interpreter_fd);
    for (file, inherited) in [
        (launch.runtime().image().file(), runtime_fd),
        (launch.original().file(), interpreter_fd),
    ] {
        let copy = fs::metadata(format!("/proc/self/fd/{inherited}")).unwrap();
        let held = file.metadata().unwrap();
        assert_eq!((copy.dev(), copy.ino()), (held.dev(), held.ino()));
        assert_ne!(
            unsafe { libc::fcntl(inherited, libc::F_GETFD) } & libc::FD_CLOEXEC,
            0
        );
        assert_eq!(
            unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GET_SEALS) },
            libc::F_SEAL_SEAL | libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_WRITE
        );
    }
    let command = launch.command_mut();
    assert_eq!(command.get_program(), program);
    assert!(format!("{command:?}").contains("original-argv-zero"));
    assert_eq!(
        command.get_args().collect::<Vec<_>>(),
        ["first", "two words"]
    );
    assert_eq!(command.get_current_dir(), Some(directory.path()));
    let env = command
        .get_envs()
        .collect::<std::collections::BTreeMap<_, _>>();
    assert_eq!(env.len(), 4);
    assert_eq!(
        env[std::ffi::OsStr::new("ORIGINAL")],
        Some(std::ffi::OsStr::new("value"))
    );
    assert_eq!(
        env[std::ffi::OsStr::new("LD_PRELOAD")],
        Some(std::ffi::OsStr::new("guest.so"))
    );
    assert_eq!(
        env[std::ffi::OsStr::new(private_runtime::IMAGE_FD_ENV)],
        Some(std::ffi::OsStr::new(&runtime_fd.to_string()))
    );
    assert_eq!(
        env[std::ffi::OsStr::new(super::super::original_interpreter::IMAGE_FD_ENV)],
        Some(std::ffi::OsStr::new(&interpreter_fd.to_string()))
    );
    let (sink, _) = reverie_rpc_transport::guest_log::retained_log(
        reverie_rpc_transport::guest_log::Options::bounded(1024),
    );
    let (observer, future) =
        reverie_liteinst::LiteinstBackend::prepare_with_owned_command_data_and_log_sink::<(), _>(
            owned_command(*launch, None),
            (),
            Vec::new(),
            sink,
            reverie_liteinst::run_evidence::StdioMode::Captured,
        );
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let error = runtime.block_on(future).unwrap_err();
    assert!(
        matches!(&error.cause, reverie::Error::Io(error) if error.kind() == io::ErrorKind::Unsupported)
    );
    assert!(!observer.try_snapshot().unwrap().spawned);
    assert!(error.to_string().contains("refusing preload fallback"));
    for descriptor in [runtime_fd, interpreter_fd] {
        assert_eq!(unsafe { libc::fcntl(descriptor, libc::F_GETFD) }, -1);
        assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::EBADF));
    }
}

#[test]
fn missing_wrong_or_overlapping_kernel_entry_never_selects_preload() {
    let mut wrong = runtime_bytes();
    put(&mut wrong, 24, 256);
    let mut overlap = runtime_bytes();
    put(&mut overlap, 344, 256);
    for bytes in [private_runtime::tests::image_bytes(), wrong, overlap] {
        let (_directory, command, image, _program) = fixture(&bytes);
        assert!(select(command, image).is_err());
    }
}

#[test]
fn private_preparation_rejects_descriptor_overrides_and_lost_container_state() {
    for variable in [
        private_runtime::IMAGE_FD_ENV,
        super::super::original_interpreter::IMAGE_FD_ENV,
    ] {
        let (_directory, mut command, image, _program) = fixture(&runtime_bytes());
        command.env(variable, "91");
        assert!(select(command, image).is_err());
    }
    let (_directory, mut command, image, _program) = fixture(&runtime_bytes());
    command.hostname("must-not-discard");
    assert!(select(command, image).is_err());
}

#[test]
fn runtime_permissions_and_held_bytes_are_not_bypassed() {
    let (_directory, command, image, _program) = fixture(&runtime_bytes());
    let path = image.lookup_path().to_owned();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    let image = PinnedImage::open_with_permissions(path.clone(), 4096, false).unwrap();
    assert!(select(command, image).is_err());
    let (_directory, command, image, _program) = fixture(&runtime_bytes());
    fs::write(image.lookup_path(), b"changed").unwrap();
    assert!(select(command, image).is_err());
}

#[test]
fn structural_fixture_does_not_bypass_provenance() {
    let (_directory, command, image, _program) = fixture(&runtime_bytes());
    assert!(prepare(command, image.lookup_path()).is_err());
}

#[test]
fn relative_runtime_lookup_uses_command_directory() {
    let (directory, command, _image, _program) = fixture(&runtime_bytes());
    assert_eq!(
        super::super::lookup_path(&command, Path::new("runtime")).unwrap(),
        directory.path().join("runtime")
    );
}

#[test]
fn private_command_uses_held_program_selection_without_later_path_search() {
    let (directory, mut command, image, program) = fixture(&runtime_bytes());
    command
        .program("guest")
        .arg0("held-program-zero")
        .env("PATH", directory.path());
    let RuntimeLaunch::Private(mut launch) = select(command, image).unwrap() else {
        panic!("private executable reached preload route");
    };
    assert_eq!(launch.command_mut().get_program(), program);
    assert_eq!(launch.original().inputs().program().lookup_path(), program);
    assert!(format!("{:?}", launch.command_mut()).contains("held-program-zero"));
}

#[test]
fn non_executable_preload_file_keeps_existing_route() {
    let mut bytes = super::super::tests::elf(None);
    put(&mut bytes, 24, 0);
    let (_directory, command, image, _program) = fixture(&bytes);
    fs::set_permissions(image.lookup_path(), fs::Permissions::from_mode(0o644)).unwrap();
    let image =
        PinnedImage::open_with_permissions(image.lookup_path().to_owned(), 4096, false).unwrap();
    assert!(matches!(
        select(command, image).unwrap(),
        RuntimeLaunch::Preload(_)
    ));
}

#[test]
fn preload_preparation_refuses_runtime_descriptor_override() {
    let mut bytes = super::super::tests::elf(None);
    put(&mut bytes, 24, 0);
    let (_directory, mut command, image, _program) = fixture(&bytes);
    command.env(
        crate::liteinst_bootstrap::RUNTIME_IMAGE_FD_ENV,
        "existing-selection",
    );
    assert_eq!(
        select(command, image).err().unwrap().kind(),
        io::ErrorKind::AlreadyExists
    );
}

#[test]
fn preload_command_boundary_loads_sealed_object_after_path_replacement() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("runtime");
    let libc = loaded_libc();
    let expected = fs::read(&libc).unwrap();
    write(&path, &expected, 0o644);
    let image =
        PinnedImage::open_with_permissions(path.clone(), private_runtime::MAX_IMAGE_BYTES, false)
            .unwrap();
    let mut command = Command::new("/bin/sh");
    command.args([
        "-c",
        "while IFS= read -r line; do case \"$line\" in *hermit-liteinst-preload*) printf '%s\\n' \"$line\";; esac; done < /proc/self/maps",
    ]);
    command.stdout(crate::Stdio::piped());
    command.stderr(crate::Stdio::piped());
    command.env_clear().env("LD_PRELOAD", &libc);
    let RuntimeLaunch::Preload(launch) = select(command, image).unwrap() else {
        panic!("shared object reached private route");
    };

    let source_fd = launch.source().file().as_raw_fd();
    let sealed_fd = launch.file().as_raw_fd();
    let inherited_fd = launch.inherited_fd();
    let sealed_identity = launch.file().metadata().unwrap();
    fs::remove_file(&path).unwrap();
    write(&path, b"replacement", 0o644);
    let (mut command, owner) = prepare_preload_command(*launch).unwrap();
    assert_eq!(owner.source().bytes(), expected);
    assert_eq!(fs::read(&path).unwrap(), b"replacement");
    assert_eq!(
        unsafe { libc::fcntl(owner.file().as_raw_fd(), libc::F_GET_SEALS) },
        libc::F_SEAL_SEAL | libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_WRITE
    );
    let preload = PathBuf::from(format!("/proc/self/fd/{inherited_fd}"));
    let expected_ld_preload = {
        let mut value = preload.clone().into_os_string();
        value.push(OsStr::new(":"));
        value.push(&libc);
        value
    };
    assert_eq!(
        command
            .get_envs()
            .find(|(key, _)| *key == OsStr::new("LD_PRELOAD"))
            .and_then(|(_, value)| value),
        Some(expected_ld_preload.as_os_str())
    );
    assert_eq!(
        command
            .get_envs()
            .find(|(key, _)| {
                *key == OsStr::new(crate::liteinst_bootstrap::RUNTIME_IMAGE_FD_ENV)
            })
            .and_then(|(_, value)| value),
        Some(OsStr::new(&inherited_fd.to_string()))
    );
    let output = command.output().unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("memfd:hermit-liteinst-preload"),
        "sealed preload mapping absent: {output:?}"
    );
    assert!(mapping_has_identity(&output.stdout, &sealed_identity));
    assert!(output.stderr.is_empty());
    drop(command);
    closed(inherited_fd);
    assert_ne!(unsafe { libc::fcntl(source_fd, libc::F_GETFD) }, -1);
    assert_ne!(unsafe { libc::fcntl(sealed_fd, libc::F_GETFD) }, -1);
    drop(owner);
    closed(source_fd);
    closed(sealed_fd);
}

#[test]
fn preload_backend_future_cancellation_before_polling_cleans_descriptors() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("runtime");
    let mut bytes = super::super::tests::elf(None);
    put(&mut bytes, 24, 0);
    write(&path, &bytes, 0o644);
    let image = PinnedImage::open_with_permissions(path.clone(), 4096, false).unwrap();
    let RuntimeLaunch::Preload(launch) = select(Command::new("/must-not-launch"), image).unwrap()
    else {
        panic!("shared object reached private route");
    };
    let source_fd = launch.source().file().as_raw_fd();
    let sealed_fd = launch.file().as_raw_fd();
    let inherited_fd = launch.inherited_fd();
    fs::remove_file(&path).unwrap();
    write(&path, b"replacement", 0o644);
    let prepared = owned_preload_command(*launch).unwrap();
    let (sink, _) = reverie_rpc_transport::guest_log::retained_log(
        reverie_rpc_transport::guest_log::Options::bounded(1024),
    );
    let (observer, future) =
        reverie_liteinst::LiteinstBackend::prepare_with_owned_command_data_and_log_sink::<(), _>(
            prepared,
            (),
            Vec::new(),
            sink,
            reverie_liteinst::run_evidence::StdioMode::Captured,
        );
    let snapshot = observer.try_snapshot().unwrap();
    assert!(!snapshot.polled);
    assert!(!snapshot.spawned);
    for descriptor in [source_fd, sealed_fd, inherited_fd] {
        assert_ne!(unsafe { libc::fcntl(descriptor, libc::F_GETFD) }, -1);
    }
    drop(future);
    for descriptor in [source_fd, sealed_fd, inherited_fd] {
        closed(descriptor);
    }
}

fn fake_preload_launch(command: Command) -> (Box<PreparedPreloadLaunch>, [i32; 3]) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("runtime");
    let mut bytes = super::super::tests::elf(None);
    put(&mut bytes, 24, 0);
    write(&path, &bytes, 0o644);
    let image = PinnedImage::open_with_permissions(path.clone(), 4096, false).unwrap();
    let RuntimeLaunch::Preload(launch) = select(command, image).unwrap() else {
        panic!("shared object reached private route");
    };
    let descriptors = [
        launch.source().file().as_raw_fd(),
        launch.file().as_raw_fd(),
        launch.inherited_fd(),
    ];
    fs::remove_file(&path).unwrap();
    write(&path, b"replacement", 0o644);
    (launch, descriptors)
}

fn backend_future(
    launch: Box<PreparedPreloadLaunch>,
) -> (
    reverie_liteinst::run_evidence::RunObserver,
    impl std::future::Future<
        Output = Result<(std::process::Output, ()), reverie_liteinst::LoggedRunError>,
    >,
) {
    let prepared = owned_preload_command(*launch).unwrap();
    let (sink, _) = reverie_rpc_transport::guest_log::retained_log(
        reverie_rpc_transport::guest_log::Options::bounded(1024),
    );
    reverie_liteinst::LiteinstBackend::prepare_with_owned_command_data_and_log_sink::<(), _>(
        prepared,
        (),
        Vec::new(),
        sink,
        reverie_liteinst::run_evidence::StdioMode::Captured,
    )
}

#[test]
fn preload_spawn_failure_cleans_all_parent_descriptors() {
    let directory = tempfile::tempdir().unwrap();
    let program = directory.path().join("invalid-executable");
    write(&program, b"not an executable image", 0o755);
    let (launch, descriptors) = fake_preload_launch(Command::new(program));
    let (observer, future) = backend_future(launch);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    assert!(runtime.block_on(future).is_err());
    let snapshot = observer.try_snapshot().unwrap();
    assert!(snapshot.polled && snapshot.worker_submitted && !snapshot.spawned && !snapshot.reaped);
    for descriptor in descriptors {
        closed(descriptor);
    }
}

#[test]
fn preload_spawn_success_retains_owner_until_reap_then_cleans_descriptors() {
    let (launch, descriptors) = fake_preload_launch(Command::new("/bin/true"));
    let (observer, future) = backend_future(launch);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    assert!(runtime.block_on(future).is_err());
    let snapshot = observer.try_snapshot().unwrap();
    assert!(snapshot.spawned && snapshot.reaped);
    assert_eq!(snapshot.wait_status.unwrap().code(), Some(0));
    for descriptor in descriptors {
        closed(descriptor);
    }
}

#[test]
fn preload_cancellation_after_spawn_reaps_before_owner_cleanup() {
    let mut command = Command::new("/bin/sleep");
    command.arg("30");
    let (launch, descriptors) = fake_preload_launch(command);
    let (observer, future) = backend_future(launch);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut future = Box::pin(future);
    runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                tokio::select! {
                    result = &mut future => panic!("sleep returned before cancellation: {result:?}"),
                    _ = tokio::task::yield_now() => {},
                }
                if observer.try_snapshot().is_ok_and(|snapshot| snapshot.spawned) {
                    break;
                }
            }
        })
        .await
        .unwrap();
    });
    assert_ne!(unsafe { libc::fcntl(descriptors[0], libc::F_GETFD) }, -1);
    assert_ne!(unsafe { libc::fcntl(descriptors[1], libc::F_GETFD) }, -1);
    drop(future);
    drop(runtime);
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while !observer
        .try_snapshot()
        .is_ok_and(|snapshot| snapshot.reaped)
    {
        assert!(std::time::Instant::now() < deadline, "child was not reaped");
        std::thread::yield_now();
    }
    assert!(observer.try_snapshot().unwrap().caller_cancelled);
    for descriptor in descriptors {
        closed(descriptor);
    }
}
