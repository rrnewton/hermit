//! The compat corpus's pinned-guest contract
//! (<https://github.com/rrnewton/hermit/issues/3712>).
//!
//! A compat cell measures a real program. When the program, its libc and its
//! loader come from the validation host, the verdict depends on that host's
//! glibc, so the same commit can pass on one machine and fail on the next.
//! Every cell a compat-family bucket (`compat` or `compat-*`) gates therefore
//! has to run from the pinned image named by `ci/hermetic/image.digest`.
//!
//! Two halves enforce that:
//!
//! * At run time, the test harness refuses a `guest_root: pinned` manifest
//!   unless it is running inside that image (`runner::detect_guest_root`).
//!   It checks the root by content: every executable the listing records
//!   must resolve to its recorded `/nix/store` file, the loader and libc
//!   must hold their recorded bytes, the listed directories must hold no
//!   other executable, the paths the listing records as absent (host-only
//!   probes, the guest `PATH` directories the image lacks, the loader's
//!   preload files) must not exist, nothing may be mounted over a directory
//!   of the image, and the whole `/nix/store` must have its recorded content
//!   digest ([`tree_digest`]), so no program, library or script in it holds
//!   bytes the image does not ([`ImagePaths::verify_root`]).
//! * Statically, this module refuses a gating compat cell whose manifest does
//!   not declare `guest_root: pinned`, and a gating cell whose argv, or a
//!   repository script or workload arm it runs, names an absolute path in any
//!   directory that the image does not contain and the container does not
//!   provide, or runs a program from the checkout (`argv_problems` lists the
//!   rules). The image's contents are listed in `ci/hermetic/image-paths.txt`,
//!   whose `image:` line must equal the digest, so a repin forces the list to
//!   be regenerated. The check reads text: a path a cell assembles at run
//!   time escapes it, and inside the pinned root such a path finds only the
//!   image and the run's mounts.
//!
//! A cell gates when its mode selects the backend for CI (`ci`) and the
//! backend is not a reported-only `diagnostic`.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::Path;
use std::path::PathBuf;

use sha2::Digest;
use sha2::Sha256;

use crate::ci_selection::CiSelectionSpec;
use crate::manifest_value::Value;

/// The listing of the pinned image's executables.
pub const IMAGE_PATHS_FILE: &str = "ci/hermetic/image-paths.txt";
/// The pinned image reference the listing must describe.
pub const IMAGE_DIGEST_FILE: &str = "ci/hermetic/image.digest";
/// The shared workload script whose `case "$PROGRAM" in` arms run most rows.
pub const REAL_COMPAT_WORKLOAD: &str = "tests/compat/real_compat_workload.sh";

const ROOT_DIR_PLACEHOLDER: &str = "{{ROOT_DIR}}/";
const WORKLOAD_CASE_LINE: &str = "case \"$PROGRAM\" in";

/// Paths every container has that name no file of the validation host's root
/// filesystem: the kernel's interfaces and the container's own scratch space.
/// A gating argv may name a path under one without the image listing it.
const RUNTIME_ROOTS: &[&str] = &["/dev/", "/proc/", "/sys/", "/tmp/"];

/// Files the container runtime writes into every container it starts.
const RUNTIME_FILES: &[&str] = &["/etc/hostname", "/etc/hosts", "/etc/resolv.conf"];

const VALIDATE_RUN_STATE_PLACEHOLDER: &str = "{{VALIDATE_RUN_STATE}}";

/// The run-state tree that only pinned-root processes write: the
/// `<run type>compatprep.fixtures` nodes of `ci/dag/validate.json` fill its
/// `real-compat-fixtures` inside the pinned root, with the image's compilers,
/// and the cells create the rest (`shell-build`, `top-home`). A gating argv
/// may read or run a file under it. Any other run-state path is refused:
/// `strict-compat/` is filled on the host, for the host-portability bucket
/// compat.yaml. The name is a convention, not proof; test-harness also
/// requires `real-compat-fixtures` to carry the receipt the pinned root
/// writes ([`verify_fixture_receipt`]) before a cell reads it.
const PINNED_FIXTURE_ROOT: &str = "{{VALIDATE_RUN_STATE}}/compat-pinned/";

/// The fixture tree below [`PINNED_FIXTURE_ROOT`] whose receipt test-harness
/// verifies before a pinned cell starts ([`PINNED_FIXTURES`],
/// [`verify_fixture_receipt`]): the only run-state tree a gating cell may run
/// a file from, and the only value `REAL_COMPAT_FIXTURES` may have. The rest
/// of [`PINNED_FIXTURE_ROOT`] is admitted as data the cell reads or writes,
/// never as a program, because nothing vouches for the bytes there.
const PINNED_FIXTURE_TREE: &str = "{{VALIDATE_RUN_STATE}}/compat-pinned/real-compat-fixtures";

/// Shells, whose first operand is the script they run unless `-c` gives the
/// script inline.
const SHELLS: &[&str] = &["sh", "bash", "dash", "ksh", "zsh"];

/// Other interpreters whose first operand is a program file they run, unless
/// an option (`-c`, `-e`, `-E`, `-m`) gives the program inline.
const OTHER_INTERPRETERS: &[&str] = &["python", "python3", "perl", "ruby", "node", "lua", "tclsh"];

/// An absolute path that a gating cell names only as data: the image does not
/// contain it, and nothing opens or runs it.
#[derive(Clone, Copy, Debug)]
pub struct DataPath {
    /// Where the path appears: the program an argv runs, as the argv writes
    /// it; the label of a workload arm in [`REAL_COMPAT_WORKLOAD`]; or the
    /// repository-relative path of a script, for its other text.
    pub consumer: &'static str,
    /// The path. It also covers every path beneath it.
    pub path: &'static str,
    /// Why nothing opens or runs it.
    pub reason: &'static str,
}

/// Every [`DataPath`] the shipped gating corpus uses. Each entry must still be
/// needed: the shipped-corpus test removes each in turn and requires a
/// violation.
pub const DATA_PATHS: &[DataPath] = &[
    DataPath {
        consumer: "/usr/bin/basename",
        path: "/opt/hermit/bin/hermit",
        reason: "basename prints the last component of the string; it opens nothing",
    },
    DataPath {
        consumer: "/usr/bin/dirname",
        path: "/opt/hermit/bin/hermit",
        reason: "dirname prints the string without its last component; it opens nothing",
    },
    DataPath {
        consumer: "pkg-config",
        path: "/opt/hermit-compat",
        reason: "the prefix of the .pc file the arm writes; pkg-config prints the -I and -L \
                 flags under it, and the arm compares them as text",
    },
    DataPath {
        consumer: REAL_COMPAT_WORKLOAD,
        path: "/test",
        reason: "compared with $PWD to recognize Hermit's guest working directory; never opened",
    },
    DataPath {
        consumer: "tests/compat/complex_shell_workload.sh",
        path: "/step",
        reason: "the replacement text of the sed expression s/^/step:/, not a path",
    },
];

/// A path under [`PINNED_FIXTURE_ROOT`], outside [`PINNED_FIXTURE_TREE`], that
/// a gating argv may pass as an operand to the repository script it runs.
/// Nothing vouches for the bytes there (the validation host can write the
/// run-state directory), so a gating argv names such a path only as a
/// declared operand, or as the value of an [`EnvValue::PinnedFixture`]
/// variable in its `env`.
#[derive(Clone, Copy, Debug)]
pub struct RunStateOperand {
    /// The repository-relative path of the shell script the argv runs.
    pub consumer: &'static str,
    /// The path, an argv element on its own. It also covers every path
    /// beneath it.
    pub path: &'static str,
    /// What the script does with it, and why it runs no byte it did not
    /// write there itself.
    pub reason: &'static str,
}

/// Every [`RunStateOperand`] the shipped gating corpus uses. Each entry must
/// still be needed: the shipped-corpus test removes each in turn and requires
/// a violation.
pub const RUN_STATE_OPERANDS: &[RunStateOperand] = &[RunStateOperand {
    consumer: "tests/compat/complex_shell_workload.sh",
    path: "{{VALIDATE_RUN_STATE}}/compat-pinned/shell-build",
    reason: "the script's work directory: it deletes the directory, creates it again, and runs \
             only the program it then builds there from its own text with the image's compiler",
}];

/// Directories a bare program name may resolve to inside the image.
const BARE_PROGRAM_DIRS: &[&str] = &["/bin/", "/usr/bin/", "/sbin/", "/usr/sbin/"];

/// A host-only path a workload arm tests with `[[ -x PATH ]]` before falling
/// back to a path the image has. Inside the image the test fails and the arm
/// runs the fallback, so the probe is not a dependency on the host. The
/// listing records the path on an `absent:` line, and
/// [`ImagePaths::verify_root`] refuses a root where it exists, so a caller
/// cannot mount a host program there and still be counted as the pinned root.
#[derive(Clone, Copy, Debug)]
pub struct HostOnlyProbe {
    pub label: &'static str,
    pub path: &'static str,
    pub fallback: &'static str,
}

/// The git arm prefers the validation host's unwrapped git binary when one
/// exists (Meta hosts wrap /usr/bin/git); the image has only /usr/bin/git.
pub const HOST_ONLY_PROBES: &[HostOnlyProbe] = &[HostOnlyProbe {
    label: "git",
    path: "/usr/local/bin/git.meta.real",
    fallback: "/usr/bin/git",
}];

/// The directories a guest searches for a bare program name, in order: the
/// `PATH` that `hermit run --base-env=minimal` gives the guest
/// (`apply_base_and_explicit_environment` in
/// `hermit-cli/src/bin/hermit/run.rs`), and the harness runs every hermetic
/// cell with `--base-env=minimal` (`require_minimal_base_env` in
/// `runner.rs`). The shipped listing must cover each one: a directory that
/// holds listed executables is checked to hold no other, and the image's
/// missing directories are `absent:` lines, so a root where something
/// supplied `/usr/local/bin`, searched before `/usr/bin`, is refused.
pub const GUEST_PATH_DIRECTORIES: &[&str] = &[
    "/usr/local/sbin",
    "/usr/local/bin",
    "/usr/sbin",
    "/usr/bin",
    "/sbin",
    "/bin",
];

/// The preload files a loader reads from the root's `/etc` before running
/// any program. The pinned image's loader reads `/etc/ld-nix.so.preload`
/// (nixpkgs renames glibc's file; besides its own in-store `ld.so.cache` it
/// is the only `/etc` path in the loader's bytes), and `/etc/ld.so.preload`
/// is glibc's own name. The image has neither, and the shipped listing
/// records both as `absent:`, so a root where one names a library outside
/// the store, which no recorded byte would show, is refused.
pub const LOADER_PRELOAD_FILES: &[&str] = &["/etc/ld-nix.so.preload", "/etc/ld.so.preload"];

/// A file of the pinned image whose bytes [`IMAGE_PATHS_FILE`] records: its
/// dynamic loader and its libc.
#[derive(Clone, Debug, Eq, PartialEq)]
struct RecordedFile {
    /// The `/nix/store` file the path resolves to through every link.
    target: String,
    /// The lowercase hex SHA-256 of that file's bytes.
    sha256: String,
}

/// The pinned image as [`IMAGE_PATHS_FILE`] records it: its executables, the
/// `/nix/store` file each resolves to, the bytes of its loader and libc, and
/// the content digest of the whole store those files live in.
#[derive(Clone, Debug)]
pub struct ImagePaths {
    pub image: String,
    /// Every executable in a [`GUEST_PATH_DIRECTORIES`] directory of the
    /// image (today `/bin`, `/sbin` and `/usr/bin`), and its target.
    targets: BTreeMap<String, String>,
    /// The files whose bytes are recorded, by the path a guest names.
    files: BTreeMap<String, RecordedFile>,
    /// Each recorded tree (the image's `/nix/store`) and the lowercase hex
    /// [`tree_digest`] of everything in it. Every target of `targets` and
    /// `files` lies in one, so a root that keeps a program's path but holds
    /// other bytes there, or other libraries and scripts beside it, is
    /// refused.
    trees: BTreeMap<String, String>,
    /// Every directory that holds a path of `targets` or `files`, `/` included.
    directories: BTreeSet<String>,
    /// Paths the image does not have: the [`HOST_ONLY_PROBES`] a workload arm
    /// tests for, the [`GUEST_PATH_DIRECTORIES`] it lacks and the
    /// [`LOADER_PRELOAD_FILES`]. The pinned root must not have them either.
    absent: BTreeSet<String>,
}

fn is_lower_hex_sha256(text: &str) -> bool {
    text.len() == 64
        && text
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

impl ImagePaths {
    /// Parse the listing: `#` comments, one `image: <reference>` line, then
    /// `PATH TARGET` lines, `file: PATH TARGET sha256:HEX` lines,
    /// `tree: DIRECTORY sha256:HEX` lines and `absent: PATH` lines, every path
    /// and target absolute and every target inside a `tree:` directory.
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut image = None;
        let mut targets = BTreeMap::new();
        let mut files = BTreeMap::new();
        let mut trees = BTreeMap::new();
        let mut absent = BTreeSet::new();
        for (index, line) in text.lines().enumerate() {
            let number = index + 1;
            if line.trim().is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some(reference) = line.strip_prefix("image: ") {
                if image.replace(reference.trim().to_owned()).is_some() {
                    return Err(format!("{IMAGE_PATHS_FILE}:{number}: second `image:` line"));
                }
                continue;
            }
            if image.is_none() {
                return Err(format!(
                    "{IMAGE_PATHS_FILE}:{number}: a path precedes the `image:` line"
                ));
            }
            let absolute = |field: &str| field.starts_with('/');
            if let Some(path) = line.strip_prefix("absent: ") {
                if !absolute(path) || path.contains(' ') {
                    return Err(format!(
                        "{IMAGE_PATHS_FILE}:{number}: expected `absent: PATH` with one absolute \
                         path, got {line:?}"
                    ));
                }
                if !absent.insert(path.to_owned()) {
                    return Err(format!(
                        "{IMAGE_PATHS_FILE}:{number}: duplicate absent path {path}"
                    ));
                }
                continue;
            }
            if let Some(entry) = line.strip_prefix("tree: ") {
                let fields: Vec<&str> = entry.split(' ').collect();
                let [directory, digest] = fields[..] else {
                    return Err(format!(
                        "{IMAGE_PATHS_FILE}:{number}: expected `tree: DIRECTORY sha256:HEX`, \
                         got {line:?}"
                    ));
                };
                let sha256 = digest.strip_prefix("sha256:").unwrap_or_default();
                if !absolute(directory)
                    || directory.len() < 2
                    || directory.ends_with('/')
                    || !is_lower_hex_sha256(sha256)
                {
                    return Err(format!(
                        "{IMAGE_PATHS_FILE}:{number}: expected `tree: DIRECTORY sha256:HEX` with \
                         an absolute directory other than / and no trailing slash, and 64 \
                         lowercase hex digits, got {line:?}"
                    ));
                }
                if trees
                    .insert(directory.to_owned(), sha256.to_owned())
                    .is_some()
                {
                    return Err(format!(
                        "{IMAGE_PATHS_FILE}:{number}: duplicate tree {directory}"
                    ));
                }
                continue;
            }
            if let Some(entry) = line.strip_prefix("file: ") {
                let fields: Vec<&str> = entry.split(' ').collect();
                let [path, target, digest] = fields[..] else {
                    return Err(format!(
                        "{IMAGE_PATHS_FILE}:{number}: expected `file: PATH TARGET sha256:HEX`, \
                         got {line:?}"
                    ));
                };
                let sha256 = digest.strip_prefix("sha256:").unwrap_or_default();
                if !absolute(path) || !absolute(target) || !is_lower_hex_sha256(sha256) {
                    return Err(format!(
                        "{IMAGE_PATHS_FILE}:{number}: expected `file: PATH TARGET sha256:HEX` \
                         with absolute paths and 64 lowercase hex digits, got {line:?}"
                    ));
                }
                let recorded = RecordedFile {
                    target: target.to_owned(),
                    sha256: sha256.to_owned(),
                };
                if files.insert(path.to_owned(), recorded).is_some() {
                    return Err(format!(
                        "{IMAGE_PATHS_FILE}:{number}: duplicate file {path}"
                    ));
                }
                continue;
            }
            let fields: Vec<&str> = line.split(' ').collect();
            let [path, target] = fields[..] else {
                return Err(format!(
                    "{IMAGE_PATHS_FILE}:{number}: expected `PATH TARGET`, got {line:?}"
                ));
            };
            if !absolute(path) || !absolute(target) {
                return Err(format!(
                    "{IMAGE_PATHS_FILE}:{number}: expected two absolute paths, got {line:?}"
                ));
            }
            if targets.insert(path.to_owned(), target.to_owned()).is_some() {
                return Err(format!(
                    "{IMAGE_PATHS_FILE}:{number}: duplicate path {path}"
                ));
            }
        }
        let image = image.ok_or_else(|| format!("{IMAGE_PATHS_FILE}: no `image:` line"))?;
        if targets.is_empty() {
            return Err(format!("{IMAGE_PATHS_FILE}: lists no paths"));
        }
        if files.is_empty() {
            return Err(format!(
                "{IMAGE_PATHS_FILE}: records no `file:` bytes, so it cannot tell the image's \
                 libc from another"
            ));
        }
        if let Some(path) = absent
            .iter()
            .find(|path| targets.contains_key(*path) || files.contains_key(*path))
        {
            return Err(format!(
                "{IMAGE_PATHS_FILE}: {path} is listed both as present and as absent"
            ));
        }
        let in_a_tree = |target: &str| {
            trees.keys().any(|directory: &String| {
                target
                    .strip_prefix(directory.as_str())
                    .is_some_and(|rest| rest.starts_with('/'))
            })
        };
        if let Some(target) = targets
            .values()
            .chain(files.values().map(|file: &RecordedFile| &file.target))
            .find(|target| !in_a_tree(target))
        {
            return Err(format!(
                "{IMAGE_PATHS_FILE}: the target {target} is in no `tree:` directory, so nothing \
                 records its bytes; regenerate the listing (the command is in its header)"
            ));
        }
        let mut directories = BTreeSet::new();
        for path in targets.keys().chain(files.keys()) {
            let mut directory = Path::new(path.as_str());
            while let Some(parent) = directory.parent() {
                directories.insert(parent.display().to_string());
                directory = parent;
            }
        }
        Ok(Self {
            image,
            targets,
            files,
            trees,
            directories,
            absent,
        })
    }

    /// Load the listing and require it to describe the pinned image.
    pub fn load(repo_root: &Path) -> Result<Self, String> {
        let read = |relative: &str| {
            std::fs::read_to_string(repo_root.join(relative))
                .map_err(|error| format!("cannot read {relative}: {error}"))
        };
        let listing = Self::parse(&read(IMAGE_PATHS_FILE)?)?;
        let digest = read(IMAGE_DIGEST_FILE)?;
        listing.require_image(digest.trim())?;
        Ok(listing)
    }

    pub fn require_image(&self, pinned: &str) -> Result<(), String> {
        if self.image == pinned {
            return Ok(());
        }
        Err(format!(
            "{IMAGE_PATHS_FILE} lists the image {}, but {IMAGE_DIGEST_FILE} pins {pinned}; \
             regenerate the listing from the pinned image (the command is in its header)",
            self.image
        ))
    }

    /// The SHA-256, as `sha256:HEX`, of everything this listing records: the
    /// image reference, every tree's content digest, every recorded file's
    /// target and bytes, every path's target and every absent path, in sorted
    /// order. A result row that ran in the pinned root carries it as
    /// `guest_root_identity`.
    pub fn identity(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(format!("image {}\n", self.image));
        for (directory, sha256) in &self.trees {
            hasher.update(format!("tree {directory} {sha256}\n"));
        }
        for (path, file) in &self.files {
            hasher.update(format!("file {path} {} {}\n", file.target, file.sha256));
        }
        for (path, target) in &self.targets {
            hasher.update(format!("{path} {target}\n"));
        }
        for path in &self.absent {
            hasher.update(format!("absent {path}\n"));
        }
        format!("sha256:{:x}", hasher.finalize())
    }

    /// Whether the root this process runs in is the image this listing
    /// records. `Err` names the first difference. See
    /// [`Self::verify_root_with_mounts`]; this reads the mount table from
    /// `/proc/self/mountinfo`.
    pub fn verify_root(&self) -> Result<(), String> {
        let mountinfo = std::fs::read_to_string("/proc/self/mountinfo")
            .map_err(|error| format!("cannot read /proc/self/mountinfo: {error}"))?;
        self.verify_root_with_mounts(&mountinfo)
    }

    /// Whether the root this process runs in, with the mount table
    /// `mountinfo` (the text of `/proc/self/mountinfo`), is the image this
    /// listing records:
    ///
    /// * every recorded path resolves through every link to its recorded
    ///   target, and every recorded file's target holds its recorded bytes.
    ///   Another image, even one with a `/nix/store` shell, resolves its
    ///   programs to other store paths, and a root that reuses the store
    ///   paths with other bytes has another libc;
    /// * every directory that holds a recorded path holds no other
    ///   executable (the listing's own rule: dot names, directories and
    ///   dangling links are not listed);
    /// * no `absent:` path exists, so a host-only program a workload arm
    ///   prefers ([`HOST_ONLY_PROBES`]) cannot have been supplied, no
    ///   directory the image lacks on the guest `PATH`
    ///   ([`GUEST_PATH_DIRECTORIES`]) can put a program ahead of the
    ///   image's, and no loader preload file ([`LOADER_PRELOAD_FILES`]) can
    ///   load a library from outside the store. With the previous rule, a
    ///   bare program name finds only the image's programs;
    /// * nothing is mounted at or below a directory that holds a recorded
    ///   path or target, and all those directories are on one filesystem, so
    ///   no mount at, below or above them replaces the image's files;
    /// * every `tree:` directory has its recorded [`tree_digest`], so each
    ///   program, library and script under it holds the image's bytes, not
    ///   bytes written over the image's file at the same path. It is checked
    ///   last because it reads the whole store (2,998,818,465 bytes in 99,818
    ///   files and 72,136 links for the pinned image).
    pub fn verify_root_with_mounts(&self, mountinfo: &str) -> Result<(), String> {
        let resolves = |path: &str, target: &str| -> Result<(), String> {
            match std::fs::canonicalize(path) {
                Ok(found) if found == Path::new(target) => Ok(()),
                Ok(found) => Err(format!(
                    "{path} resolves to {}, not to the pinned image's {target}",
                    found.display()
                )),
                Err(error) => Err(format!(
                    "{path} does not resolve ({error}); in the pinned image it resolves to {target}"
                )),
            }
        };
        for (path, file) in &self.files {
            resolves(path, &file.target)?;
            let bytes = std::fs::read(PathBuf::from(&file.target))
                .map_err(|error| format!("cannot read {}: {error}", file.target))?;
            let found = format!("{:x}", Sha256::digest(&bytes));
            if found != file.sha256 {
                return Err(format!(
                    "{} has sha256 {found}, not the pinned image's {}",
                    file.target, file.sha256
                ));
            }
        }
        for (path, target) in &self.targets {
            resolves(path, target)?;
        }
        self.verify_listed_directories_are_complete()?;
        for path in &self.absent {
            match std::fs::symlink_metadata(path) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Ok(_) => {
                    return Err(format!(
                        "{path} exists, but the pinned image has no such path; something \
                         outside the image supplied it"
                    ));
                }
                Err(error) => {
                    return Err(format!(
                        "cannot tell whether {path}, absent from the pinned image, exists: {error}"
                    ));
                }
            }
        }
        self.verify_no_mount_replaces_the_image(mountinfo)?;
        for (directory, recorded) in &self.trees {
            let found = tree_digest(Path::new(directory))?;
            if &found != recorded {
                return Err(format!(
                    "the tree {directory} has content digest {found}, not the pinned image's \
                     {recorded}: a file, link or executable bit under it differs from the image"
                ));
            }
        }
        Ok(())
    }

    /// Every executable in a directory that holds a recorded path must be a
    /// recorded path: the listing's generator lists `"$d"/*` entries that
    /// pass `[ -x ]` and `[ ! -d ]`, so dot names, directories and dangling
    /// links are skipped here as there.
    fn verify_listed_directories_are_complete(&self) -> Result<(), String> {
        use std::os::unix::fs::PermissionsExt;
        let directories: BTreeSet<&str> = self
            .targets
            .keys()
            .filter_map(|path| path.rsplit_once('/').map(|(directory, _)| directory))
            .collect();
        for directory in directories {
            let shown = if directory.is_empty() { "/" } else { directory };
            let entries = std::fs::read_dir(shown)
                .map_err(|error| format!("cannot list {shown}: {error}"))?;
            for entry in entries {
                let entry = entry.map_err(|error| format!("cannot list {shown}: {error}"))?;
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if name.starts_with('.') {
                    continue;
                }
                let path = format!("{directory}/{name}");
                let Ok(metadata) = std::fs::metadata(&path) else {
                    continue;
                };
                if metadata.is_dir() || metadata.permissions().mode() & 0o111 == 0 {
                    continue;
                }
                if !self.targets.contains_key(&path) {
                    return Err(format!(
                        "{path} is an executable the pinned image does not have"
                    ));
                }
            }
        }
        Ok(())
    }

    /// Refuse a mount at or below a directory that holds a recorded path or
    /// target, and recorded directories on more than one filesystem (a mount
    /// above one of them, such as over `/usr` or `/nix`, puts it on another).
    fn verify_no_mount_replaces_the_image(&self, mountinfo: &str) -> Result<(), String> {
        use std::os::unix::fs::MetadataExt;
        let directories = self.image_directories()?;
        let mut first: Option<(&Path, u64)> = None;
        for directory in &directories {
            let device = std::fs::metadata(directory)
                .map_err(|error| format!("cannot stat {}: {error}", directory.display()))?
                .dev();
            match first {
                None => first = Some((directory.as_path(), device)),
                Some((other, other_device)) if other_device != device => {
                    return Err(format!(
                        "{} and {} are on different filesystems; in the pinned image every \
                         directory of a recorded path is on the image's root filesystem",
                        other.display(),
                        directory.display()
                    ));
                }
                Some(_) => {}
            }
        }
        for mount_point in mount_points(mountinfo)? {
            let mount_point = Path::new(&mount_point);
            if mount_point == Path::new("/") {
                continue;
            }
            if let Some(directory) = directories
                .iter()
                .find(|directory| mount_point.starts_with(directory))
            {
                return Err(format!(
                    "{} is mounted over {}, a directory of the pinned image",
                    mount_point.display(),
                    directory.display()
                ));
            }
        }
        Ok(())
    }

    /// The directory, resolved through every link, of every recorded path
    /// and of every recorded target.
    fn image_directories(&self) -> Result<BTreeSet<PathBuf>, String> {
        let mut directories = BTreeSet::new();
        let paths = self
            .targets
            .iter()
            .flat_map(|(path, target)| [path, target])
            .chain(
                self.files
                    .iter()
                    .flat_map(|(path, file)| [path, &file.target]),
            );
        for path in paths {
            let Some(parent) = Path::new(path).parent() else {
                continue;
            };
            let directory = std::fs::canonicalize(parent)
                .map_err(|error| format!("{} does not resolve: {error}", parent.display()))?;
            directories.insert(directory);
        }
        Ok(directories)
    }

    /// Whether the image's files stay as [`Self::verify_root`] found them for
    /// the rest of the run: every directory of a recorded path or target, and
    /// the nearest existing ancestor of every `absent:` path, must be on a
    /// filesystem mounted read-only. `verify_root` vouches for the root once,
    /// when a run starts; on a writable root a guest of one cell could then
    /// rewrite `/usr/bin/env`, a store file or a loader preload file, and a
    /// later cell would run that instead of the image. In the pinned root this
    /// holds when `ci/hermetic/run-in-pinned-root.sh` is given
    /// `--read-only-root` (podman `--read-only`). It is not part of
    /// `verify_root`, which also vouches for roots made in tests, and it does
    /// not stop a deliberate remount by a privileged guest.
    pub fn verify_read_only(&self) -> Result<(), String> {
        self.verify_read_only_with(&statvfs_read_only)
    }

    /// [`Self::verify_read_only`], with `read_only` telling whether the
    /// filesystem holding a directory is mounted read-only.
    pub fn verify_read_only_with(
        &self,
        read_only: &dyn Fn(&Path) -> Result<bool, String>,
    ) -> Result<(), String> {
        let mut directories = self.image_directories()?;
        for path in &self.absent {
            let mut ancestor = Path::new(path.as_str());
            while let Some(parent) = ancestor.parent() {
                ancestor = parent;
                if let Ok(directory) = std::fs::canonicalize(ancestor) {
                    directories.insert(directory);
                    break;
                }
            }
        }
        for directory in &directories {
            if !read_only(directory)? {
                return Err(format!(
                    "{} is on a writable filesystem, so a guest could change the pinned image's \
                     files after they were verified; run the cells with \
                     ci/hermetic/run-in-pinned-root.sh --read-only-root",
                    directory.display()
                ));
            }
        }
        Ok(())
    }

    /// Whether `path` is one of the image's executables.
    pub fn contains(&self, path: &str) -> bool {
        self.targets.contains_key(path)
    }

    /// Whether the listing records `path` as absent from the image.
    pub fn records_absent(&self, path: &str) -> bool {
        self.absent.contains(path)
    }

    /// Whether `path` is an executable or a recorded file of the image.
    fn names(&self, path: &str) -> bool {
        self.contains(path) || self.files.contains_key(path)
    }

    /// Whether `path`, with or without a trailing `/`, is a directory that
    /// holds a path the listing records.
    fn is_directory(&self, path: &str) -> bool {
        let trimmed = path.trim_end_matches('/');
        self.directories
            .contains(if trimmed.is_empty() { "/" } else { trimmed })
    }

    fn resolves_bare(&self, program: &str) -> bool {
        BARE_PROGRAM_DIRS
            .iter()
            .any(|directory| self.contains(&format!("{directory}{program}")))
    }
}

