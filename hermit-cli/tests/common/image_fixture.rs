/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The OCI image fixture shared by the `--image` end-to-end tests: a
//! no-libc guest binary and the two layer tars both local image
//! formats (`oci-dir:` and `docker-archive:`) are built from, in-test
//! with no network and no podman. Extracted from
//! `tests/image_archive.rs` so `tests/record_image.rs` records and
//! replays against the same fixture `hermit run --image` runs.

#![allow(dead_code)]

use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;

pub const GUEST_MTIME: i64 = 1_650_000_000;
pub const APPDIR_MTIME: i64 = 1_600_000_123;

pub static GUEST: OnceLock<PathBuf> = OnceLock::new();

/// The no-libc guest: prints a marker, the image-declared env, and its
/// working directory so tests can prove config application. It uses raw
/// x86_64 syscalls (write, getcwd, exit) from its own `_start` and links
/// with `-nostdlib -nodefaultlibs`, so building it needs no libc at all —
/// the pinned validation root ships no static libc for `cc -static`.
/// Its stdout is byte-for-byte the old libc guest's: the marker line,
/// `TEST_ENV=`/`STORE_ENV=` (or `(unset)`), and `CWD=` when getcwd
/// succeeds.
pub fn guest_binary() -> &'static Path {
    GUEST.get_or_init(|| {
        let build_root = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join("image-archive-guest")
            .join(std::process::id().to_string());
        std::fs::create_dir_all(&build_root).expect("create guest build dir");
        let source = build_root.join("guest.c");
        std::fs::write(
            &source,
            r#"
/* No-libc guest: raw x86_64 syscalls only (write=1, getcwd=79, exit=60). */
typedef unsigned long u64;
static inline long sys3(long n, long a, long b, long c) {
    long ret;
    __asm__ volatile("syscall" : "=a"(ret)
                     : "a"(n), "D"(a), "S"(b), "d"(c)
                     : "rcx", "r11", "memory");
    return ret;
}
static u64 slen(const char *s) {
    u64 n = 0;
    while (s[n] != '\0') n++;
    return n;
}
static void write_str(const char *s) {
    u64 left = slen(s);
    const char *p = s;
    while (left > 0) {
        long w = sys3(1, 1, (long)p, (long)left);
        if (w <= 0) return;
        p += w;
        left -= (u64)w;
    }
}
static const char *find_env(char **envp, const char *name) {
    u64 nlen = slen(name);
    for (u64 i = 0; envp[i] != 0; i++) {
        const char *e = envp[i];
        u64 j = 0;
        while (j < nlen && e[j] == name[j]) j++;
        if (j == nlen && e[j] == '=') return e + nlen + 1;
    }
    return "(unset)";
}
static char cwd_buf[4096];
static char io_buf[4096];
/* Mount-test operations, one per guest argument after argv[0]:
 * "read:<path>" prints FILE <path>=<contents> (or =ERR<errno>),
 * "write:<path>:<text>" writes and prints WROTE <path> (or ERR<errno>).
 * With no such arguments the guest's output is exactly as before. */
static void write_errno(long r) {
    long e = -r;
    char digits[24];
    int n = 0;
    if (e == 0) digits[n++] = '0';
    while (e > 0) { digits[n++] = (char)('0' + (e % 10)); e /= 10; }
    write_str("ERR");
    while (n > 0) { char c[2] = {digits[--n], '\0'}; write_str(c); }
}
static int has_prefix(const char *s, const char *prefix) {
    while (*prefix != '\0') {
        if (*s++ != *prefix++) return 0;
    }
    return 1;
}
static void guest_ops(char **argv, long argc) {
    for (long i = 1; i < argc; i++) {
        const char *arg = argv[i];
        if (has_prefix(arg, "read:")) {
            const char *path = arg + 5;
            long fd = sys3(2, (long)path, 0, 0);
            write_str("FILE ");
            write_str(path);
            write_str("=");
            if (fd < 0) {
                write_errno(fd);
            } else {
                long n;
                while ((n = sys3(0, fd, (long)io_buf, (long)sizeof(io_buf) - 1)) > 0) {
                    io_buf[n] = '\0';
                    write_str(io_buf);
                }
                if (n < 0) write_errno(n);
                sys3(3, fd, 0, 0);
            }
            write_str("\n");
        } else if (has_prefix(arg, "write:")) {
            const char *rest = arg + 6;
            const char *colon = rest;
            while (*colon != '\0' && *colon != ':') colon++;
            if (*colon != ':') continue;
            /* The path is rest[..colon-rest]; copy it out NUL-terminated. */
            char path[512];
            u64 plen = (u64)(colon - rest);
            if (plen >= sizeof(path)) continue;
            for (u64 j = 0; j < plen; j++) path[j] = rest[j];
            path[plen] = '\0';
            long fd = sys3(2, (long)path, 0x241, 0644);
            write_str("WROTE ");
            write_str(path);
            write_str("=");
            if (fd < 0) {
                write_errno(fd);
            } else {
                const char *text = colon + 1;
                long w = sys3(1, fd, (long)text, (long)slen(text));
                if (w < 0) write_errno(w); else write_str(text);
                sys3(3, fd, 0, 0);
            }
            write_str("\n");
        }
    }
}
void guest_main(long *stack) {
    long argc = stack[0];
    char **argv = (char **)(stack + 1);
    char **envp = argv + argc + 1;
    write_str("GUEST_MARKER=hello-from-image\n");
    guest_ops(argv, argc);
    write_str("TEST_ENV=");
    write_str(find_env(envp, "TEST_ENV"));
    write_str("\n");
    write_str("STORE_ENV=");
    write_str(find_env(envp, "STORE_ENV"));
    write_str("\n");
    long r = sys3(79, (long)cwd_buf, (long)sizeof(cwd_buf), 0);
    if (r > 0) {
        cwd_buf[sizeof(cwd_buf) - 1] = '\0';
        write_str("CWD=");
        write_str(cwd_buf);
        write_str("\n");
    }
}
__asm__(
    ".globl _start\n"
    "_start:\n"
    " mov %rsp, %rdi\n"
    " and $-16, %rsp\n"
    " call guest_main\n"
    " mov $60, %rax\n"
    " xor %edi, %edi\n"
    " syscall\n");
"#,
        )
        .expect("write guest source");
        let guest = build_root.join("guest");
        let output = Command::new("cc")
            .args([
                "-nostdlib",
                "-nodefaultlibs",
                "-ffreestanding",
                "-fno-builtin",
                "-static",
                "-O2",
                "-Wall",
                "-Wextra",
                "-Werror",
                "-o",
            ])
            .arg(&guest)
            .arg(&source)
            .output()
            .expect("failed to run cc for the image guest");
        assert!(
            output.status.success(),
            "image guest compilation failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        guest
    })
}

