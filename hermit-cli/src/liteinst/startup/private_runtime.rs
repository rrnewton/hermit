//! Unactivated controlled-startup image preparation and descriptor transfer.
//!
//! Preparation must run in the actual entered filesystem before interpreter
//! replacement. A caller-selected digest identifies bytes, not trusted CRT
//! semantics. The future interpreter binder must use this retained sealed file
//! object, not reopen its diagnostic source path or infer identity from a name.
//! Neither preparation nor attachment mounts or activates a replacement loader.

use std::fs::File;
use std::io;
use std::os::fd::AsFd;
use std::path::Path;

use goblin::elf::Elf;
use goblin::elf::header;
use goblin::elf::program_header;
use goblin::elf::section_header;
use goblin::elf::sym;

use super::PinnedImage;
use crate::Command;

/// A single canonical decimal descriptor, at least 3, in the initial environment.
/// Pre-TLS discovery must reject duplicates, malformed values and overflow.
/// This is independent of, and changes nothing in, bootstrap schemas V1–V4.
pub const IMAGE_FD_ENV: &str = "HERMIT_LITEINST_PRIVATE_RUNTIME_FD";
/// Required same-file symbol; never substituted with ELF e_entry.
pub const CRT_SYMBOL: &str = "pe_private_crt_entry";
pub const KERNEL_SYMBOL: &str = "pe_kernel_entry";
/// The mapped-input acquisition component's maximum file-byte view.
pub const MAX_IMAGE_BYTES: usize = 512 * 1024 * 1024;

/// Necessary linked metadata, not proof that the function implements the CRT.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CrtSymbol {
    pub virtual_address: u64,
    pub file_offset: u64,
    pub size: u64,
}

/// One retained source snapshot and its immutable runtime file object.
/// Duplication of `file()` preserves the object; reopening `lookup_path` does not.
pub struct PrivateRuntimeImage {
    source: PinnedImage,
    sealed: File,
    crt: CrtSymbol,
}

impl PrivateRuntimeImage {
    pub(super) fn from_source(source: PinnedImage) -> io::Result<Self> {
        if source.identity().mode & 0o111 == 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "private runtime is not executable",
            ));
        }
        let crt = validate_crt(source.bytes())?;
        let sealed = reverie::process::sealed::create(
            c"hermit-liteinst-private-runtime",
            source.bytes(),
            libc::STDERR_FILENO + 1,
        )?;
        source.revalidate()?;
        Ok(Self {
            source,
            sealed: sealed.into(),
            crt,
        })
    }

    pub fn source(&self) -> &PinnedImage {
        &self.source
    }

    pub fn file(&self) -> &File {
        &self.sealed
    }

    pub fn crt_symbol(&self) -> &CrtSymbol {
        &self.crt
    }

    /// Attach this exact artifact to a real command without changing its program.
    /// No environment selection may be silently overwritten. The command owns a
    /// private duplicate; the original sealed descriptor/provenance stay together
    /// in the returned object for the eventual interpreter binding operation.
    pub fn attach(self, mut command: Command) -> io::Result<PreparedRuntimeLaunch> {
        if command.get_env(IMAGE_FD_ENV).is_some() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "private runtime descriptor discovery is already selected",
            ));
        }
        let inherited_fd = command.inherit_fd(self.sealed.as_fd())?;
        command.env(IMAGE_FD_ENV, inherited_fd.to_string());
        Ok(PreparedRuntimeLaunch {
            command,
            image: self,
            inherited_fd,
        })
    }
}

/// Keeps command transfer and future interpreter binding tied to one artifact.
///
/// The parent descriptors live until this owner/command is dropped, including
/// spawn failures; successful exec leaves a separate child descriptor. Acquisition
/// only borrows it. The future continuation must arrange its protection and final
/// close, not infer that dropping this parent closes the child's copy.
/// The command must not subsequently overwrite discovery or close the inherited
/// descriptor in an unsafe pre-exec callback. No executing-bootstrap proof is minted.
pub struct PreparedRuntimeLaunch<C = Command> {
    command: C,
    image: PrivateRuntimeImage,
    inherited_fd: i32,
}

impl<C> PreparedRuntimeLaunch<C> {
    pub(super) fn into_command(self) -> (C, PreparedRuntimeLaunch<()>) {
        (
            self.command,
            PreparedRuntimeLaunch {
                command: (),
                image: self.image,
                inherited_fd: self.inherited_fd,
            },
        )
    }

    pub fn command_mut(&mut self) -> &mut C {
        &mut self.command
    }

    pub fn image(&self) -> &PrivateRuntimeImage {
        &self.image
    }

    pub fn inherited_fd(&self) -> i32 {
        self.inherited_fd
    }
}

impl PreparedRuntimeLaunch {
    /// Use the existing checked conversion, preserving the owned pre-exec callback.
    /// Unsupported container configuration remains an error, with RAII cleanup.
    pub fn try_into_std(self) -> io::Result<PreparedRuntimeLaunch<std::process::Command>> {
        Ok(PreparedRuntimeLaunch {
            command: self.command.try_into_std()?,
            image: self.image,
            inherited_fd: self.inherited_fd,
        })
    }
}