/// Whether the filesystem holding `directory` is mounted read-only
/// (`ST_RDONLY` in its `statvfs` flags).
fn statvfs_read_only(directory: &Path) -> Result<bool, String> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(directory.as_os_str().as_bytes())
        .map_err(|_| format!("{} holds a NUL byte", directory.display()))?;
    // SAFETY: `path` is a NUL-terminated string and `stat` is a writable
    // `statvfs` the call fills in.
    let mut stat = unsafe { std::mem::zeroed::<libc::statvfs>() };
    if unsafe { libc::statvfs(path.as_ptr(), &mut stat) } != 0 {
        return Err(format!(
            "cannot statvfs {}: {}",
            directory.display(),
            std::io::Error::last_os_error()
        ));
    }
    Ok(stat.f_flag & libc::ST_RDONLY != 0)
}

/// The mount points of the mount table `mountinfo` (the text of
/// `/proc/self/mountinfo`): the fifth field of each line, with the kernel's
/// octal escapes (`\040` for a space, for example) decoded.
fn mount_points(mountinfo: &str) -> Result<Vec<String>, String> {
    let mut points = Vec::new();
    for line in mountinfo.lines().filter(|line| !line.trim().is_empty()) {
        let Some(field) = line.split(' ').nth(4) else {
            return Err(format!("unreadable mountinfo line {line:?}"));
        };
        let bytes = field.as_bytes();
        let mut decoded = Vec::with_capacity(bytes.len());
        let mut index = 0;
        while index < bytes.len() {
            let escape = bytes.get(index + 1..index + 4).filter(|digits| {
                bytes[index] == b'\\' && digits.iter().all(|digit| (b'0'..=b'7').contains(digit))
            });
            if let Some(digits) = escape {
                decoded.push(
                    digits
                        .iter()
                        .fold(0u8, |value, digit| value.wrapping_mul(8) + (digit - b'0')),
                );
                index += 4;
            } else {
                decoded.push(bytes[index]);
                index += 1;
            }
        }
        points.push(String::from_utf8_lossy(&decoded).into_owned());
    }
    Ok(points)
}

/// The content digest of the tree at `directory`, as lowercase hex: the
/// SHA-256 of one line per entry below it, sorted bytewise and each ended by
/// a newline:
///
/// * `PATH<TAB>x<TAB>SHA256` for a regular file with any execute bit set, and
///   `PATH<TAB>f<TAB>SHA256` for any other regular file, where SHA256 is the
///   lowercase hex digest of its bytes;
/// * `PATH<TAB>l<TAB>TARGET` for a symbolic link, by its target text.
///
/// PATH is the entry's full path, starting with `directory`. Directories add
/// no line. An entry of any other type, or a path or link target holding a
/// newline (or a path holding a tab), is refused, because the format cannot
/// show it. The `tree:` generator in [`IMAGE_PATHS_FILE`]'s header computes
/// the same lines with `find`, `sha256sum` and `LC_ALL=C sort`.
pub fn tree_digest(directory: &Path) -> Result<String, String> {
    use std::os::unix::ffi::OsStrExt;
    let mut lines: Vec<Vec<u8>> = tree_entries(directory)?
        .into_iter()
        .map(|(path, entry)| {
            let (kind, value) = entry.kind_and_value();
            [path.as_os_str().as_bytes(), b"\t", kind, b"\t", value].concat()
        })
        .collect();
    lines.sort_unstable();
    let mut hasher = Sha256::new();
    for line in &lines {
        hasher.update(line);
        hasher.update(b"\n");
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// One entry of a tree as [`tree_digest`] and a fixture receipt record it.
#[derive(Clone, Debug, Eq, PartialEq)]
enum TreeEntry {
    /// A regular file: whether any execute bit is set, and the lowercase hex
    /// SHA-256 of its bytes.
    File { executable: bool, sha256: String },
    /// A symbolic link, by its target text.
    Link(Vec<u8>),
}

impl TreeEntry {
    /// The kind (`x`, `f` or `l`) and the value of the entry's line.
    fn kind_and_value(&self) -> (&[u8], &[u8]) {
        match self {
            Self::File {
                executable: true,
                sha256,
            } => (b"x", sha256.as_bytes()),
            Self::File {
                executable: false,
                sha256,
            } => (b"f", sha256.as_bytes()),
            Self::Link(target) => (b"l", target),
        }
    }
}

/// Every file and symbolic link below `directory`, by its full path, without
/// following links, in no particular order. Directories are walked and not
/// listed; an entry of any other type, a path holding a tab or a newline, and
/// a link target holding a newline are refused (see [`tree_digest`]).
fn tree_entries(directory: &Path) -> Result<Vec<(PathBuf, TreeEntry)>, String> {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::PermissionsExt;
    let mut entries: Vec<(PathBuf, TreeEntry)> = Vec::new();
    let mut files: Vec<(PathBuf, bool)> = Vec::new();
    let mut pending = vec![directory.to_path_buf()];
    while let Some(current) = pending.pop() {
        let listing = std::fs::read_dir(&current)
            .map_err(|error| format!("cannot list {}: {error}", current.display()))?;
        for entry in listing {
            let entry =
                entry.map_err(|error| format!("cannot list {}: {error}", current.display()))?;
            let path = entry.path();
            if path
                .as_os_str()
                .as_bytes()
                .iter()
                .any(|byte| matches!(byte, b'\t' | b'\n'))
            {
                return Err(format!(
                    "{} holds a tab or a newline, which a tree digest line cannot show",
                    path.display()
                ));
            }
            let kind = entry
                .file_type()
                .map_err(|error| format!("cannot stat {}: {error}", path.display()))?;
            if kind.is_dir() {
                pending.push(path);
            } else if kind.is_symlink() {
                let target = std::fs::read_link(&path)
                    .map_err(|error| format!("cannot read the link {}: {error}", path.display()))?;
                let target = target.as_os_str().as_bytes().to_vec();
                if target.contains(&b'\n') {
                    return Err(format!(
                        "the link {} has a newline in its target, which a tree digest line \
                         cannot show",
                        path.display()
                    ));
                }
                entries.push((path, TreeEntry::Link(target)));
            } else if kind.is_file() {
                let mode = entry
                    .metadata()
                    .map_err(|error| format!("cannot stat {}: {error}", path.display()))?
                    .permissions()
                    .mode();
                files.push((path, mode & 0o111 != 0));
            } else {
                return Err(format!(
                    "{} is not a regular file, a symbolic link or a directory",
                    path.display()
                ));
            }
        }
    }
    let digests = sha256_files(files.iter().map(|(path, _)| path.as_path()).collect())?;
    for ((path, executable), sha256) in files.into_iter().zip(digests) {
        entries.push((path, TreeEntry::File { executable, sha256 }));
    }
    Ok(entries)
}

/// The lowercase hex SHA-256 of each file in `paths`, in order, read by up to
/// 16 threads.
fn sha256_files(paths: Vec<&Path>) -> Result<Vec<String>, String> {
    use std::io::Read;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;
    let threads = std::thread::available_parallelism()
        .map_or(1, |count| count.get())
        .clamp(1, 16)
        .min(paths.len().max(1));
    let next = AtomicUsize::new(0);
    let hash_some = || -> Result<Vec<(usize, String)>, String> {
        let mut done = Vec::new();
        let mut buffer = vec![0u8; 1 << 16];
        loop {
            let index = next.fetch_add(1, Ordering::Relaxed);
            let Some(path) = paths.get(index) else {
                return Ok(done);
            };
            let mut file = std::fs::File::open(path)
                .map_err(|error| format!("cannot open {}: {error}", path.display()))?;
            let mut hasher = Sha256::new();
            loop {
                let count = file
                    .read(&mut buffer)
                    .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
                if count == 0 {
                    break;
                }
                hasher.update(&buffer[..count]);
            }
            done.push((index, format!("{:x}", hasher.finalize())));
        }
    };
    let finished = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..threads).map(|_| scope.spawn(hash_some)).collect();
        workers
            .into_iter()
            .map(|worker| {
                worker
                    .join()
                    .unwrap_or_else(|_| Err("a hashing thread panicked".to_owned()))
            })
            .collect::<Vec<_>>()
    });
    let mut digests = vec![String::new(); paths.len()];
    for worker in finished {
        for (index, digest) in worker? {
            digests[index] = digest;
        }
    }
    Ok(digests)
}

/// Where, below the run-state directory (`VALIDATE_RUN_STATE`), the
/// pinned-root `<run type>compatprep.fixtures` nodes write what
/// `tests/compat/prepare_real_compat_fixtures.sh` builds.
pub const PINNED_FIXTURES: &str = "compat-pinned/real-compat-fixtures";

/// The first line of a fixture receipt.
const FIXTURE_RECEIPT_HEADER: &str = "hermit-compat-fixture-receipt 1";

/// The repository files `tests/compat/prepare_real_compat_fixtures.sh` builds
/// the fixtures from, the script included, which its receipt records by
/// SHA-256.
const FIXTURE_RECEIPT_SOURCES: &[&str] = &[
    "README.md",
    "tests/compat/localhost_http_server.c",
    "tests/compat/lsof_mount_redirect.c",
    "tests/compat/prepare_real_compat_fixtures.sh",
];

/// Whether `text`, an argv element or the shell text of a manifest `direct`
/// command, names the pinned-root fixtures (`{{VALIDATE_RUN_STATE}}/` and
/// [`PINNED_FIXTURES`]).
pub fn names_pinned_fixtures(text: &str) -> bool {
    text.contains(&format!(
        "{VALIDATE_RUN_STATE_PLACEHOLDER}/{PINNED_FIXTURES}"
    ))
}

/// The receipt of the fixture tree at `fixtures`: the same path with
/// `.receipt` appended, beside the tree rather than in it.
pub fn fixture_receipt_path(fixtures: &Path) -> PathBuf {
    let mut path = fixtures.as_os_str().to_owned();
    path.push(".receipt");
    PathBuf::from(path)
}

/// Whether the fixture tree at `fixtures` is what
/// `tests/compat/prepare_real_compat_fixtures.sh` built inside the pinned
/// image `image` from the checkout at `repo_root`, by the receipt the script
/// writes beside the tree when it runs in the pinned root
/// ([`fixture_receipt_path`]). `Err` names the first problem:
///
/// * the receipt is missing: the script writes one only when
///   `ci/hermetic/run-in-pinned-root.sh` has set `HERMIT_PINNED_ROOT_IMAGE`,
///   and removes the old one before it rebuilds, so a tree built on the host
///   has none;
/// * its `image` is not the listing's pinned image, or its `compiler` is not
///   the `/nix/store` file a listed `gcc` resolves to;
/// * its `source` lines do not name exactly [`FIXTURE_RECEIPT_SOURCES`], each
///   with the SHA-256 of the checkout's file, so fixtures another checkout
///   built are refused;
/// * the tree does not hold exactly the receipt's entries: every file with
///   its bytes and execute bit and every link with its target, by the
///   [`tree_digest`] rules, nothing missing and nothing extra;
/// * a link points anywhere but at an absolute path the listing records or
///   a path inside a recorded tree (the `toolchain/cargo` and `rustc` links
///   name the image's `/bin/cargo` and `/bin/rustc`).
///
/// The threat is a mistake, not a forger: the script writes the receipt, and
/// nothing stops a process that can write the run state from writing another.
pub fn verify_fixture_receipt(
    image: &ImagePaths,
    repo_root: &Path,
    fixtures: &Path,
) -> Result<(), String> {
    use std::os::unix::ffi::OsStrExt;
    let receipt_path = fixture_receipt_path(fixtures);
    let receipt = receipt_path.display();
    let text = match std::fs::read_to_string(&receipt_path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(format!(
                "{} has no receipt {receipt}: tests/compat/prepare_real_compat_fixtures.sh \
                 writes one only inside the pinned root (ci/hermetic/run-in-pinned-root.sh), so \
                 these fixtures were not built there; rebuild them with the run type's \
                 compatprep.fixtures node",
                fixtures.display()
            ));
        }
        Err(error) => return Err(format!("cannot read {receipt}: {error}")),
    };
    let mut lines = text.lines();
    if lines.next() != Some(FIXTURE_RECEIPT_HEADER) {
        return Err(format!(
            "{receipt} does not start with {FIXTURE_RECEIPT_HEADER:?}"
        ));
    }
    let mut built_in = None;
    let mut compiler = None;
    let mut sources: BTreeMap<&str, &str> = BTreeMap::new();
    let mut recorded: BTreeMap<&[u8], TreeEntry> = BTreeMap::new();
    for line in lines {
        let unreadable = || format!("{receipt} has an unreadable line {line:?}");
        let twice = |what: &str| format!("{receipt} records {what} twice");
        if let Some(value) = line.strip_prefix("image ") {
            if built_in.replace(value).is_some() {
                return Err(twice("the image"));
            }
        } else if let Some(value) = line.strip_prefix("compiler ") {
            if compiler.replace(value).is_some() {
                return Err(twice("the compiler"));
            }
        } else if let Some(rest) = line.strip_prefix("source ") {
            let (sha256, path) = rest
                .split_once(' ')
                .filter(|(sha256, path)| is_lower_hex_sha256(sha256) && !path.is_empty())
                .ok_or_else(unreadable)?;
            if sources.insert(path, sha256).is_some() {
                return Err(twice(&format!("the source {path}")));
            }
        } else {
            let fields: Vec<&str> = line.split('\t').collect();
            let [kind, path, value] = fields[..] else {
                return Err(unreadable());
            };
            let entry = match kind {
                "x" | "f" if is_lower_hex_sha256(value) => TreeEntry::File {
                    executable: kind == "x",
                    sha256: value.to_owned(),
                },
                "l" if !value.is_empty() => TreeEntry::Link(value.as_bytes().to_vec()),
                _ => return Err(unreadable()),
            };
            if path.is_empty() {
                return Err(unreadable());
            }
            if recorded.insert(path.as_bytes(), entry).is_some() {
                return Err(twice(&format!("the entry {path}")));
            }
        }
    }
    let built_in = built_in.ok_or_else(|| format!("{receipt} has no image line"))?;
    if built_in != image.image {
        return Err(format!(
            "{receipt} says the fixtures were built in the image {built_in}, not in the pinned \
             image {}; rebuild them",
            image.image
        ));
    }
    let compiler = compiler.ok_or_else(|| format!("{receipt} has no compiler line"))?;
    let image_gcc: BTreeSet<&str> = image_gcc_targets(image);
    if !image_gcc.contains(compiler) {
        return Err(format!(
            "{receipt} says the fixtures were compiled by {compiler}, not by the pinned image's \
             gcc ({})",
            image_gcc.into_iter().collect::<Vec<_>>().join(", ")
        ));
    }
    let named: BTreeSet<&str> = sources.keys().copied().collect();
    let expected: BTreeSet<&str> = FIXTURE_RECEIPT_SOURCES.iter().copied().collect();
    if named != expected {
        return Err(format!(
            "{receipt} records the sources {named:?}, not {expected:?}"
        ));
    }
    for (path, built_from) in &sources {
        let bytes = std::fs::read(repo_root.join(path))
            .map_err(|error| format!("cannot read {path}: {error}"))?;
        let checkout = format!("{:x}", Sha256::digest(&bytes));
        if checkout != *built_from {
            return Err(format!(
                "the fixtures in {} were built from a {path} with sha256 {built_from}, but this \
                 checkout's has sha256 {checkout}; rebuild them from this checkout",
                fixtures.display()
            ));
        }
    }
    let mut found: BTreeMap<Vec<u8>, TreeEntry> = BTreeMap::new();
    for (path, entry) in tree_entries(fixtures)? {
        let relative = path
            .strip_prefix(fixtures)
            .map_err(|_| format!("{} is not below {}", path.display(), fixtures.display()))?;
        found.insert(relative.as_os_str().as_bytes().to_vec(), entry);
    }
    let show = |path: &[u8]| String::from_utf8_lossy(path).into_owned();
    let describe = |entry: &TreeEntry| {
        let (kind, value) = entry.kind_and_value();
        format!("{} {}", show(kind), show(value))
    };
    let mut differences = Vec::new();
    for (path, entry) in &recorded {
        match found.get(*path) {
            None => differences.push(format!("{} is missing", show(path))),
            Some(other) if other != entry => differences.push(format!(
                "{} is {}, not the receipt's {}",
                show(path),
                describe(other),
                describe(entry)
            )),
            Some(_) => {}
        }
    }
    for path in found.keys() {
        if !recorded.contains_key(path.as_slice()) {
            differences.push(format!("{} is not in the receipt", show(path)));
        }
    }
    if !differences.is_empty() {
        return Err(format!(
            "the fixtures in {} differ from their receipt {receipt}: {}",
            fixtures.display(),
            differences.join("; ")
        ));
    }
    for (path, entry) in &recorded {
        let TreeEntry::Link(target) = entry else {
            continue;
        };
        let target = show(target);
        let in_image = target.starts_with('/')
            && !has_parent_component(&target)
            && (image.names(&target)
                || image
                    .trees
                    .keys()
                    .any(|tree| Path::new(&target).starts_with(tree)));
        if !in_image {
            return Err(format!(
                "the fixture link {} points at {target}, which is not a path the pinned image \
                 records",
                show(path)
            ));
        }
    }
    Ok(())
}

