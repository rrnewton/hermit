/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! LB2 retained-object and LB4 caller-file controls in bounded ordinary children.
//! Host qualification is explicitly MODELED. A changed interpreter pathname is
//! outside the frozen input contract: strict verification refuses it. Reading
//! or mapping the retained object demonstrates identity binding only, and does
//! not qualify that changed filesystem for a future launch.

mod exec_support;

use std::ffi::CString;
use std::fs::File;
use std::fs::{self};
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::RawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Stdio;
use std::time::Duration;

use reverie_elf_loader::ExecCheckOutcome;
use reverie_elf_loader::ExecRefusal;
use reverie_elf_loader::ExecRequest;
use reverie_elf_loader::Limits;
use reverie_elf_loader::LoaderHostFacts;
use reverie_elf_loader::PinnedStart;
use reverie_elf_loader::PrepareExecOptions;
use reverie_elf_loader::descriptors::DescriptorReservation;
use reverie_elf_loader::descriptors::START_DESCRIPTOR_SLOTS;
use reverie_elf_loader::descriptors::transfer_private_files;
use reverie_elf_loader::exec::FileIdentity;
use reverie_elf_loader::prepare_exec;
use reverie_elf_loader::prepare_start_from_files;

const ROLE_ENV: &str = "REVERIE_LB_PINNED_IMAGE_ROLE";
const CHILD_TEST: &str = "pinned_image_child";
const COMPLETE: &str = "LB_PINNED_IMAGE_CONTROL_COMPLETE";

#[test]
fn lb_prepare_child() {
    exec_support::prepare_child_entry();
}

#[test]
fn pinned_image_child() {
    let Some(role) = std::env::var_os(ROLE_ENV) else {
        return;
    };
    match role.to_str().unwrap() {
        "replace" => retained_interpreter(false),
        "unlink" => retained_interpreter(true),
        "full" => caller_files_with_full_table(),
        other => panic!("unknown pinned-image child: {other}"),
    }
    println!("{COMPLETE}");
}

#[test]
fn lb2_prepared_interpreter_replacement_retains_original_mapping() {
    child("replace");
}

#[test]
fn lb2_prepared_interpreter_unlink_retains_original_mapping() {
    child("unlink");
}

#[test]
fn lb4_caller_files_prepare_with_full_table_and_reserved_resources() {
    child("full");
}

fn child(role: &str) {
    let directory =
        exec_support::fixture_dir(&format!("pinned-images-parent-{}", std::process::id()));
    let stdout = directory.join(format!("{role}.stdout"));
    let stderr = directory.join(format!("{role}.stderr"));
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", CHILD_TEST, "--nocapture", "--test-threads=1"])
        .env(ROLE_ENV, role)
        .stdin(Stdio::null())
        .stdout(Stdio::from(File::create(&stdout).unwrap()))
        .stderr(Stdio::from(File::create(&stderr).unwrap()));
    // Expiry kills the child's process group and fails immediately; reaping
    // is bounded and asynchronous, never a blocking wait after SIGKILL.
    let status = exec_support::run_monitored_with_timeout(
        command,
        &format!("pinned-image {role}, {stderr:?}"),
        Duration::from_secs(2),
    );
    assert!(
        status.success(),
        "pinned-image {role} failed: {status}\n{}",
        fs::read_to_string(&stderr).unwrap()
    );
    assert!(fs::read_to_string(stdout).unwrap().contains(COMPLETE));
}

struct Fixture {
    directory: PathBuf,
    interpreter: PathBuf,
    request: ExecRequest,
    interpreter_bytes: Vec<u8>,
}