/// Select the actual runtime artifact in the already-entered launch filesystem.
/// Relative paths use the command's eventual cwd. The expected digest is supplied
/// by the artifact-selection caller; there is no host fallback or /proc/self/exe.
/// A named symbol is only necessary metadata. Actual CRT source/link binding and
/// use of this same sealed object by a controlled interpreter remain prerequisites
/// for activation; existing CLI launch paths do not call this function.
pub fn prepare_in_current_filesystem(
    command: &Command,
    path: &Path,
    expected_sha256: [u8; 32],
    max_bytes: usize,
) -> io::Result<PrivateRuntimeImage> {
    if max_bytes == 0 || max_bytes > MAX_IMAGE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid runtime image limit",
        ));
    }
    let lookup = super::lookup_path(command, path)?;
    let source = PinnedImage::open(lookup, max_bytes)?;
    if source.identity().sha256 != expected_sha256 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "runtime artifact digest mismatch",
        ));
    }
    PrivateRuntimeImage::from_source(source)
}

fn unsupported() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "private runtime lacks supported same-file pe_private_crt_entry binding",
    )
}

fn inside(offset: u64, length: u64, size: u64) -> bool {
    offset <= size && length <= size - offset
}

fn validate_crt(bytes: &[u8]) -> io::Result<CrtSymbol> {
    validate_symbol(bytes, CRT_SYMBOL)
}

pub(super) fn validate_kernel_entry(bytes: &[u8]) -> io::Result<CrtSymbol> {
    let kernel = validate_symbol(bytes, KERNEL_SYMBOL)?;
    let crt = validate_crt(bytes)?;
    let elf = Elf::parse(bytes).map_err(|_| unsupported())?;
    if elf.entry != kernel.virtual_address
        || kernel.virtual_address < crt.virtual_address + crt.size
            && crt.virtual_address < kernel.virtual_address + kernel.size
    {
        return Err(unsupported());
    }
    Ok(kernel)
}

fn validate_symbol(bytes: &[u8], name: &str) -> io::Result<CrtSymbol> {
    let elf = Elf::parse(bytes).map_err(|_| unsupported())?;
    if elf.header.e_ident[header::EI_CLASS] != header::ELFCLASS64
        || elf.header.e_ident[header::EI_DATA] != header::ELFDATA2LSB
        || elf.header.e_ident[header::EI_VERSION] != header::EV_CURRENT
        || elf.header.e_version != u32::from(header::EV_CURRENT)
        || elf.header.e_machine != header::EM_X86_64
        || elf.header.e_type != header::ET_DYN
        || elf.header.e_ehsize != 64
        || elf.header.e_phentsize != 56
        || elf.header.e_shentsize != 64
        || !(1..=128).contains(&elf.program_headers.len())
        || !(1..=4096).contains(&elf.section_headers.len())
        || elf
            .section_headers
            .iter()
            .filter(|section| section.sh_type == section_header::SHT_SYMTAB)
            .count()
            != 1
        || elf.syms.len() > 1_000_000
    {
        return Err(unsupported());
    }
    let mut matches = elf
        .syms
        .iter()
        .filter(|symbol| elf.strtab.get_at(symbol.st_name) == Some(name));
    let symbol = matches.next().ok_or_else(unsupported)?;
    if matches.next().is_some()
        || symbol.st_type() != sym::STT_FUNC
        || !((symbol.st_bind() == sym::STB_GLOBAL && symbol.st_other == sym::STV_HIDDEN)
            || (symbol.st_bind() == sym::STB_LOCAL
                && matches!(symbol.st_other, sym::STV_DEFAULT | sym::STV_HIDDEN)))
        || symbol.st_shndx == 0
        || symbol.st_value == 0
        || symbol.st_size == 0
    {
        return Err(unsupported());
    }
    let end = symbol
        .st_value
        .checked_add(symbol.st_size)
        .ok_or_else(unsupported)?;
    let section = elf
        .section_headers
        .get(symbol.st_shndx)
        .ok_or_else(unsupported)?;
    let flags = u64::from(
        section_header::SHF_ALLOC | section_header::SHF_EXECINSTR | section_header::SHF_WRITE,
    );
    if section.sh_type != section_header::SHT_PROGBITS
        || section.sh_flags & flags
            != u64::from(section_header::SHF_ALLOC | section_header::SHF_EXECINSTR)
        || !inside(section.sh_offset, section.sh_size, bytes.len() as u64)
        || symbol.st_value < section.sh_addr
        || !inside(
            symbol.st_value - section.sh_addr,
            symbol.st_size,
            section.sh_size,
        )
    {
        return Err(unsupported());
    }
    let file_offset = section.sh_offset + (symbol.st_value - section.sh_addr);
    let mut covering = 0;
    for segment in &elf.program_headers {
        if segment.p_type == program_header::PT_INTERP {
            return Err(unsupported());
        }
        if segment.p_type != program_header::PT_LOAD {
            continue;
        }
        let segment_end = segment
            .p_vaddr
            .checked_add(segment.p_memsz)
            .ok_or_else(unsupported)?;
        if segment.p_filesz > segment.p_memsz
            || !inside(segment.p_offset, segment.p_filesz, bytes.len() as u64)
        {
            return Err(unsupported());
        }
        if segment.p_vaddr < end && segment_end > symbol.st_value {
            covering += 1;
            if segment.p_flags != program_header::PF_R | program_header::PF_X
                || symbol.st_value < segment.p_vaddr
                || !inside(
                    symbol.st_value - segment.p_vaddr,
                    symbol.st_size,
                    segment.p_filesz,
                )
                || segment.p_offset + (symbol.st_value - segment.p_vaddr) != file_offset
            {
                return Err(unsupported());
            }
        }
    }
    if covering != 1 {
        return Err(unsupported());
    }
    Ok(CrtSymbol {
        virtual_address: symbol.st_value,
        file_offset,
        size: symbol.st_size,
    })
}

#[cfg(test)]
pub(super) mod tests;