/// The `/nix/store` files the listing's `gcc` paths resolve to.
fn image_gcc_targets(image: &ImagePaths) -> BTreeSet<&str> {
    image
        .targets
        .iter()
        .filter(|(path, _)| base_name(path) == "gcc")
        .map(|(_, target)| target.as_str())
        .collect()
}

/// Whether `bucket` belongs to the compat family this contract covers.
pub fn is_compat_bucket(bucket: &str) -> bool {
    bucket == "compat" || bucket.starts_with("compat-")
}

/// Every contract violation in the expanded manifest `documents`.
///
/// `read_script` returns the text of a repository-relative file that a gating
/// argv names through `{{ROOT_DIR}}/`.
pub fn violations(
    documents: &[Value],
    image: &ImagePaths,
    read_script: &dyn Fn(&str) -> Result<String, String>,
) -> Vec<String> {
    violations_with(
        documents,
        image,
        read_script,
        DATA_PATHS,
        RUN_STATE_OPERANDS,
    )
}

fn violations_with(
    documents: &[Value],
    image: &ImagePaths,
    read_script: &dyn Fn(&str) -> Result<String, String>,
    data_paths: &[DataPath],
    run_state_operands: &[RunStateOperand],
) -> Vec<String> {
    let mut errors = Vec::new();
    for document in documents {
        let Some(bucket) = document.get("bucket").and_then(Value::as_str) else {
            continue;
        };
        if !is_compat_bucket(bucket) {
            continue;
        }
        let pinned = document.get("guest_root").and_then(Value::as_str) == Some("pinned");
        for test in document
            .get("test")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let id = test.get("id").and_then(Value::as_str).unwrap_or("<no id>");
            let gating = gating_cells(test);
            if gating.is_empty() {
                continue;
            }
            let cells = gating.join(", ");
            if !pinned {
                errors.push(format!(
                    "{id}: gates {cells} in bucket `{bucket}`, whose manifest does not declare \
                     `guest_root: pinned`, so the program, its libc and its loader come from the \
                     validation host; a gating compat cell must run from the pinned image \
                     (https://github.com/rrnewton/hermit/issues/3712)"
                ));
            }
            let context = Context {
                image,
                read_script,
                data_paths,
                run_state_operands,
            };
            for problem in argv_problems(test, &context)
                .into_iter()
                .chain(recipe_problems(test, &gating, &context))
            {
                errors.push(format!("{id}: gates {cells}, but {problem}"));
            }
        }
    }
    errors
}

/// What the value of an [`AdmittedEnv`] variable may be.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EnvValue {
    /// Text that names no path: it has no `/`.
    Text,
    /// An absolute path, whose provenance the path check decides as it does
    /// for a path in the argv.
    Path,
    /// A path under [`PINNED_FIXTURE_ROOT`], the tree only pinned-root
    /// processes write.
    PinnedFixture,
    /// Exactly [`PINNED_FIXTURE_TREE`], the receipt-verified fixture tree,
    /// and only in the `env` that starts the argv: test-harness verifies the
    /// receipt when a cell's direct command names the tree, and it does not
    /// read a recipe's `env`.
    FixtureTree,
}

/// A guest variable a gating cell may set.
#[derive(Clone, Copy, Debug)]
struct AdmittedEnv {
    name: &'static str,
    value: EnvValue,
    /// What the variable does, and why that selects no program, library or
    /// startup file outside the pinned root.
    reason: &'static str,
}

/// The only guest variables a gating cell may set, in the `env` that starts
/// its argv or in a recipe's `env`; every other name is refused whatever its
/// value. Names are admitted for what they are known to do, not refused for
/// what they are known to do: `PATH` decides where bare names resolve, the
/// loader reads every `LD_*` name, a shell sources the file `BASH_ENV` or
/// `ENV` names before its script, and other programs read their own, so a
/// list of dangerous names is never complete. `HOME` and `XDG_CONFIG_HOME`
/// choose where programs read startup files, so each may only name the
/// pinned-root fixture tree.
const ADMITTED_ENV: &[AdmittedEnv] = &[
    AdmittedEnv {
        name: "TMPDIR",
        value: EnvValue::Path,
        reason: "where programs create scratch files; the corpus recipe sets /tmp",
    },
    AdmittedEnv {
        name: "REAL_COMPAT_FIXTURES",
        value: EnvValue::FixtureTree,
        reason: "the fixtures tests/compat/real_compat_workload.sh reads and runs",
    },
    AdmittedEnv {
        name: "HERMIT_COMPAT",
        value: EnvValue::Text,
        reason: "the text the printenv row prints",
    },
    AdmittedEnv {
        name: "HOME",
        value: EnvValue::PinnedFixture,
        reason: "programs read startup files under it (top reads its toprc)",
    },
    AdmittedEnv {
        name: "XDG_CONFIG_HOME",
        value: EnvValue::PinnedFixture,
        reason: "programs read configuration files under it",
    },
];

/// Why a gating cell may not set guest variable `name` to `value`, if it may
/// not. `source` says where the assignment is, and `in_argv` whether it is in
/// the `env` that starts the argv rather than in a recipe's `env`. A `..`
/// component and the provenance of an absolute path are checked by the
/// caller, with the argv's other paths or the recipe's other values.
fn env_problems(name: &str, value: &str, source: &str, in_argv: bool) -> Vec<String> {
    let Some(admitted) = ADMITTED_ENV.iter().find(|admitted| admitted.name == name) else {
        let names: Vec<&str> = ADMITTED_ENV.iter().map(|admitted| admitted.name).collect();
        return vec![format!(
            "{source} sets `{name}`, which this contract does not admit; a gating cell may set \
             only {}, variables that select no program, library or startup file outside the \
             pinned root (a shell sources the file BASH_ENV names, the loader reads LD_*, and \
             PATH decides where bare names resolve, against the guest PATH {})",
            names.join(", "),
            GUEST_PATH_DIRECTORIES.join(":")
        )];
    };
    let problem = match admitted.value {
        EnvValue::Text if value.contains('/') => "names a path, but it is admitted only as text",
        EnvValue::Path if !value.starts_with('/') => {
            "is not an absolute path, so it would resolve against the guest's working directory"
        }
        EnvValue::PinnedFixture if !value.starts_with(PINNED_FIXTURE_ROOT) => {
            "is outside the pinned-root fixture tree, the only place it may name"
        }
        EnvValue::FixtureTree if value != PINNED_FIXTURE_TREE => {
            "is not the receipt-verified fixture tree, the only value it may have; programs run \
             from below it, and test-harness verifies no other tree's receipt"
        }
        EnvValue::FixtureTree if !in_argv => {
            "is set in a recipe; test-harness verifies the fixture receipt for a tree the direct \
             command names and does not read a recipe's `env`, so set it in the argv's `env`"
        }
        _ => return Vec::new(),
    };
    vec![format!(
        "{source} sets `{name}` to {value:?}, which {problem} ({}; {})",
        admitted.reason,
        match admitted.value {
            EnvValue::Text => "a value with no `/`".to_owned(),
            EnvValue::Path => "an absolute path the path check admits".to_owned(),
            EnvValue::PinnedFixture => format!("a path under {PINNED_FIXTURE_ROOT}"),
            EnvValue::FixtureTree => format!("exactly {PINNED_FIXTURE_TREE}, in the argv's `env`"),
        }
    )]
}

/// Every way the mode recipes behind `test`'s gating `cells` could change
/// what its argv runs, which [`argv_problems`] alone does not see.
///
/// * `env` adds guest variables (`--env NAME=VALUE`; `cell_mode_env` in
///   `runner.rs`). It may set only an [`ADMITTED_ENV`] name, a value may
///   have no `..` component, and every absolute path in a value is checked
///   as one in the argv is.
/// * `guest_args` extend the argv per backend, `workdir` chooses the guest's
///   working directory, and a custom mode's `args` are Hermit flags. This
///   contract checks none of them, so a gating cell may not declare any and
///   states its whole command in `direct`.
///
/// A replay cell runs with its verify recipe's `env`, `guest_args` and
/// `workdir` unless it declares its own (`inherited_recipes` in
/// `runner.rs`), so a gating replay cell's verify recipe is checked too.
/// `hermit_args` need no check here: manifest validation admits only
/// `ALLOWED_HERMIT_ARGS` (`runner.rs`), none of which names a path.
fn recipe_problems(test: &Value, cells: &[String], context: &Context) -> Vec<String> {
    let Some(modes) = test.get("modes").and_then(Value::as_table) else {
        return Vec::new();
    };
    let mut checked = BTreeSet::new();
    for cell in cells {
        let mode = cell.split_once('/').map_or(cell.as_str(), |(mode, _)| mode);
        checked.insert(mode);
        if mode == "replay" {
            checked.insert("verify");
        }
    }
    let consumer = direct_argv(test)
        .and_then(|argv| env_prefix(&argv).map(|prefix| argv[prefix.program_index].clone()))
        .unwrap_or_default();
    let mut problems = Vec::new();
    for mode in checked {
        let Some(recipe) = modes.get(mode) else {
            continue;
        };
        match recipe.get("env") {
            None | Some(Value::Null) => {}
            Some(Value::Table(env)) => {
                for (name, value) in env {
                    let source = format!("its {mode} `env` {name}");
                    let Some(value) = value.as_str() else {
                        problems.push(format!("{source} is not a string"));
                        continue;
                    };
                    problems.extend(env_problems(
                        name,
                        value,
                        &format!("its {mode} `env`"),
                        false,
                    ));
                    if has_parent_component(value) {
                        problems.push(format!(
                            "{source} is {value:?}, which has a `..` path component, so no \
                             prefix it starts with shows where it resolves"
                        ));
                    }
                    problems.extend(path_problems(value, &consumer, &source, context));
                }
            }
            Some(_) => problems.push(format!("its {mode} `env` is not a mapping")),
        }
        for field in ["guest_args", "workdir", "args"] {
            let declared = match recipe.get(field) {
                None | Some(Value::Null) => false,
                Some(Value::Array(items)) => !items.is_empty(),
                Some(Value::Table(table)) => table.values().any(|value| match value {
                    Value::Array(items) => !items.is_empty(),
                    Value::Null => false,
                    _ => true,
                }),
                Some(_) => true,
            };
            if declared {
                problems.push(format!(
                    "its {mode} recipe declares `{field}`, which this contract does not check; \
                     a gating compat cell states its whole command in `direct`"
                ));
            }
        }
    }
    problems
}

/// The argv `test`'s `direct` command runs, or `None` when it is not one.
fn direct_argv(test: &Value) -> Option<Vec<String>> {
    match test.get("direct") {
        Some(Value::Array(items)) => Some(
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect(),
        ),
        Some(Value::String(command)) => {
            Some(command.split_whitespace().map(str::to_owned).collect())
        }
        _ => None,
    }
}

/// What [`argv_problems`] checks an argv against.
struct Context<'a> {
    image: &'a ImagePaths,
    read_script: &'a dyn Fn(&str) -> Result<String, String>,
    data_paths: &'a [DataPath],
    run_state_operands: &'a [RunStateOperand],
}

/// The `mode/backend` cells of `test` that gate.
fn gating_cells(test: &Value) -> Vec<String> {
    let mut cells = Vec::new();
    let Some(modes) = test.get("modes").and_then(Value::as_table) else {
        return cells;
    };
    for (mode, spec) in modes {
        let enabled: Vec<&str> = spec
            .get("backends_enabled")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect();
        let selection = spec
            .get("ci")
            .and_then(|value| serde_json::to_value(value).ok())
            .and_then(|value| serde_json::from_value::<CiSelectionSpec>(value).ok())
            .unwrap_or_default();
        let diagnostic = spec.get("diagnostic").and_then(Value::as_table);
        for backend in enabled {
            let selected = match &selection {
                CiSelectionSpec::Uniform(selected) => *selected,
                CiSelectionSpec::PerBackend(map) => map.get(backend).copied().unwrap_or(false),
            };
            let reported_only = diagnostic.is_some_and(|table| table.contains_key(backend));
            if selected && !reported_only {
                cells.push(format!("{mode}/{backend}"));
            }
        }
    }
    cells
}

/// Every way the argv of `test` could reach outside the pinned image.
///
/// * The program it runs must be an executable of the image (by path, or by a
///   bare name the image has in [`BARE_PROGRAM_DIRS`]), a file under
///   [`PINNED_FIXTURE_TREE`], or a repository shell script whose `#!` line
///   runs a shell the image has, with no other word ([`shebang_problems`]). A built program from the checkout, a
///   relative path, or any absolute path the image lacks is refused, whatever
///   its directory.
/// * A program file an interpreter runs must be a repository shell script run
///   by a shell (this check cannot read other languages), an executable of
///   the image, or a file under [`PINNED_FIXTURE_TREE`].
/// * A repository shell script it runs, and the workload code it selects,
///   has no `..` path component on any line ([`script_problems`]).
/// * Every absolute path in the argv, in a repository shell script it runs
///   and in the workload arm it selects must be in the image, a directory of
///   the image, a runtime path ([`RUNTIME_ROOTS`], [`RUNTIME_FILES`]) or a
///   declared [`DataPath`]. A runtime path that reaches another file tree
///   (`/proc/self/root/src/target/x` names the host-mounted `/src/target`;
///   [`aliases_another_tree`]) is not one.
/// * An `env` that starts the argv uses only the options [`env_prefix`]
///   admits, assigns only [`ADMITTED_ENV`] names, and does not run `env`
///   again.
/// * No argv element has a `..` path component, so a prefix the rules below
///   accept (`{{ROOT_DIR}}/`, [`PINNED_FIXTURE_ROOT`], an image directory)
///   cannot be walked out of.
/// * `{{ROOT_DIR}}` may only start an element, and a non-script repository
///   file it names must be text without a `#!` line (data, not a program);
///   `{{VALIDATE_RUN_STATE}}` must be followed by `/compat-pinned/`; no other
///   placeholder is understood.
/// * Below `/compat-pinned/`, an argv names a path outside
///   [`PINNED_FIXTURE_TREE`] only as the value of an `env` variable that
///   admits one ([`EnvValue::PinnedFixture`]) or as a declared
///   [`RunStateOperand`] ([`run_state_problems`]).
///
/// Bare names inside shell text are not resolved: inside the pinned root they
/// can only find the image's executables, so a missing one fails the cell
/// instead of reaching the host.
fn argv_problems(test: &Value, context: &Context) -> Vec<String> {
    let Some(argv) = direct_argv(test) else {
        return vec!["its direct command is not an argv".to_owned()];
    };
    let Some(prefix) = env_prefix(&argv) else {
        return vec!["its argv is empty".to_owned()];
    };
    let program_index = prefix.program_index;
    let program = argv[program_index].as_str();
    let mut problems = prefix.problems;
    for (name, value) in prefix.assignments {
        problems.extend(env_problems(name, value, "its argv's `env`", true));
    }
    for element in &argv {
        problems.extend(traversal_problems(element));
        problems.extend(placeholder_problems(element));
    }
    problems.extend(program_problems(program, context));
    let script = script_operand(&argv, program_index);
    let runs_script = std::iter::once(program_index)
        .chain(script)
        .find_map(|index| {
            argv[index]
                .strip_prefix(ROOT_DIR_PLACEHOLDER)
                .filter(|relative| relative.ends_with(".sh"))
        });
    for (index, element) in argv.iter().enumerate() {
        // The env's own operands are checked by `env_problems`, the program
        // and the file an interpreter runs by `program_problems` and
        // `script_operand_problems`, and a `..` component by
        // `traversal_problems`.
        if index <= program_index || Some(index) == script || has_parent_component(element) {
            continue;
        }
        problems.extend(run_state_problems(element, runs_script, context));
    }
    if let Some(index) = script {
        problems.extend(script_operand_problems(program, &argv[index], context));
    }
    for (index, element) in argv.iter().enumerate() {
        if element.starts_with(ROOT_DIR_PLACEHOLDER) && has_parent_component(element) {
            // `traversal_problems` refused it; reading the file it names
            // would read outside the checkout.
            continue;
        }
        if index == program_index {
            // `program_problems` checked the program itself; a repository
            // script it runs still has its text checked.
            if let Some(relative) = element
                .strip_prefix(ROOT_DIR_PLACEHOLDER)
                .filter(|relative| relative.ends_with(".sh"))
            {
                problems.extend(script_problems(relative, argv.get(index + 1), context));
            }
            continue;
        }
        let Some(relative) = element.strip_prefix(ROOT_DIR_PLACEHOLDER) else {
            problems.extend(path_problems(element, program, "its argv", context));
            continue;
        };
        if relative.ends_with(".sh") {
            problems.extend(script_problems(relative, argv.get(index + 1), context));
        } else if Some(index) != script {
            problems.extend(repository_data_problems(relative, context));
        }
    }
    problems
}

/// What the `env` that starts an argv does before its program runs.
struct EnvPrefix<'a> {
    /// The index of the program the argv runs: 0 when the argv does not start
    /// with `env`, or names no program after it (then `env` itself runs).
    program_index: usize,
    /// The `NAME=VALUE` operands `env` assigns, in order.
    assignments: Vec<(&'a str, &'a str)>,
    /// What this check cannot read in the prefix.
    problems: Vec<String>,
}

/// The `env` options a gating argv may use. Each one only removes variables;
/// none names a file, changes the working directory or splits an operand
/// into more words.
const ADMITTED_ENV_OPTIONS: &[&str] = &[
    "-i",
    "--ignore-environment",
    "-u NAME",
    "--unset NAME",
    "--unset=NAME",
    "--",
];

