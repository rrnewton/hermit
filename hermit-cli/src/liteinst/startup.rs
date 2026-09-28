//! File inputs for controlled startup, resolved only in the caller's current filesystem.
//! This is not an entered-container token, an exec owner, or an interpreter mount plan.

pub mod kernel_binding;
pub(crate) mod launch;
pub mod original_interpreter;
pub mod private_runtime;

use std::fs::File;
use std::fs::Metadata;
use std::fs::OpenOptions;
use std::io;
use std::io::Cursor;
use std::ops::Range;
use std::os::unix::fs::FileExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::path::PathBuf;

use goblin::elf::Elf;
use goblin::elf::header;
pub use reverie_liteinst::startup::AuxvSnapshot;
use reverie_liteinst::startup::InterpreterImage;
pub use reverie_liteinst::startup::InterpreterPlan;
pub use reverie_liteinst::startup::PlanError;
use sha2::Digest;
use sha2::Sha256;

use crate::Command;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileIdentity {
    pub device: u64,
    pub inode: u64,
    pub size: u64,
    pub mode: u32,
    pub modified: (i64, i64),
    pub changed: (i64, i64),
    pub sha256: [u8; 32],
}

fn identity(metadata: &Metadata, bytes: &[u8]) -> FileIdentity {
    FileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
        size: metadata.len(),
        mode: metadata.mode(),
        modified: (metadata.mtime(), metadata.mtime_nsec()),
        changed: (metadata.ctime(), metadata.ctime_nsec()),
        sha256: Sha256::digest(bytes).into(),
    }
}

fn snapshot(
    file: &File,
    max_bytes: usize,
    executable: bool,
) -> io::Result<(FileIdentity, Box<[u8]>)> {
    let before = file.metadata()?;
    if !before.is_file() || executable && before.mode() & 0o111 == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "startup input is not a regular executable file",
        ));
    }
    let size = usize::try_from(before.len())
        .ok()
        .filter(|size| *size <= max_bytes)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "startup input exceeds the image byte limit",
            )
        })?;
    let mut bytes = Vec::new();
    bytes.try_reserve_exact(size).map_err(io::Error::other)?;
    bytes.resize(size, 0);
    file.read_exact_at(&mut bytes, 0)?;
    let after = file.metadata()?;
    let result = identity(&before, &bytes);
    if result != identity(&after, &bytes) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "startup input changed while reading",
        ));
    }
    Ok((result, bytes.into_boxed_slice()))
}

/// An open file and the exact bytes inspected on that descriptor.
/// Keeping the descriptor pins the inode, not its contents or its pathname.
pub struct PinnedImage {
    file: File,
    lookup_path: PathBuf,
    identity: FileIdentity,
    bytes: Box<[u8]>,
    executable: bool,
}

impl PinnedImage {
    fn open(path: PathBuf, max_bytes: usize) -> io::Result<Self> {
        Self::open_with_permissions(path, max_bytes, true)
    }

    fn open_with_permissions(
        path: PathBuf,
        max_bytes: usize,
        executable: bool,
    ) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&path)?;
        let (identity, bytes) = snapshot(&file, max_bytes, executable)?;
        Ok(Self {
            file,
            lookup_path: path,
            identity,
            bytes,
            executable,
        })
    }

    pub fn file(&self) -> &File {
        &self.file
    }
    pub fn lookup_path(&self) -> &Path {
        &self.lookup_path
    }
    pub fn identity(&self) -> &FileIdentity {
        &self.identity
    }
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Recheck the retained descriptor, without reopening a possibly replaced path.
    /// This detects observed changes; it does not prevent later or racing writes.
    pub fn revalidate(&self) -> io::Result<()> {
        let (current, bytes) = snapshot(&self.file, self.bytes.len(), self.executable)?;
        if current != self.identity || bytes != self.bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "retained startup input changed",
            ));
        }
        Ok(())
    }
}

/// Original executable/interpreter inputs, not a controlled-artifact descriptor.
pub struct InterpreterInputs {
    program: PinnedImage,
    interpreter: PinnedImage,
    interpreter_path: PathBuf,
    required_span: u64,
    required_alignment: u64,
}

impl InterpreterInputs {
    pub fn program(&self) -> &PinnedImage {
        &self.program
    }
    pub fn interpreter(&self) -> &PinnedImage {
        &self.interpreter
    }
    pub fn interpreter_path(&self) -> &Path {
        &self.interpreter_path
    }
    pub fn required_span(&self) -> u64 {
        self.required_span
    }
    pub fn required_alignment(&self) -> u64 {
        self.required_alignment
    }

    /// The future entry owner supplies real initial auxv and complete mapping data.
    /// Success validates layout only, not provenance, reservation ownership or readiness.
    pub fn plan_at_original_base(
        &self,
        initial: &AuxvSnapshot,
        reservation: Range<u64>,
        other_mappings: &[Range<u64>],
    ) -> Result<InterpreterPlan<'_>, PlanError> {
        InterpreterImage::parse(self.interpreter.bytes())?.plan_at_original_base(
            initial,
            reservation,
            other_mappings,
        )
    }
}

/// Prepare before interpreter replacement, from inside the already-entered launch
/// filesystem (including image chroot/binds). Never call this to inspect a host
/// pathname on behalf of a not-yet-entered container. No context proof is minted.
/// Resolution reuses the same Command::find_program used by the actual backend.
/// max_image_bytes bounds each retained image, not blocking filesystem I/O time.
pub fn prepare_in_current_filesystem(
    command: &Command,
    max_image_bytes: usize,
) -> io::Result<InterpreterInputs> {
    let program = PinnedImage::open(command.find_program()?, max_image_bytes)?;
    let header = Elf::parse_header(program.bytes())
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if header.e_ident[header::EI_CLASS] != header::ELFCLASS64
        || header.e_ident[header::EI_DATA] != header::ELFDATA2LSB
        || header.e_machine != header::EM_X86_64
        || !matches!(header.e_type, header::ET_EXEC | header::ET_DYN)
    {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "startup preparation requires an x86-64 ELF executable",
        ));
    }
    let interpreter_path = crate::interp::elf_interp_from_reader(&mut Cursor::new(
        program.bytes(),
    ))?
    .ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::Unsupported,
            "startup preparation requires PT_INTERP",
        )
    })?;
    let lookup = lookup_path(command, &interpreter_path)?;
    let interpreter = PinnedImage::open(lookup, max_image_bytes)?;
    let image = InterpreterImage::parse(interpreter.bytes())
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let required_span = image.required_span();
    let required_alignment = image.required_alignment();
    Ok(InterpreterInputs {
        program,
        interpreter,
        interpreter_path,
        required_span,
        required_alignment,
    })
}

fn lookup_path(command: &Command, path: &Path) -> io::Result<PathBuf> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    let directory = match command.get_current_dir() {
        Some(directory) if directory.is_absolute() => directory.to_path_buf(),
        Some(directory) => std::env::current_dir()?.join(directory),
        None => std::env::current_dir()?,
    };
    Ok(directory.join(path))
}

#[cfg(test)]
mod tests;