fn fixture(role: &str) -> Fixture {
    let directory =
        exec_support::fixture_dir(&format!("pinned-images-{role}-{}", std::process::id()));
    // A normal dynamic native executable keeps this fixture independent of
    // the freestanding consumer, which is not selected by the LB API.
    let mut image = fs::read("/bin/true").unwrap();
    assert_eq!(&image[..4], b"\x7fELF");
    assert_eq!(image[4], 2);
    let phoff = u64::from_le_bytes(image[32..40].try_into().unwrap()) as usize;
    let phnum = usize::from(u16::from_le_bytes(image[56..58].try_into().unwrap()));
    assert_eq!(u16::from_le_bytes(image[54..56].try_into().unwrap()), 56);
    let phdr = (0..phnum)
        .map(|index| phoff + index * 56)
        .find(|offset| u32::from_le_bytes(image[*offset..*offset + 4].try_into().unwrap()) == 3)
        .expect("dynamic native fixture has PT_INTERP");
    let old_offset = u64::from_le_bytes(image[phdr + 8..phdr + 16].try_into().unwrap()) as usize;
    let old_size = u64::from_le_bytes(image[phdr + 32..phdr + 40].try_into().unwrap()) as usize;
    let original_name =
        std::ffi::CStr::from_bytes_with_nul(&image[old_offset..old_offset + old_size]).unwrap();
    let interpreter_bytes = fs::read(Path::new(std::ffi::OsStr::from_bytes(
        original_name.to_bytes(),
    )))
    .unwrap();
    let interpreter = directory.join("retained-interpreter.elf");
    fs::write(&interpreter, &interpreter_bytes).unwrap();
    fs::set_permissions(&interpreter, fs::Permissions::from_mode(0o755)).unwrap();
    let new_name = interpreter.as_os_str().as_bytes();
    let new_offset = image.len() as u64;
    let new_size = new_name.len() as u64 + 1;
    image[phdr + 8..phdr + 16].copy_from_slice(&new_offset.to_le_bytes());
    image[phdr + 32..phdr + 40].copy_from_slice(&new_size.to_le_bytes());
    image[phdr + 40..phdr + 48].copy_from_slice(&new_size.to_le_bytes());
    image.extend_from_slice(new_name);
    image.push(0);
    let target = directory.join("dynamic-program.elf");
    fs::write(&target, image).unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
    let request =
        ExecRequest::execve(&target, vec![CString::new("true").unwrap()], Vec::new()).unwrap();
    assert_eq!(
        exec_support::run_native(&request, &exec_support::ChildSetup::default(), &directory),
        exec_support::NativeObservation::Exited(0),
        "unmutated native qualifying companion"
    );
    assert_eq!(
        exec_support::run_check(&request, &exec_support::ChildSetup::default(), &directory),
        0,
        "unmutated authorization-only CHECK companion"
    );
    Fixture {
        directory,
        interpreter,
        request,
        interpreter_bytes,
    }
}

fn initial_prepare(
    fixture: &Fixture,
) -> (
    PinnedStart,
    reverie_elf_loader::host::HostQualification,
    LoaderHostFacts,
    Limits,
) {
    let host = exec_support::modeled_host(None);
    let loader_host = LoaderHostFacts::current().unwrap();
    let limits = Limits::current().unwrap();
    let options = PrepareExecOptions {
        launcher_link: Path::new("/lb"),
        host: &host,
        limits,
        loader_host: &loader_host,
        inherited_virtual_proc_state: false,
        interpreter_writer_fds: &[],
    };
    // SAFETY: a fresh ordinary child is the sole FD-table mutator. The inputs
    // are stable and unmutated. Its host-policy qualification is MODELED and
    // cannot authorize production activation.
    let start = match unsafe { prepare_exec(&fixture.request, &options) } {
        ExecCheckOutcome::Prepared(start) => *start,
        other => panic!("qualifying dynamic fixture was not prepared: {other:?}"),
    };
    start.verify_objects().unwrap();
    assert_eq!(start.evidence.original_check, Some(0));
    assert!(start.evidence.pinned_checks.iter().all(|errno| *errno == 0));
    assert_eq!(start.evidence.read_pins, [0, 0]);
    (start, host, loader_host, limits)
}

fn mapped_bytes(file: &File) -> Vec<u8> {
    let length = usize::try_from(file.metadata().unwrap().len()).unwrap();
    assert_ne!(length, 0);
    // SAFETY: mmap maps only this retained regular-file descriptor read-only;
    // immutable fixture contents and its owner remain live through the copy.
    let mapping = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            length,
            libc::PROT_READ,
            libc::MAP_PRIVATE,
            file.as_raw_fd(),
            0,
        )
    };
    assert_ne!(
        mapping,
        libc::MAP_FAILED,
        "{}",
        std::io::Error::last_os_error()
    );
    // SAFETY: the successful map covers exactly length readable bytes.
    let bytes = unsafe { std::slice::from_raw_parts(mapping.cast::<u8>(), length) }.to_vec();
    // SAFETY: the mapping is owned here and its borrowed slice has expired.
    assert_eq!(unsafe { libc::munmap(mapping, length) }, 0);
    bytes
}

