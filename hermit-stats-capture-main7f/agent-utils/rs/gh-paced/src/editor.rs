//! The editor guard: the content check for text that gh composes in an editor.
//!
//! An interactive `gh issue create` or `gh pr merge` asks its questions on the terminal and opens
//! an editor for the body or the merge commit message. That text never appears in gh's
//! arguments, so the argument-based content guard cannot see it. For every WRITE call gh-paced
//! therefore points gh's editor setting (`GH_EDITOR`, which gh prefers over everything else) at
//! itself: gh runs `gh-paced --edit-guard ACCOUNT FILE`, which runs the editor the user would
//! have had, waits for it, and then checks the saved file with the same size and base64 limits
//! as any other write body. If the text fails, the guard keeps a copy, explains why, and exits
//! non-zero, so gh abandons the command before it sends anything.

use crate::config::Config;
use crate::guard::{self, Verdict};
use std::path::Path;
use std::process::Command;

/// The argument that selects the editor guard: `gh-paced --edit-guard ACCOUNT FILE...`.
pub const EDIT_GUARD_FLAG: &str = "--edit-guard";

/// Environment variable carrying the editor the user would have had without gh-paced.
pub const ORIGINAL_EDITOR_VAR: &str = "GH_PACED_EDITOR";

/// Environment variable carrying the body bytes the gh call already gave on its command line
/// (set by the wrapper for every WRITE call), so the editor text is checked against what is
/// left of the same `max_body_bytes` allowance. Unset counts as 0; a value that is not a byte
/// count refuses the text.
pub const PRIOR_BYTES_VAR: &str = "GH_PACED_BODY_BYTES_USED";

/// The editor gh would use, in gh's order: `GH_EDITOR`, the `editor` key of gh's
/// configuration file, `GIT_EDITOR`, `VISUAL`, `EDITOR`, then `nano`.
pub fn gh_editor(env: &dyn Fn(&str) -> Option<String>) -> String {
    let set = |k: &str| env(k).filter(|v| !v.trim().is_empty());
    if let Some(e) = set("GH_EDITOR") {
        return e;
    }
    let configured = crate::alias::config_file(env)
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|t| crate::alias::top_level_value(&t, "editor"))
        .filter(|v| !v.trim().is_empty());
    if let Some(e) = configured {
        return e;
    }
    for k in ["GIT_EDITOR", "VISUAL", "EDITOR"] {
        if let Some(e) = set(k) {
            return e;
        }
    }
    "nano".to_string()
}

/// Quote `s` for gh's editor-command splitting (POSIX single quotes).
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// The variables that route gh's editor through the guard, for the child of a WRITE call.
///
/// When this gh-paced runs inside another one's gh (an alias or extension calling gh again), the
/// outer guard is already installed; its settings are passed on unchanged so the guard never
/// ends up running itself. Without its own executable path gh-paced cannot be the editor, so the
/// editor is replaced by a command that refuses: the text could not be checked.
pub fn guard_env(
    env: &dyn Fn(&str) -> Option<String>,
    self_exe: Option<&Path>,
    account: &str,
) -> Vec<(String, String)> {
    if let (Some(current), Some(original)) = (env("GH_EDITOR"), env(ORIGINAL_EDITOR_VAR)) {
        if current.contains(EDIT_GUARD_FLAG) {
            return vec![
                ("GH_EDITOR".to_string(), current),
                (ORIGINAL_EDITOR_VAR.to_string(), original),
            ];
        }
    }
    let editor = match self_exe.and_then(Path::to_str) {
        Some(exe) => format!("{} {EDIT_GUARD_FLAG} {}", sh_quote(exe), sh_quote(account)),
        None => "sh -c 'echo \"GH-PACED REFUSED gh-paced cannot find its own executable, so \
                 it cannot check text written in an editor (exit 65)\" >&2; exit 65'"
            .to_string(),
    };
    vec![
        ("GH_EDITOR".to_string(), editor),
        (ORIGINAL_EDITOR_VAR.to_string(), gh_editor(env)),
    ]
}

fn refuse(account: &str, lines: &[String]) {
    let rule = format!("GH-PACED {}", "*".repeat(68));
    eprintln!("{rule}");
    for l in lines {
        eprintln!("GH-PACED REFUSED [{account}] {l}");
    }
    eprintln!("{rule}");
}

