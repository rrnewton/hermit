/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! LB1/LB2/LB4 native errno and public inactive preparation comparisons.
//! Each call runs in a bounded ordinary process, never Hermit. Policy inputs
//! are explicitly modeled, so these tests are not privileged activation gates.

mod exec_support;

use std::ffi::CString;
use std::fs::File;
use std::fs::OpenOptions;
use std::fs::{self};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use exec_support::ChildSetup;
use exec_support::NativeObservation;
use exec_support::PrepareObservation;
use exec_support::RawFault;
use reverie_elf_loader::ExecRequest;
use reverie_elf_loader::exec::E2bigClassification;

// Parallel Command forks could transiently inherit another test's writable
// fixture fd before exec closes CLOEXEC. Native ETXTBSY controls require a
// deliberately stable writer set, so serialize fixture construction/calls.
static SERIAL: Mutex<()> = Mutex::new(());
static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

#[test]
fn lb_prepare_child() {
    exec_support::prepare_child_entry();
}

fn directory(name: &str) -> PathBuf {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .join("target");
    loop {
        let ordinal = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let directory = root.join(format!("{name}-{}-{ordinal}", std::process::id()));
        match fs::create_dir(&directory) {
            Ok(()) => return directory,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => panic!("create fixture directory: {error}"),
        }
    }
}

fn request(path: &Path) -> ExecRequest {
    ExecRequest::execve(path, vec![CString::new("guest-argv0").unwrap()], Vec::new()).unwrap()
}

fn executable(path: &Path, bytes: &[u8]) {
    fs::write(path, bytes).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn copy_program(directory: &Path, name: &str) -> PathBuf {
    let path = directory.join(name);
    executable(&path, &fs::read("/bin/true").unwrap());
    path
}

fn u16_at(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap())
}

fn u64_at(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
}

fn interpreter_header(bytes: &[u8]) -> usize {
    let start = u64_at(bytes, 32) as usize;
    let size = usize::from(u16_at(bytes, 54));
    (0..usize::from(u16_at(bytes, 56)))
        .map(|index| start + index * size)
        .find(|offset| u32::from_le_bytes(bytes[*offset..*offset + 4].try_into().unwrap()) == 3)
        .expect("native true has PT_INTERP")
}

fn with_interpreter(directory: &Path, name: &str, interpreter: &Path) -> PathBuf {
    let mut bytes = fs::read("/bin/true").unwrap();
    let phdr = interpreter_header(&bytes);
    let offset = bytes.len() as u64;
    let interpreter_name = CString::new(interpreter.as_os_str().as_bytes()).unwrap();
    let size = interpreter_name.as_bytes_with_nul().len() as u64;
    bytes.extend_from_slice(interpreter_name.as_bytes_with_nul());
    bytes[phdr + 8..phdr + 16].copy_from_slice(&offset.to_le_bytes());
    bytes[phdr + 32..phdr + 40].copy_from_slice(&size.to_le_bytes());
    bytes[phdr + 40..phdr + 48].copy_from_slice(&size.to_le_bytes());
    let path = directory.join(name);
    executable(&path, &bytes);
    path
}

fn assert_errno(
    directory: &Path,
    request: &ExecRequest,
    setup: &ChildSetup,
    errno: i32,
) -> PrepareObservation {
    assert_eq!(
        exec_support::run_native(request, setup, directory),
        NativeObservation::Errno(errno)
    );
    let prepared = exec_support::run_prepare(request, setup, directory);
    assert!(
        matches!(prepared, PrepareObservation::NativeErrno { errno: got, .. } if got == errno),
        "native errno {errno} disagrees with {prepared:?}"
    );
    prepared
}

fn assert_success(directory: &Path, request: &ExecRequest, setup: &ChildSetup) {
    assert_eq!(
        exec_support::run_native(request, setup, directory),
        NativeObservation::Exited(0)
    );
    assert!(matches!(
        exec_support::run_prepare(request, setup, directory),
        PrepareObservation::Prepared(_)
    ));
}

fn assert_refusal(directory: &Path, request: &ExecRequest, setup: &ChildSetup, name: &str) {
    assert_eq!(
        exec_support::run_prepare(request, setup, directory),
        PrepareObservation::Refusal(name.into())
    );
}