/// Read the `env` that starts `argv`, the way GNU env parses it: options
/// come first and end at `--` or at the first operand; then each operand
/// with a `=` is an assignment, and the first without one is the program.
///
/// It fails closed. An option outside [`ADMITTED_ENV_OPTIONS`] is a problem
/// (`-S`/`--split-string` turns one operand into assignments and a program
/// that no argv element shows; `-C` moves the working directory relative
/// names resolve against), and so is a program that is `env` again, whose
/// own operands this walk would not read. `None` for an empty argv.
fn env_prefix(argv: &[String]) -> Option<EnvPrefix<'_>> {
    let first = argv.first()?.as_str();
    let mut prefix = EnvPrefix {
        program_index: 0,
        assignments: Vec::new(),
        problems: Vec::new(),
    };
    // `/usr/bin/env` itself is checked as a program; the one after it is
    // still resolved inside the image.
    if base_name(first) != "env" {
        return Some(prefix);
    }
    let mut index = 1;
    let mut options = true;
    while let Some(element) = argv.get(index) {
        let element = element.as_str();
        if options {
            match element {
                "-i" | "--ignore-environment" => {
                    index += 1;
                    continue;
                }
                // The name to unset; a missing one makes env fail.
                "-u" | "--unset" => {
                    index += 2;
                    continue;
                }
                "--" => {
                    options = false;
                    index += 1;
                    continue;
                }
                unset if unset.starts_with("--unset=") => {
                    index += 1;
                    continue;
                }
                option if option.starts_with('-') => {
                    prefix.problems.push(format!(
                        "its argv's `env` takes the option `{option}`, which this contract does \
                         not read; a gating argv's `env` may use only {}",
                        ADMITTED_ENV_OPTIONS.join(", ")
                    ));
                    index += 1;
                    continue;
                }
                _ => options = false,
            }
        }
        match element.split_once('=') {
            Some(assignment) => prefix.assignments.push(assignment),
            None => {
                prefix.program_index = index;
                break;
            }
        }
        index += 1;
    }
    let program = argv[prefix.program_index].as_str();
    if prefix.program_index > 0 && base_name(program) == "env" {
        prefix.problems.push(format!(
            "its argv's `env` runs `{program}`, another env, whose operands this contract does \
             not read; a gating argv starts with at most one env"
        ));
    }
    Some(prefix)
}

fn base_name(program: &str) -> &str {
    program.rsplit('/').next().unwrap_or(program)
}

/// Whether the program an argv runs, by path or by bare name, is a shell.
fn is_shell(program: &str) -> bool {
    SHELLS.contains(&base_name(program))
}

/// The index of the program file an interpreter runs: its first operand,
/// unless an option gives the program inline first.
fn script_operand(argv: &[String], program_index: usize) -> Option<usize> {
    let program = argv[program_index].as_str();
    let shell = is_shell(program);
    if !shell && !OTHER_INTERPRETERS.contains(&base_name(program)) {
        return None;
    }
    let mut index = program_index + 1;
    while let Some(element) = argv.get(index) {
        let element = element.as_str();
        if element == "--" {
            return argv.get(index + 1).map(|_| index + 1);
        }
        let flags = element
            .strip_prefix('-')
            .or_else(|| element.strip_prefix('+'));
        let Some(flags) = flags.filter(|flags| !flags.is_empty()) else {
            return Some(index);
        };
        if shell {
            // `-c` (alone or in a group such as `-ec`) gives the script
            // inline; `-o NAME` and `-O NAME` take the option's name.
            if !flags.starts_with('-') && flags.contains('c') {
                return None;
            }
            if !flags.starts_with('-') && (flags.ends_with('o') || flags.ends_with('O')) {
                index += 1;
            }
        } else if matches!(flags, "c" | "e" | "E" | "m") {
            return None;
        }
        index += 1;
    }
    None
}

fn program_problems(program: &str, context: &Context) -> Vec<String> {
    if has_parent_component(program) {
        // `traversal_problems` refused it, before any exemption below.
        return Vec::new();
    }
    if let Some(relative) = program.strip_prefix(ROOT_DIR_PLACEHOLDER) {
        if !relative.ends_with(".sh") {
            return vec![format!(
                "its argv runs the repository file {relative}, which is not a shell script this \
                 check can read; a program the checkout builds runs on the validation host's \
                 libraries, so a gating cell's program must come from the pinned image"
            )];
        }
        return match (context.read_script)(relative) {
            Ok(text) => shebang_problems(relative, &text, context.image),
            Err(error) => vec![format!("its script {relative} cannot be read: {error}")],
        };
    }
    if let Some(problems) = run_state_file_problems(program, "") {
        return problems;
    }
    if program.contains("{{") {
        return vec![placeholder_inside_problem(program)];
    }
    if program.starts_with('/') {
        if context.image.names(program) {
            return Vec::new();
        }
        return vec![format!(
            "its argv runs `{program}`, which the pinned image does not contain \
             ({IMAGE_PATHS_FILE}), so the cell would need the validation host's copy"
        )];
    }
    if program.contains('/') {
        return vec![format!(
            "its argv runs the relative path `{program}`, which resolves against the working \
             directory instead of inside the pinned image"
        )];
    }
    if context.image.resolves_bare(program) {
        return Vec::new();
    }
    vec![format!(
        "its argv runs the bare program `{program}`, which is in none of {} of the pinned image \
         ({IMAGE_PATHS_FILE})",
        BARE_PROGRAM_DIRS.join(", ")
    )]
}

/// A repository script the argv runs directly starts with a `#!` line this
/// check reads whole: `#!SHELL` or `#!ENV SHELL`, where `ENV` and an absolute
/// `SHELL` are executables of the image and a bare `SHELL` resolves in
/// [`BARE_PROGRAM_DIRS`]. Linux passes everything after the interpreter as one
/// argument, so any other word is refused rather than interpreted: `env -S`
/// splits that argument into a command no argv element shows, a shell option
/// such as `-c` runs text instead of the script, and an `env` assignment
/// changes what the program loads. The interpreter must be a shell because
/// this check reads the script as shell text ([`script_problems`]); the `#!`
/// line itself is not scanned again.
fn shebang_problems(relative: &str, text: &str, image: &ImagePaths) -> Vec<String> {
    let Some(line) = text.lines().next().and_then(|line| line.strip_prefix("#!")) else {
        return vec![format!(
            "its argv runs {relative}, which has no `#!` line naming its interpreter"
        )];
    };
    let words: Vec<&str> = line.split_whitespace().collect();
    let (interpreter, program) = match words.as_slice() {
        [interpreter] => (*interpreter, None),
        [interpreter, program] if base_name(interpreter) == "env" => (*interpreter, Some(*program)),
        [] => {
            return vec![format!(
                "its argv runs {relative}, which has no `#!` line naming its interpreter"
            )];
        }
        _ => {
            return vec![format!(
                "its script {relative} starts with `#!{line}`; this check reads only a `#!SHELL` \
                 or `#!ENV SHELL` line, with no other word, so that it sees the whole command the \
                 kernel runs (`env -S` would split a command out of one word, a shell option \
                 such as `-c` would run text instead of the script, and an `env` assignment \
                 changes what the program loads)"
            )];
        }
    };
    if !image.names(interpreter) {
        return vec![format!(
            "its script {relative} starts with `#!{line}`, whose interpreter `{interpreter}` the \
             pinned image does not contain ({IMAGE_PATHS_FILE})"
        )];
    }
    let shell = match program {
        None => interpreter,
        Some(program) if program.starts_with('-') || program.contains('=') => {
            return vec![format!(
                "its script {relative} starts with `#!{line}`, whose `{program}` is an env option \
                 or assignment rather than the program env runs"
            )];
        }
        Some(program) if program.starts_with('/') && !image.names(program) => {
            return vec![format!(
                "its script {relative} starts with `#!{line}`, which runs `{program}`, a path the \
                 pinned image does not contain ({IMAGE_PATHS_FILE})"
            )];
        }
        Some(program) if !program.starts_with('/') && program.contains('/') => {
            return vec![format!(
                "its script {relative} starts with `#!{line}`, which runs the relative path \
                 `{program}`, resolved against the working directory instead of inside the \
                 pinned image"
            )];
        }
        Some(program) if !program.contains('/') && !image.resolves_bare(program) => {
            return vec![format!(
                "its script {relative} starts with `#!{line}`, which runs the bare program \
                 `{program}`, in none of {} of the pinned image ({IMAGE_PATHS_FILE})",
                BARE_PROGRAM_DIRS.join(", ")
            )];
        }
        Some(program) => program,
    };
    if !is_shell(shell) {
        return vec![format!(
            "its script {relative} starts with `#!{line}`, which runs it with `{shell}`, not a \
             shell ({}); this check reads a script it runs as shell text",
            SHELLS.join(", ")
        )];
    }
    Vec::new()
}

/// What is wrong with running `path`, a program or the program file an
/// interpreter runs (`through` names the interpreter), when it starts with a
/// placeholder other than `{{ROOT_DIR}}`; `None` when it does not. A file
/// below [`PINNED_FIXTURE_TREE`] may run: test-harness verifies that tree's
/// receipt before any pinned cell starts. Any other file below
/// [`PINNED_FIXTURE_ROOT`] may not, because nothing vouches for its bytes:
/// the run-state directory is the validation host's, mounted into the pinned
/// root. Any other placeholder is reported by [`placeholder_problems`].
fn run_state_file_problems(path: &str, through: &str) -> Option<Vec<String>> {
    let in_tree = path
        .strip_prefix(PINNED_FIXTURE_TREE)
        .and_then(|rest| rest.strip_prefix('/'))
        .is_some_and(|rest| !rest.is_empty());
    if in_tree {
        return Some(Vec::new());
    }
    if path.starts_with(PINNED_FIXTURE_ROOT) {
        return Some(vec![format!(
            "its argv runs `{path}`{through}, a file under {PINNED_FIXTURE_ROOT} outside \
             {PINNED_FIXTURE_TREE}/, the only run-state tree whose receipt test-harness verifies; \
             nothing vouches for other bytes there, which the validation host can write"
        )]);
    }
    path.starts_with("{{").then(Vec::new)
}

/// Every path under [`PINNED_FIXTURE_ROOT`] that argv element `element`
/// names outside [`PINNED_FIXTURE_TREE`] and that is not a declared
/// [`RunStateOperand`] of `script`, the repository script the argv runs.
/// Wherever such a path appears it may run: a shell's `-c` text runs it, so
/// do the positional parameters that text expands, and so does an operand of
/// a program such as `timeout`. Only the tree whose receipt test-harness
/// verifies is admitted everywhere. Run state outside [`PINNED_FIXTURE_ROOT`]
/// is reported by [`placeholder_problems`].
fn run_state_problems(element: &str, script: Option<&str>, context: &Context) -> Vec<String> {
    let mut problems = Vec::new();
    let mut offset = 0;
    while let Some(start) = element[offset..]
        .find(VALIDATE_RUN_STATE_PLACEHOLDER)
        .map(|start| offset + start)
    {
        offset = start + VALIDATE_RUN_STATE_PLACEHOLDER.len();
        let rest = &element[start..];
        if !rest.starts_with(PINNED_FIXTURE_ROOT) {
            continue;
        }
        let in_tree = rest.strip_prefix(PINNED_FIXTURE_TREE).is_some_and(|after| {
            after
                .bytes()
                .next()
                .is_none_or(|byte| byte == b'/' || !is_path_byte(byte))
        });
        let declared = start == 0
            && context.run_state_operands.iter().any(|operand| {
                Some(operand.consumer) == script
                    && element
                        .strip_prefix(operand.path)
                        .is_some_and(|after| after.is_empty() || after.starts_with('/'))
            });
        if !in_tree && !declared {
            problems.push(format!(
                "its argv element {element:?} names run state under {PINNED_FIXTURE_ROOT} outside \
                 {PINNED_FIXTURE_TREE}, the only run-state tree whose receipt test-harness \
                 verifies; nothing vouches for other bytes there, which the validation host can \
                 write, so a gating argv names such a path only as the value of an `env` \
                 variable that admits one, or as a declared operand of the repository script it \
                 runs (RUN_STATE_OPERANDS)"
            ));
        }
    }
    problems
}

/// A program or a program file whose path names a placeholder after its
/// start. `placeholder_problems` admits `{{VALIDATE_RUN_STATE}}/compat-pinned/`
/// anywhere in an element, for an `env` assignment's value; a file that runs
/// must start with it, or a prefix such as `/proc/self/root` would decide
/// where the file comes from.
fn placeholder_inside_problem(path: &str) -> String {
    format!(
        "its argv runs `{path}`, which names a placeholder after its start; a file that runs \
         from run state must start with {PINNED_FIXTURE_TREE}/"
    )
}

/// The program file an interpreter runs must come from the image, the
/// pinned-root fixture tree, or be a repository shell script a shell runs.
/// The path check alone is not enough for it: a runtime root, an image
/// directory or a [`DataPath`] admits a path nothing runs, and this file runs.
fn script_operand_problems(program: &str, operand: &str, context: &Context) -> Vec<String> {
    if has_parent_component(operand) {
        // `traversal_problems` refused it, before any exemption below.
        return Vec::new();
    }
    if let Some(relative) = operand.strip_prefix(ROOT_DIR_PLACEHOLDER) {
        if is_shell(program) && relative.ends_with(".sh") {
            return Vec::new();
        }
        return vec![format!(
            "its argv runs the repository file {relative} through `{program}`; this check reads \
             only shell scripts run by a shell, so it cannot tell which paths that file names"
        )];
    }
    if let Some(problems) = run_state_file_problems(operand, &format!(" through `{program}`")) {
        return problems;
    }
    if operand.contains("{{") {
        return vec![placeholder_inside_problem(operand)];
    }
    if operand.starts_with('/') {
        if context.image.names(operand) {
            return Vec::new();
        }
        return vec![format!(
            "its argv runs `{operand}` through `{program}`, a program file that is not an \
             executable of the pinned image ({IMAGE_PATHS_FILE}); a file an interpreter runs \
             must come from the image, from {PINNED_FIXTURE_TREE}/, or be a repository shell \
             script"
        )];
    }
    vec![format!(
        "its argv runs the relative path `{operand}` through `{program}`, which resolves against \
         the working directory instead of inside the pinned image"
    )]
}

/// Whether `text` has a `..` path component: a word, split at every
/// character other than an ASCII letter, a digit, `.`, `_` or `-`, that is
/// exactly `..`. Splitting at every other character, not only at `/` and
/// whitespace, is what finds the component in shell text such as
/// `LD_PRELOAD=../x`, `cd '..'` or `cd ..;`. A range written next to its
/// bounds, such as `(1..5)` or `{1..5}`, is not one; a quoted range such as
/// Ruby's `('a'..'z')` is, and fails closed.
fn has_parent_component(text: &str) -> bool {
    text.split(|character: char| {
        !(character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-'))
    })
    .any(|component| component == "..")
}

/// An argv element with a `..` component is refused whatever it starts with:
/// `{{VALIDATE_RUN_STATE}}/compat-pinned/../strict-compat/x` starts with the
/// pinned fixture root and names host-built run state, and
/// `{{ROOT_DIR}}/../x.sh` names a file outside the checkout. Refusing it here,
/// before any placeholder or prefix rule is applied, is what makes a prefix
/// mean containment.
fn traversal_problems(element: &str) -> Vec<String> {
    if !has_parent_component(element) {
        return Vec::new();
    }
    vec![format!(
        "its argv element {element:?} has a `..` path component, so the prefix it starts with \
         does not show where it resolves; name the path without `..`"
    )]
}

/// `{{ROOT_DIR}}` may only start an element, `{{VALIDATE_RUN_STATE}}` must be
/// followed by `/compat-pinned/`, and no other placeholder is understood.
/// [`traversal_problems`] has already refused a `..` component, so a
/// `/compat-pinned/` prefix here is containment.
fn placeholder_problems(element: &str) -> Vec<String> {
    let mut problems = Vec::new();
    let mut offset = 0;
    while let Some(start) = element[offset..].find("{{").map(|start| offset + start) {
        let Some(length) = element[start..].find("}}").map(|end| end + 2) else {
            problems.push(format!(
                "its argv element {element:?} opens a placeholder it never closes"
            ));
            break;
        };
        let placeholder = &element[start..start + length];
        let rest = &element[start..];
        if placeholder == VALIDATE_RUN_STATE_PLACEHOLDER {
            if !rest.starts_with(PINNED_FIXTURE_ROOT) {
                problems.push(format!(
                    "its argv element {element:?} names run state outside {PINNED_FIXTURE_ROOT}, \
                     the tree only pinned-root processes write; other run state is built on the \
                     validation host"
                ));
            }
        } else if placeholder == ROOT_DIR_PLACEHOLDER.trim_end_matches('/') {
            if start != 0 || !rest.starts_with(ROOT_DIR_PLACEHOLDER) {
                problems.push(format!(
                    "its argv element {element:?} names {placeholder} inside a longer word; the \
                     checkout may only start an element, so that this check can read the file it \
                     names"
                ));
            }
        } else {
            problems.push(format!(
                "its argv names the placeholder {placeholder}, which this check cannot place \
                 inside or outside the pinned image"
            ));
        }
        offset = start + length;
    }
    problems
}

/// A repository file the argv passes as an operand rather than runs must be
/// text without a `#!` line: data. A script would run paths this check never
/// reads, and a built program is not text.
fn repository_data_problems(relative: &str, context: &Context) -> Vec<String> {
    match (context.read_script)(relative) {
        Ok(text) if !text.starts_with("#!") => Vec::new(),
        Ok(_) => vec![format!(
            "its argv passes the repository script {relative} as an operand; this check reads \
             only shell scripts run by a shell, so it cannot tell which paths that script names"
        )],
        Err(error) => vec![format!(
            "its argv names {relative}, which is not a text file of the checkout ({error}); a \
             program the checkout builds runs on the validation host's libraries"
        )],
    }
}

/// The paths and `..` components of a repository shell script the argv
/// names, and for [`REAL_COMPAT_WORKLOAD`] of the parts the selected arm
/// runs. Every line is read, comment lines included
/// ([`script_traversal_problems`] says why).
fn script_problems(relative: &str, label: Option<&String>, context: &Context) -> Vec<String> {
    let text = match (context.read_script)(relative) {
        Ok(text) => text,
        Err(error) => return vec![format!("its script {relative} cannot be read: {error}")],
    };
    if relative != REAL_COMPAT_WORKLOAD {
        let source = format!("its script {relative}");
        let mut problems = script_traversal_problems(&text, &source);
        problems.extend(path_problems(&text, relative, &source, context));
        return problems;
    }
    let Some(label) = label else {
        return vec![format!("its argv runs {relative} without a workload label")];
    };
    let Some(arm) = workload_arm(&text, label) else {
        return vec![format!(
            "{relative} has no `{label})` arm in its `{WORKLOAD_CASE_LINE}` block"
        )];
    };
    let arm_source = format!("its workload arm `{label}` in {relative}");
    let shared = workload_shared_code(&text, &arm);
    let shared_source = format!("the code of {relative} that its workload arm `{label}` runs");
    let mut problems = script_traversal_problems(&arm, &arm_source);
    problems.extend(script_traversal_problems(&shared, &shared_source));
    let arm = match strip_host_only_probes(label, &arm, context.image) {
        Ok(arm) => arm,
        Err(error) => {
            problems.push(error);
            return problems;
        }
    };
    problems.extend(path_problems(&arm, label, &arm_source, context));
    problems.extend(path_problems(&shared, relative, &shared_source, context));
    problems
}

/// Every line of shell text `text` with a `..` path component
/// ([`has_parent_component`]). A script runs from a working directory such as
/// the pinned root's `/test`, so `LD_PRELOAD=../src/target/host.so` or
/// `cd ..; src/target/host` reaches the host-mounted checkout through no
/// absolute path the path check could see. Every line counts, comment lines
/// too: a line that is a shell comment can be a here-document's text, such
/// as a C `#include`, which the program reading it acts on.
fn script_traversal_problems(text: &str, source: &str) -> Vec<String> {
    text.lines()
        .filter(|line| has_parent_component(line))
        .map(|line| {
            format!(
                "{source} has the line {:?}, with a `..` path component, so no prefix it starts \
                 with shows where it resolves; name the path without `..`",
                line.trim()
            )
        })
        .collect()
}

/// Every absolute path in `text` that is neither in the image, a directory of
/// the image, a runtime path nor a [`DataPath`] of `consumer`. A path with a
/// `..` component is refused whatever its prefix.
fn path_problems(text: &str, consumer: &str, source: &str, context: &Context) -> Vec<String> {
    let mut seen = BTreeSet::new();
    absolute_paths(text)
        .into_iter()
        .filter(|path| seen.insert(*path))
        .filter(|path| !permitted(path, consumer, context))
        .map(|path| {
            if aliases_another_tree(path) {
                return format!(
                    "{source} names `{path}`, which reaches another file tree through a process's \
                     root, working directory, mapped files or an open descriptor; through it the \
                     run's mounts of the validation host are visible, so it is not a path every \
                     container provides"
                );
            }
            format!(
                "{source} names `{path}`, which is neither in the pinned image \
                 ({IMAGE_PATHS_FILE}) nor a path every container provides ({}, {}), so the \
                 cell would need the validation host's copy",
                RUNTIME_ROOTS.join(", "),
                RUNTIME_FILES.join(", ")
            )
        })
        .collect()
}

/// The `/proc/<pid>` entries that are another file tree rather than a kernel
/// interface: a process's root directory, its working directory and its
/// mapped files.
const PROC_ALIAS_COMPONENTS: &[&str] = &["root", "cwd", "map_files"];

/// The `/dev` links to a process's open descriptors (`/dev/fd` and
/// `/dev/stdin` link into `/proc/self/fd`).
const DEV_DESCRIPTOR_LINKS: &[&str] = &["fd", "stdin", "stdout", "stderr"];

/// Whether `path`, under a runtime root, reaches a file tree that root does
/// not provide: `/proc/self/root/src/target/x` is the host-mounted
/// `/src/target/x`, and so is the same path through `cwd`, `map_files` or
/// any process's or thread's entry. A path below an open descriptor
/// (`/proc/self/fd/3/x`, `/dev/fd/3/x`) walks a directory that descriptor
/// holds. A descriptor itself, `/proc/self/fd/0`, names a file the process
/// already has open, and is not refused.
fn aliases_another_tree(path: &str) -> bool {
    let components: Vec<&str> = path
        .split('/')
        .filter(|component| !component.is_empty())
        .collect();
    let below_descriptor = |components: &[&str], links: &[&str]| {
        components.iter().enumerate().any(|(index, component)| {
            links.contains(component)
                && components.len() > index + if *component == "fd" { 2 } else { 1 }
        })
    };
    match components.split_first() {
        Some((&"proc", rest)) => {
            rest.iter()
                .any(|component| PROC_ALIAS_COMPONENTS.contains(component))
                || below_descriptor(rest, &["fd"])
        }
        Some((&"dev", rest)) => below_descriptor(rest, DEV_DESCRIPTOR_LINKS),
        _ => false,
    }
}

fn permitted(path: &str, consumer: &str, context: &Context) -> bool {
    // `/tmp/../usr/lib64/x` is under no root it names.
    if path.split('/').any(|component| component == "..") {
        return false;
    }
    if aliases_another_tree(path) {
        return false;
    }
    let under = |path: &str, root: &str| {
        path == root
            || path
                .strip_prefix(root)
                .is_some_and(|rest| rest.starts_with('/'))
    };
    context.image.names(path)
        || context.image.is_directory(path)
        || RUNTIME_ROOTS
            .iter()
            .any(|root| under(path, root.trim_end_matches('/')))
        || RUNTIME_FILES.contains(&path)
        || context
            .data_paths
            .iter()
            .any(|data| data.consumer == consumer && under(path, data.path))
}

/// The body of the `label)` arm of the workload script's
/// `case "$PROGRAM" in` block, or `None` when the script has no such arm.
pub fn workload_arm(script: &str, label: &str) -> Option<String> {
    let mut lines = script.lines();
    lines.by_ref().find(|line| *line == WORKLOAD_CASE_LINE)?;
    let mut body: Option<Vec<&str>> = None;
    for line in lines {
        if line == "esac" {
            break;
        }
        if let Some(body) = body.as_mut() {
            if line == "        ;;" {
                return Some(body.join("\n"));
            }
            body.push(line);
            continue;
        }
        let Some(patterns) = line
            .strip_prefix("    ")
            .and_then(|rest| rest.strip_suffix(')'))
        else {
            continue;
        };
        if !patterns.starts_with(' ') && patterns.split('|').any(|pattern| pattern == label) {
            body = Some(Vec::new());
        }
    }
    None
}

/// The code of the workload script that runs for every arm, the lines before
/// and after the `case` block outside function definitions, and the body of
/// every function `arm` calls, directly or through another such function. A
/// function is a `function NAME {` line and the lines up to the next `}` line;
/// text in any other form counts as code every arm runs.
fn workload_shared_code(script: &str, arm: &str) -> String {
    let mut shared = Vec::new();
    let mut functions: Vec<(&str, Vec<&str>)> = Vec::new();
    let mut current: Option<(&str, Vec<&str>)> = None;
    let mut in_case = false;
    for line in script.lines() {
        if let Some((name, mut body)) = current.take() {
            if line == "}" {
                functions.push((name, body));
            } else {
                body.push(line);
                current = Some((name, body));
            }
            continue;
        }
        if in_case {
            in_case = line != "esac";
            continue;
        }
        if line == WORKLOAD_CASE_LINE {
            in_case = true;
            continue;
        }
        let name = line
            .strip_prefix("function ")
            .and_then(|rest| rest.strip_suffix(" {"));
        match name {
            Some(name) => current = Some((name, Vec::new())),
            None => shared.push(line),
        }
    }
    if let Some((_, body)) = current {
        shared.extend(body);
    }
    let mut searched = arm.to_owned();
    let mut included = BTreeSet::new();
    loop {
        let before = included.len();
        for (name, body) in &functions {
            if !included.contains(name) && calls(&searched, name) {
                included.insert(*name);
                searched.push('\n');
                searched.push_str(&body.join("\n"));
                shared.extend(body.iter().copied());
            }
        }
        if included.len() == before {
            break;
        }
    }
    shared.join("\n")
}

/// Whether `text` names the shell function `name` as a whole word.
fn calls(text: &str, name: &str) -> bool {
    let word = |byte: u8| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-';
    text.match_indices(name).any(|(start, _)| {
        let end = start + name.len();
        (start == 0 || !word(text.as_bytes()[start - 1]))
            && text.as_bytes().get(end).is_none_or(|byte| !word(*byte))
    })
}

/// Replace each declared host-only probe block of `arm` with its fallback
/// assignment, after checking that the listing records the probe as absent
/// and the fallback as present. Only this exact block, on consecutive lines,
/// is exempt:
///
/// ```text
/// if [[ -x PROBE ]]; then
///     [readonly ]NAME=PROBE
/// else
///     [readonly ]NAME=FALLBACK
/// fi
/// ```
///
/// The arm must contain at least one, and the probe path anywhere else in the
/// arm is refused, so an unguarded use cannot ride on a guarded one.
fn strip_host_only_probes(label: &str, arm: &str, image: &ImagePaths) -> Result<String, String> {
    let mut arm = arm.to_owned();
    for probe in HOST_ONLY_PROBES.iter().filter(|probe| probe.label == label) {
        let guard = format!("if [[ -x {} ]]; then", probe.path);
        let stale = || {
            format!(
                "the declared host-only probe {} of workload arm `{label}` is no longer a \
                 `{guard}` block assigning it, or else {}, to one name; update HOST_ONLY_PROBES",
                probe.path, probe.fallback
            )
        };
        if !image.contains(probe.fallback) || !image.records_absent(probe.path) {
            return Err(format!(
                "the host-only probe {} of workload arm `{label}` needs an `absent:` line in \
                 {IMAGE_PATHS_FILE} and its fallback {} listed as present; regenerate the \
                 listing (the command is in its header)",
                probe.path, probe.fallback
            ));
        }
        let assigned = |line: &str, value: &str| -> Option<String> {
            let line = line.trim();
            let line = line.strip_prefix("readonly ").unwrap_or(line);
            let (name, assigned) = line.split_once('=')?;
            let simple = !name.is_empty()
                && name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_');
            (simple && assigned == value).then(|| name.to_owned())
        };
        let lines: Vec<&str> = arm.lines().collect();
        let mut kept: Vec<String> = Vec::with_capacity(lines.len());
        let mut blocks = 0;
        let mut index = 0;
        while index < lines.len() {
            let block = lines.get(index..index + 5).and_then(|block| {
                let name = assigned(block[1], probe.path)?;
                (block[0].trim() == guard
                    && block[2].trim() == "else"
                    && assigned(block[3], probe.fallback).as_deref() == Some(name.as_str())
                    && block[4].trim() == "fi")
                    .then_some(block[3])
            });
            if let Some(fallback) = block {
                kept.push(fallback.to_owned());
                blocks += 1;
                index += 5;
            } else {
                kept.push(lines[index].to_owned());
                index += 1;
            }
        }
        arm = kept.join("\n");
        if blocks == 0 {
            return Err(stale());
        }
        if arm.contains(probe.path) {
            return Err(format!(
                "workload arm `{label}` names the host-only probe {} outside its guarded \
                 `{guard}` block; only that block may name it",
                probe.path
            ));
        }
    }
    Ok(arm)
}

fn is_path_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || b"_.+-/".contains(&byte)
}