fn retained_interpreter(unlink: bool) {
    let fixture = fixture(if unlink { "unlink" } else { "replace" });
    let (start, _host, loader_host, limits) = initial_prepare(&fixture);
    let original_identity = start.interpreter_identity.clone();
    assert_eq!(mapped_bytes(&start.interpreter), fixture.interpreter_bytes);
    if unlink {
        fs::remove_file(&fixture.interpreter).unwrap();
        assert_eq!(
            File::open(&fixture.interpreter).unwrap_err().raw_os_error(),
            Some(libc::ENOENT)
        );
    } else {
        let replacement_path = fixture.directory.join("replacement.elf");
        let replacement_bytes = fs::read("/bin/true").unwrap();
        assert_ne!(replacement_bytes, fixture.interpreter_bytes);
        fs::write(&replacement_path, &replacement_bytes).unwrap();
        fs::rename(replacement_path, &fixture.interpreter).unwrap();
        let reopened = File::open(&fixture.interpreter).unwrap();
        let replacement_identity = FileIdentity::of(&reopened).unwrap();
        assert_ne!(replacement_identity.inode, original_identity.inode);
        assert_eq!(mapped_bytes(&reopened), replacement_bytes);
        assert_ne!(mapped_bytes(&reopened), fixture.interpreter_bytes);
        assert!(matches!(
            original_identity.verify(&reopened),
            Err(ExecRefusal::PinnedObjectChanged)
        ));
    }
    let retained_identity = FileIdentity::of(&start.interpreter).unwrap();
    assert_eq!(retained_identity.device, original_identity.device);
    assert_eq!(retained_identity.inode, original_identity.inode);
    assert_eq!(retained_identity.mount_id, original_identity.mount_id);
    assert_eq!(mapped_bytes(&start.interpreter), fixture.interpreter_bytes);
    // Unlink/replacement updates ctime. Keep the production guard strict: a
    // retained inode is not proof of the unchanged frozen filesystem input.
    assert!(matches!(
        start.verify_objects(),
        Err(ExecRefusal::PinnedObjectChanged)
    ));

    // This explicitly MODELED identity-only caller-file control binds I to the
    // retained original descriptor. It does not claim native agreement after
    // pathname mutation or permission to launch that changed input. A fresh
    // modeled snapshot makes the immutability scope the following layout call.
    let identity_host = exec_support::modeled_host(None);
    let program = start.program.try_clone().unwrap();
    let interpreter = start.interpreter.try_clone().unwrap();
    let options = PrepareExecOptions {
        launcher_link: Path::new("/lb"),
        host: &identity_host,
        limits,
        loader_host: &loader_host,
        inherited_virtual_proc_state: false,
        interpreter_writer_fds: &[],
    };
    // SAFETY: the MODEL control supplies the explicit retained T/I binding and
    // fixed contents for layout only; no image replacement/commit occurs.
    let from_files = unsafe {
        prepare_start_from_files(
            program,
            interpreter,
            &start.original_invocation,
            start.arguments.clone(),
            &options,
        )
    }
    .unwrap();
    assert_eq!(from_files.start.layout.entry, start.start.layout.entry);
    assert_eq!(
        from_files.interpreter_identity.inode,
        original_identity.inode
    );
    assert_eq!(
        mapped_bytes(&from_files.interpreter),
        fixture.interpreter_bytes
    );
    from_files.verify_objects().unwrap();
    fs::write(fixture.directory.join("identity.result"),
        format!("PASS retained interpreter; unlink={unlink}\nOriginal read-only mapping and dev/ino/mount retained; strict metadata guard refuses mutation; caller-file layout uses retained I only. MODEL ONLY, no changed-filesystem launch qualification.\n")).unwrap();
}

fn fd_count(limit: RawFd) -> usize {
    (0..limit)
        .filter(|fd| unsafe { libc::fcntl(*fd, libc::F_GETFD) } >= 0)
        .count()
}

fn socket_pair() -> std::io::Result<(File, File)> {
    let mut descriptors = [-1; 2];
    // SAFETY: the output buffer is writable for the two returned owned FDs.
    if unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            0,
            descriptors.as_mut_ptr(),
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: socketpair returned exactly two distinct newly owned descriptors.
    Ok(unsafe {
        (
            File::from_raw_fd(descriptors[0]),
            File::from_raw_fd(descriptors[1]),
        )
    })
}