#[test]
fn lb1_original_path_flags_and_user_fault_errno_matrix() {
    let _serial = SERIAL.lock().unwrap();
    let dir = directory("lb1-request");
    let base = request(Path::new("/bin/true"));
    let default = ChildSetup::default();
    assert_success(&dir, &base, &default);
    assert_errno(&dir, &request(&dir.join("absent")), &default, libc::ENOENT);
    let mut badfd =
        ExecRequest::execveat(1_000_000, "child", base.argv.clone(), Vec::new(), 0).unwrap();
    assert_errno(&dir, &badfd, &default, libc::EBADF);
    // Capacity allocation must not turn an originally closed fd3 into ENOTDIR
    // or a valid AT_EMPTY_PATH reference to a private memfd placeholder.
    badfd.dirfd = 3;
    let closed = ChildSetup {
        closed_fds: vec![3],
        ..ChildSetup::default()
    };
    assert_errno(&dir, &badfd, &closed, libc::EBADF);
    badfd.path = CString::new("").unwrap();
    badfd.flags = libc::AT_EMPTY_PATH;
    assert_errno(&dir, &badfd, &closed, libc::EBADF);
    let regular = File::open("/bin/true").unwrap();
    let notdir = ExecRequest::execveat(
        regular.as_raw_fd(),
        "child",
        base.argv.clone(),
        Vec::new(),
        0,
    )
    .unwrap();
    assert_errno(&dir, &notdir, &default, libc::ENOTDIR);
    let symlink = dir.join("symlink");
    std::os::unix::fs::symlink("/bin/true", &symlink).unwrap();
    let mut nofollow = request(&symlink);
    nofollow.flags = libc::AT_SYMLINK_NOFOLLOW;
    assert_errno(&dir, &nofollow, &default, libc::ELOOP);
    let mut flags = base.clone();
    flags.flags = 0x4000;
    assert_errno(&dir, &flags, &default, libc::EINVAL);
    flags.path = CString::new(vec![b'x'; 4096]).unwrap();
    // Flag/getname precedence has changed between the audited source and this
    // running kernel. CHECK must preserve the actual native result, even here.
    let native = exec_support::run_native(&flags, &default, &dir);
    let NativeObservation::Errno(errno) = native else {
        panic!("invalid request executed")
    };
    assert_errno(&dir, &flags, &default, errno);
    flags.flags = 0;
    assert_errno(&dir, &flags, &default, libc::ENAMETOOLONG);
    for fault in [
        RawFault::Path,
        RawFault::Argv,
        RawFault::Envp,
        RawFault::ArgumentString,
        RawFault::EnvironmentString,
    ] {
        let setup = ChildSetup {
            raw_fault: fault,
            ..ChildSetup::default()
        };
        assert_errno(&dir, &base, &setup, libc::EFAULT);
    }
    let fault_and_flags = ChildSetup {
        raw_fault: RawFault::Path,
        ..ChildSetup::default()
    };
    assert_errno(&dir, &flags, &fault_and_flags, libc::EFAULT);
    let mut giant = base;
    giant.envp =
        vec![CString::new(vec![b'e'; reverie_elf_loader::arguments::MAX_ARG_STRLEN]).unwrap()];
    assert!(matches!(
        assert_errno(&dir, &giant, &default, libc::E2BIG),
        PrepareObservation::NativeErrno {
            e2big: Some(E2bigClassification::NativeBudget),
            ..
        }
    ));
}

#[test]
fn lb1_regular_dac_writer_and_namespace_member_controls() {
    let _serial = SERIAL.lock().unwrap();
    let dir = directory("lb1-authorization");
    let default = ChildSetup::default();
    assert_errno(&dir, &request(&dir), &default, libc::EACCES);
    let path = copy_program(&dir, "noexecute");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    assert_errno(&dir, &request(&path), &default, libc::EACCES);
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    let writer = OpenOptions::new().write(true).open(&path).unwrap();
    let setup = ChildSetup {
        writer_fds: vec![writer.as_raw_fd()],
        ..ChildSetup::default()
    };
    assert_errno(&dir, &request(&path), &setup, libc::ETXTBSY);
    drop(writer);
    assert_success(&dir, &request(&path), &default);
    // A real ordinary PID-namespace member is admitted. Namespace PID1 and
    // mismatched-ID/root-capability contexts are separately refused by the
    // explicit host context models; no privileged namespace is fabricated.
    assert_ne!(std::process::id(), 1);
}