/// Every absolute path in `text`, whatever its directory. A path starts at a
/// `/` that begins the text or follows a byte that cannot continue a word, a
/// path or a shell expansion, so `$W/bin/x`, `"$W"/bin/x` and `{{X}}/lib/x`
/// are not taken for absolute paths. A path also starts after the `:-` or `:+`
/// of a default expansion (`${X:-/bin/x}`) and after a short option written
/// against it (`-I/usr/include`). The authority of a `scheme://` URL is not a
/// path.
pub fn absolute_paths(text: &str) -> Vec<&str> {
    let bytes = text.as_bytes();
    let mut paths = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'/' || !starts_path(bytes, index) {
            index += 1;
            continue;
        }
        let end = bytes[index..]
            .iter()
            .position(|byte| !is_path_byte(*byte))
            .map_or(bytes.len(), |offset| index + offset);
        let url = index > 0 && bytes[index - 1] == b':' && bytes.get(index + 1) == Some(&b'/');
        let path = text[index..end].trim_end_matches('.');
        if !url && !path.is_empty() {
            paths.push(path);
        }
        index = end;
    }
    paths
}

fn starts_path(bytes: &[u8], index: usize) -> bool {
    let back = |offset: usize| index.checked_sub(offset).map(|at| bytes[at]);
    let Some(before) = back(1) else {
        return true;
    };
    match before {
        b'$' | b'}' => false,
        b'"' => !closes_expansion(bytes, index - 1),
        b'-' | b'+' if back(2) == Some(b':') => true,
        letter
            if letter.is_ascii_alphabetic()
                && back(2) == Some(b'-')
                && back(3).is_none_or(|byte| !is_path_byte(byte)) =>
        {
            true
        }
        other => !is_path_byte(other),
    }
}