fn caller_files_with_full_table() {
    let fixture = fixture("full");
    let (mut initial, host, loader_host, limits) = initial_prepare(&fixture);
    let options = PrepareExecOptions {
        launcher_link: Path::new("/lb"),
        host: &host,
        limits,
        loader_host: &loader_host,
        inherited_virtual_proc_state: false,
        interpreter_writer_fds: &[],
    };
    // The caller retains readable T/I and all bootstrap facts before the table
    // fills. The initial pool is replaced by a fresh explicit reservation.
    drop(initial.reservation.take());
    let mut reservation = DescriptorReservation::reserve(START_DESCRIPTOR_SLOTS).unwrap();
    // Establish real unrelated guest descriptors, including one above the
    // later soft limit. They share a caller OFD with a nonzero cursor; T/I flag
    // transfer must change only the two exact private descriptor words.
    let mut guest_source = initial.program.try_clone().unwrap();
    guest_source.seek(SeekFrom::Start(4)).unwrap();
    let guests: Vec<File> = [(100, 0), (102, libc::FD_CLOEXEC), (1024, 0)]
        .into_iter()
        .map(|(number, flags)| {
            // SAFETY: fresh duplication never overwrites an existing owner.
            let fd =
                unsafe { libc::fcntl(guest_source.as_raw_fd(), libc::F_DUPFD_CLOEXEC, number) };
            assert_eq!(fd, number);
            // SAFETY: these fixture owners are configured before preparation;
            // this setup is the caller's original guest descriptor state.
            assert_eq!(unsafe { libc::fcntl(fd, libc::F_SETFD, flags) }, 0);
            // SAFETY: the duplicate is a unique newly returned owner.
            unsafe { File::from_raw_fd(fd) }
        })
        .collect();
    let guest_state: Vec<_> = guests
        .iter()
        .map(|file| {
            (
                file.as_raw_fd(),
                unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFD) },
                FileIdentity::of(file).unwrap(),
            )
        })
        .collect();
    let scratch = reservation.scratch_slot();
    let scratch_fd = reservation.fd(scratch).unwrap();
    let target_alias = format!("/proc/self/fd/{}", initial.program.as_raw_fd());
    let nofile = libc::rlimit {
        rlim_cur: 128,
        rlim_max: 128,
    };
    // SAFETY: this ordinary child owns its process limits and FD table.
    assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &nofile) }, 0);
    let anchor = reservation.file(0).unwrap().as_raw_fd();
    let mut fillers = Vec::new();
    loop {
        // SAFETY: duplicate only the live owned anchor into free numbers.
        let fd = unsafe { libc::fcntl(anchor, libc::F_DUPFD_CLOEXEC, 0) };
        if fd < 0 {
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::EMFILE)
            );
            break;
        }
        // SAFETY: the creating syscall returned a unique descriptor owner.
        fillers.push(unsafe { File::from_raw_fd(fd) });
    }
    assert_eq!(fd_count(128), 128);
    for _ in 0..3 {
        // SAFETY: release/reuse only the reserved scratch in this serialized
        // child; the alias names the caller's retained program descriptor.
        assert_eq!(
            unsafe { reservation.open_into(scratch, || File::open(&target_alias)) }.unwrap(),
            scratch_fd
        );
        let mut header = [0; 4];
        std::os::unix::fs::FileExt::read_exact_at(
            reservation.file(scratch).unwrap(),
            &mut header,
            0,
        )
        .unwrap();
        assert_eq!(&header, b"\x7fELF");
        assert_eq!(fd_count(128), 128);
        // SAFETY: no scratch borrower survives the replacement.
        assert_eq!(unsafe { reservation.reset(scratch) }.unwrap(), scratch_fd);
    }
    // SAFETY: this closure returns only the two owners allocated into the two
    // serialized reserved vacancies, so full-table connection needs no extra FD.
    let (one, two) = unsafe { reservation.open_pair_into(11, 12, socket_pair) }.unwrap();
    let payload = b"full-table retained connection";
    reservation.file(11).unwrap().write_all(payload).unwrap();
    let mut received = vec![0; payload.len()];
    reservation
        .file(12)
        .unwrap()
        .read_exact(&mut received)
        .unwrap();
    assert_eq!(received, payload);
    let transfer = reservation.transfer(&[11, 12, scratch]).unwrap();
    for fd in [one, two, scratch_fd] {
        assert_eq!(unsafe { libc::fcntl(fd, libc::F_GETFD) }, 0);
    }
    assert_eq!(fd_count(128), 128);
    // SAFETY: initial prepare_exec established the explicit T/I binding in the
    // unchanged MODEL input, with stable contents/context and serialized table.
    // This owning API must consume only those files, with no hidden read-open.
    let from_files = unsafe {
        prepare_start_from_files(
            initial.program,
            initial.interpreter,
            &initial.original_invocation,
            initial.arguments,
            &options,
        )
    }
    .unwrap();
    assert_eq!(
        fd_count(128),
        128,
        "caller-file preparation allocates no hidden FD"
    );
    from_files.verify_objects().unwrap();
    assert_eq!(from_files.start.layout.entry, initial.start.layout.entry);
    assert_eq!(
        mapped_bytes(&from_files.interpreter),
        fixture.interpreter_bytes
    );
    let private_files = [&from_files.program, &from_files.interpreter];
    let private_state: Vec<_> = private_files
        .iter()
        .map(|file| {
            (
                file.as_raw_fd(),
                unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFD) },
                FileIdentity::of(file).unwrap(),
            )
        })
        .collect();
    for (_, flags, _) in &private_state {
        assert_eq!(*flags, libc::FD_CLOEXEC);
    }
    // SAFETY: these are precisely the preparation-owned private readable T/I;
    // the ordinary child serializes their FD table, flags and owner lifetimes.
    let file_transfer = unsafe { transfer_private_files(&private_files) }.unwrap();
    assert_eq!(
        file_transfer.private_fds().collect::<Vec<_>>(),
        private_state
            .iter()
            .map(|state| state.0)
            .collect::<Vec<_>>()
    );
    for (fd, _, _) in &private_state {
        assert_eq!(unsafe { libc::fcntl(*fd, libc::F_GETFD) }, 0);
    }
    from_files.verify_objects().unwrap();
    assert_eq!(
        fd_count(128),
        128,
        "T/I flag transfer allocates no hidden FD"
    );
    for (file, (fd, flags, identity)) in guests.iter().zip(&guest_state) {
        assert_eq!(file.as_raw_fd(), *fd);
        assert_eq!(unsafe { libc::fcntl(*fd, libc::F_GETFD) }, *flags);
        identity.verify(file).unwrap();
        assert_eq!(unsafe { libc::lseek(*fd, 0, libc::SEEK_CUR) }, 4);
    }
    file_transfer.rollback().unwrap();
    for (file, (fd, flags, identity)) in private_files.iter().zip(&private_state) {
        assert_eq!(unsafe { libc::fcntl(*fd, libc::F_GETFD) }, *flags);
        identity.verify(file).unwrap();
    }
    transfer.rollback().unwrap();
    for fd in [one, two, scratch_fd] {
        assert_eq!(unsafe { libc::fcntl(fd, libc::F_GETFD) }, libc::FD_CLOEXEC);
    }
    for (file, (fd, flags, identity)) in guests.iter().zip(&guest_state) {
        assert_eq!(unsafe { libc::fcntl(*fd, libc::F_GETFD) }, *flags);
        identity.verify(file).unwrap();
        assert_eq!(unsafe { libc::lseek(*fd, 0, libc::SEEK_CUR) }, 4);
    }
    let mut byte = [0];
    guest_source.read_exact(&mut byte).unwrap();
    assert_eq!(byte[0], 2, "ELF class byte from the original caller OFD");
    for guest in &guests {
        assert_eq!(
            unsafe { libc::lseek(guest.as_raw_fd(), 0, libc::SEEK_CUR) },
            5,
            "guest dup cursors still share their original OFD"
        );
    }
    drop(fillers);
    fs::write(fixture.directory.join("full-table.result"),
        "PASS caller-file preparation with all 128 allocatable FD slots occupied\nReadable pinned T/I and host facts preexist; reserved scratch reused three times; reserved connection works; actual returned T/I become non-CLOEXEC and rollback restores exact flags and identities; guest100/102/1024 flags, content, numbers and shared OFD cursor stay intact; no hidden preparation FD allocation. MODEL ONLY host qualification, no LC consumer.\n").unwrap();
}
