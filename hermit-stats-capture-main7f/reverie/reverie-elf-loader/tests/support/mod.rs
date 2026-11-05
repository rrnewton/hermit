/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

mod compare;
mod credentials;
mod geometry;
mod hugetlb;
mod kernel_controls;
mod mdwe;
mod pkeys;
mod shadow;
mod trace;

use std::ffi::CString;
use std::ffi::OsStr;
use std::fs::File;
use std::fs::OpenOptions;
use std::fs::Permissions;
use std::fs::{self};
use std::io::Write;
use std::io::{self};
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::fs::symlink;
use std::os::unix::process::CommandExt;
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Mutex;
use std::time::Instant;

pub use credentials::privileged_executable_capability_controls;
pub use credentials::privileged_executable_mode_controls;
pub use geometry::duplicate_header_mapping;
pub use geometry::interpreter_address_hint;
pub use geometry::interpreter_empty_bss_reserved_range;
pub use geometry::interpreter_empty_negative_bias_reserved_range;
pub use geometry::interpreter_hint_exceptions;
pub use geometry::interpreter_hint_reserved_range;
pub use geometry::interpreter_load_span;
pub use geometry::interpreter_mixed_empty_reserved_range;
pub use geometry::main_reserved_range;
pub use geometry::unaligned_pie_alignment;
pub use hugetlb::hugetlb_inode_controls;
pub use kernel_controls::kernel_bss_right_merge_controls;
pub use kernel_controls::kernel_initial_stack_overlap_controls;
pub use kernel_controls::kernel_interpreter_entry_controls;
pub use kernel_controls::kernel_mapping_size_controls;
pub use kernel_controls::kernel_reserved_top_page_controls;
pub use mdwe::mdwe_executable_bss_controls;
pub use mdwe::mdwe_supported_bss_controls;
pub use pkeys::pkey_text_case;
use reverie_elf_loader::Error;
use reverie_elf_loader::Invocation;
use reverie_elf_loader::Limits;
use reverie_elf_loader::PreparedStart;
use reverie_elf_loader::prepare_start_with_limits;
pub use shadow::empty_last_load_zero_data_controls;

pub type TestResult<T = ()> = Result<T, String>;

const DIRECTORY_FD: i32 = 12;
const TARGET_FD: i32 = reverie_elf_loader::TARGET_FD;
const METADATA_FD: i32 = reverie_elf_loader::METADATA_FD;
static TEST_LOCK: Mutex<()> = Mutex::new(());

#[derive(Clone, Copy, Debug)]
pub enum Fixture {
    Pie,
    NonPie,
    EntryPie,
    EntryNonPie,
    True,
}