/// Whether the `"` at `quote` closes a quoted word that ends in a shell
/// expansion (`"$dir"`, `"${dir}"`), so a `/` after it continues that value.
fn closes_expansion(bytes: &[u8], quote: usize) -> bool {
    if quote > 0 && bytes[quote - 1] == b'}' {
        return true;
    }
    let mut index = quote;
    while index > 0 && is_path_byte(bytes[index - 1]) {
        index -= 1;
    }
    index < quote && index > 0 && bytes[index - 1] == b'$'
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo_root() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
    }

    const LIBC_SHA256: &str = "0000000000000000000000000000000000000000000000000000000000000000";
    const TREE_SHA256: &str = "2222222222222222222222222222222222222222222222222222222222222222";

    fn image() -> ImagePaths {
        let mut text = format!(
            "# listing\nimage: example@sha256:00\ntree: /s sha256:{TREE_SHA256}\nfile: /lib64/ld.so \
             /s/glibc/lib/ld.so sha256:{LIBC_SHA256}\nabsent: /usr/local/bin/git.meta.real\n"
        );
        for path in [
            "/bin/bash",
            "/bin/env",
            "/usr/bin/basename",
            "/usr/bin/env",
            "/usr/bin/git",
            "/bin/cat",
        ] {
            text.push_str(&format!("{path} /s/{}\n", path.rsplit('/').next().unwrap()));
        }
        ImagePaths::parse(&text).unwrap()
    }

    fn document(yaml: &str) -> Value {
        let yaml: serde_yaml::Value = serde_yaml::from_str(yaml).unwrap();
        Value::from_yaml(crate::manifest_corpus::expand_corpus(yaml).unwrap()).unwrap()
    }

    fn no_scripts(relative: &str) -> Result<String, String> {
        Err(format!("unexpected script {relative}"))
    }

    /// One test whose verify cell gates on ptrace, the shape every gating
    /// compat cell had before https://github.com/rrnewton/hermit/issues/3712.
    fn single_test(bucket: &str, guest_root: &str, direct: &str, verify_extra: &str) -> Value {
        document(&format!(
            r#"
schema: 3
bucket: {bucket}
{guest_root}
test:
  - id: {bucket}/probe
    description: probe
    lane: portable
    direct: {direct}
    modes:
      verify:
        ci: true
        backends_enabled: [ptrace]
        backends_disabled: {{sabre: x, dbt: x, kvm: x, liteinst: x}}
{verify_extra}
"#
        ))
    }

    /// The expanded documents of every shipped manifest, and the compat-family
    /// buckets among them.
    fn shipped_documents() -> (Vec<Value>, BTreeSet<String>) {
        let manifests = repo_root().join("tests/e2e/manifests");
        let mut documents = Vec::new();
        let mut buckets = BTreeSet::new();
        for entry in std::fs::read_dir(&manifests).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("yaml") {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap();
            let yaml: serde_yaml::Value = serde_yaml::from_str(&text).unwrap();
            let document =
                Value::from_yaml(crate::manifest_corpus::expand_corpus(yaml).unwrap()).unwrap();
            // Only bucket documents carry `bucket`; the defaults file does not.
            if let Some(bucket) = document.get("bucket").and_then(Value::as_str) {
                if is_compat_bucket(bucket) {
                    buckets.insert(bucket.to_owned());
                }
            }
            documents.push(document);
        }
        (documents, buckets)
    }

    fn read_checkout(relative: &str) -> Result<String, String> {
        std::fs::read_to_string(repo_root().join(relative)).map_err(|error| error.to_string())
    }

    #[test]
    fn shipped_compat_manifests_satisfy_the_pinned_guest_contract() {
        let image = ImagePaths::load(&repo_root()).unwrap();
        let (documents, buckets) = shipped_documents();
        assert_eq!(
            buckets,
            BTreeSet::from(["compat".to_owned(), "compat-pinned".to_owned()]),
            "the contract must see both the gating and the host-portability manifest"
        );
        let errors = violations(&documents, &image, &read_checkout);
        assert!(errors.is_empty(), "{}", errors.join("\n"));
        let gating = documents
            .iter()
            .filter(|document| {
                document.get("bucket").and_then(Value::as_str) == Some("compat-pinned")
            })
            .flat_map(|document| document.get("test").and_then(Value::as_array).unwrap())
            .filter(|test| !gating_cells(test).is_empty())
            .count();
        assert!(gating > 100, "only {gating} compat-pinned tests gate");
    }

    #[test]
    fn every_declared_data_path_is_still_needed() {
        let image = ImagePaths::load(&repo_root()).unwrap();
        let (documents, _) = shipped_documents();
        for (index, data) in DATA_PATHS.iter().enumerate() {
            let mut fewer = DATA_PATHS.to_vec();
            fewer.remove(index);
            let errors = violations_with(
                &documents,
                &image,
                &read_checkout,
                &fewer,
                RUN_STATE_OPERANDS,
            );
            assert!(
                errors
                    .iter()
                    .any(|error| error.contains(&format!("names `{}`", data.path))),
                "{data:?} is not needed by the shipped corpus: {errors:?}"
            );
        }
        for (index, operand) in RUN_STATE_OPERANDS.iter().enumerate() {
            let mut fewer = RUN_STATE_OPERANDS.to_vec();
            fewer.remove(index);
            let errors = violations_with(&documents, &image, &read_checkout, DATA_PATHS, &fewer);
            assert!(
                errors
                    .iter()
                    .any(|error| error.contains(operand.path)
                        && error.contains("names run state under")),
                "{operand:?} is not needed by the shipped corpus: {errors:?}"
            );
        }
    }

    /// Each row of compat-pinned.yaml's `unavailable` table stays out of the
    /// gating set because the pinned image lacks its program. Once a repin
    /// adds that program (29a8d19b added curl), the entry is stale and the row
    /// must gate again, so a program the listing resolves is refused here.
    #[test]
    fn every_unavailable_row_names_a_program_the_image_lacks() {
        let image = ImagePaths::load(&repo_root()).unwrap();
        let text = read_checkout("tests/e2e/manifests/compat-pinned.yaml").unwrap();
        let yaml: serde_yaml::Value = serde_yaml::from_str(&text).unwrap();
        let rows = yaml["corpus"]["unavailable"]["rows"]
            .as_mapping()
            .expect("compat-pinned.yaml declares corpus.unavailable.rows");
        assert!(!rows.is_empty());
        let stale: Vec<String> = rows
            .iter()
            .filter_map(|(label, program)| {
                let program = program.as_str().unwrap();
                image
                    .resolves_bare(program)
                    .then(|| format!("{}: {program}", label.as_str().unwrap()))
            })
            .collect();
        assert!(
            stale.is_empty(),
            "the pinned image has the program of these unavailable rows, so they must gate: {stale:?}"
        );
    }

    #[test]
    fn a_gating_compat_cell_without_the_pinned_guest_root_is_refused() {
        let base = single_test("compat", "", "[/usr/bin/basename, x]", "");
        let errors = violations(&[base], &image(), &no_scripts);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(
            errors[0].contains("does not declare `guest_root: pinned`"),
            "{errors:?}"
        );
        assert!(errors[0].contains("verify/ptrace"), "{errors:?}");
    }

    #[test]
    fn a_gating_argv_naming_a_host_program_outside_the_image_is_refused() {
        let pinned = single_test(
            "compat-pinned",
            "guest_root: pinned",
            "[/usr/bin/curl, --version]",
            "",
        );
        let errors = violations(&[pinned], &image(), &no_scripts);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].contains("`/usr/bin/curl`"), "{errors:?}");
        // A host library named inside a shell string is refused the same way.
        let pinned = single_test(
            "compat-pinned",
            "guest_root: pinned",
            "[bash, -c, 'LD_PRELOAD=/usr/lib64/libz.so.1 /bin/cat x']",
            "",
        );
        let errors = violations(&[pinned], &image(), &no_scripts);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].contains("`/usr/lib64/libz.so.1`"), "{errors:?}");
    }

    #[test]
    fn a_gating_bare_program_the_image_lacks_is_refused() {
        let pinned = single_test(
            "compat-pinned",
            "guest_root: pinned",
            "[env, HERMIT_COMPAT=1, bc, -q]",
            "",
        );
        let errors = violations(&[pinned], &image(), &no_scripts);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].contains("bare program `bc`"), "{errors:?}");
        let pinned = single_test(
            "compat-pinned",
            "guest_root: pinned",
            "[env, HERMIT_COMPAT=1, cat]",
            "",
        );
        assert!(violations(&[pinned], &image(), &no_scripts).is_empty());
        let pinned = single_test(
            "compat-pinned",
            "guest_root: pinned",
            "[/usr/bin/env, -i, -u, X, HERMIT_COMPAT=1, bc]",
            "",
        );
        let errors = violations(&[pinned], &image(), &no_scripts);
        assert!(
            errors.len() == 1 && errors[0].contains("bare program `bc`"),
            "{errors:?}"
        );
    }

    /// A gating cell's recipe can change what its argv runs without the argv
    /// naming it: through the guest variables its `env` adds, or through a
    /// field that extends the argv or moves its working directory.
    #[test]
    fn a_gating_recipe_cannot_change_what_its_argv_runs() {
        let errors_with = |direct: &str, verify_extra: &str| {
            let test = single_test("compat-pinned", "guest_root: pinned", direct, verify_extra);
            violations(&[test], &image(), &no_scripts)
        };
        // The corpus's own variable is admitted.
        assert!(errors_with("[/bin/cat, x]", "        env: {TMPDIR: /tmp}").is_empty());
        // PATH and the loader's variables are refused whatever their value:
        // a relative entry searches the working directory, and even a value
        // inside the image moves where bare names and libraries resolve.
        for (env, name) in [
            ("{PATH: /usr/bin}", "PATH"),
            ("{PATH: bin}", "PATH"),
            ("{LD_LIBRARY_PATH: .}", "LD_LIBRARY_PATH"),
            ("{LD_PRELOAD: /bin/cat}", "LD_PRELOAD"),
        ] {
            assert_one(
                &errors_with("[/bin/cat, x]", &format!("        env: {env}")),
                &format!("its verify `env` sets `{name}`"),
            );
        }
        // So is a host path in any value.
        assert_one(
            &errors_with("[/bin/cat, x]", "        env: {TMPDIR: /build/fixtures}"),
            "its verify `env` TMPDIR names `/build/fixtures`",
        );
        // An `env` that starts the argv obeys the same names rule.
        assert_one(
            &errors_with("[env, PATH=/usr/bin, cat, x]", ""),
            "its argv's `env` sets `PATH`",
        );
        assert_one(
            &errors_with("[/usr/bin/env, -u, X, LD_PRELOAD=/bin/cat, cat]", ""),
            "its argv's `env` sets `LD_PRELOAD`",
        );
        // Fields this contract does not read are refused outright.
        for (extra, field) in [
            ("        guest_args: {ptrace: [x]}", "guest_args"),
            ("        workdir: /tmp", "workdir"),
        ] {
            assert_one(
                &errors_with("[/bin/cat]", extra),
                &format!("its verify recipe declares `{field}`"),
            );
        }
        // A gating replay cell runs with its verify recipe's environment, so
        // that recipe is checked even when its own cells do not gate; a
        // recipe no gating cell uses is not.
        let replay = |replay_ci: bool| {
            document(&format!(
                r#"
schema: 3
bucket: compat-pinned
guest_root: pinned
test:
  - id: compat-pinned/probe
    description: probe
    lane: portable
    direct: [/bin/cat, x]
    modes:
      verify:
        ci: false
        ci_disabled_reason: not measured
        backends_enabled: [ptrace]
        backends_disabled: {{sabre: x, dbt: x, kvm: x, liteinst: x}}
        env: {{LD_PRELOAD: /bin/cat}}
      replay:
        ci: {replay_ci}
        ci_disabled_reason: not measured
        backends_enabled: [ptrace]
        backends_disabled: {{sabre: x, dbt: x, kvm: x, liteinst: x}}
"#
            ))
        };
        let errors = violations(&[replay(true)], &image(), &no_scripts);
        assert_one(&errors, "its verify `env` sets `LD_PRELOAD`");
        assert!(errors[0].contains("gates replay/ptrace"), "{errors:?}");
        assert!(violations(&[replay(false)], &image(), &no_scripts).is_empty());
    }

    /// Round 6 of the review: a recipe variable a shell reads (`BASH_ENV`) and
    /// a value of the wrong kind for an admitted name are refused.
    #[test]
    fn a_gating_cell_sets_only_admitted_variables_with_values_of_their_kind() {
        let errors_with = |direct: &str, verify_extra: &str| {
            let test = single_test("compat-pinned", "guest_root: pinned", direct, verify_extra);
            violations(&[test], &image(), &no_scripts)
        };
        // A shell sources the file BASH_ENV names before its script; a
        // relative value resolves against the working directory, which may be
        // the host-mounted checkout.
        let errors = errors_with(
            "[/bin/bash, -c, 'true']",
            "        env: {BASH_ENV: ../src/target/host.sh}",
        );
        assert!(
            errors
                .iter()
                .any(|error| error.contains("sets `BASH_ENV`, which this contract does not admit"))
                && errors
                    .iter()
                    .any(|error| error.contains("has a `..` path component")),
            "{errors:?}"
        );
        assert_one(
            &errors_with("[/bin/cat, x]", "        env: {ENV: /tmp/x}"),
            "sets `ENV`, which this contract does not admit",
        );
        // An admitted name must hold a value of its kind.
        assert_one(
            &errors_with("[/bin/cat, x]", "        env: {REAL_COMPAT_FIXTURES: /tmp}"),
            "sets `REAL_COMPAT_FIXTURES` to \"/tmp\", which is not the receipt-verified fixture tree",
        );
        assert_one(
            &errors_with("[/bin/cat, x]", "        env: {TMPDIR: tmp}"),
            "sets `TMPDIR` to \"tmp\", which is not an absolute path",
        );
        let errors = errors_with("[env, HERMIT_COMPAT=/bin/cat, /bin/cat]", "");
        assert_one(&errors, "admitted only as text");
        assert!(
            errors[0].contains("its argv's `env` sets `HERMIT_COMPAT`"),
            "{errors:?}"
        );
        assert_one(
            &errors_with("[env, HOME=/tmp/home, /bin/cat]", ""),
            "sets `HOME` to \"/tmp/home\", which is outside the pinned-root fixture tree",
        );
        // The shipped values are admitted.
        assert!(
            errors_with(
                "[env, 'REAL_COMPAT_FIXTURES={{VALIDATE_RUN_STATE}}/compat-pinned/real-compat-fixtures', \
                 'HOME={{VALIDATE_RUN_STATE}}/compat-pinned/top-home', HERMIT_COMPAT=printenv, /bin/cat]",
                "        env: {TMPDIR: /tmp}",
            )
            .is_empty()
        );
    }

    /// Round 6 of the review: the `env` that starts an argv may use only the
    /// options that remove variables, and may not run `env` again.
    #[test]
    fn a_gating_argv_env_uses_only_options_this_check_reads() {
        let errors_with = |direct: &str| pinned_errors(direct, &no_scripts);
        for (direct, option) in [
            (
                "[env, '--split-string=LD_PRELOAD=/bin/cat /bin/cat']",
                "--split-string=",
            ),
            ("[env, -S, 'LD_PRELOAD=/bin/cat /bin/cat']", "-S"),
            ("[/usr/bin/env, -C, /, cat]", "-C"),
            ("[env, --chdir=/, cat]", "--chdir=/"),
            ("[env, -iS, 'cat x']", "-iS"),
        ] {
            let errors = errors_with(direct);
            assert!(
                errors
                    .iter()
                    .any(|error| error.contains(&format!("takes the option `{option}"))),
                "{direct}: {errors:?}"
            );
        }
        assert_one(
            &errors_with("[env, env, LD_PRELOAD=/bin/cat, /bin/cat]"),
            "runs `env`, another env",
        );
        assert_one(
            &errors_with("[env, -i, /usr/bin/env, LD_PRELOAD=/bin/cat, /bin/cat]"),
            "runs `/usr/bin/env`, another env",
        );
        for direct in [
            "[env, -i, HERMIT_COMPAT=1, /bin/cat]",
            "[env, --ignore-environment, --unset, X, --unset=Y, -u, Z, HERMIT_COMPAT=1, cat]",
            "[env, --, HERMIT_COMPAT=1, /bin/cat]",
        ] {
            let errors = errors_with(direct);
            assert!(errors.is_empty(), "{direct}: {errors:?}");
        }
    }

    /// Round 6 of the review: a runtime root admits kernel interfaces, not the
    /// other file trees `/proc` and `/dev` reach, and a file an interpreter
    /// runs must come from the image.
    #[test]
    fn a_runtime_path_that_reaches_another_tree_is_refused() {
        let errors_with = |direct: &str| pinned_errors(direct, &no_scripts);
        for path in [
            "/proc/self/root/src/target/host.sh",
            "/proc/self/cwd/x",
            "/proc/1/root/usr/lib64/libz.so.1",
            "/proc/self/task/1/root",
            "/proc/thread-self/cwd",
            "/proc/self/map_files/7f00-7f01",
            "/proc/self/fd/3/x",
            "/dev/fd/3/x",
            "/dev/stdin/x",
        ] {
            assert_one(
                &errors_with(&format!("[/bin/cat, {path}]")),
                &format!("names `{path}`, which reaches another file tree"),
            );
        }
        // A descriptor itself, and the kernel's own interfaces, are admitted.
        for path in [
            "/proc/self/fd/0",
            "/dev/stdin",
            "/dev/fd/0",
            "/proc/self/status",
            "/dev/null",
        ] {
            let errors = errors_with(&format!("[/bin/cat, {path}]"));
            assert!(errors.is_empty(), "{path}: {errors:?}");
        }
        // The program file an interpreter runs must be an image executable.
        let errors = errors_with("[/bin/bash, /proc/self/root/src/target/host.sh]");
        assert!(
            errors.len() == 2
                && errors.iter().any(|error| error
                    .contains("runs `/proc/self/root/src/target/host.sh` through `/bin/bash`"))
                && errors
                    .iter()
                    .any(|error| error.contains("reaches another file tree")),
            "{errors:?}"
        );
        assert_one(
            &errors_with("[/bin/bash, /tmp/x.sh]"),
            "runs `/tmp/x.sh` through `/bin/bash`, a program file that is not an executable of \
             the pinned image",
        );
        assert_one(
            &errors_with("[bash, --, /dev/stdin]"),
            "runs `/dev/stdin` through `bash`",
        );
        assert!(errors_with("[bash, /bin/cat]").is_empty());
        // A placeholder after the start of a file that runs is refused, so a
        // prefix cannot decide where a run-state file comes from.
        assert_one(
            &errors_with("['/x{{VALIDATE_RUN_STATE}}/compat-pinned/y']"),
            "names a placeholder after its start",
        );
        let errors =
            errors_with("[bash, '/proc/self/root{{VALIDATE_RUN_STATE}}/compat-pinned/x.sh']");
        assert!(
            errors
                .iter()
                .any(|error| error.contains("names a placeholder after its start")),
            "{errors:?}"
        );
        // Round 7 of the review: run state outside the receipt-verified
        // fixture tree is not a program file, even under compat-pinned/.
        assert_one(
            &errors_with("[bash, '{{VALIDATE_RUN_STATE}}/compat-pinned/shell-build/x.sh']"),
            "outside {{VALIDATE_RUN_STATE}}/compat-pinned/real-compat-fixtures/, the only",
        );
        assert!(
            errors_with("[bash, '{{VALIDATE_RUN_STATE}}/compat-pinned/real-compat-fixtures/x.sh']")
                .is_empty()
        );
    }

    #[test]
    fn diagnostic_and_unselected_cells_do_not_gate() {
        let diagnostic = single_test(
            "compat",
            "",
            "[/usr/bin/curl]",
            "        diagnostic: {ptrace: reported only}",
        );
        let unselected = document(
            r#"
schema: 3
bucket: compat
test:
  - id: compat/probe
    description: probe
    lane: portable
    direct: [/usr/bin/curl]
    modes:
      verify:
        ci: false
        ci_disabled_reason: not measured
        backends_enabled: [ptrace]
        backends_disabled: {sabre: x, dbt: x, kvm: x, liteinst: x}
"#,
        );
        assert!(violations(&[diagnostic, unselected], &image(), &no_scripts).is_empty());
    }

    #[test]
    fn buckets_outside_the_compat_family_are_not_checked() {
        let other = single_test("compatibility", "", "[/usr/bin/curl]", "");
        assert!(violations(&[other], &image(), &no_scripts).is_empty());
    }

    #[test]
    fn a_workload_arm_naming_a_host_program_is_refused() {
        let script = "#!/bin/bash\ncase \"$PROGRAM\" in\n    clang)\n        # build x.c\n        /usr/bin/clang -O2 x.c\n        ;;\n    cat)\n        /bin/cat x\n        ;;\nesac\n";
        let read = |relative: &str| {
            assert_eq!(relative, REAL_COMPAT_WORKLOAD);
            Ok(script.to_owned())
        };
        let argv = |label: &str| {
            format!(
                "[env, HERMIT_COMPAT=1, bash, '{{{{ROOT_DIR}}}}/{REAL_COMPAT_WORKLOAD}', {label}]"
            )
        };
        let clang = single_test("compat-pinned", "guest_root: pinned", &argv("clang"), "");
        let errors = violations(&[clang], &image(), &read);
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].contains("`/usr/bin/clang`"), "{errors:?}");
        assert!(errors[0].contains("workload arm `clang`"), "{errors:?}");
        let cat = single_test("compat-pinned", "guest_root: pinned", &argv("cat"), "");
        assert!(violations(&[cat], &image(), &read).is_empty());
        let absent = single_test("compat-pinned", "guest_root: pinned", &argv("bc"), "");
        let errors = violations(&[absent], &image(), &read);
        assert!(errors[0].contains("no `bc)` arm"), "{errors:?}");
    }

    #[test]
    fn a_declared_host_only_probe_must_still_guard_an_image_fallback() {
        let guarded = "case \"$PROGRAM\" in\n    git)\n        if [[ -x /usr/local/bin/git.meta.real ]]; then\n            GIT=/usr/local/bin/git.meta.real\n        else\n            GIT=/usr/bin/git\n        fi\n        ;;\nesac\n";
        assert_eq!(
            strip_host_only_probes("git", &workload_arm(guarded, "git").unwrap(), &image()).map(
                |arm| absolute_paths(&arm)
                    .into_iter()
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            ),
            Ok(vec!["/usr/bin/git".to_owned()])
        );
        let strip = |script: &str, image: &ImagePaths| {
            strip_host_only_probes("git", &workload_arm(script, "git").unwrap(), image)
        };
        let unguarded = guarded.replace("[[ -x /usr/local/bin/git.meta.real ]]", "true");
        assert!(strip(&unguarded, &image()).is_err());
        // Only the guarded block is exempt: a use of the probe after a valid
        // block, or a block whose branches assign different names, is refused.
        let extra = guarded.replace(
            "        fi\n",
            "        fi\n        /usr/local/bin/git.meta.real --version\n",
        );
        let error = strip(&extra, &image()).unwrap_err();
        assert!(error.contains("outside its guarded"), "{error}");
        let renamed = guarded.replace("    GIT=/usr/bin/git", "    OTHER=/usr/bin/git");
        assert!(strip(&renamed, &image()).is_err());
        // The listing must record the probe as absent from the image.
        let mut unrecorded = image();
        unrecorded.absent.clear();
        let error = strip(guarded, &unrecorded).unwrap_err();
        assert!(error.contains("absent:"), "{error}");
        // The shipped script still carries the probe block this table
        // declares, and the shipped listing records each probe as absent.
        let shipped = std::fs::read_to_string(repo_root().join(REAL_COMPAT_WORKLOAD)).unwrap();
        let listing = ImagePaths::load(&repo_root()).unwrap();
        for probe in HOST_ONLY_PROBES {
            let arm = workload_arm(&shipped, probe.label).unwrap();
            assert!(
                strip_host_only_probes(probe.label, &arm, &listing).is_ok(),
                "{probe:?}"
            );
            assert!(listing.records_absent(probe.path), "{probe:?}");
        }
    }

    /// The violations of one gating compat-pinned test running `direct`, with
    /// `read` serving the repository files it names.
    fn pinned_errors(direct: &str, read: &dyn Fn(&str) -> Result<String, String>) -> Vec<String> {
        let test = single_test("compat-pinned", "guest_root: pinned", direct, "");
        violations(&[test], &image(), read)
    }

    /// Assert exactly one violation, containing `needle`.
    fn assert_one(errors: &[String], needle: &str) {
        assert!(
            errors.len() == 1 && errors[0].contains(needle),
            "expected one violation containing {needle:?}: {errors:?}"
        );
    }

    #[test]
    fn a_host_path_in_any_directory_is_refused_wherever_the_argv_names_it() {
        assert_one(
            &pinned_errors("[/opt/host/bin/bc, -q]", &no_scripts),
            "runs `/opt/host/bin/bc`",
        );
        // As an operand a shell runs through "$1", and inside the script text.
        assert_one(
            &pinned_errors(r#"[bash, -c, '"$1" -q', x, /opt/host/bin/bc]"#, &no_scripts),
            "names `/opt/host/bin/bc`",
        );
        assert_one(
            &pinned_errors("[bash, -c, 'exec /opt/host/bin/bc -q']", &no_scripts),
            "names `/opt/host/bin/bc`",
        );
        // A runtime root does not cover a path that climbs out of it: the
        // path check refuses it, besides the argv's `..` refusal.
        let errors = pinned_errors("[/bin/cat, /tmp/../usr/lib64/libz.so.1]", &no_scripts);
        assert!(
            errors.len() == 2
                && errors[0].contains("has a `..` path component")
                && errors[1].contains("names `/tmp/../usr/lib64/libz.so.1`"),
            "{errors:?}"
        );
        // Inside a repository script the script's own `..` refusal and the
        // path check both see it.
        let read = |relative: &str| {
            assert_eq!(relative, "ci/probe.sh");
            Ok("#!/bin/bash\n/bin/cat /tmp/../usr/lib64/libz.so.1\n".to_owned())
        };
        let errors = pinned_errors("[bash, '{{ROOT_DIR}}/ci/probe.sh']", &read);
        assert!(
            errors.len() == 2
                && errors[0].contains("with a `..` path component")
                && errors[1].contains("names `/tmp/../usr/lib64/libz.so.1`"),
            "{errors:?}"
        );
    }

    #[test]
    fn runtime_paths_are_permitted_and_other_host_files_are_not() {
        assert!(pinned_errors(
            "[bash, -c, 'cat /dev/null /tmp/x /proc/self/maps /etc/hostname > /dev/null; ls /usr/bin/ /']",
            &no_scripts
        )
        .is_empty());
        assert_one(
            &pinned_errors("[bash, -c, 'cat /home/u/x']", &no_scripts),
            "names `/home/u/x`",
        );
        assert_one(
            &pinned_errors("[/bin/cat, /etc/passwd]", &no_scripts),
            "names `/etc/passwd`",
        );
    }

    #[test]
    fn a_declared_data_path_covers_only_its_consumer() {
        assert!(
            pinned_errors("[/usr/bin/basename, /opt/hermit/bin/hermit]", &no_scripts).is_empty()
        );
        assert_one(
            &pinned_errors("[/bin/cat, /opt/hermit/bin/hermit]", &no_scripts),
            "names `/opt/hermit/bin/hermit`",
        );
        assert_one(
            &pinned_errors(
                "[bash, -c, '/usr/bin/basename /opt/hermit/bin/hermit']",
                &no_scripts,
            ),
            "names `/opt/hermit/bin/hermit`",
        );
    }

    #[test]
    fn a_program_the_checkout_builds_is_refused() {
        let read = |relative: &str| match relative {
            "target/release/probe" => Err("stream did not contain valid UTF-8".to_owned()),
            "tests/x.py" => Ok("#!/usr/bin/env python3\nprint(1)\n".to_owned()),
            other => Err(format!("unexpected read of {other}")),
        };
        assert_one(
            &pinned_errors("['{{ROOT_DIR}}/target/release/probe']", &read),
            "not a shell script this check can read",
        );
        assert_one(
            &pinned_errors("[bash, '{{ROOT_DIR}}/target/release/probe']", &read),
            "runs the repository file target/release/probe through `bash`",
        );
        assert_one(
            &pinned_errors("[/bin/cat, '{{ROOT_DIR}}/target/release/probe']", &read),
            "not a text file of the checkout",
        );
        assert_one(
            &pinned_errors("[/bin/cat, '{{ROOT_DIR}}/tests/x.py']", &read),
            "passes the repository script tests/x.py",
        );
        // A built program reached through a directory operand.
        let read_directory = |relative: &str| {
            assert_eq!(relative, "target/release");
            Err("Is a directory (os error 21)".to_owned())
        };
        assert_one(
            &pinned_errors(
                "[bash, -c, 'cd \"$1\" && ./probe', x, '{{ROOT_DIR}}/target/release']",
                &read_directory,
            ),
            "not a text file of the checkout",
        );
        let errors = pinned_errors("[python3, '{{ROOT_DIR}}/tests/x.py']", &read);
        assert!(
            errors.iter().any(
                |error| error.contains("runs the repository file tests/x.py through `python3`")
            ),
            "{errors:?}"
        );
    }

    #[test]
    fn a_repository_script_needs_an_image_interpreter_and_image_paths() {
        let script = |text: &'static str| {
            move |relative: &str| {
                assert_eq!(relative, "tests/x.sh");
                Ok(text.to_owned())
            }
        };
        let run = "['{{ROOT_DIR}}/tests/x.sh']";
        assert!(pinned_errors(run, &script("#!/usr/bin/env bash\n/bin/cat x\n")).is_empty());
        assert!(pinned_errors(run, &script("#!/bin/bash\n/bin/cat x\n")).is_empty());
        // The `#!` line is read by the interpreter check and, like every
        // other line, by the path check.
        let errors = pinned_errors(run, &script("#!/opt/host/bin/bash\n"));
        assert!(
            errors.len() == 2
                && errors[0].contains("interpreter `/opt/host/bin/bash`")
                && errors[1].contains("names `/opt/host/bin/bash`"),
            "{errors:?}"
        );
        assert_one(
            &pinned_errors(run, &script("#!/usr/bin/env bc\n")),
            "runs the bare program `bc`",
        );
        assert_one(&pinned_errors(run, &script("echo hi\n")), "no `#!` line");
        // The script's own text is read whether the argv runs it directly or
        // through a shell.
        let host = script("#!/bin/bash\n# a comment\n/opt/host/bin/bc -q\n");
        assert_one(&pinned_errors(run, &host), "names `/opt/host/bin/bc`");
        assert_one(
            &pinned_errors("[bash, '{{ROOT_DIR}}/tests/x.sh']", &host),
            "names `/opt/host/bin/bc`",
        );
        // Round 7 of the review: comment lines are read too. A line that is a
        // shell comment can be a here-document's text that a program acts on,
        // here a C `#include` of a host-mounted header.
        let included =
            script("#!/bin/bash\n/bin/cat >x.c <<'EOF'\n#include \"/src/target/host.h\"\nEOF\n");
        for direct in [run, "[bash, '{{ROOT_DIR}}/tests/x.sh']"] {
            assert_one(
                &pinned_errors(direct, &included),
                "names `/src/target/host.h`",
            );
        }
    }

    /// Round 7 of the review: a `#!` line is read whole. Linux passes every
    /// word after the interpreter as one argument, so only `#!SHELL` and
    /// `#!ENV SHELL` are admitted; `env -S` would split a command out of it.
    #[test]
    fn a_shebang_line_is_read_whole_and_must_run_a_shell() {
        let run = "['{{ROOT_DIR}}/tests/x.sh']";
        let errors_with = |first: &str| {
            let text = format!("{first}\n/bin/cat x\n");
            pinned_errors(run, &move |relative: &str| {
                assert_eq!(relative, "tests/x.sh");
                Ok(text.clone())
            })
        };
        for first in [
            "#!/usr/bin/env -S bash -c 'exec /src/target/host-program'",
            "#!/usr/bin/env -S bash",
            "#!/bin/bash -c 'exec /src/target/host-program'",
            "#!/bin/bash -e",
            "#!/usr/bin/env LD_PRELOAD=/src/target/host.so bash",
            "#!/usr/bin/env bash -e",
        ] {
            let errors = errors_with(first);
            assert!(
                errors
                    .iter()
                    .any(|error| error.contains("this check reads only a `#!SHELL`")),
                "{first}: {errors:?}"
            );
        }
        assert_one(
            &errors_with("#!/usr/bin/env -S"),
            "is an env option or assignment",
        );
        assert_one(
            &errors_with("#!/usr/bin/env LD_PRELOAD=/bin/cat"),
            "is an env option or assignment",
        );
        assert_one(
            &errors_with("#!/usr/bin/env ./bash"),
            "runs the relative path `./bash`",
        );
        assert_one(
            &errors_with("#!/usr/bin/env cat"),
            "with `cat`, not a shell",
        );
        assert_one(&errors_with("#!/bin/cat"), "with `/bin/cat`, not a shell");
        assert_one(
            &errors_with("#!/usr/bin/env"),
            "with `/usr/bin/env`, not a shell",
        );
        assert_one(&errors_with("#!"), "no `#!` line");
        for first in [
            "#!/usr/bin/env bash",
            "#!/bin/bash",
            "#! /bin/bash",
            "#!/bin/env bash",
        ] {
            let errors = errors_with(first);
            assert!(errors.is_empty(), "{first}: {errors:?}");
        }
    }

    /// Round 7 of the review: every line of a repository script and of the
    /// workload code an arm runs is refused when it has a `..` component,
    /// which reaches the host-mounted checkout from the guest's working
    /// directory through no absolute path.
    #[test]
    fn a_parent_component_in_script_text_is_refused() {
        let script = |text: &'static str| {
            move |relative: &str| {
                assert_eq!(relative, "tests/x.sh");
                Ok(text.to_owned())
            }
        };
        for text in [
            "#!/bin/bash\ncd /tmp; LD_PRELOAD=../src/target/host.so /bin/cat x\n",
            "#!/bin/bash\ncd ..\nsrc/target/host\n",
            "#!/bin/bash\n/bin/cat >x.c <<'EOF'\n#include \"../src/target/host.h\"\nEOF\n",
            "#!/bin/bash\n# cd .. && src/target/host\n",
        ] {
            for direct in [
                "['{{ROOT_DIR}}/tests/x.sh']",
                "[bash, '{{ROOT_DIR}}/tests/x.sh']",
            ] {
                assert_one(
                    &pinned_errors(direct, &script(text)),
                    "with a `..` path component",
                );
            }
        }
        // A range is not a component.
        assert!(
            pinned_errors(
                "[bash, '{{ROOT_DIR}}/tests/x.sh']",
                &script("#!/bin/bash\nfor i in {1..5}; do /bin/cat x; done\n")
            )
            .is_empty()
        );
        // The workload arm the argv selects, and the code every arm runs.
        let workload = "#!/bin/bash\ncase \"$PROGRAM\" in\n    cat)\n        cd /tmp && BASH_ENV=../src/target/host.sh /bin/bash -c true\n        ;;\n    tac)\n        /bin/cat x\n        ;;\nesac\n";
        let argv =
            |label: &str| format!("[bash, '{{{{ROOT_DIR}}}}/{REAL_COMPAT_WORKLOAD}', {label}]");
        let read = |relative: &str| {
            assert_eq!(relative, REAL_COMPAT_WORKLOAD);
            Ok(workload.to_owned())
        };
        let errors = pinned_errors(&argv("cat"), &read);
        assert_one(&errors, "with a `..` path component");
        assert!(errors[0].contains("workload arm `cat`"), "{errors:?}");
        assert!(pinned_errors(&argv("tac"), &read).is_empty());
        let shared = workload.replace(
            "case \"$PROGRAM\"",
            "LD_PRELOAD=../src/target/host.so\ncase \"$PROGRAM\"",
        );
        let read = |relative: &str| {
            assert_eq!(relative, REAL_COMPAT_WORKLOAD);
            Ok(shared.clone())
        };
        let errors = pinned_errors(&argv("tac"), &read);
        assert_one(&errors, "with a `..` path component");
        assert!(errors[0].contains("the code of"), "{errors:?}");
    }

    /// Round 7 of the review: a file runs from run state only below the
    /// fixture tree whose receipt test-harness verifies, and the rest of the
    /// pinned run-state tree is named only where nothing runs it.
    #[test]
    fn only_the_receipt_verified_fixture_tree_may_run_from_run_state() {
        assert_eq!(
            PINNED_FIXTURE_TREE,
            format!("{VALIDATE_RUN_STATE_PLACEHOLDER}/{PINNED_FIXTURES}")
        );
        assert!(PINNED_FIXTURE_TREE.starts_with(PINNED_FIXTURE_ROOT));
        // As the program, and as the file an interpreter runs.
        for direct in [
            "['{{VALIDATE_RUN_STATE}}/compat-pinned/host-program']",
            "['{{VALIDATE_RUN_STATE}}/compat-pinned/real-compat-fixtures-evil/x']",
            "['{{VALIDATE_RUN_STATE}}/compat-pinned/real-compat-fixtures']",
            "['{{VALIDATE_RUN_STATE}}/compat-pinned/real-compat-fixtures/']",
            "[bash, '{{VALIDATE_RUN_STATE}}/compat-pinned/host.sh']",
            "[env, HERMIT_COMPAT=1, /bin/bash, -e, '{{VALIDATE_RUN_STATE}}/compat-pinned/top-home/x.sh']",
        ] {
            assert_one(
                &pinned_errors(direct, &no_scripts),
                "outside {{VALIDATE_RUN_STATE}}/compat-pinned/real-compat-fixtures/, the only",
            );
        }
        // Anywhere else in the argv: shell text and its positional
        // parameters run it, and so does an operand of `timeout`.
        for direct in [
            "[bash, -c, '{{VALIDATE_RUN_STATE}}/compat-pinned/host-program']",
            "[bash, -c, 'source {{VALIDATE_RUN_STATE}}/compat-pinned/top-home/rc']",
            "[bash, -c, 'exec {{VALIDATE_RUN_STATE}}/compat-pinned/real-compat-fixtures-evil/x']",
            "[bash, -c, '\"$1\"', bash, '{{VALIDATE_RUN_STATE}}/compat-pinned/host-program']",
            "[/bin/cat, '{{VALIDATE_RUN_STATE}}/compat-pinned/shell-build/x']",
            "[bash, '{{ROOT_DIR}}/tests/x.sh', '{{VALIDATE_RUN_STATE}}/compat-pinned/shell-build']",
        ] {
            assert_one(
                &pinned_errors(
                    direct,
                    &|_: &str| Ok("#!/bin/bash\n/bin/cat x\n".to_owned()),
                ),
                "names run state under {{VALIDATE_RUN_STATE}}/compat-pinned/ outside",
            );
        }
        // The declared operand of its own script, the fixture tree anywhere,
        // and the variables that admit a pinned run-state path.
        let workload = |relative: &str| {
            assert_eq!(relative, "tests/compat/complex_shell_workload.sh");
            Ok("#!/bin/bash\n/bin/cat x\n".to_owned())
        };
        for direct in [
            "[bash, '{{ROOT_DIR}}/tests/compat/complex_shell_workload.sh', '{{VALIDATE_RUN_STATE}}/compat-pinned/shell-build']",
            "[bash, '{{ROOT_DIR}}/tests/compat/complex_shell_workload.sh', '{{VALIDATE_RUN_STATE}}/compat-pinned/shell-build/x']",
        ] {
            let errors = pinned_errors(direct, &workload);
            assert!(errors.is_empty(), "{direct}: {errors:?}");
        }
        assert_one(
            &pinned_errors(
                "[bash, '{{ROOT_DIR}}/tests/compat/complex_shell_workload.sh', '{{VALIDATE_RUN_STATE}}/compat-pinned/shell-build-evil']",
                &workload,
            ),
            "names run state under",
        );
        for direct in [
            "['{{VALIDATE_RUN_STATE}}/compat-pinned/real-compat-fixtures/bin/probe']",
            "[bash, '{{VALIDATE_RUN_STATE}}/compat-pinned/real-compat-fixtures/x.sh']",
            "[bash, -c, '/bin/cat {{VALIDATE_RUN_STATE}}/compat-pinned/real-compat-fixtures/README.md; /bin/cat x']",
            "[/bin/cat, '{{VALIDATE_RUN_STATE}}/compat-pinned/real-compat-fixtures']",
            "[env, 'HOME={{VALIDATE_RUN_STATE}}/compat-pinned/top-home', 'XDG_CONFIG_HOME={{VALIDATE_RUN_STATE}}/compat-pinned/top-home/.config', /bin/cat, x]",
        ] {
            let errors = pinned_errors(direct, &no_scripts);
            assert!(errors.is_empty(), "{direct}: {errors:?}");
        }
        // REAL_COMPAT_FIXTURES is exactly the tree, and only in the argv's
        // `env`, the command test-harness reads to decide whether to verify
        // the receipt.
        for value in [
            "{{VALIDATE_RUN_STATE}}/compat-pinned/top-home",
            "{{VALIDATE_RUN_STATE}}/compat-pinned/real-compat-fixtures-evil",
            "{{VALIDATE_RUN_STATE}}/compat-pinned/real-compat-fixtures/",
            "{{VALIDATE_RUN_STATE}}/compat-pinned/real-compat-fixtures/bin",
        ] {
            assert_one(
                &pinned_errors(
                    &format!("[env, 'REAL_COMPAT_FIXTURES={value}', /bin/cat, x]"),
                    &no_scripts,
                ),
                "which is not the receipt-verified fixture tree",
            );
        }
        let recipe = single_test(
            "compat-pinned",
            "guest_root: pinned",
            "[/bin/cat, x]",
            "        env: {REAL_COMPAT_FIXTURES: '{{VALIDATE_RUN_STATE}}/compat-pinned/real-compat-fixtures'}",
        );
        assert_one(
            &violations(&[recipe], &image(), &no_scripts),
            "which is set in a recipe",
        );
    }

    #[test]
    fn placeholders_and_relative_programs_are_refused_outside_their_rules() {
        assert_one(
            &pinned_errors(
                "[bash, -c, 'cd {{ROOT_DIR}}/target && ./probe']",
                &no_scripts,
            ),
            "inside a longer word",
        );
        assert_one(
            &pinned_errors(
                "[/bin/cat, '{{VALIDATE_RUN_STATE}}/strict-compat/x']",
                &no_scripts,
            ),
            "names run state outside",
        );
        assert!(
            pinned_errors(
                "['{{VALIDATE_RUN_STATE}}/compat-pinned/real-compat-fixtures/bin/probe']",
                &no_scripts
            )
            .is_empty()
        );
        assert_one(
            &pinned_errors("[/bin/cat, '{{HOME}}/x']", &no_scripts),
            "the placeholder {{HOME}}",
        );
        assert_one(
            &pinned_errors("[/bin/cat, '{{ROOT_DIR']", &no_scripts),
            "never closes",
        );
        assert_one(
            &pinned_errors("[./probe]", &no_scripts),
            "runs the relative path `./probe`",
        );
        assert_one(
            &pinned_errors("[bash, -e, x.sh]", &no_scripts),
            "runs the relative path `x.sh` through `bash`",
        );
    }

    #[test]
    fn a_parent_component_is_refused_before_any_prefix_exemption() {
        let escaped = "{{VALIDATE_RUN_STATE}}/compat-pinned/../strict-compat/real-compat-fixtures/gprof/program";
        // As the program, as an operand, and as the script a shell runs.
        assert_one(
            &pinned_errors(&format!("['{escaped}']"), &no_scripts),
            "has a `..` path component",
        );
        assert_one(
            &pinned_errors(&format!("[/bin/cat, '{escaped}']"), &no_scripts),
            "has a `..` path component",
        );
        assert_one(
            &pinned_errors(&format!("[bash, '{escaped}']"), &no_scripts),
            "has a `..` path component",
        );
        // A repository path walked out of the checkout is refused without
        // being read (`no_scripts` would add a second violation).
        for direct in [
            "['{{ROOT_DIR}}/../x.sh']",
            "[bash, '{{ROOT_DIR}}/../x.sh']",
            "[/bin/cat, '{{ROOT_DIR}}/ci/../../x.txt']",
        ] {
            assert_one(
                &pinned_errors(direct, &no_scripts),
                "has a `..` path component",
            );
        }
        // An image directory walked out of, as an operand and inside shell
        // text; the path check refuses the same path again.
        for direct in [
            "[/bin/cat, /bin/../opt/host/x]",
            "[bash, -c, 'cat /tmp/../opt/host/x']",
        ] {
            let errors = pinned_errors(direct, &no_scripts);
            assert!(
                errors.len() == 2
                    && errors[0].contains("has a `..` path component")
                    && errors[1].contains("names `/"),
                "{direct}: {errors:?}"
            );
        }
        // Inside shell text a component need not stand between `/` and
        // whitespace: after `=`, inside quotes and before `;` it still walks
        // out of the working directory, here the pinned root's /test, to the
        // host-mounted /src/target.
        for direct in [
            "[bash, -c, 'LD_PRELOAD=../src/target/host.so cat x']",
            "[bash, -c, 'BASH_ENV=../src/target/host.sh bash -c true']",
            "[bash, -c, \"cd '..' && src/target/host\"]",
            "[bash, -c, 'cd ..; src/target/host']",
        ] {
            assert_one(
                &pinned_errors(direct, &no_scripts),
                "has a `..` path component",
            );
        }
        // A range in shell, Perl or Ruby text is not a path component.
        assert!(
            pinned_errors(
                "[/bin/bash, -c, 'for i in {1..5}; do echo \"$((i * i))\"; done']",
                &no_scripts
            )
            .is_empty()
        );
        assert!(
            pinned_errors(
                "[/bin/bash, -c, '\"$1\" -e \"$2\"', bash, /bin/cat, 'for my $n (2..int(sqrt(100))) { print $n }']",
                &no_scripts
            )
            .is_empty()
        );
    }

    #[test]
    fn the_workload_code_an_arm_runs_is_checked_and_other_code_is_not() {
        let script = "#!/bin/bash\nfunction helper {\n    nested\n}\nfunction nested {\n    /opt/host/bin/nested-tool\n}\nfunction unused {\n    /opt/host/bin/unused-tool\n}\ncase \"$PROGRAM\" in\n    cat)\n        helper\n        /bin/cat x\n        ;;\n    tac)\n        /usr/bin/git --version\n        ;;\nesac\n";
        let read = |relative: &str| {
            assert_eq!(relative, REAL_COMPAT_WORKLOAD);
            Ok(script.to_owned())
        };
        let argv =
            |label: &str| format!("[bash, '{{{{ROOT_DIR}}}}/{REAL_COMPAT_WORKLOAD}', {label}]");
        assert_one(
            &pinned_errors(&argv("cat"), &read),
            "names `/opt/host/bin/nested-tool`",
        );
        assert!(pinned_errors(&argv("tac"), &read).is_empty());
        // Code outside the case block and the functions runs for every arm.
        let shared = script.replace(
            "case \"$PROGRAM\"",
            "/opt/host/bin/setup\ncase \"$PROGRAM\"",
        );
        let read = |relative: &str| {
            assert_eq!(relative, REAL_COMPAT_WORKLOAD);
            Ok(shared.clone())
        };
        assert_one(
            &pinned_errors(&argv("tac"), &read),
            "names `/opt/host/bin/setup`",
        );
    }

    #[test]
    fn absolute_paths_are_found_in_any_directory_but_not_after_expansions() {
        assert_eq!(
            absolute_paths(
                "/bin/a $W/bin/b ${W}/bin/c {{X}}/lib/d x=/usr/lib/e / /etc/hostname /usr/bin/f."
            ),
            vec!["/bin/a", "/usr/lib/e", "/", "/etc/hostname", "/usr/bin/f"]
        );
        // Any directory, a default expansion and a glued short option.
        assert_eq!(
            absolute_paths("'/opt/host/bin/bc' ${X:-/home/u/x} ${Y:+/srv/y} -I/usr/include"),
            vec!["/opt/host/bin/bc", "/home/u/x", "/srv/y", "/usr/include"]
        );
        // A quoted expansion continues past its closing quote; a quoted
        // literal does not; a URL's authority is not a path.
        assert_eq!(
            absolute_paths(
                r#"cat "$d"/part-* "${d}"/x "$HOME/a"/b "/opt/q"/r http://127.0.0.1:80/p"#
            ),
            vec!["/opt/q", "/r"]
        );
    }

    #[test]
    fn the_listing_must_describe_the_pinned_image() {
        let listing = image();
        assert!(listing.require_image("example@sha256:00").is_ok());
        let error = listing.require_image("example@sha256:11").unwrap_err();
        assert!(error.contains("regenerate"), "{error}");
        let tree = format!("tree: /s sha256:{TREE_SHA256}\n");
        let file = format!("file: /l /s/l sha256:{LIBC_SHA256}\n");
        let parse = |body: &str| ImagePaths::parse(&format!("image: x\n{tree}{file}{body}"));
        assert!(parse("/bin/a /s/a\n").is_ok());
        assert!(ImagePaths::parse(&format!("/bin/a /s/a\nimage: x\n{tree}{file}")).is_err());
        assert!(
            ImagePaths::parse(&format!("image: x\nimage: y\n{tree}{file}/bin/a /s/a\n")).is_err()
        );
        assert!(parse("/bin/a /s/a\n/bin/a /s/b\n").is_err());
        assert!(parse("bin/a /s/a\n").is_err());
        assert!(parse("/bin/a s/a\n").is_err());
        // A path without its target is the listing's old, unverifiable form.
        assert!(parse("/bin/a\n").is_err());
        assert!(parse("/bin/a /s/a /s/b\n").is_err());
        assert!(parse("").is_err());
        // The loader and libc bytes must be recorded, as 64 lowercase hex.
        assert!(ImagePaths::parse(&format!("image: x\n{tree}/bin/a /s/a\n")).is_err());
        assert!(
            ImagePaths::parse(&format!(
                "image: x\n{tree}file: /l /s/l sha256:00\n/bin/a /s/a\n"
            ))
            .is_err()
        );
        assert!(
            ImagePaths::parse(&format!(
                "image: x\n{tree}file: /l /s/l {LIBC_SHA256}\n/bin/a /s/a\n"
            ))
            .is_err()
        );
        assert!(parse(&format!("{file}/bin/a /s/a\n")).is_err());
        // Every target, of a path or of a recorded file, lies inside a
        // `tree:` directory, so the tree's digest covers its bytes.
        let error = ImagePaths::parse(&format!("image: x\n{file}/bin/a /s/a\n")).unwrap_err();
        assert!(error.contains("in no `tree:` directory"), "{error}");
        for body in ["/bin/a /t/a\n", "/bin/a /sx/a\n", "/bin/a /s\n"] {
            let error = parse(body).unwrap_err();
            assert!(error.contains("in no `tree:` directory"), "{body}: {error}");
        }
        let error = ImagePaths::parse(&format!(
            "image: x\n{tree}file: /l /t/l sha256:{LIBC_SHA256}\n/bin/a /s/a\n"
        ))
        .unwrap_err();
        assert!(error.contains("/t/l is in no `tree:` directory"), "{error}");
        assert!(parse(&format!("tree: /t sha256:{TREE_SHA256}\n/bin/a /t/a\n")).is_ok());
        // A `tree:` line names one absolute directory other than /, without a
        // trailing slash, once, with 64 lowercase hex digits.
        for line in [
            format!("tree: / sha256:{TREE_SHA256}"),
            format!("tree: /t/ sha256:{TREE_SHA256}"),
            format!("tree: t sha256:{TREE_SHA256}"),
            format!("tree: /t {TREE_SHA256}"),
            format!(
                "tree: /t sha256:{}",
                TREE_SHA256.to_uppercase().replace('2', "A")
            ),
            "tree: /t sha256:22".to_owned(),
            "tree: /t".to_owned(),
            format!("tree: /t sha256:{TREE_SHA256} extra"),
            format!("tree: /s sha256:{TREE_SHA256}"),
        ] {
            assert!(parse(&format!("{line}\n/bin/a /s/a\n")).is_err(), "{line}");
        }
        // `absent:` paths are absolute, unique and not also listed.
        assert!(parse("/bin/a /s/a\nabsent: /usr/local/bin/b\n").is_ok());
        assert!(parse("/bin/a /s/a\nabsent: usr/local/bin/b\n").is_err());
        assert!(parse("/bin/a /s/a\nabsent: /b\nabsent: /b\n").is_err());
        assert!(parse("/bin/a /s/a\nabsent: /bin/a\n").is_err());
        assert!(parse("/bin/a /s/a\nabsent: /l\n").is_err());
    }

    /// The shipped listing records the image's shell, `env`, loader and libc
    /// by their /nix/store targets, so the harness can tell the pinned root
    /// from another root by content.
    #[test]
    fn the_shipped_listing_records_store_targets_and_the_libc_bytes() {
        let listing = ImagePaths::load(&repo_root()).unwrap();
        for path in ["/bin/sh", "/usr/bin/env", "/bin/bash"] {
            assert!(
                listing.targets[path].starts_with("/nix/store/"),
                "{path} -> {}",
                listing.targets[path]
            );
        }
        assert!(
            listing
                .targets
                .values()
                .all(|target| target.starts_with("/nix/store/"))
        );
        let loader = &listing.files["/lib64/ld-linux-x86-64.so.2"];
        assert!(loader.target.starts_with("/nix/store/") && loader.target.contains("-glibc-2.42"));
        let libc = Path::new(&loader.target).with_file_name("libc.so.6");
        assert!(
            listing.files.contains_key(libc.to_str().unwrap()),
            "{:?}",
            listing.files
        );
        assert_eq!(
            listing.trees.keys().collect::<Vec<_>>(),
            vec!["/nix/store"],
            "the listing records the content digest of the image's whole store"
        );
        let identity = listing.identity();
        assert!(
            identity
                .strip_prefix("sha256:")
                .is_some_and(is_lower_hex_sha256),
            "{identity}"
        );
    }

    /// The shipped listing covers every directory a guest searches for a
    /// bare program and every loader preload file: each `PATH` directory
    /// either holds listed executables, which `verify_root` requires to be
    /// the only executables there, or is recorded absent, and each preload
    /// file is recorded absent. The directories are read back from the
    /// `PATH` that Hermit's minimal base environment sets, so a change there
    /// cannot leave a searched directory unchecked.
    #[test]
    fn the_shipped_listing_covers_the_guest_path_and_the_loader_preload_files() {
        let run =
            std::fs::read_to_string(repo_root().join("hermit-cli/src/bin/hermit/run.rs")).unwrap();
        let function = run
            .split("fn apply_base_and_explicit_environment(")
            .nth(1)
            .unwrap();
        let minimal = function.split("BaseEnv::Minimal =>").nth(1).unwrap();
        let minimal = minimal.split("BaseEnv::Host").next().unwrap();
        let path = minimal
            .split("\"PATH\",")
            .nth(1)
            .and_then(|rest| rest.split('"').nth(1))
            .unwrap();
        assert_eq!(path, GUEST_PATH_DIRECTORIES.join(":"));
        let listing = ImagePaths::load(&repo_root()).unwrap();
        let listed: BTreeSet<&str> = listing
            .targets
            .keys()
            .filter_map(|path| path.rsplit_once('/').map(|(directory, _)| directory))
            .collect();
        for directory in GUEST_PATH_DIRECTORIES {
            assert!(
                listed.contains(directory) != listing.records_absent(directory),
                "{directory} must hold listed executables or be recorded absent, not both or \
                 neither"
            );
        }
        for file in LOADER_PRELOAD_FILES {
            assert!(listing.records_absent(file), "{file}");
        }
    }

    /// `identity` covers the image reference, every target and every
    /// recorded byte digest.
    #[test]
    fn the_identity_changes_with_any_recorded_entry() {
        let base = image().identity();
        assert_eq!(base, image().identity());
        let mut other = image();
        other.image = "example@sha256:01".to_owned();
        assert_ne!(other.identity(), base);
        let mut other = image();
        other
            .targets
            .insert("/bin/cat".to_owned(), "/s/other-cat".to_owned());
        assert_ne!(other.identity(), base);
        let mut other = image();
        other.files.get_mut("/lib64/ld.so").unwrap().sha256 = "1".repeat(64);
        assert_ne!(other.identity(), base);
        let mut other = image();
        other.absent.clear();
        assert_ne!(other.identity(), base);
        let mut other = image();
        other.trees.insert("/s".to_owned(), "3".repeat(64));
        assert_ne!(other.identity(), base);
        let mut other = image();
        other.trees.insert("/t".to_owned(), TREE_SHA256.to_owned());
        assert_ne!(other.identity(), base);
    }

    /// `verify_root` resolves the recorded paths on this filesystem and
    /// hashes the recorded files: a root whose links point at other store
    /// paths (another Nix image) or whose libc has other bytes is refused.
    /// So is a root whose store holds other bytes, executable bits or
    /// entries than the recorded tree digest, even at a recorded program's
    /// own path, a root with an unlisted executable in a listed directory, an
    /// `absent:` path that exists, a mount over a directory of the image, or
    /// recorded directories on two filesystems.
    #[test]
    fn verify_root_compares_targets_and_bytes_on_the_filesystem() {
        let scratch = std::env::temp_dir().join(format!(
            "compat-guest-root-verify-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&scratch);
        let store = scratch.join("store");
        let bin = scratch.join("bin");
        std::fs::create_dir_all(&store).unwrap();
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(store.join("bash"), "bash").unwrap();
        std::fs::write(store.join("other-bash"), "bash").unwrap();
        std::fs::write(store.join("libc.so.6"), "libc").unwrap();
        std::os::unix::fs::symlink(store.join("bash"), bin.join("sh")).unwrap();
        std::os::unix::fs::symlink(store.join("libc.so.6"), scratch.join("libc")).unwrap();
        let show = |path: PathBuf| path.display().to_string();
        let probe = scratch.join("probe");
        let tree = tree_digest(&store).unwrap();
        let listing = |sha256: &str| {
            ImagePaths::parse(&format!(
                "image: x\ntree: {} sha256:{tree}\nfile: {} {} sha256:{sha256}\n{} {}\nabsent: {}\n",
                show(store.clone()),
                show(scratch.join("libc")),
                show(store.join("libc.so.6")),
                show(bin.join("sh")),
                show(store.join("bash")),
                show(probe.clone()),
            ))
            .unwrap()
        };
        let libc_sha256 = format!("{:x}", Sha256::digest(b"libc"));
        assert_eq!(listing(&libc_sha256).verify_root(), Ok(()));
        let error = listing(&"1".repeat(64)).verify_root().unwrap_err();
        assert!(error.contains("has sha256"), "{error}");
        // The recorded program's path still resolves to its recorded store
        // file, but the file holds other bytes; or a file of the store gains
        // an execute bit; or the store gains a file.
        use std::os::unix::fs::PermissionsExt;
        let refused_then_restored = |change: &dyn Fn(), restore: &dyn Fn()| {
            change();
            let error = listing(&libc_sha256).verify_root().unwrap_err();
            assert!(error.contains("content digest"), "{error}");
            restore();
            assert_eq!(listing(&libc_sha256).verify_root(), Ok(()));
        };
        refused_then_restored(
            &|| std::fs::write(store.join("bash"), "host bash").unwrap(),
            &|| std::fs::write(store.join("bash"), "bash").unwrap(),
        );
        let mode = |path: PathBuf, mode: u32| {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap()
        };
        refused_then_restored(&|| mode(store.join("libc.so.6"), 0o755), &|| {
            mode(store.join("libc.so.6"), 0o644)
        });
        refused_then_restored(
            &|| std::fs::write(store.join("libpreload.so"), "x").unwrap(),
            &|| std::fs::remove_file(store.join("libpreload.so")).unwrap(),
        );
        // A listed directory holds only listed executables; dot names,
        // non-executable files and dangling links are not listed.
        let executable = |path: PathBuf| {
            std::fs::write(&path, "x").unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        };
        executable(bin.join(".hidden"));
        std::fs::write(bin.join("data"), "x").unwrap();
        std::os::unix::fs::symlink(scratch.join("gone"), bin.join("dangling")).unwrap();
        assert_eq!(listing(&libc_sha256).verify_root(), Ok(()));
        executable(bin.join("extra"));
        let error = listing(&libc_sha256).verify_root().unwrap_err();
        assert!(error.contains("extra is an executable"), "{error}");
        std::fs::remove_file(bin.join("extra")).unwrap();
        // An `absent:` path that exists, even as a dangling link, is refused.
        std::os::unix::fs::symlink(scratch.join("gone"), &probe).unwrap();
        let error = listing(&libc_sha256).verify_root().unwrap_err();
        assert!(error.contains("probe exists"), "{error}");
        std::fs::remove_file(&probe).unwrap();
        // So is one that exists as a directory, such as a guest `PATH`
        // directory the image lacks holding a program searched before the
        // image's.
        std::fs::create_dir(&probe).unwrap();
        executable(probe.join("bash"));
        let error = listing(&libc_sha256).verify_root().unwrap_err();
        assert!(error.contains("probe exists"), "{error}");
        std::fs::remove_dir_all(&probe).unwrap();
        assert_eq!(listing(&libc_sha256).verify_root(), Ok(()));
        // A mount at or below a directory of a recorded path or target is
        // refused; a mount elsewhere, the root mount included, is not.
        let mount = |point: &str| format!("36 35 98:0 / {point} rw - ext4 /dev/x rw\n");
        let parent = show(scratch.parent().unwrap().to_path_buf());
        let unrelated = format!("{}{}", mount("/"), mount(&parent));
        assert_eq!(
            listing(&libc_sha256).verify_root_with_mounts(&unrelated),
            Ok(())
        );
        for point in [show(bin.clone()), show(store.join("bash"))] {
            let error = listing(&libc_sha256)
                .verify_root_with_mounts(&mount(&point))
                .unwrap_err();
            assert!(error.contains("is mounted over"), "{error}");
        }
        // The kernel escapes a space in a mount point as \040.
        assert_eq!(
            mount_points(&mount("/a\\040b")),
            Ok(vec!["/a b".to_owned()])
        );
        // A recorded target on another filesystem: /proc is procfs.
        std::os::unix::fs::symlink("/proc/version", bin.join("version")).unwrap();
        let other_filesystem = ImagePaths::parse(&format!(
            "image: x\ntree: {} sha256:{tree}\ntree: /proc sha256:{TREE_SHA256}\nfile: {} {} \
             sha256:{libc_sha256}\n{} {}\n{} /proc/version\n",
            show(store.clone()),
            show(scratch.join("libc")),
            show(store.join("libc.so.6")),
            show(bin.join("sh")),
            show(store.join("bash")),
            show(bin.join("version")),
        ))
        .unwrap();
        let error = other_filesystem.verify_root().unwrap_err();
        assert!(error.contains("different filesystems"), "{error}");
        std::fs::remove_file(bin.join("version")).unwrap();
        std::fs::remove_file(bin.join("sh")).unwrap();
        std::os::unix::fs::symlink(store.join("other-bash"), bin.join("sh")).unwrap();
        let error = listing(&libc_sha256).verify_root().unwrap_err();
        assert!(error.contains("other-bash"), "{error}");
        std::fs::remove_file(bin.join("sh")).unwrap();
        let error = listing(&libc_sha256).verify_root().unwrap_err();
        assert!(error.contains("does not resolve"), "{error}");
        std::fs::remove_dir_all(&scratch).unwrap();
    }

    /// `verify_read_only` asks about the directory of every recorded path
    /// and target and the nearest existing ancestor of every `absent:` path,
    /// and refuses the first that is on a writable filesystem, naming the
    /// wrapper flag that mounts the root read-only. A test's temporary
    /// directory is writable, so the real `statvfs` check refuses it.
    #[test]
    fn verify_read_only_refuses_an_image_directory_on_a_writable_filesystem() {
        let scratch = std::env::temp_dir().join(format!(
            "compat-guest-root-read-only-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&scratch);
        let store = scratch.join("store");
        let bin = scratch.join("bin");
        let etc = scratch.join("etc");
        for directory in [&store, &bin, &etc] {
            std::fs::create_dir_all(directory).unwrap();
        }
        std::fs::write(store.join("bash"), "bash").unwrap();
        std::fs::write(store.join("libc.so.6"), "libc").unwrap();
        std::os::unix::fs::symlink(store.join("bash"), bin.join("sh")).unwrap();
        std::os::unix::fs::symlink(store.join("libc.so.6"), scratch.join("libc")).unwrap();
        let show = |path: &Path| path.display().to_string();
        let canonical = |path: &Path| std::fs::canonicalize(path).unwrap();
        let listing = ImagePaths::parse(&format!(
            "image: x\ntree: {} sha256:{}\nfile: {} {} sha256:{}\n{} {}\nabsent: {}\n",
            show(&store),
            tree_digest(&store).unwrap(),
            show(&scratch.join("libc")),
            show(&store.join("libc.so.6")),
            format_args!("{:x}", Sha256::digest(b"libc")),
            show(&bin.join("sh")),
            show(&store.join("bash")),
            show(&etc.join("missing/ld.so.preload")),
        ))
        .unwrap();
        let error = listing.verify_read_only().unwrap_err();
        assert!(error.contains("is on a writable filesystem"), "{error}");
        assert!(
            error.contains("run-in-pinned-root.sh --read-only-root"),
            "{error}"
        );
        let asked = std::cell::RefCell::new(BTreeSet::new());
        let all_read_only = |directory: &Path| {
            asked.borrow_mut().insert(directory.to_path_buf());
            Ok::<bool, String>(true)
        };
        assert_eq!(listing.verify_read_only_with(&all_read_only), Ok(()));
        assert_eq!(
            asked.into_inner(),
            BTreeSet::from([
                canonical(&scratch),
                canonical(&store),
                canonical(&bin),
                canonical(&etc)
            ])
        );
        // Only the ancestor of the absent preload file is writable: a guest
        // could create it.
        let etc_writable = |directory: &Path| Ok::<bool, String>(directory != canonical(&etc));
        let error = listing.verify_read_only_with(&etc_writable).unwrap_err();
        assert!(
            error.starts_with(&format!(
                "{} is on a writable filesystem",
                show(&canonical(&etc))
            )),
            "{error}"
        );
        let unreadable = |_: &Path| Err::<bool, String>("no statvfs".to_owned());
        assert_eq!(
            listing.verify_read_only_with(&unreadable),
            Err("no statvfs".to_owned())
        );
        assert_eq!(statvfs_read_only(&scratch), Ok(false));
        let error = statvfs_read_only(&scratch.join("gone")).unwrap_err();
        assert!(error.starts_with("cannot statvfs"), "{error}");
        std::fs::remove_dir_all(&scratch).unwrap();
    }

    /// `tree_digest` is the SHA-256 of the sorted lines the listing header's
    /// `tree:` generator prints, and it changes with any file's bytes or
    /// execute bit, any link's target and any added entry, but not with an
    /// empty directory. An entry the line format cannot show is refused.
    #[test]
    fn the_tree_digest_covers_bytes_execute_bits_links_and_entries() {
        use std::os::unix::fs::PermissionsExt;
        let scratch = std::env::temp_dir().join(format!(
            "compat-guest-root-tree-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&scratch);
        let tree = scratch.join("t");
        std::fs::create_dir_all(tree.join("d/e")).unwrap();
        std::fs::write(tree.join("a"), "a").unwrap();
        std::fs::write(tree.join("d/b"), "b").unwrap();
        let mode = |path: PathBuf, mode: u32| {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap()
        };
        mode(tree.join("a"), 0o644);
        mode(tree.join("d/b"), 0o755);
        std::os::unix::fs::symlink("../a", tree.join("d/e/l")).unwrap();
        let show = |relative: &str| tree.join(relative).display().to_string();
        let sha256 = |bytes: &[u8]| format!("{:x}", Sha256::digest(bytes));
        let lines = format!(
            "{}\tf\t{}\n{}\tx\t{}\n{}\tl\t../a\n",
            show("a"),
            sha256(b"a"),
            show("d/b"),
            sha256(b"b"),
            show("d/e/l"),
        );
        let base = tree_digest(&tree).unwrap();
        assert_eq!(base, sha256(lines.as_bytes()));
        std::fs::create_dir(tree.join("empty")).unwrap();
        assert_eq!(tree_digest(&tree).unwrap(), base);
        let changes_then_restores = |change: &dyn Fn(), restore: &dyn Fn()| {
            change();
            assert_ne!(tree_digest(&tree).unwrap(), base);
            restore();
            assert_eq!(tree_digest(&tree).unwrap(), base);
        };
        changes_then_restores(&|| std::fs::write(tree.join("a"), "A").unwrap(), &|| {
            std::fs::write(tree.join("a"), "a").unwrap()
        });
        changes_then_restores(&|| mode(tree.join("a"), 0o744), &|| {
            mode(tree.join("a"), 0o644)
        });
        changes_then_restores(&|| mode(tree.join("d/b"), 0o644), &|| {
            mode(tree.join("d/b"), 0o755)
        });
        let relink = |target: &str| {
            std::fs::remove_file(tree.join("d/e/l")).unwrap();
            std::os::unix::fs::symlink(target, tree.join("d/e/l")).unwrap();
        };
        changes_then_restores(&|| relink("../b"), &|| relink("../a"));
        changes_then_restores(
            &|| std::fs::write(tree.join("empty/new"), "").unwrap(),
            &|| std::fs::remove_file(tree.join("empty/new")).unwrap(),
        );
        // A socket, a tab in a name, and a newline in a link target.
        let socket = std::os::unix::net::UnixListener::bind(tree.join("empty/socket")).unwrap();
        let error = tree_digest(&tree).unwrap_err();
        assert!(error.contains("not a regular file"), "{error}");
        drop(socket);
        std::fs::remove_file(tree.join("empty/socket")).unwrap();
        std::fs::write(tree.join("empty/a\tb"), "").unwrap();
        let error = tree_digest(&tree).unwrap_err();
        assert!(error.contains("tab or a newline"), "{error}");
        std::fs::remove_file(tree.join("empty/a\tb")).unwrap();
        std::os::unix::fs::symlink("x\ny", tree.join("empty/l")).unwrap();
        let error = tree_digest(&tree).unwrap_err();
        assert!(error.contains("newline in its target"), "{error}");
        std::fs::remove_file(tree.join("empty/l")).unwrap();
        assert_eq!(tree_digest(&tree).unwrap(), base);
        assert!(tree_digest(&scratch.join("missing")).is_err());
        std::fs::remove_dir_all(&scratch).unwrap();
    }

    /// A fixture receipt vouches for a tree only when it names the pinned
    /// image, the image's gcc and this checkout's sources, and lists exactly
    /// the tree's files and links, each link pointing into the image. Each
    /// mutation below is refused, and undoing it is accepted again.
    #[test]
    fn a_fixture_receipt_vouches_only_for_the_pinned_build_of_this_checkout() {
        use std::os::unix::fs::PermissionsExt;
        let scratch = std::env::temp_dir().join(format!(
            "compat-guest-root-receipt-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&scratch);
        let repo = scratch.join("repo");
        for path in FIXTURE_RECEIPT_SOURCES {
            let path = repo.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, path.file_name().unwrap().as_encoded_bytes()).unwrap();
        }
        let fixtures = scratch.join("state").join(PINNED_FIXTURES);
        std::fs::create_dir_all(fixtures.join("toolchain")).unwrap();
        std::fs::create_dir_all(fixtures.join("empty")).unwrap();
        std::fs::write(fixtures.join("data"), "d").unwrap();
        std::fs::write(fixtures.join("program"), "elf").unwrap();
        let mode = |relative: &str, mode: u32| {
            std::fs::set_permissions(
                fixtures.join(relative),
                std::fs::Permissions::from_mode(mode),
            )
            .unwrap()
        };
        mode("data", 0o644);
        mode("program", 0o755);
        let relink = |target: &str| {
            let _ = std::fs::remove_file(fixtures.join("toolchain/cargo"));
            std::os::unix::fs::symlink(target, fixtures.join("toolchain/cargo")).unwrap();
        };
        relink("/bin/cargo");
        let image = ImagePaths::parse(&format!(
            "image: example@sha256:00\ntree: /s sha256:{TREE_SHA256}\nfile: /lib64/ld.so \
             /s/glibc/lib/ld.so sha256:{LIBC_SHA256}\n/bin/gcc /s/gcc/bin/gcc\n\
             /bin/cargo /s/rust/bin/cargo\n"
        ))
        .unwrap();
        let sha256 = |bytes: &[u8]| format!("{:x}", Sha256::digest(bytes));
        let source = |path: &str| {
            let name = path.rsplit('/').next().unwrap();
            format!("source {} {path}\n", sha256(name.as_bytes()))
        };
        let sources: String = FIXTURE_RECEIPT_SOURCES
            .iter()
            .map(|path| source(path))
            .collect();
        let valid = format!(
            "{FIXTURE_RECEIPT_HEADER}\nimage example@sha256:00\ncompiler /s/gcc/bin/gcc\n\
             {sources}l\ttoolchain/cargo\t/bin/cargo\nf\tdata\t{}\nx\tprogram\t{}\n",
            sha256(b"d"),
            sha256(b"elf"),
        );
        let receipt = fixture_receipt_path(&fixtures);
        assert_eq!(
            receipt,
            scratch.join("state/compat-pinned/real-compat-fixtures.receipt")
        );
        let verify = || verify_fixture_receipt(&image, &repo, &fixtures);
        let refused = |expected: &str| {
            let error = verify().unwrap_err();
            assert!(error.contains(expected), "{expected:?} not in {error}");
        };

        let error = verify().unwrap_err();
        assert!(error.contains("has no receipt"), "{error}");
        std::fs::write(&receipt, &valid).unwrap();
        verify().unwrap();

        let readme = source("README.md");
        let receipt_mutations: &[(&str, String, &str)] = &[
            (
                FIXTURE_RECEIPT_HEADER,
                "hermit-compat-fixture-receipt 2".into(),
                "does not start with",
            ),
            (
                "image example@sha256:00\n",
                "image example@sha256:11\n".into(),
                "not in the pinned image",
            ),
            ("image example@sha256:00\n", String::new(), "no image line"),
            (
                "image example@sha256:00\n",
                "image example@sha256:00\n".repeat(2),
                "twice",
            ),
            (
                "compiler /s/gcc/bin/gcc",
                "compiler /usr/bin/gcc".into(),
                "compiled by /usr/bin/gcc",
            ),
            (
                "compiler /s/gcc/bin/gcc\n",
                String::new(),
                "no compiler line",
            ),
            (
                &readme,
                readme.replace(&sha256(b"README.md"), &sha256(b"other")),
                "from this checkout",
            ),
            (&readme, String::new(), "records the sources"),
            (
                &readme,
                format!("{readme}source {} extra.c\n", sha256(b"x")),
                "records the sources",
            ),
            (&readme, readme.repeat(2), "twice"),
            ("x\tprogram", "f\tprogram".into(), "program is x"),
            ("x\tprogram\t", "x\tprogram\tnot-hex".into(), "unreadable"),
            ("f\tdata", "f\tdata\tx\nf\tdata".into(), "unreadable"),
            (
                "l\ttoolchain/cargo\t/bin/cargo\n",
                String::new(),
                "toolchain/cargo is not in the receipt",
            ),
            (
                "l\ttoolchain/cargo\t/bin/cargo\n",
                "l\ttoolchain/cargo\t/bin/rustc\n".into(),
                "not the receipt's l /bin/rustc",
            ),
        ];
        for (from, to, expected) in receipt_mutations {
            assert!(valid.contains(from), "{from:?}");
            std::fs::write(&receipt, valid.replacen(from, to, 1)).unwrap();
            refused(expected);
        }
        std::fs::write(&receipt, format!("{valid}f\tdata\t{}\n", sha256(b"d"))).unwrap();
        refused("records the entry data twice");
        std::fs::write(&receipt, format!("{valid}f\tgone\t{}\n", sha256(b"g"))).unwrap();
        refused("gone is missing");
        std::fs::write(&receipt, &valid).unwrap();
        verify().unwrap();

        // The tree changes under a receipt that does not.
        std::fs::write(fixtures.join("data"), "D").unwrap();
        refused("data is f");
        std::fs::write(fixtures.join("data"), "d").unwrap();
        mode("program", 0o644);
        refused("program is f");
        mode("program", 0o755);
        std::fs::write(fixtures.join("empty/new"), "").unwrap();
        refused("empty/new is not in the receipt");
        std::fs::remove_file(fixtures.join("empty/new")).unwrap();
        std::fs::remove_file(fixtures.join("data")).unwrap();
        refused("data is missing");
        std::fs::write(fixtures.join("data"), "d").unwrap();
        mode("data", 0o644);
        verify().unwrap();

        // The checkout's sources change after the build.
        std::fs::write(repo.join("tests/compat/localhost_http_server.c"), "changed").unwrap();
        refused("from this checkout");
        std::fs::write(
            repo.join("tests/compat/localhost_http_server.c"),
            "localhost_http_server.c",
        )
        .unwrap();
        verify().unwrap();

        // A link the receipt records faithfully, but that leaves the image.
        for target in ["/usr/bin/cargo", "../program", "/s/../etc/passwd"] {
            relink(target);
            std::fs::write(
                &receipt,
                valid.replace("\t/bin/cargo\n", &format!("\t{target}\n")),
            )
            .unwrap();
            refused("not a path the pinned image records");
        }
        relink("/s/rust/bin/cargo");
        std::fs::write(
            &receipt,
            valid.replace("\t/bin/cargo\n", "\t/s/rust/bin/cargo\n"),
        )
        .unwrap();
        verify().unwrap();
        relink("/bin/cargo");
        std::fs::write(&receipt, &valid).unwrap();
        verify().unwrap();

        assert!(names_pinned_fixtures(
            "{{VALIDATE_RUN_STATE}}/compat-pinned/real-compat-fixtures/gprof/program"
        ));
        assert!(!names_pinned_fixtures(
            "{{VALIDATE_RUN_STATE}}/strict-compat/real-compat-fixtures/gprof/program"
        ));
        assert!(!names_pinned_fixtures(
            "{{VALIDATE_RUN_STATE}}/compat-pinned/other"
        ));
        std::fs::remove_dir_all(&scratch).unwrap();
    }
}