// ---------------------------------------------------------------------------
// Layer fixture construction (shared verbatim by both image formats)
// ---------------------------------------------------------------------------

pub struct Entry {
    pub path: String,
    pub kind: tar::EntryType,
    pub mode: u32,
    pub mtime: u64,
    pub uid: u64,
    pub gid: u64,
    pub data: Vec<u8>,
    pub link: Option<String>,
}

pub fn entry(path: &str, kind: tar::EntryType, mode: u32, data: &[u8]) -> Entry {
    Entry {
        path: path.to_string(),
        kind,
        mode,
        mtime: 1_700_000_000,
        uid: 0,
        gid: 0,
        data: data.to_vec(),
        link: None,
    }
}

pub fn dir_entry(path: &str, mode: u32, mtime: u64) -> Entry {
    let mut e = entry(path, tar::EntryType::Directory, mode, b"");
    e.mtime = mtime;
    e
}

pub fn build_tar(entries: &[Entry]) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    for e in entries {
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(e.kind);
        header.set_mode(e.mode);
        header.set_mtime(e.mtime);
        header.set_uid(e.uid);
        header.set_gid(e.gid);
        header.set_size(e.data.len() as u64);
        if let Some(link) = &e.link {
            header.set_link_name(link).unwrap();
        }
        if e.kind == tar::EntryType::Char {
            header.set_device_major(5).unwrap();
            header.set_device_minor(1).unwrap();
        }
        builder
            .append_data(&mut header, &e.path, e.data.as_slice())
            .unwrap();
    }
    builder.into_inner().unwrap()
}