impl Fixture {
    fn path(self) -> &'static str {
        match self {
            Self::Pie => env!("ELF_LOADER_LAYOUT_PIE"),
            Self::NonPie => env!("ELF_LOADER_LAYOUT_NONPIE"),
            Self::EntryPie => env!("ELF_LOADER_ENTRY_PIE"),
            Self::EntryNonPie => env!("ELF_LOADER_ENTRY_NONPIE"),
            Self::True => "/bin/true",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Pie => "pie",
            Self::NonPie => "nonpie",
            Self::EntryPie => "entry-pie",
            Self::EntryNonPie => "entry-nonpie",
            Self::True => "true",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub enum Naming {
    Absolute,
    Relative,
    DirectoryDescriptor,
    EmptyPath,
    Deleted,
    Memfd,
}

impl Naming {
    fn label(self) -> &'static str {
        match self {
            Self::Absolute => "absolute",
            Self::Relative => "relative",
            Self::DirectoryDescriptor => "dirfd",
            Self::EmptyPath => "empty-path",
            Self::Deleted => "deleted",
            Self::Memfd => "memfd",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mutation {
    None,
    OmitPlaceholder,
    SkipVdsoRelocation,
    DirtyRegisters,
    DirtyX87Status,
    DirtyX87Tag,
    DirtySelectors,
    DirtySegmentBases,
}

impl Mutation {
    fn flags(self) -> u8 {
        match self {
            Self::None => 0,
            Self::SkipVdsoRelocation => 1,
            Self::DirtyRegisters => 2,
            Self::OmitPlaceholder => 4,
            Self::DirtyX87Status => 8,
            Self::DirtySelectors => 16,
            Self::DirtySegmentBases => 32,
            Self::DirtyX87Tag => 64,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::OmitPlaceholder => "omit-placeholder",
            Self::SkipVdsoRelocation => "skip-vdso-relocation",
            Self::DirtyRegisters => "dirty-registers",
            Self::DirtyX87Status => "dirty-x87-status",
            Self::DirtyX87Tag => "dirty-x87-tag",
            Self::DirtySelectors => "dirty-selectors",
            Self::DirtySegmentBases => "dirty-segment-bases",
        }
    }

    fn is_entry_state(self) -> bool {
        matches!(
            self,
            Self::DirtyRegisters
                | Self::DirtyX87Status
                | Self::DirtyX87Tag
                | Self::DirtySelectors
                | Self::DirtySegmentBases
        )
    }
}

struct Case {
    root: PathBuf,
    cwd: CString,
    target: File,
    directory: Option<File>,
    invocation: Invocation,
    argv: Vec<CString>,
    env: Vec<CString>,
    limits: Limits,
    mdwe: bool,
}

impl Case {
    fn new(test: &str, fixture: Fixture, naming: Naming, data: Option<u64>) -> TestResult<Self> {
        let root =
            output_root()?
                .join(test)
                .join(format!("{}-{}", fixture.label(), naming.label()));
        io(fs::create_dir_all(&root))?;
        let source = Path::new(fixture.path());
        let source = io(fs::canonicalize(source))?;
        replace_symlink(&root.join("fixture"), &source)?;
        let (target, directory, invocation) = match naming {
            Naming::Absolute => (
                io(File::open(&source))?,
                None,
                error(Invocation::execve(fixture.path()))?,
            ),
            Naming::Relative => (
                io(File::open(&source))?,
                None,
                error(Invocation::execve("./fixture"))?,
            ),
            Naming::DirectoryDescriptor => (
                io(File::open(&source))?,
                Some(io(File::open(&root))?),
                error(Invocation::execveat(DIRECTORY_FD, "fixture", 0))?,
            ),
            Naming::EmptyPath => (
                io(File::open(&source))?,
                None,
                error(Invocation::execveat(TARGET_FD, "", libc::AT_EMPTY_PATH))?,
            ),
            Naming::Deleted => {
                let deleted = root.join(format!("gone-{}", fixture.label()));
                io(fs::copy(&source, &deleted))?;
                let target = io(File::open(&deleted))?;
                io(fs::remove_file(&deleted))?;
                (
                    target,
                    None,
                    error(Invocation::execveat(TARGET_FD, "", libc::AT_EMPTY_PATH))?,
                )
            }
            Naming::Memfd => (
                copy_to_memfd(&source, &format!("{}-memfd", fixture.label()))?,
                None,
                error(Invocation::execveat(TARGET_FD, "", libc::AT_EMPTY_PATH))?,
            ),
        };
        let argv = [
            "chosen-argv-zero",
            "one",
            "argument with spaces",
            "",
            "last",
        ]
        .iter()
        .map(|value| CString::new(*value).expect("literal has no NUL"))
        .collect();
        let mut env = [
            "PATH=/usr/bin:/bin",
            "LANG=C",
            "EXACT_ENV=value with spaces",
        ]
        .iter()
        .map(|value| CString::new(*value).expect("literal has no NUL"))
        .collect::<Vec<_>>();
        env.push(
            CString::new(format!(
                "ELF_LOADER_HEAP_PAGES={}",
                if data.is_some() { 1024 } else { 16 }
            ))
            .expect("numeric string has no NUL"),
        );
        let mut limits = error(Limits::current())?;
        if let Some(data) = data {
            limits.data = data;
        }
        let cwd = cstring(root.as_os_str())?;
        Ok(Self {
            root,
            cwd,
            target,
            directory,
            invocation,
            argv,
            env,
            limits,
            mdwe: false,
        })
    }

    fn prepare(&self) -> TestResult<PreparedStart> {
        error(prepare_start_with_limits(
            &self.target,
            &self.invocation,
            Path::new("./h"),
            self.limits,
        ))
    }
}

struct Artifacts {
    prepared: PreparedStart,
    image: PathBuf,
    metadata: File,
    template: Vec<u8>,
    identity: ImageIdentity,
}

struct ImageIdentity {
    device: (u64, u64),
    inode: u64,
    name: String,
}

impl Artifacts {
    fn new(case: &Case, mutation: Mutation, test_template: bool) -> TestResult<Self> {
        let prepared = case.prepare()?;
        let image = case.root.join("image");
        let template_path = if test_template {
            env!("ELF_LOADER_TEST_TEMPLATE")
        } else {
            env!("ELF_LOADER_TEMPLATE")
        };
        let template = if !test_template {
            reverie_elf_loader::loader_template().to_vec()
        } else {
            io(fs::read(env!("ELF_LOADER_TEST_TEMPLATE")))?
        };
        let mut output = io(File::create(&image))?;
        if mutation == Mutation::OmitPlaceholder {
            io(output.write_all(&template))?;
            io(output.set_permissions(Permissions::from_mode(0o755)))?;
        } else {
            error(prepared.write_image_from_template(&output, &template))?;
        }
        // A writable descriptor to a regular executable would cause ETXTBSY.
        drop(output);
        let identity = compare::image_identity(&image)?;
        let hash = io(Command::new("sha256sum").arg(&image).output())?;
        if !hash.status.success() {
            return Err("sha256sum failed for generated per-start loader image".into());
        }
        let mut provenance = io(OpenOptions::new()
            .create(true)
            .append(true)
            .open(output_root()?.join("images.provenance")))?;
        io(writeln!(
            provenance,
            "generation: cargo --offline test -p reverie-elf-loader --test parity; operation={}; template={}; native_execfn={:?}; test_mutation={}\nsha256: {}",
            if mutation == Mutation::OmitPlaceholder {
                "File::write_all(raw_test_template) without placeholder"
            } else {
                "PreparedStart::write_image_from_template"
            },
            template_path,
            prepared.native_execfn,
            mutation.label(),
            String::from_utf8_lossy(&hash.stdout).trim_end(),
        ))?;
        replace_symlink(&case.root.join("h"), Path::new("image"))?;
        let mut bytes = prepared.metadata();
        *bytes.last_mut().expect("metadata contains a flag") = mutation.flags();
        let metadata_path = case.root.join("metadata");
        io(fs::write(&metadata_path, &bytes))?;
        let metadata = io(File::open(metadata_path))?;
        Ok(Self {
            prepared,
            image,
            metadata,
            template,
            identity,
        })
    }
}

struct Pair {
    native: trace::Run,
    loaded: trace::Run,
    artifacts: Artifacts,
}

fn run_pair(case: &Case, mutation: Mutation) -> TestResult<Pair> {
    run_pair_using(case, mutation, mutation != Mutation::None)
}

fn run_pair_using(case: &Case, mutation: Mutation, test_template: bool) -> TestResult<Pair> {
    let artifacts = Artifacts::new(case, mutation, test_template)?;
    let native = trace::run(case, None)?;
    let loaded = trace::run(case, Some(&artifacts))?;
    println!(
        "runs {} native={:.3}s loader={:.3}s peak_VmData_native={}KiB peak_VmData_loader={}KiB",
        case.root.display(),
        native.elapsed.as_secs_f64(),
        loaded.elapsed.as_secs_f64(),
        native.peak_data_kib,
        loaded.peak_data_kib,
    );
    Ok(Pair {
        native,
        loaded,
        artifacts,
    })
}

pub fn layout_case(test: &str, fixture: Fixture, naming: Naming, data: Option<u64>) -> TestResult {
    let case = Case::new(test, fixture, naming, data)?;
    let pair = run_pair(&case, Mutation::None)?;
    compare::entries(&case, &pair).map_err(|failure| failure.to_string())?;
    if matches!(fixture, Fixture::True) {
        if !pair.native.stdout.is_empty() || !pair.loaded.stdout.is_empty() {
            return Err("/bin/true unexpectedly wrote output".into());
        }
    } else {
        compare::layouts(&case, &pair, data.is_some()).map_err(|failure| failure.to_string())?;
    }
    Ok(())
}

pub fn observer_case(test: &str, fixture: Fixture, mutation: Mutation) -> TestResult {
    let case = Case::new(test, fixture, Naming::Absolute, None)?;
    let pair = run_pair(&case, mutation)?;
    compare::entries(&case, &pair).map_err(|failure| failure.to_string())?;
    compare::observers(&pair).map_err(|failure| failure.to_string())
}

pub fn mutation_case(mutation: Mutation, assertion: &str) -> TestResult {
    let fixture = if mutation.is_entry_state() {
        Fixture::EntryPie
    } else {
        Fixture::Pie
    };
    let case = Case::new(mutation.label(), fixture, Naming::Absolute, None)?;
    // Qualify the exact separately compiled mutation template with every
    // switch off before enabling the intended fault in that same template.
    let baseline = run_pair_using(&case, Mutation::None, true)?;
    compare::entries(&case, &baseline).map_err(|failure| failure.to_string())?;
    if mutation.is_entry_state() {
        compare::observers(&baseline).map_err(|failure| failure.to_string())?;
    } else {
        compare::layouts(&case, &baseline, false).map_err(|failure| failure.to_string())?;
    }
    drop(baseline);
    let pair = run_pair(&case, mutation)?;
    match compare::entries(&case, &pair) {
        Ok(()) => Err(format!(
            "mutation {} escaped the normal entry comparator",
            mutation.label()
        )),
        Err(failure) if failure.assertion == assertion => {
            let named = failure.to_string();
            let witnessed = match mutation {
                Mutation::OmitPlaceholder => named.starts_with("heap_metadata: stat field45:"),
                Mutation::SkipVdsoRelocation => {
                    named.starts_with("mapping_parity: special mapping [vvar]:")
                }
                Mutation::DirtyRegisters => {
                    named.starts_with("entry_registers: GPR12:")
                        && pair.native.entry.registers.gpr[12] == 0
                        && pair.loaded.entry.registers.gpr[12] == 0x1234
                }
                Mutation::DirtyX87Status => {
                    named.starts_with("entry_registers: x87 status word:")
                        && pair.native.entry.registers.x87_status == 0
                        && pair.loaded.entry.registers.x87_status == 0x3800
                        && pair.native.entry.registers.x87_tag == 0
                        && pair.loaded.entry.registers.x87_tag == 0
                }
                Mutation::DirtyX87Tag => {
                    named.starts_with("entry_registers: x87 abridged tag word:")
                        && pair.native.entry.registers.x87_tag == 0
                        && pair.loaded.entry.registers.x87_tag == 0x80
                        && pair.native.entry.registers.x87_status == 0
                        && pair.loaded.entry.registers.x87_status == 0
                }
                Mutation::DirtySelectors => {
                    named.starts_with("entry_registers: DS selector:")
                        && pair.native.entry.registers.selectors[2..4] == [0, 0]
                        && pair.loaded.entry.registers.selectors[2..4] == [0x2b, 0x2b]
                }
                Mutation::DirtySegmentBases => {
                    named.starts_with("entry_registers: FS base:")
                        && pair.native.entry.registers.fs_base == 0
                        && pair.native.entry.registers.gs_base == 0
                        && pair.loaded.entry.registers.fs_base == 0x12345000
                        && pair.loaded.entry.registers.gs_base == 0x23456000
                }
                Mutation::None => false,
            };
            if !witnessed {
                return Err(format!(
                    "mutation {} did not fail at its specific intended witness: {failure}",
                    mutation.label()
                ));
            }
            if mutation.is_entry_state() {
                // Both assembly observations must first agree with their own
                // ptrace captures, then reject the dirty loader's full record.
                match compare::observers(&pair) {
                    Err(observer_failure)
                        if observer_failure.assertion == "entry_registers"
                            && observer_failure.to_string().starts_with(
                                "entry_registers: full assembly entry observer record:",
                            ) =>
                    {
                        println!(
                            "mutation {} independently rejected by {observer_failure}",
                            mutation.label()
                        );
                    }
                    result => {
                        return Err(format!(
                            "mutation {} escaped or failed before the independent observer parity gate: {result:?}",
                            mutation.label()
                        ));
                    }
                }
            }
            println!("mutation {} rejected by {failure}", mutation.label());
            let mut output = io(OpenOptions::new()
                .create(true)
                .append(true)
                .open(output_root()?.join("mutations.tsv")))?;
            io(writeln!(output, "{}\t{}", mutation.label(), failure))
        }
        Err(failure) => Err(format!(
            "mutation {} failed at unexpected assertion {failure}; expected {assertion}",
            mutation.label()
        )),
    }
}

pub fn path_boundary() -> TestResult {
    let mut case = Case::new(
        "path-boundary-relative",
        Fixture::Pie,
        Naming::Relative,
        None,
    )?;
    let filename = "fixture";
    let relative = format!(".{}{}", "/".repeat(4095 - 1 - filename.len()), filename);
    assert_eq!(relative.len(), 4095);
    case.invocation = error(Invocation::execve(&relative))?;
    let prepared = case.prepare()?;
    assert_eq!(prepared.padded_path.as_bytes().len(), 4095);
    let pair = run_pair(&case, Mutation::None)?;
    compare::entries(&case, &pair).map_err(|failure| failure.to_string())?;
    compare::layouts(&case, &pair, false).map_err(|failure| failure.to_string())?;

    let mut case = Case::new(
        "path-boundary-dirfd",
        Fixture::Pie,
        Naming::DirectoryDescriptor,
        None,
    )?;
    case.invocation = error(Invocation::execveat(DIRECTORY_FD, &relative, 0))?;
    let native = trace::run(&case, None)?;
    if native.exit != 0 {
        return Err(format!(
            "native execveat at the 4095-byte dirfd boundary failed: {}",
            native.exit
        ));
    }
    let expected = format!("/dev/fd/{DIRECTORY_FD}/{relative}");
    let actual = native.entry.execfn();
    if actual != expected.as_bytes() {
        return Err(
            "native execveat synthesized AT_EXECFN differs from the boundary control".into(),
        );
    }
    match prepare_start_with_limits(
        &case.target,
        &case.invocation,
        Path::new("./h"),
        case.limits,
    ) {
        Err(Error::PaddedPathTooLong {
            length_with_nul,
            maximum,
        }) if length_with_nul == expected.len() + 1 && maximum == 4096 => {
            println!(
                "native dirfd pathname4095 succeeded; padded filename{}+NUL refused atPATH_MAX4096",
                expected.len()
            );
            Ok(())
        }
        result => Err(format!(
            "wrong refusal at the dirfd pathname boundary: {result:?}"
        )),
    }
}

pub fn finite_startup_boundary() -> TestResult {
    for fixture in [Fixture::EntryPie, Fixture::EntryNonPie] {
        let mut case = Case::new(
            "finite-data-startup-boundary",
            fixture,
            Naming::Absolute,
            None,
        )?;
        let minimum = case.prepare()?.layout.minimum_data_limit;
        // Independent fixture geometry: one main writable file page, one
        // interpreter writable file page, and 67 interpreter BSS pages.
        if minimum != 69 * 4096 {
            return Err(format!(
                "observer startup DATA footprint was {minimum}, expected 69 * 4096 bytes"
            ));
        }
        for limit in [0, minimum - 1] {
            case.limits.data = limit;
            match prepare_start_with_limits(
                &case.target,
                &case.invocation,
                Path::new("./h"),
                case.limits,
            ) {
                Err(Error::FiniteDataLimitBelowStartupFootprint {
                    limit: actual,
                    minimum: boundary,
                }) if actual == limit && boundary == minimum => {}
                result => {
                    return Err(format!(
                        "wrong named startup DATA refusal at limit{limit}, minimum{minimum}: {result:?}"
                    ));
                }
            }
        }
        native_below_startup_boundary(&case, minimum - 1)?;
        case.limits.data = minimum;
        let pair = run_pair(&case, Mutation::None)?;
        compare::entries(&case, &pair).map_err(|failure| failure.to_string())?;
        compare::observers(&pair).map_err(|failure| failure.to_string())?;
        if pair.native.peak_data_kib * 1024 != minimum {
            return Err(format!(
                "observed native startup DATA peak {} KiB does not equal the named {} byte boundary",
                pair.native.peak_data_kib, minimum
            ));
        }
        println!(
            "finite startup DATA {} boundary={}bytes: limit0 and boundary-1 refused, exact boundary native/loader PASS",
            fixture.label(),
            minimum
        );
    }
    Ok(())
}

fn native_below_startup_boundary(case: &Case, limit: u64) -> TestResult {
    let mut command = Command::new(OsStr::from_bytes(case.invocation.path().to_bytes()));
    command.arg0(OsStr::from_bytes(case.argv[0].as_bytes()));
    for arg in &case.argv[1..] {
        command.arg(OsStr::from_bytes(arg.as_bytes()));
    }
    command.current_dir(&case.root).env_clear();
    for variable in &case.env {
        let bytes = variable.as_bytes();
        let separator = bytes
            .iter()
            .position(|byte| *byte == b'=')
            .expect("fixture variable contains '='");
        command.env(
            OsStr::from_bytes(&bytes[..separator]),
            OsStr::from_bytes(&bytes[separator + 1..]),
        );
    }
    command.stdout(io(File::create(
        case.root.join("native-below-minimum.stdout"),
    ))?);
    command.stderr(io(File::create(
        case.root.join("native-below-minimum.stderr"),
    ))?);
    // SAFETY: this child callback allocates nothing and only issues syscalls.
    // Its values are fully prepared before spawn and it performs no Rust I/O.
    unsafe {
        command.pre_exec(move || {
            let current = libc::personality(!0 as libc::c_ulong);
            if current < 0
                || libc::personality(
                    current as libc::c_ulong | libc::ADDR_NO_RANDOMIZE as libc::c_ulong,
                ) < 0
                || libc::personality(!0 as libc::c_ulong) & libc::ADDR_NO_RANDOMIZE == 0
            {
                return Err(io::Error::last_os_error());
            }
            let data = libc::rlimit {
                rlim_cur: limit,
                rlim_max: limit,
            };
            let core = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            if libc::setrlimit(libc::RLIMIT_DATA, &data) < 0
                || libc::setrlimit(libc::RLIMIT_CORE, &core) < 0
            {
                return Err(io::Error::last_os_error());
            }
            libc::alarm(30);
            Ok(())
        });
    }
    let status = io(command.status())?;
    if status.signal() != Some(libc::SIGSEGV) {
        return Err(format!(
            "native start below DATA boundary at {limit} bytes did not fail with SIGSEGV: {status}"
        ));
    }
    if !io(fs::read(case.root.join("native-below-minimum.stdout")))?.is_empty()
        || !io(fs::read(case.root.join("native-below-minimum.stderr")))?.is_empty()
    {
        return Err(
            "native below-boundary control wrote output before its expected kernel load failure"
                .into(),
        );
    }
    println!("native startup DATA {limit} bytes: SIGSEGV (expected boundary control)");
    Ok(())
}

pub fn test(name: &str, body: impl FnOnce() -> TestResult) {
    // The child performs only raw syscalls after fork. Serialization also makes
    // each case's ptrace ownership and generated evidence unambiguous.
    let _guard = TEST_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let start = Instant::now();
    let result = body();
    let elapsed = start.elapsed();
    let status = if result.is_ok() { "PASS" } else { "FAIL" };
    if let Ok(root) = output_root()
        && let Ok(mut file) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(root.join("timings.tsv"))
    {
        let _ = writeln!(file, "{name}\t{status}\t{:.6}", elapsed.as_secs_f64());
    }
    println!("{name}: {status} ({:.3}s)", elapsed.as_secs_f64());
    if let Err(error) = result {
        panic!("{name}: {error}");
    }
}

fn output_root() -> TestResult<PathBuf> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../target/elf-loader-tests");
    io(fs::create_dir_all(&root))?;
    io(fs::canonicalize(root))
}

fn replace_symlink(link: &Path, target: &Path) -> TestResult {
    match fs::remove_file(link) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.to_string()),
    }
    io(symlink(target, link))
}

fn copy_to_memfd(path: &Path, name: &str) -> TestResult<File> {
    let name = CString::new(name).map_err(|error| error.to_string())?;
    // SAFETY: name is terminated, and this syscall returns an owned descriptor.
    let fd =
        unsafe { libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING) };
    if fd < 0 {
        return Err(io::Error::last_os_error().to_string());
    }
    // SAFETY: fd was just created and has one owner.
    let mut writable = unsafe { File::from_raw_fd(fd) };
    io(io::copy(&mut io(File::open(path))?, &mut writable))?;
    io(writable.set_permissions(Permissions::from_mode(0o755)))?;
    // Both launches must execute exactly the same immutable inode.
    let seals = libc::F_SEAL_WRITE | libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL;
    // SAFETY: writable owns fd, and F_ADD_SEALS takes an integer argument.
    if unsafe { libc::fcntl(fd, libc::F_ADD_SEALS, seals) } < 0 {
        return Err(io::Error::last_os_error().to_string());
    }
    let readonly = io(File::open(format!(
        "/proc/self/fd/{}",
        writable.as_raw_fd()
    )))?;
    drop(writable);
    Ok(readonly)
}

fn cstring(value: &OsStr) -> TestResult<CString> {
    CString::new(value.as_bytes()).map_err(|error| error.to_string())
}

fn io<T>(result: io::Result<T>) -> TestResult<T> {
    result.map_err(|error| error.to_string())
}

fn error<T>(result: Result<T, Error>) -> TestResult<T> {
    result.map_err(|error| error.to_string())
}