/// Keep a private copy of refused text in the state directory, since gh deletes its file.
fn keep_copy(env: &dyn Fn(&str) -> Option<String>, account: &str, text: &[u8]) -> Option<String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let dir = crate::state::state_dir(env).ok()?;
    std::fs::create_dir_all(&dir).ok()?;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let path = dir.join(format!("{account}.refused-edit.{nanos}.md"));
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .ok()?;
    f.write_all(text).ok()?;
    Some(path.display().to_string())
}

/// Most bytes of refused editor text kept in the state directory (1 MiB). The guard reads at
/// most this much more than the content limit, however large the saved file is.
pub const MAX_KEPT_EDIT_BYTES: u64 = 1 << 20;

/// Read at most [`MAX_KEPT_EDIT_BYTES`] of `path`; the flag is true when the file is longer.
fn kept_text(path: &str) -> Option<(Vec<u8>, bool)> {
    use std::io::Read;
    let mut text = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(MAX_KEPT_EDIT_BYTES + 1)
        .read_to_end(&mut text)
        .ok()?;
    let longer = text.len() as u64 > MAX_KEPT_EDIT_BYTES;
    text.truncate(MAX_KEPT_EDIT_BYTES as usize);
    Some((text, longer))
}

/// `gh-paced --edit-guard ACCOUNT FILE...`: run the original editor on the files gh passed, then
/// check what was saved. Returns the exit status for gh: the editor's own failure, 65 when the
/// text is refused, 0 when gh may use it.
pub fn run_guard(args: &[String], env: &dyn Fn(&str) -> Option<String>, cfg: &Config) -> i32 {
    let (account, files) = match args.split_first() {
        Some((a, f)) if !f.is_empty() => (a.as_str(), f),
        _ => {
            eprintln!(
                "GH-PACED ERROR {EDIT_GUARD_FLAG} is run by gh as its editor: \
                 gh-paced {EDIT_GUARD_FLAG} ACCOUNT FILE..."
            );
            return crate::wrapper::EXIT_USAGE;
        }
    };
    let editor = env(ORIGINAL_EDITOR_VAR)
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "nano".to_string());
    let words = match crate::alias::split(&editor) {
        Some(w) if !w.is_empty() => w,
        _ => {
            refuse(
                account,
                &[format!("cannot read the editor command {editor:?}")],
            );
            return crate::cli::EXIT_CONFIG;
        }
    };
    let prior = match env(PRIOR_BYTES_VAR) {
        None => 0,
        Some(v) => match v.trim().parse::<u64>() {
            Ok(n) => n,
            Err(_) => {
                refuse(
                    account,
                    &[format!(
                        "{PRIOR_BYTES_VAR}={v:?} is not a byte count, so the editor text cannot \
                         be checked against what is left of the write limit"
                    )],
                );
                return crate::cli::EXIT_CONFIG;
            }
        },
    };
    if words.iter().any(|w| w == EDIT_GUARD_FLAG) {
        refuse(
            account,
            &[format!(
                "the editor command {editor:?} is the guard itself; set GH_EDITOR, VISUAL or EDITOR to a real editor"
            )],
        );
        return crate::cli::EXIT_CONFIG;
    }
    match Command::new(&words[0])
        .args(&words[1..])
        .args(files)
        .status()
    {
        Err(e) => {
            eprintln!("GH-PACED ERROR cannot run the editor {:?}: {e}", words[0]);
            return crate::wrapper::EXIT_NO_GH;
        }
        Ok(s) if !s.success() => {
            use std::os::unix::process::ExitStatusExt;
            return s.code().unwrap_or_else(|| 128 + s.signal().unwrap_or(0));
        }
        Ok(_) => {}
    }
    if cfg.allow_large_body {
        return 0;
    }
    let Verdict::Refuse(reason) = guard::check_composed_files(files, cfg, prior) else {
        return 0;
    };
    let mut lines = vec![
        format!("content guard: {reason}"),
        format!(
            "the text saved in the editor was not used, and gh stops here (exit {})",
            crate::wrapper::EXIT_CONTENT
        ),
    ];
    for f in files {
        match kept_text(f).and_then(|(t, longer)| Some((keep_copy(env, account, &t)?, longer))) {
            Some((p, false)) => lines.push(format!("your text is kept in {p}")),
            Some((p, true)) => lines.push(format!(
                "the first {MAX_KEPT_EDIT_BYTES} bytes of your text are kept in {p}; the rest \
                 was not copied, and gh deletes its file"
            )),
            None => lines.push("your text could not be kept: gh deletes its file".to_string()),
        }
    }
    lines.push(
        "GitHub text is for short human notes. Keep evidence on the host and post a pointer \
         (path + sha256, a tracked file, or a commit)."
            .to_string(),
    );
    refuse(account, &lines);
    crate::wrapper::EXIT_CONTENT
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let m: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k: &str| m.get(k).cloned()
    }

    /// Refused text is copied with a bound: a huge saved file is never read whole.
    #[test]
    fn refused_text_is_kept_up_to_a_bound() {
        let dir = std::env::temp_dir().join(format!("gh-paced-kept-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let small = dir.join("small.md");
        std::fs::write(&small, b"short note\n").unwrap();
        assert_eq!(
            kept_text(small.to_str().unwrap()),
            Some((b"short note\n".to_vec(), false))
        );
        let big = dir.join("big.md");
        let f = std::fs::File::create(&big).unwrap();
        // A sparse 4 GiB file: reading it whole would take 4 GiB of memory.
        f.set_len(4 << 30).unwrap();
        let (text, longer) = kept_text(big.to_str().unwrap()).unwrap();
        assert_eq!(text.len() as u64, MAX_KEPT_EDIT_BYTES);
        assert!(longer);
        let exact = dir.join("exact.md");
        std::fs::write(&exact, vec![b'x'; MAX_KEPT_EDIT_BYTES as usize]).unwrap();
        let (text, longer) = kept_text(exact.to_str().unwrap()).unwrap();
        assert_eq!(text.len() as u64, MAX_KEPT_EDIT_BYTES);
        assert!(!longer);
        assert_eq!(kept_text(dir.join("missing").to_str().unwrap()), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_editor_is_chosen_in_ghs_order() {
        let dir = std::env::temp_dir().join(format!("gh-paced-editor-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cfg_dir = dir.display().to_string();
        let all = [
            ("GH_CONFIG_DIR", cfg_dir.as_str()),
            ("GH_EDITOR", "ghe"),
            ("GIT_EDITOR", "gite"),
            ("VISUAL", "vis"),
            ("EDITOR", "ed"),
        ];
        std::fs::write(
            dir.join("config.yml"),
            "version: 1\neditor: 'cfg -w'\naliases:\n    editor: no\n",
        )
        .unwrap();
        assert_eq!(gh_editor(&env_of(&all)), "ghe");
        assert_eq!(gh_editor(&env_of(&all[..1])), "cfg -w");
        // gh writes `editor:` with no value by default.
        std::fs::write(dir.join("config.yml"), "editor:\n").unwrap();
        assert_eq!(
            gh_editor(&env_of(&[all[0], all[2], all[3], all[4]])),
            "gite"
        );
        assert_eq!(gh_editor(&env_of(&[all[0], all[3], all[4]])), "vis");
        assert_eq!(gh_editor(&env_of(&[all[0], all[4]])), "ed");
        assert_eq!(gh_editor(&env_of(&[all[0]])), "nano");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_guard_is_installed_once_and_quoted() {
        let exe = Path::new("/opt/it's here/gh-paced");
        let env = guard_env(&env_of(&[("EDITOR", "vim -n")]), Some(exe), "octo-cat");
        assert_eq!(
            env,
            vec![
                (
                    "GH_EDITOR".to_string(),
                    "'/opt/it'\\''s here/gh-paced' --edit-guard 'octo-cat'".to_string()
                ),
                ("GH_PACED_EDITOR".to_string(), "vim -n".to_string()),
            ]
        );
        assert_eq!(
            crate::alias::split(&env[0].1).unwrap(),
            vec!["/opt/it's here/gh-paced", "--edit-guard", "octo-cat"]
        );
        // Inside an outer gh-paced, the outer guard and the user's editor pass through.
        let nested = guard_env(
            &env_of(&[("GH_EDITOR", &env[0].1), ("GH_PACED_EDITOR", "vim -n")]),
            Some(Path::new("/other/gh-paced")),
            "octo-cat",
        );
        assert_eq!(nested, env);
        // Without its own path, the editor is a command that refuses.
        let none = guard_env(&env_of(&[]), None, "octo-cat");
        assert!(none[0].1.contains("exit 65"), "{none:?}");
    }
}