#[test]
fn lb1_check_success_never_proves_elf_load_and_source_order() {
    let _serial = SERIAL.lock().unwrap();
    let dir = directory("lb1-formats");
    let default = ChildSetup::default();
    let malformed = dir.join("malformed");
    executable(&malformed, b"not an ELF image\n");
    assert_eq!(
        exec_support::run_check(&request(&malformed), &default, &dir),
        0
    );
    assert_errno(&dir, &request(&malformed), &default, libc::ENOEXEC);
    let missing = dir.join("absent-interpreter");
    let main = with_interpreter(&dir, "missing-interpreter-main", &missing);
    assert_eq!(exec_support::run_check(&request(&main), &default, &dir), 0);
    assert_errno(&dir, &request(&main), &default, libc::ENOENT);
    let short = dir.join("short-interpreter");
    executable(&short, b"\x7fELF");
    let main = with_interpreter(&dir, "short-interpreter-main", &short);
    assert_errno(&dir, &request(&main), &default, libc::EIO);
    let bad = dir.join("bad-interpreter");
    executable(&bad, &[0; 64]);
    let main = with_interpreter(&dir, "bad-interpreter-main", &bad);
    assert_errno(&dir, &request(&main), &default, libc::ELIBBAD);
    let mut bytes = fs::read("/bin/true").unwrap();
    bytes[54..56].copy_from_slice(&55u16.to_le_bytes());
    let phdr = dir.join("bad-phentsize");
    executable(&phdr, &bytes);
    assert_errno(&dir, &request(&phdr), &default, libc::ENOEXEC);
    let mut bytes = fs::read("/bin/true").unwrap();
    let header = interpreter_header(&bytes);
    let last_byte = bytes.len() as u64 - 1;
    bytes[header + 8..header + 16].copy_from_slice(&last_byte.to_le_bytes());
    let truncated = dir.join("short-PT_INTERP");
    executable(&truncated, &bytes);
    assert_errno(&dir, &request(&truncated), &default, libc::EIO);
    // Invalid main type precedes the PT_INTERP lookup. This pair distinguishes
    // errno provenance from an eager interpreter-open mutation.
    let first = with_interpreter(&dir, "type-before-missing-I", &missing);
    let mut bytes = fs::read(&first).unwrap();
    bytes[16..18].copy_from_slice(&1u16.to_le_bytes());
    executable(&first, &bytes);
    assert_errno(&dir, &request(&first), &default, libc::ENOEXEC);
}

#[test]
fn lb2_fifo_and_device_main_interpreter_and_script_never_read_open() {
    let _serial = SERIAL.lock().unwrap();
    let dir = directory("lb2-nonregular");
    let fifo = dir.join("fifo-without-writer");
    let name = CString::new(fifo.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o755) }, 0);
    let default = ChildSetup::default();
    assert_errno(&dir, &request(&fifo), &default, libc::EACCES);
    assert_errno(
        &dir,
        &request(Path::new("/dev/null")),
        &default,
        libc::EACCES,
    );
    let main = with_interpreter(&dir, "fifo-I-main", &fifo);
    assert_errno(&dir, &request(&main), &default, libc::EACCES);
    let main = with_interpreter(&dir, "device-I-main", Path::new("/dev/null"));
    assert_eq!(
        exec_support::run_native(&request(&main), &default, &dir),
        NativeObservation::Errno(libc::EACCES)
    );
    assert_refusal(&dir, &request(&main), &default, "LookupMountUnverified");
    let script = dir.join("fifo-script");
    executable(&script, format!("#!{}\n", fifo.display()).as_bytes());
    assert_errno(&dir, &request(&script), &default, libc::EACCES);
    let device = dir.join("device-script");
    executable(&device, b"#!/dev/null\n");
    assert_eq!(
        exec_support::run_native(&request(&device), &default, &dir),
        NativeObservation::Errno(libc::EACCES)
    );
    assert_refusal(&dir, &request(&device), &default, "LookupMountUnverified");
}

#[test]
fn lb2_execute_only_main_and_both_interpreter_kinds() {
    let _serial = SERIAL.lock().unwrap();
    let dir = directory("lb2-execute-only");
    let default = ChildSetup::default();
    let main = copy_program(&dir, "execute-only-main");
    fs::set_permissions(&main, fs::Permissions::from_mode(0o111)).unwrap();
    assert_eq!(
        exec_support::run_native(&request(&main), &default, &dir),
        NativeObservation::Exited(0)
    );
    assert_refusal(&dir, &request(&main), &default, "ExecutableReadRequired");
    let bytes = fs::read("/bin/true").unwrap();
    let phdr = interpreter_header(&bytes);
    let start = u64_at(&bytes, phdr + 8) as usize;
    let size = u64_at(&bytes, phdr + 32) as usize;
    let native_i = Path::new(std::ffi::OsStr::from_bytes(&bytes[start..start + size - 1]));
    let interpreter = dir.join("execute-only-I");
    executable(&interpreter, &fs::read(native_i).unwrap());
    fs::set_permissions(&interpreter, fs::Permissions::from_mode(0o111)).unwrap();
    let target = with_interpreter(&dir, "readable-T", &interpreter);
    assert_eq!(
        exec_support::run_native(&request(&target), &default, &dir),
        NativeObservation::Exited(0)
    );
    assert_refusal(&dir, &request(&target), &default, "ExecutableReadRequired");
    let script = dir.join("outer-script");
    executable(&script, format!("#!{}\n", main.display()).as_bytes());
    assert_eq!(
        exec_support::run_native(&request(&script), &default, &dir),
        NativeObservation::Exited(0)
    );
    assert_refusal(&dir, &request(&script), &default, "ExecutableReadRequired");
}