/// The two fixture layers (plain tar bytes). Layer 1 carries a directory
/// tree with a guest binary, a hardlink, a symlink, a FIFO, a `/dev`
/// device entry to skip, and files later layers delete; layer 2 edits one
/// file, whiteouts one file, and makes one directory opaque.
pub fn fixture_layers(
    extra_layer1: &[Entry],
    layer1_patch: Option<(&str, &str)>,
) -> (Vec<u8>, Vec<u8>) {
    let guest = std::fs::read(guest_binary()).unwrap();
    let mut guest_entry = entry("bin/guest", tar::EntryType::Regular, 0o755, &guest);
    guest_entry.mtime = GUEST_MTIME as u64;
    guest_entry.uid = 1234;
    guest_entry.gid = 4321;
    let mut hardlink = entry("bin/guest-hard", tar::EntryType::Link, 0o755, b"");
    hardlink.link = Some("bin/guest".to_string());
    let mut symlink = entry("link.txt", tar::EntryType::Symlink, 0o777, b"");
    symlink.link = Some("etc/marker.txt".to_string());
    let mut layer1 = vec![
        dir_entry("bin", 0o755, 1_600_000_000),
        guest_entry,
        hardlink,
        dir_entry("etc", 0o755, 1_600_000_000),
        entry(
            "etc/marker.txt",
            tar::EntryType::Regular,
            0o640,
            b"hello-layer1\n",
        ),
        entry(
            "etc/to-delete.txt",
            tar::EntryType::Regular,
            0o644,
            b"gone\n",
        ),
        dir_entry("opt", 0o755, 1_600_000_000),
        dir_entry("opt/opaque", 0o755, 1_600_000_000),
        entry(
            "opt/opaque/old.txt",
            tar::EntryType::Regular,
            0o644,
            b"old\n",
        ),
        dir_entry("opt/appdir", 0o750, APPDIR_MTIME as u64),
        entry(
            "opt/appdir/data.txt",
            tar::EntryType::Regular,
            0o600,
            b"data\n",
        ),
        symlink,
        dir_entry("dev", 0o755, 1_600_000_000),
        entry("dev/console", tar::EntryType::Char, 0o600, b""),
        dir_entry("run", 0o755, 1_600_000_000),
        entry("run/myfifo", tar::EntryType::Fifo, 0o644, b""),
    ];
    for extra in extra_layer1 {
        layer1.push(Entry {
            path: extra.path.clone(),
            kind: extra.kind,
            mode: extra.mode,
            mtime: extra.mtime,
            uid: extra.uid,
            gid: extra.gid,
            data: extra.data.clone(),
            link: extra.link.clone(),
        });
    }
    let mut layer1 = build_tar(&layer1);
    if let Some((placeholder, target)) = layer1_patch {
        patch_entry_name(&mut layer1, placeholder, target);
    }
    let layer2 = build_tar(&[
        entry(
            "etc/marker.txt",
            tar::EntryType::Regular,
            0o640,
            b"hello-layer2\n",
        ),
        entry("etc/.wh.to-delete.txt", tar::EntryType::Regular, 0o644, b""),
        entry(
            "opt/opaque/.wh..wh..opq",
            tar::EntryType::Regular,
            0o644,
            b"",
        ),
        entry(
            "opt/opaque/new.txt",
            tar::EntryType::Regular,
            0o644,
            b"new\n",
        ),
    ]);
    (layer1, layer2)
}

/// Rewrite a built tar's placeholder entry name (same length) and fix the
/// header checksum: the tar writer refuses `..`/absolute names, so hostile
/// reader inputs are patched into a valid archive.
pub fn patch_entry_name(tar: &mut [u8], placeholder: &str, target: &str) {
    assert_eq!(placeholder.len(), target.len());
    let at = tar
        .windows(placeholder.len())
        .position(|w| w == placeholder.as_bytes())
        .expect("placeholder name in built tar");
    tar[at..at + target.len()].copy_from_slice(target.as_bytes());
    let block = at / 512 * 512;
    for byte in &mut tar[block + 148..block + 156] {
        *byte = b' ';
    }
    let sum: u32 = tar[block..block + 512].iter().map(|b| *b as u32).sum();
    let encoded = format!("{sum:06o}\0 ");
    tar[block + 148..block + 156].copy_from_slice(encoded.as_bytes());
}

pub fn sha256_hex(data: &[u8]) -> String {
    detcore::Digest::new(data).to_string()
}

pub fn gzip(data: &[u8]) -> Vec<u8> {
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    encoder.write_all(data).unwrap();
    encoder.finish().unwrap()
}

pub fn config_bytes(diff_ids: &[String]) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "architecture": "amd64",
        "os": "linux",
        "created": "2024-01-01T00:00:00Z",
        "config": {
            "Env": ["TEST_ENV=from-image", "PATH=/bin:/usr/bin"],
            "WorkingDir": "/opt/appdir",
            "Cmd": ["/bin/guest"]
        },
        "rootfs": {
            "type": "layers",
            "diff_ids": diff_ids.iter().map(|d| format!("sha256:{d}")).collect::<Vec<_>>()
        }
    }))
    .unwrap()
}

/// One layer as stored: blob bytes plus its OCI media type.
pub struct StoredLayer {
    pub blob: Vec<u8>,
    pub media_type: &'static str,
}

pub const MT_TAR: &str = "application/vnd.oci.image.layer.v1.tar";
pub const MT_TAR_GZIP: &str = "application/vnd.oci.image.layer.v1.tar+gzip";
pub const MT_TAR_ZSTD: &str = "application/vnd.oci.image.layer.v1.tar+zstd";

