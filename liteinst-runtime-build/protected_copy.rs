//! Create-new copy used by both Cargo and native artifact staging.

use std::fs::File;
use std::fs::OpenOptions;
use std::io;
use std::path::Path;

pub(crate) fn copy_into_protected_stage(source: &Path, destination: &Path) -> io::Result<()> {
    let mut source_file = File::open(source)?;
    let source_permissions = source_file.metadata()?.permissions();
    let mut destination_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(destination)?;
    if !destination_file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "LiteInst stage is not a regular file",
        ));
    }
    io::copy(&mut source_file, &mut destination_file)?;
    destination_file.set_permissions(source_permissions)?;
    destination_file.sync_all()
}