#[test]
fn lb2_deleted_sealed_memfd_and_o_path_empty_path() {
    let _serial = SERIAL.lock().unwrap();
    let dir = directory("lb2-memfd");
    let fd = unsafe {
        libc::memfd_create(
            c"lb-deleted-program".as_ptr(),
            libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
        )
    };
    assert!(fd >= 0);
    let mut memfd = unsafe { File::from_raw_fd(fd) };
    memfd.write_all(&fs::read("/bin/true").unwrap()).unwrap();
    let seals = libc::F_SEAL_SEAL | libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_WRITE;
    assert_eq!(unsafe { libc::fcntl(fd, libc::F_ADD_SEALS, seals) }, 0);
    let mut empty = ExecRequest::execveat(
        fd,
        "",
        vec![CString::new("memfd-program").unwrap()],
        Vec::new(),
        libc::AT_EMPTY_PATH,
    )
    .unwrap();
    let setup = ChildSetup {
        sealed_memfd: Some((fd, seals)),
        ..ChildSetup::default()
    };
    assert_success(&dir, &empty, &setup);
    let path = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH)
        .open(format!("/proc/self/fd/{fd}"))
        .unwrap();
    assert_eq!(
        unsafe { libc::fcntl(path.as_raw_fd(), libc::F_GET_SEALS) },
        -1
    );
    empty.dirfd = path.as_raw_fd();
    let setup = ChildSetup {
        inherited_fds: vec![path.as_raw_fd()],
        sealed_memfd: Some((fd, seals)),
        ..ChildSetup::default()
    };
    assert_success(&dir, &empty, &setup);
    assert_refusal(
        &dir,
        &empty,
        &ChildSetup::default(),
        "LookupMountUnverified",
    );
    let disk = copy_program(&dir, "deleted-disk-program");
    let pinned = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH)
        .open(&disk)
        .unwrap();
    fs::remove_file(&disk).unwrap();
    empty.dirfd = pinned.as_raw_fd();
    assert_success(&dir, &empty, &ChildSetup::default());
}

#[test]
fn lb4_full_table_before_public_preparation_is_capacity_refusal() {
    let _serial = SERIAL.lock().unwrap();
    let dir = directory("lb4-prepare-capacity");
    let setup = ChildSetup {
        fill_fd_table: true,
        ..ChildSetup::default()
    };
    let valid = request(Path::new("/bin/true"));
    assert_eq!(
        exec_support::run_native(&valid, &setup, &dir),
        NativeObservation::Exited(0)
    );
    assert_refusal(&dir, &valid, &setup, "LauncherFdCapacity");
    let missing = request(&dir.join("absent"));
    assert_eq!(
        exec_support::run_native(&missing, &setup, &dir),
        NativeObservation::Errno(libc::ENOENT)
    );
    assert_refusal(&dir, &missing, &setup, "LauncherFdCapacity");
}

#[test]
fn lb6_setid_and_inherited_proc_state_are_timely_named_refusals() {
    let _serial = SERIAL.lock().unwrap();
    let dir = directory("lb6-prepare-refusals");
    let path = copy_program(&dir, "setid");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o4755)).unwrap();
    assert_eq!(
        exec_support::run_native(&request(&path), &ChildSetup::default(), &dir),
        NativeObservation::Exited(0)
    );
    assert_refusal(
        &dir,
        &request(&path),
        &ChildSetup::default(),
        "SecureExecUnsupported",
    );
    let inherited = ChildSetup {
        inherited_virtual_proc_state: true,
        ..ChildSetup::default()
    };
    let valid = request(Path::new("/bin/true"));
    assert_eq!(
        exec_support::run_native(&valid, &inherited, &dir),
        NativeObservation::Exited(0)
    );
    assert_refusal(
        &dir,
        &valid,
        &inherited,
        "InheritedVirtualProcStateUnsupported",
    );
}