pub fn blob_path(root: &Path, hex: &str) -> PathBuf {
    root.join("blobs").join("sha256").join(hex)
}

pub fn write_blob(root: &Path, bytes: &[u8]) -> String {
    let hex = sha256_hex(bytes);
    let path = blob_path(root, &hex);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, bytes).unwrap();
    hex
}

/// Write a complete OCI layout directory; returns the manifest digest hex.
pub fn write_oci_dir(root: &Path, config: &[u8], layers: &[StoredLayer]) -> String {
    std::fs::create_dir_all(root).unwrap();
    std::fs::write(
        root.join("oci-layout"),
        br#"{"imageLayoutVersion":"1.0.0"}"#,
    )
    .unwrap();
    let config_hex = write_blob(root, config);
    let layer_descriptors: Vec<_> = layers
        .iter()
        .map(|layer| {
            let hex = write_blob(root, &layer.blob);
            serde_json::json!({
                "mediaType": layer.media_type,
                "digest": format!("sha256:{hex}"),
                "size": layer.blob.len(),
            })
        })
        .collect();
    let manifest = serde_json::to_vec(&serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {
            "mediaType": "application/vnd.oci.image.config.v1+json",
            "digest": format!("sha256:{config_hex}"),
            "size": config.len(),
        },
        "layers": layer_descriptors,
    }))
    .unwrap();
    let manifest_hex = write_blob(root, &manifest);
    let index = serde_json::to_vec(&serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.index.v1+json",
        "manifests": [{
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "digest": format!("sha256:{manifest_hex}"),
            "size": manifest.len(),
            "platform": {"architecture": "amd64", "os": "linux"},
        }],
    }))
    .unwrap();
    std::fs::write(root.join("index.json"), index).unwrap();
    manifest_hex
}

/// The valid OCI fixture: layer 1 plain, layer 2 gzip (gzip coverage), both
/// verified against diff_ids of the plain tars.
pub fn write_valid_oci_dir(root: &Path) {
    let (layer1, layer2) = fixture_layers(&[], None);
    let config = config_bytes(&[sha256_hex(&layer1), sha256_hex(&layer2)]);
    write_oci_dir(
        root,
        &config,
        &[
            StoredLayer {
                blob: layer1,
                media_type: MT_TAR,
            },
            StoredLayer {
                blob: gzip(&layer2),
                media_type: MT_TAR_GZIP,
            },
        ],
    );
}

/// Write a docker-archive outer tar from raw entries.
pub fn write_outer_tar(path: &Path, entries: &[Entry]) {
    std::fs::write(path, build_tar(entries)).unwrap();
}

pub fn outer_file(path: &str, data: &[u8]) -> Entry {
    entry(path, tar::EntryType::Regular, 0o644, data)
}

/// The valid docker-archive fixture with gzip-compressed layer tars
/// (`diff_id`s still name the plain tars).
pub fn write_valid_docker_archive_gzip(path: &Path) {
    let (layer1, layer2) = fixture_layers(&[], None);
    let config = config_bytes(&[sha256_hex(&layer1), sha256_hex(&layer2)]);
    let manifest = serde_json::to_vec(&serde_json::json!([{
        "Config": "config.json",
        "RepoTags": ["hermit-test:latest"],
        "Layers": ["layer1/layer.tar", "layer2/layer.tar"],
    }]))
    .unwrap();
    write_outer_tar(
        path,
        &[
            outer_file("config.json", &config),
            outer_file("layer1/layer.tar", &gzip(&layer1)),
            outer_file("layer2/layer.tar", &gzip(&layer2)),
            outer_file("manifest.json", &manifest),
        ],
    );
}

/// The valid docker-archive fixture over the same layers (plain layer tars,
/// the canonical `docker save` shape).
pub fn write_valid_docker_archive(path: &Path) {
    let (layer1, layer2) = fixture_layers(&[], None);
    let config = config_bytes(&[sha256_hex(&layer1), sha256_hex(&layer2)]);
    let manifest = serde_json::to_vec(&serde_json::json!([{
        "Config": "config.json",
        "RepoTags": ["hermit-test:latest"],
        "Layers": ["layer1/layer.tar", "layer2/layer.tar"],
    }]))
    .unwrap();
    write_outer_tar(
        path,
        &[
            outer_file("config.json", &config),
            outer_file("layer1/layer.tar", &layer1),
            outer_file("layer2/layer.tar", &layer2),
            outer_file("manifest.json", &manifest),
        ],
    );
}

// ---------------------------------------------------------------------------
// Running Hermit and inspecting the materialized cache
// ---------------------------------------------------------------------------
