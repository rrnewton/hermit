use std::env;
use std::fs;
use std::io;
use std::path::Path;
use std::path::PathBuf;

use goblin::elf::Elf;
use goblin::elf::header;
use goblin::elf::section_header;

fn has_preload_constructor(path: &Path) -> io::Result<bool> {
    let bytes = fs::read(path)?;
    let elf =
        Elf::parse(&bytes).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if elf.header.e_type != header::ET_DYN || elf.header.e_machine != header::EM_X86_64 {
        return Ok(false);
    }
    let Some((initializer_index, initializer)) =
        elf.dynsyms.iter().enumerate().find(|(_, symbol)| {
            elf.dynstrtab.get_at(symbol.st_name) == Some("reverie_liteinst_initialize")
        })
    else {
        return Ok(false);
    };
    let Some(init_array) = elf
        .section_headers
        .iter()
        .find(|section| section.sh_type == section_header::SHT_INIT_ARRAY)
    else {
        return Ok(false);
    };
    let init_start = init_array.sh_addr;
    let init_end = init_start.saturating_add(init_array.sh_size);
    let relocated = elf
        .dynrelas
        .iter()
        .chain(elf.dynrels.iter())
        .any(|relocation| {
            (init_start..init_end).contains(&relocation.r_offset)
                && relocation.r_sym == initializer_index
        });
    let direct = usize::try_from(init_array.sh_offset)
        .ok()
        .and_then(|start| {
            usize::try_from(init_array.sh_size)
                .ok()
                .and_then(|size| bytes.get(start..start.checked_add(size)?))
        })
        .unwrap_or_default()
        .chunks_exact(8)
        .any(|entry| {
            u64::from_le_bytes(entry.try_into().expect("eight-byte constructor entry"))
                == initializer.st_value
        });
    Ok(relocated || direct)
}

fn main() {
    println!("cargo:rerun-if-env-changed=HERMIT_LITEINST_STAGE");
    println!("cargo:rerun-if-changed=Cargo.lock");
    let destination = PathBuf::from(
        env::var_os("HERMIT_LITEINST_STAGE")
            .expect("HERMIT_LITEINST_STAGE must name the stable runtime output path"),
    );
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo did not set OUT_DIR"));
    let profile_dir = out_dir
        .ancestors()
        .nth(3)
        .expect("standalone runtime OUT_DIR has no profile ancestor");
    let deps = profile_dir.join("deps");
    let candidates = fs::read_dir(&deps)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", deps.display()))
        .map(|entry| entry.expect("failed to read LiteInst build dependency artifact"))
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name().is_some_and(|name| {
                let name = name.as_encoded_bytes();
                name.starts_with(b"libreverie_liteinst-") && name.ends_with(b".so")
            })
        })
        .filter(|path| {
            has_preload_constructor(path)
                .unwrap_or_else(|error| panic!("failed to validate {}: {error}", path.display()))
        })
        .collect::<Vec<_>>();
    assert_eq!(
        candidates.len(),
        1,
        "expected one constructor-enabled LiteInst DSO in {}, found {candidates:?}",
        deps.display(),
    );
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)
            .unwrap_or_else(|error| panic!("failed to create {}: {error}", parent.display()));
    }
    if fs::symlink_metadata(&destination).is_ok() {
        fs::remove_file(&destination).unwrap_or_else(|error| {
            panic!(
                "failed to replace existing LiteInst stage {}: {error}",
                destination.display()
            )
        });
    }
    fs::copy(&candidates[0], &destination).unwrap_or_else(|error| {
        panic!(
            "failed to stage {} as {}: {error}",
            candidates[0].display(),
            destination.display()
        )
    });
    assert!(
        destination.is_file()
            && !fs::symlink_metadata(&destination)
                .expect("read staged LiteInst runtime metadata")
                .file_type()
                .is_symlink(),
        "LiteInst runtime stage is missing or not a real file: {}",
        destination.display()
    );
}