#[test]
fn lb1_interpreter_writer_and_noexecute_denials_have_native_sources() {
    let _serial = SERIAL.lock().unwrap();
    let dir = directory("lb1-interpreter-authorization");
    let bytes = fs::read("/bin/true").unwrap();
    let phdr = interpreter_header(&bytes);
    let start = u64_at(&bytes, phdr + 8) as usize;
    let size = u64_at(&bytes, phdr + 32) as usize;
    let native_i = Path::new(std::ffi::OsStr::from_bytes(&bytes[start..start + size - 1]));
    let interpreter = dir.join("I");
    executable(&interpreter, &fs::read(native_i).unwrap());
    let target = with_interpreter(&dir, "T", &interpreter);
    let writer = OpenOptions::new().write(true).open(&interpreter).unwrap();
    let setup = ChildSetup {
        writer_fds: vec![writer.as_raw_fd()],
        ..ChildSetup::default()
    };
    assert_errno(&dir, &request(&target), &setup, libc::ETXTBSY);
    drop(writer);
    fs::set_permissions(&interpreter, fs::Permissions::from_mode(0o644)).unwrap();
    assert_errno(
        &dir,
        &request(&target),
        &ChildSetup::default(),
        libc::EACCES,
    );
}

#[test]
fn lb1_la_static_refusal_has_native_success_companion() {
    let _serial = SERIAL.lock().unwrap();
    let dir = directory("lb1-static");
    let mut elf = vec![0u8; 4096];
    elf[..7].copy_from_slice(b"\x7fELF\x02\x01\x01");
    elf[16..18].copy_from_slice(&2u16.to_le_bytes());
    elf[18..20].copy_from_slice(&62u16.to_le_bytes());
    elf[20..24].copy_from_slice(&1u32.to_le_bytes());
    elf[24..32].copy_from_slice(&0x400100u64.to_le_bytes());
    elf[32..40].copy_from_slice(&64u64.to_le_bytes());
    elf[52..54].copy_from_slice(&64u16.to_le_bytes());
    elf[54..56].copy_from_slice(&56u16.to_le_bytes());
    elf[56..58].copy_from_slice(&2u16.to_le_bytes());
    elf[64..68].copy_from_slice(&1u32.to_le_bytes()); // PT_LOAD
    elf[68..72].copy_from_slice(&5u32.to_le_bytes()); // PF_R | PF_X
    elf[80..88].copy_from_slice(&0x400000u64.to_le_bytes());
    elf[96..104].copy_from_slice(&4096u64.to_le_bytes());
    elf[104..112].copy_from_slice(&4096u64.to_le_bytes());
    elf[112..120].copy_from_slice(&4096u64.to_le_bytes());
    elf[120..124].copy_from_slice(&0x6474e551u32.to_le_bytes()); // GNU_STACK
    elf[124..128].copy_from_slice(&6u32.to_le_bytes()); // PF_R | PF_W
    // exit(0), with no libc/interpreter or writable data.
    elf[0x100..0x109].copy_from_slice(&[0xb8, 60, 0, 0, 0, 0x31, 0xff, 0x0f, 0x05]);
    let path = dir.join("static");
    executable(&path, &elf);
    let request = request(&path);
    assert_eq!(
        exec_support::run_native(&request, &ChildSetup::default(), &dir),
        NativeObservation::Exited(0)
    );
    assert_refusal(&dir, &request, &ChildSetup::default(), "MissingInterpreter");
}

#[test]
fn lb2_original_procfd_lookup_excludes_new_private_descriptors() {
    let _serial = SERIAL.lock().unwrap();
    let dir = directory("lb2-private-alias");
    let setup = ChildSetup {
        closed_fds: vec![3],
        ..ChildSetup::default()
    };
    for path in ["/proc/self/fd/3", "/proc/self/fd/3/child"] {
        assert_errno(&dir, &request(Path::new(path)), &setup, libc::ENOENT);
    }
    let target = with_interpreter(&dir, "T", Path::new("/proc/self/fd/3/child"));
    assert_eq!(
        exec_support::run_native(&request(&target), &setup, &dir),
        NativeObservation::Errno(libc::ENOENT)
    );
    assert_refusal(&dir, &request(&target), &setup, "LookupMountUnverified");
    let script = dir.join("S");
    executable(&script, b"#!/proc/self/fd/3/child\n");
    assert_eq!(
        exec_support::run_native(&request(&script), &setup, &dir),
        NativeObservation::Errno(libc::ENOENT)
    );
    assert_refusal(&dir, &request(&script), &setup, "LookupMountUnverified");
}
