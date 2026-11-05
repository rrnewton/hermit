//! gh's own command aliases, resolved before anything else looks at the command line.
//!
//! gh expands an ordinary alias inside its own process: with the alias
//! `upload: api -X POST repos/o/r/issues/$1/comments --input -`, `gh upload 7` sends stdin as a
//! request body, and with `w: run watch 99`, `gh w` polls. gh-paced therefore resolves the alias
//! the way gh does and runs gh with the expanded command line. Classification, pacing, the watch
//! gate, the content guard and the file snapshots all apply to that expanded command line, and gh
//! never sees the alias name. gh still reads its configuration again when it starts, so the
//! command checked is the command gh runs only while that configuration stays as gh-paced read
//! it; a configuration rewritten during the call is outside what gh-paced guards against (see
//! the design note).
//!
//! A shell alias (`!...`) is passed on as an opaque WRITE: gh runs it with `sh -c`, and a `gh` that
//! the shell command runs is a new gh-paced invocation when gh-paced is installed in front of `gh`
//! on `PATH`.
//!
//! The lookup must never miss an alias that gh would find, because gh would then expand it
//! unchecked. When gh-paced cannot tell, it refuses instead of guessing: gh's configuration file
//! cannot be read or uses YAML this reader does not handle (every command line that names a
//! command is refused then, gh's own commands included), two aliases share a name, where the
//! alias name sits depends on how gh parses a flag, an alias shares its name with an extension,
//! or aliases nest more than [`MAX_ALIAS_DEPTH`] deep.

use std::collections::HashSet;
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::classify::builtin_child;

/// Deepest chain of aliases followed (an alias whose expansion starts with another alias).
pub const MAX_ALIAS_DEPTH: usize = 5;

/// Largest gh configuration file read. A larger one is treated as unreadable.
pub const MAX_CONFIG_BYTES: u64 = 1 << 20;

/// The aliases gh has when its configuration file is missing or has no settings at all.
const DEFAULT_ALIASES: &[(&str, &str)] = &[("co", "pr checkout")];

/// Root words cobra adds after gh has loaded its aliases. An alias with one of these names may or
/// may not be the command that runs.
const LATE_ROOT_COMMANDS: &[&str] = &["help", "__complete", "__completeNoDesc"];

/// The variables gh finds its configuration file and extensions directory through.
const LOOKUP_VARS: [&str; 4] = ["GH_CONFIG_DIR", "XDG_CONFIG_HOME", "XDG_DATA_HOME", "HOME"];

/// gh's configuration file: `$GH_CONFIG_DIR/config.yml`, else
/// `$XDG_CONFIG_HOME/gh/config.yml`, else `$HOME/.config/gh/config.yml`, as gh looks it up. With
/// `HOME` unset or empty, gh uses `.config/gh/config.yml` relative to the working directory, and
/// so does this. Always `Some`.
pub fn config_file(env: &dyn Fn(&str) -> Option<String>) -> Option<PathBuf> {
    let set = |k: &str| env(k).filter(|v| !v.is_empty());
    if let Some(d) = set("GH_CONFIG_DIR") {
        return Some(PathBuf::from(d).join("config.yml"));
    }
    if let Some(d) = set("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(d).join("gh").join("config.yml"));
    }
    Some(home(env).join(".config").join("gh").join("config.yml"))
}

/// gh's extensions directory: `$XDG_DATA_HOME/gh/extensions`, else
/// `$HOME/.local/share/gh/extensions` (relative when `HOME` is unset or empty).
pub fn extensions_dir(env: &dyn Fn(&str) -> Option<String>) -> PathBuf {
    match env("XDG_DATA_HOME").filter(|v| !v.is_empty()) {
        Some(d) => PathBuf::from(d).join("gh").join("extensions"),
        None => home(env)
            .join(".local")
            .join("share")
            .join("gh")
            .join("extensions"),
    }
}

fn home(env: &dyn Fn(&str) -> Option<String>) -> PathBuf {
    PathBuf::from(env("HOME").unwrap_or_default())
}

/// The installed extensions as gh-paced reads [`extensions_dir`].
#[derive(Debug, Clone, Default)]
pub struct Extensions {
    /// The command word of each `gh-<name>` entry: `<name>` up to its first space, as cobra
    /// names a command (`<name>` itself when it has no space).
    pub words: Vec<String>,
    /// Whether gh certainly registers every one of them. gh lists its extensions all at once and
    /// registers none when one entry cannot be read; gh-paced only checks the entries that cannot
    /// fail that way (a directory without a `manifest.yml`, or a symbolic link), so a regular
    /// file, a binary extension's directory or any other entry makes this `false`.
    pub certain: bool,
}

/// The installed extensions (`gh-<name>` entries of [`extensions_dir`]), or `None` when the
/// directory exists but cannot be listed.
fn list_extensions(dir: &Path) -> Option<Extensions> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Some(Extensions {
                words: Vec::new(),
                certain: true,
            })
        }
        Err(_) => return None,
    };
    let mut out = Extensions {
        words: Vec::new(),
        certain: true,
    };
    for entry in entries {
        let entry = entry.ok()?;
        let name = entry.file_name();
        let Some(n) = name.to_str() else {
            if name.as_encoded_bytes().starts_with(b"gh-") {
                return None;
            }
            continue;
        };
        let Some(ext) = n.strip_prefix("gh-") else {
            continue;
        };
        let simple = match entry.file_type() {
            Ok(t) if t.is_symlink() => true,
            // A directory with a `manifest.yml` is a binary extension. gh-paced does not model
            // whether gh can load it, so it counts it as uncertain. That over-refuses: such a
            // directory can make gh-paced refuse an alias that gh would in fact shadow with the
            // extension. This is a documented open item, not a pacing bypass.
            Ok(t) if t.is_dir() => std::fs::metadata(entry.path().join("manifest.yml")).is_err(),
            _ => false,
        };
        out.certain &= simple;
        let word = ext.split(' ').next().unwrap_or_default();
        if !word.is_empty() {
            out.words.push(word.to_string());
        }
    }
    Some(out)
}

/// Read gh's configuration file: `Ok(None)` when it does not exist.
fn read_config(path: &Path) -> Result<Option<String>, String> {
    let shown = path.display();
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("cannot read {shown}: {e}")),
    };
    if !meta.is_file() {
        return Err(format!("{shown} is not a regular file"));
    }
    if meta.len() > MAX_CONFIG_BYTES {
        return Err(format!("{shown} is larger than {MAX_CONFIG_BYTES} bytes"));
    }
    let mut buf = Vec::new();
    std::fs::File::open(path)
        .and_then(|f| f.take(MAX_CONFIG_BYTES + 1).read_to_end(&mut buf))
        .map_err(|e| format!("cannot read {shown}: {e}"))?;
    if buf.len() as u64 > MAX_CONFIG_BYTES {
        return Err(format!("{shown} is larger than {MAX_CONFIG_BYTES} bytes"));
    }
    String::from_utf8(buf)
        .map(Some)
        .map_err(|_| format!("{shown} is not UTF-8"))
}

/// One alias, placed where gh adds it in its command tree.
#[derive(Debug, Clone)]
struct Alias {
    /// The name as written in the configuration file.
    name: String,
    /// The built-in commands it is added beneath (canonical names; empty for the root).
    parent: Vec<&'static str>,
    /// The command word that invokes it.
    word: String,
    expansion: String,
    /// Why gh-paced cannot tell whether gh runs this alias for its word.
    doubt: Option<String>,
}

/// gh's aliases as gh-paced resolves them (see the module documentation).
#[derive(Debug, Clone, Default)]
pub struct GhAliases {
    entries: Vec<Alias>,
    /// Parent words of alias names that are not built-in gh commands this table knows: a command
    /// line reaching one of these words is refused, since gh may know the command.
    unplaced: Vec<(String, String)>,
    /// Why gh's configuration cannot be read. Every command word is refused then: any of them,
    /// gh's own commands, extensions and `help` included, can be an alias in that file.
    unreadable: Option<String>,
    /// The installed extensions, or `None` when they cannot be listed.
    extensions: Option<Vec<String>>,
}

/// What a command line resolves to through gh's aliases.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    /// No alias is involved: run the command line as given.
    NotAlias,
    /// Run `argv` instead: the command gh would run after expanding the aliases in `chain`.
    Expanded {
        /// The expanded command line.
        argv: Vec<String>,
        /// The alias names followed, outermost first.
        chain: Vec<String>,
    },
    /// The command line reaches a shell alias, which gh runs with `sh -c`. Run `argv`: the same
    /// alias, named right after its command path so gh finds exactly it.
    Shell {
        /// The command line naming the shell alias.
        argv: Vec<String>,
        /// The alias names followed, outermost first (the shell alias last).
        chain: Vec<String>,
    },
    /// gh-paced cannot tell what gh would run, or gh would refuse the expansion.
    Refused {
        /// What to tell the caller.
        reason: String,
        /// The cause is gh's configuration file (exit 78) rather than the command line (exit 64).
        config: bool,
    },
}

/// How a flag token takes a value during cobra's command lookup.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Arity {
    Bool,
    Value,
    /// Not known here: both readings are followed.
    Maybe,
}

/// The positions cobra may take as the next command word in `tokens` (its `stripFlags`: `--` ends
/// the search; `--name` without `=` and a two-byte `-x` take the next token as a value unless the
/// flag is boolean, and a value flag with at most one token after it ends the search; any other
/// token starting with `-`, and the empty token, are skipped; the first other token is the word).
/// `None` stands for "no word". A flag whose arity is not known yields both readings.
fn first_words(tokens: &[String], at_root: bool) -> Vec<Option<usize>> {
    let arity = |t: &str| -> Option<Arity> {
        if let Some(name) = t.strip_prefix("--") {
            if t.contains('=') {
                return None;
            }
            return Some(match name {
                "help" => Arity::Bool,
                "version" if at_root => Arity::Bool,
                "repo" => Arity::Value,
                _ => Arity::Maybe,
            });
        }
        if t.len() == 2 && t.starts_with('-') && !t.contains('=') {
            return Some(if t == "-R" {
                Arity::Value
            } else {
                Arity::Maybe
            });
        }
        None
    };
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut stack = vec![0usize];
    while let Some(mut i) = stack.pop() {
        loop {
            if !seen.insert(i) {
                break;
            }
            let Some(t) = tokens.get(i) else {
                out.push(None);
                break;
            };
            if t == "--" {
                out.push(None);
                break;
            }
            let remaining = tokens.len() - i - 1;
            match arity(t) {
                Some(Arity::Bool) => i += 1,
                Some(Arity::Value) => {
                    if remaining <= 1 {
                        out.push(None);
                        break;
                    }
                    i += 2;
                }
                Some(Arity::Maybe) => {
                    if remaining <= 1 {
                        out.push(None);
                    } else {
                        stack.push(i + 2);
                    }
                    i += 1;
                }
                None if t.is_empty() || t.starts_with('-') => i += 1,
                None => {
                    out.push(Some(i));
                    break;
                }
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// One way the lookup can end.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Hit {
    None,
    Alias {
        entry: usize,
        path: Vec<&'static str>,
        rest: Vec<String>,
    },
    Doubt {
        reason: String,
        config: bool,
    },
}

impl GhAliases {
    /// gh's aliases from its configuration file and extensions directory, found through `env`.
    pub fn load(env: &dyn Fn(&str) -> Option<String>) -> GhAliases {
        let listed = list_extensions(&extensions_dir(env));
        let path = config_file(env).unwrap_or_default();
        let parsed = read_config(&path).and_then(|text| match text {
            None => Ok(default_pairs()),
            Some(t) => parse_aliases(&t).map_err(|e| format!("{}: {e}", path.display())),
        });
        match parsed {
            Ok(pairs) => GhAliases::place(pairs, listed),
            // Nothing else is consulted when the file cannot be read (see `walk`), so neither the
            // extension list nor whether gh registers it is kept.
            Err(why) => GhAliases {
                unreadable: Some(why),
                ..GhAliases::default()
            },
        }
    }

    /// [`GhAliases::load`], after checking that none of the variables gh finds its files through
    /// (`raw`, the environment as the operating system gives it) holds bytes that are not UTF-8.
    /// gh would use such a value as a path; gh-paced cannot, so it treats the configuration as
    /// unreadable (refusing every command word, as for any unreadable file) instead of as unset.
    pub fn load_checked(
        env: &dyn Fn(&str) -> Option<String>,
        raw: &dyn Fn(&str) -> Option<std::ffi::OsString>,
    ) -> GhAliases {
        for k in LOOKUP_VARS {
            if raw(k).is_some_and(|v| v.to_str().is_none()) {
                return GhAliases {
                    unreadable: Some(format!(
                        "`{k}` is not UTF-8, so gh-paced cannot tell which files gh reads"
                    )),
                    extensions: None,
                    ..GhAliases::default()
                };
            }
        }
        GhAliases::load(env)
    }

    /// Place `(name, expansion)` pairs read from a configuration file the way gh adds them to its
    /// command tree. `extensions` is the command words of the installed extensions, all certainly
    /// registered by gh, or `None` when they are not known. A name gh rejects is dropped; a name
    /// gh-paced cannot be sure about is kept and refused when used.
    pub fn from_pairs(pairs: Vec<(String, String)>, extensions: Option<Vec<String>>) -> GhAliases {
        GhAliases::place(
            pairs,
            extensions.map(|words| Extensions {
                words,
                certain: true,
            }),
        )
    }

    /// [`GhAliases::from_pairs`] with extensions that gh may not register (see [`Extensions`]).
    fn place(pairs: Vec<(String, String)>, listed: Option<Extensions>) -> GhAliases {
        let certain = listed.as_ref().is_some_and(|e| e.certain);
        let extensions = listed.map(|e| e.words);
        let mut entries = Vec::new();
        let mut unplaced = Vec::new();
        'pairs: for (name, expansion) in pairs {
            let Some(words) = split(&name) else { continue };
            if words.is_empty() || words.iter().any(|w| w.is_empty() || w.starts_with('-')) {
                // cobra never takes such a word as a command, so gh can never run the alias.
                continue;
            }
            let (full, parents) = words.split_last().expect("non-empty");
            // gh names the alias command after its last word, and cobra takes a command's name
            // from its usage text up to the first space: `issue 'publish now'` runs as
            // `gh issue publish`.
            let word = full.split(' ').next().unwrap_or_default().to_string();
            if word.is_empty() {
                // cobra skips an empty word during lookup, so gh can never run the alias.
                continue;
            }
            let renamed = word != *full;
            let mut parent: Vec<&'static str> = Vec::new();
            for p in parents {
                match builtin_child(&parent, p) {
                    Some((canon, true)) => parent.push(canon),
                    Some((_, false)) => continue 'pairs,
                    None => {
                        let listed =
                            parent.is_empty() && extensions.as_ref().is_some_and(|e| e.contains(p));
                        if !listed {
                            unplaced.push((p.clone(), name.clone()));
                        }
                        continue 'pairs;
                    }
                }
            }
            let clash = format!(
                "the name `{full}` makes gh add a second command named `{word}`, and which of \
                 the two cobra finds first is not known"
            );
            let mut doubt = None;
            if parent.is_empty() && LATE_ROOT_COMMANDS.contains(&word.as_str()) {
                doubt = Some(format!(
                    "cobra adds its own `{word}` command after gh loads aliases, so which of the \
                     two runs is not known"
                ));
            } else if builtin_child(&parent, &word).is_some() {
                if !renamed {
                    // A built-in command of that name wins; gh does not add the alias.
                    continue;
                }
                doubt = Some(clash);
            } else if parent.is_empty() {
                match &extensions {
                    None => {
                        doubt = Some(
                            "gh's extensions directory cannot be listed, so whether an extension \
                             of the same name runs instead is not known"
                                .to_string(),
                        )
                    }
                    Some(e) if e.contains(&word) => {
                        if certain && !renamed {
                            // gh registers the extension first and then rejects an alias whose
                            // name finds a runnable command: the extension runs.
                            continue;
                        }
                        doubt = Some(if renamed {
                            clash
                        } else {
                            format!(
                                "an extension is also named `{word}`, and gh registers no \
                                 extension at all when one entry of its extensions directory \
                                 cannot be read, which gh-paced does not check"
                            )
                        });
                    }
                    Some(_) => {}
                }
            }
            entries.push(Alias {
                name,
                parent,
                word,
                expansion,
                doubt,
            });
        }
        let root_words: HashSet<String> = entries
            .iter()
            .filter(|a| a.parent.is_empty())
            .map(|a| a.word.clone())
            .collect();
        entries.retain(|a| expansion_may_be_valid(&a.expansion, &extensions, &root_words));
        GhAliases {
            entries,
            unplaced,
            unreadable: None,
            extensions,
        }
    }

    /// Why gh's configuration cannot be read, if it cannot.
    pub fn unreadable(&self) -> Option<&str> {
        self.unreadable.as_deref()
    }

    /// The number of aliases gh-paced knows.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether gh-paced knows no alias.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Resolve a gh command line (without the program name) through the aliases.
    pub fn resolve(&self, args: &[String]) -> Resolution {
        let mut current = args.to_vec();
        let mut chain: Vec<String> = Vec::new();
        loop {
            let (entry, path, rest) = match self.hit(&current) {
                Ok(None) => break,
                Ok(Some(h)) => h,
                Err((reason, config)) => return Resolution::Refused { reason, config },
            };
            let alias = &self.entries[entry];
            if chain.len() >= MAX_ALIAS_DEPTH {
                return Resolution::Refused {
                    reason: format!(
                        "aliases nest more than {MAX_ALIAS_DEPTH} deep (`{}` -> `{}`), which \
                         gh-paced does not follow",
                        chain.join("` -> `"),
                        alias.name
                    ),
                    config: false,
                };
            }
            chain.push(alias.name.clone());
            if alias.expansion.starts_with('!') {
                let mut argv: Vec<String> = path.iter().map(|p| p.to_string()).collect();
                argv.push(alias.word.clone());
                argv.extend(rest);
                return Resolution::Shell { argv, chain };
            }
            match expand(&alias.expansion, &rest) {
                Some(next) => current = next,
                None => {
                    // The expansion is not repeated here: it can hold the text of a body,
                    // and this reason is written to the audit log.
                    return Resolution::Refused {
                        reason: format!(
                            "gh would refuse the alias `{}` with these arguments: a `$N` in its \
                             expansion has no argument, or a quote is left open",
                            alias.name
                        ),
                        config: false,
                    };
                }
            }
        }
        if chain.is_empty() {
            Resolution::NotAlias
        } else {
            Resolution::Expanded {
                argv: current,
                chain,
            }
        }
    }

    /// The alias `tokens` invoke: its entry, command path and the arguments it receives (every
    /// token except the command path words and the alias word). `Err` with a reason (and whether
    /// the configuration file is the cause) when that is not certain.
    #[allow(clippy::type_complexity)]
    fn hit(
        &self,
        tokens: &[String],
    ) -> Result<Option<(usize, Vec<&'static str>, Vec<String>)>, (String, bool)> {
        let mentioned = |t: &String| {
            self.entries.iter().any(|a| a.word == *t) || self.unplaced.iter().any(|(p, _)| p == t)
        };
        if self.unreadable.is_none() && !tokens.iter().any(mentioned) {
            return Ok(None);
        }
        let mut hits = Vec::new();
        self.walk(tokens.to_vec(), Vec::new(), &mut hits);
        if let Some(Hit::Doubt { reason, config }) =
            hits.iter().find(|h| matches!(h, Hit::Doubt { .. }))
        {
            return Err((reason.clone(), *config));
        }
        match hits.as_slice() {
            [] | [Hit::None] => Ok(None),
            [Hit::Alias { entry, path, rest }] => Ok(Some((*entry, path.clone(), rest.clone()))),
            _ => Err((
                "whether gh takes this as an alias depends on whether a flag before the alias \
                 name takes a value; put the alias name right after the command words"
                    .to_string(),
                false,
            )),
        }
    }

    /// Follow cobra's command lookup through `tokens` beneath `path`, collecting every way it can
    /// end in `out`.
    fn walk(&self, tokens: Vec<String>, path: Vec<&'static str>, out: &mut Vec<Hit>) {
        let push = |h: Hit, out: &mut Vec<Hit>| {
            if !out.contains(&h) {
                out.push(h);
            }
        };
        for cand in first_words(&tokens, path.is_empty()) {
            let Some(i) = cand else {
                push(Hit::None, out);
                continue;
            };
            let w = tokens[i].as_str();
            if let Some(why) = &self.unreadable {
                // Any word can be an alias in a file gh-paced cannot read: gh adds `help` and its
                // completion commands after the aliases, an alias whose name is quoted to include
                // a space (`'pr x'`) adds a second command named `pr`, and an alias named after
                // an extension runs whenever gh registers no extensions. So not even gh's own
                // commands or the installed extensions are known to run as themselves.
                push(
                    Hit::Doubt {
                        reason: format!(
                            "gh's configuration cannot be read ({why}), so whether `{w}` runs as \
                             itself or as an alias of that name is not known"
                        ),
                        config: true,
                    },
                    out,
                );
                continue;
            }
            let here: Vec<usize> = (0..self.entries.len())
                .filter(|&k| self.entries[k].parent == path && self.entries[k].word == w)
                .collect();
            if let Some(a) = here
                .iter()
                .map(|&k| &self.entries[k])
                .find(|a| a.doubt.is_some())
            {
                let why = a.doubt.as_deref().unwrap_or_default();
                push(
                    Hit::Doubt {
                        reason: format!("`{w}` is the alias `{}`, but {why}", a.name),
                        config: false,
                    },
                    out,
                );
                continue;
            }
            if let Some((canon, accepts)) = builtin_child(&path, w) {
                if accepts {
                    let mut rest = tokens.clone();
                    rest.remove(i);
                    let mut deeper = path.clone();
                    deeper.push(canon);
                    self.walk(rest, deeper, out);
                } else {
                    push(Hit::None, out);
                }
                continue;
            }
            if let Some(&first) = here.first() {
                let a = &self.entries[first];
                if here
                    .iter()
                    .any(|&k| self.entries[k].expansion != a.expansion)
                {
                    push(
                        Hit::Doubt {
                            reason: format!(
                                "several aliases are named `{w}`, and gh picks one of them at \
                                 random"
                            ),
                            config: false,
                        },
                        out,
                    );
                } else {
                    let mut rest = tokens.clone();
                    rest.remove(i);
                    push(
                        Hit::Alias {
                            entry: first,
                            path: path.clone(),
                            rest,
                        },
                        out,
                    );
                }
                continue;
            }
            if path.is_empty()
                && self
                    .extensions
                    .as_ref()
                    .is_some_and(|e| e.iter().any(|x| x == w))
            {
                // An installed extension: gh runs it, and no alias can sit beneath it.
                push(Hit::None, out);
                continue;
            }
            if let Some((_, name)) = self.unplaced.iter().find(|(p, _)| p == w) {
                push(
                    Hit::Doubt {
                        reason: format!(
                            "`{w}` is not a gh command gh-paced knows, and the alias `{name}` \
                             would sit beneath it if gh knows it"
                        ),
                        config: false,
                    },
                    out,
                );
                continue;
            }
            push(Hit::None, out);
        }
    }
}

/// Whether gh could accept `expansion` as an alias body (`ValidAliasExpansionFunc`: a `!` prefix,
/// or words whose command lookup does not fail). Only an expansion that certainly fails is
/// rejected.
fn expansion_may_be_valid(
    expansion: &str,
    extensions: &Option<Vec<String>>,
    root_words: &HashSet<String>,
) -> bool {
    if expansion.starts_with('!') {
        return true;
    }
    let Some(tokens) = split(expansion) else {
        return false;
    };
    if tokens.is_empty() {
        return false;
    }
    first_words(&tokens, true).into_iter().any(|c| match c {
        None => true,
        Some(i) => {
            let w = &tokens[i];
            builtin_child(&[], w).is_some()
                || root_words.contains(w)
                || extensions.as_ref().is_none_or(|e| e.contains(w))
        }
    })
}

fn default_pairs() -> Vec<(String, String)> {
    DEFAULT_ALIASES
        .iter()
        .map(|(n, e)| (n.to_string(), e.to_string()))
        .collect()
}

// ---------------------------------------------------------------------------------------------
// A reader for the YAML subset gh's configuration file uses.
//
// gh reads `config.yml` with a full YAML parser. This reader handles the forms a person or gh
// writes: top-level `key: value` settings, and an `aliases` mapping in block form (plain, quoted
// and block-scalar values, multi-line values) or flow form (`{name: value, ...}`). Anything else
// it cannot be sure about (tags, anchors, merge keys, nested collections, tabs used for
// indentation, duplicate keys) is an error, and the caller treats the file as unreadable.
// ---------------------------------------------------------------------------------------------

/// The text being read, with its line starts.
struct Yaml<'a> {
    text: &'a str,
    b: &'a [u8],
    starts: Vec<usize>,
}

impl<'a> Yaml<'a> {
    fn new(text: &'a str) -> Yaml<'a> {
        let mut starts = vec![0];
        starts.extend(text.match_indices('\n').map(|(i, _)| i + 1));
        Yaml {
            text,
            b: text.as_bytes(),
            starts,
        }
    }

    /// Number of lines; the last one has no line break after it.
    fn n(&self) -> usize {
        self.starts.len()
    }

    fn line(&self, k: usize) -> &'a str {
        let end = self.starts.get(k + 1).map_or(self.text.len(), |s| s - 1);
        &self.text[self.starts[k]..end]
    }

    fn line_of(&self, p: usize) -> usize {
        self.starts.partition_point(|&s| s <= p) - 1
    }

    /// Byte offset of `s`, a slice of the text.
    fn off(&self, s: &str) -> usize {
        s.as_ptr() as usize - self.text.as_ptr() as usize
    }

    fn col0(&self, p: usize) -> bool {
        p == 0 || self.b[p - 1] == b'\n'
    }

    fn at(&self, p: usize, msg: &str) -> String {
        format!("line {}: {msg}", self.line_of(p.min(self.b.len())) + 1)
    }
}

/// The aliases gh reads from the text of its `config.yml`: the `aliases` mapping, gh's default
/// aliases (`co: pr checkout`) when the file has no settings at all, and none when it has
/// settings but no `aliases`. `Err` when the file uses YAML this reader does not handle.
pub fn parse_aliases(text: &str) -> Result<Vec<(String, String)>, String> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    if let Some((i, c)) = text.char_indices().find(|&(_, c)| refused_char(c)) {
        return Err(format!(
            "line {}: the character U+{:04X}, which gh-paced does not read",
            text[..i].matches('\n').count() + 1,
            c as u32
        ));
    }
    let normalized;
    let text = if text.contains('\r') {
        normalized = text.replace("\r\n", "\n");
        if normalized.contains('\r') {
            return Err("a carriage return outside a line ending".to_string());
        }
        normalized.as_str()
    } else {
        text
    };
    let y = Yaml::new(text);
    let mut keys: HashSet<String> = HashSet::new();
    let mut aliases = None;
    let mut started = false;
    let mut k = 0;
    while k < y.n() {
        let line = y.line(k);
        if blank_or_comment(line) {
            k += 1;
            continue;
        }
        let at = |m: &str| format!("line {}: {m}", k + 1);
        if line.starts_with([' ', '\t']) {
            return Err(at("indented text outside any setting"));
        }
        if !started && strip_comment(line).trim_end() == "---" {
            started = true;
            k += 1;
            continue;
        }
        started = true;
        if (line.starts_with("---") || line.starts_with("..."))
            && matches!(line.as_bytes().get(3), None | Some(b' ' | b'\t'))
        {
            return Err(at("a document marker after the first setting"));
        }
        let (key, rest) = block_key(&y, line)?;
        if !keys.insert(key.clone()) {
            return Err(at(&format!("`{key}` is set twice")));
        }
        if key == "aliases" {
            let (pairs, next) = aliases_value(&y, k, rest)?;
            aliases = Some(pairs);
            k = next;
        } else {
            other_value(rest).map_err(|e| at(&e))?;
            k += 1;
            while k < y.n() && (y.line(k).starts_with([' ', '\t']) || blank_or_comment(y.line(k))) {
                k += 1;
            }
        }
    }
    Ok(match aliases {
        Some(a) => a,
        None if keys.is_empty() => default_pairs(),
        None => Vec::new(),
    })
}

/// A character this reader refuses rather than guess how gh's YAML library reads it: a byte
/// order mark after the first one (libyaml skips one at the start of any line), NEL, LS and PS
/// (line breaks to libyaml, ordinary characters to a line splitter), and the control characters
/// and non-characters libyaml rejects.
fn refused_char(c: char) -> bool {
    matches!(
        c,
        '\u{feff}' | '\u{85}' | '\u{2028}' | '\u{2029}' | '\u{7f}' | '\u{fffe}' | '\u{ffff}'
    ) || (c < ' ' && !matches!(c, '\t' | '\n' | '\r'))
        || ('\u{80}'..='\u{9f}').contains(&c)
}

fn blank_or_comment(line: &str) -> bool {
    let t = line.trim_start_matches([' ', '\t']);
    t.is_empty() || t.starts_with('#')
}

fn leading_spaces(line: &str) -> usize {
    line.len() - line.trim_start_matches(' ').len()
}

/// A tab before the first character of a line, where YAML forbids it.
fn tab_in_indent(line: &str) -> bool {
    line[..line.len() - line.trim_start_matches([' ', '\t']).len()].contains('\t')
}

/// Characters that cannot start a plain YAML scalar or key, except as [`plain_start`] allows.
fn indicator(c: u8) -> bool {
    b"-?:,[]{}#&*!|>'\"%@`".contains(&c)
}

/// Whether a plain scalar starts at `b` (text up to the end of its line) in block context, as
/// libyaml decides: a character that is not an indicator, or `-`, `?` or `:` followed by a
/// character other than a space or tab (`-R`, `?x`, `:x`; `- x` is a sequence entry).
fn plain_start(b: &[u8]) -> bool {
    match b {
        [] => false,
        [b'-' | b'?' | b':', rest @ ..] => !matches!(rest.first(), None | Some(b' ' | b'\t')),
        [c, ..] => !indicator(*c),
    }
}

/// Split a block-mapping line into its key and the text after `:`.
fn block_key<'a>(y: &Yaml<'a>, line: &'a str) -> Result<(String, &'a str), String> {
    let p = y.off(line);
    let b = line.as_bytes();
    match b.first() {
        Some(b'"' | b'\'') => {
            let (key, end) = quoted(y, p)?;
            if y.line_of(end - 1) != y.line_of(p) {
                return Err(y.at(p, "a quoted key spans lines"));
            }
            let after = y.text[end..y.off(line) + line.len()].trim_start_matches([' ', '\t']);
            let rest = after
                .strip_prefix(':')
                .ok_or_else(|| y.at(p, "a quoted key without `:` after it"))?;
            if !rest.is_empty() && !rest.starts_with([' ', '\t']) {
                return Err(y.at(p, "text right after `:`"));
            }
            Ok((key, rest))
        }
        Some(&c) if !plain_start(b) => Err(y.at(
            p,
            &format!(
                "`{}` at the start of an entry, which gh-paced does not read",
                c as char
            ),
        )),
        Some(_) if line.starts_with("<<") => Err(y.at(p, "a merge key (`<<`)")),
        Some(_) => {
            for (i, &c) in b.iter().enumerate() {
                if c == b'#' && i > 0 && matches!(b[i - 1], b' ' | b'\t') {
                    break;
                }
                if c == b':' && matches!(b.get(i + 1), None | Some(b' ' | b'\t')) {
                    let key = line[..i].trim_end_matches([' ', '\t']);
                    return Ok((key.to_string(), &line[i + 1..]));
                }
            }
            Err(y.at(p, "a line that is not `key: value`"))
        }
        None => Err(y.at(p, "an empty key")),
    }
}

/// Check the value of a setting other than `aliases`, which is skipped: it must not continue
/// onto later lines in a way that would hide where the next setting starts.
fn other_value(rest: &str) -> Result<(), String> {
    let v = rest.trim_start_matches([' ', '\t']);
    let b = v.as_bytes();
    match b.first() {
        Some(&q @ (b'"' | b'\'')) => {
            let mut i = 1;
            while i < b.len() {
                if q == b'"' && b[i] == b'\\' {
                    i += 2;
                    continue;
                }
                if b[i] == q {
                    if q == b'\'' && b.get(i + 1) == Some(&b'\'') {
                        i += 2;
                        continue;
                    }
                    return Ok(());
                }
                i += 1;
            }
            Err("a quoted value that continues on the next line".to_string())
        }
        Some(b'{' | b'[') => {
            let mut depth = 0i32;
            let mut quote = None;
            let mut i = 0;
            while i < b.len() {
                let c = b[i];
                match quote {
                    Some(b'"') if c == b'\\' => i += 1,
                    Some(q) if c == q => quote = None,
                    Some(_) => {}
                    None => match c {
                        b'"' | b'\'' => quote = Some(c),
                        b'{' | b'[' => depth += 1,
                        b'}' | b']' => {
                            depth -= 1;
                            if depth == 0 {
                                return Ok(());
                            }
                        }
                        b'#' if i > 0 && matches!(b[i - 1], b' ' | b'\t') => break,
                        _ => {}
                    },
                }
                i += 1;
            }
            Err("a flow collection that continues on the next line".to_string())
        }
        _ => Ok(()),
    }
}

/// The value of `aliases:` on line `k`, whose text after the colon is `rest`. Returns the pairs
/// and the next line to read.
fn aliases_value(y: &Yaml, k: usize, rest: &str) -> Result<(Vec<(String, String)>, usize), String> {
    let v = rest.trim_start_matches([' ', '\t']);
    if v.starts_with('{') {
        return flow_map(y, y.off(v));
    }
    let bare = strip_comment(v).trim_end();
    if bare.is_empty() {
        return block_map(y, k + 1);
    }
    if matches!(bare, "~" | "null" | "Null" | "NULL") {
        let mut j = k + 1;
        while j < y.n() && blank_or_comment(y.line(j)) {
            j += 1;
        }
        if j < y.n() && y.line(j).starts_with([' ', '\t']) {
            return Err(format!(
                "line {}: indented text after `aliases: {bare}`",
                j + 1
            ));
        }
        return Ok((Vec::new(), j));
    }
    Err(format!(
        "line {}: `aliases` is not a mapping gh-paced can read",
        k + 1
    ))
}

/// An `aliases` mapping in block form, starting at line `k`.
fn block_map(y: &Yaml, mut k: usize) -> Result<(Vec<(String, String)>, usize), String> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut indent = None;
    while k < y.n() {
        let line = y.line(k);
        if blank_or_comment(line) {
            k += 1;
            continue;
        }
        let at = |m: &str| format!("line {}: {m}", k + 1);
        if tab_in_indent(line) {
            return Err(at("a tab in the indentation"));
        }
        let ind = leading_spaces(line);
        if ind == 0 {
            break;
        }
        let n = *indent.get_or_insert(ind);
        if ind != n {
            return Err(at("indentation that does not match the first alias"));
        }
        let (key, rest) = block_key(y, &line[ind..])?;
        if out.iter().any(|(name, _)| *name == key) {
            return Err(at(&format!("the alias `{key}` is set twice")));
        }
        let (value, next) = block_value(y, k, n, rest)?;
        out.push((key, value));
        k = next;
    }
    Ok((out, k))
}

/// The value of the block-mapping entry on line `k` (entries indented `n`), whose text after the
/// colon is `rest`. Returns the value as gh reads it and the next line to read.
fn block_value(y: &Yaml, k: usize, n: usize, rest: &str) -> Result<(String, usize), String> {
    if !rest.is_empty() && !rest.starts_with([' ', '\t']) {
        return Err(format!("line {}: text right after `:`", k + 1));
    }
    let v = rest.trim_start_matches([' ', '\t']);
    if v.is_empty() || v.starts_with('#') {
        // The value, if any, is on the following, more indented lines.
        let mut j = k + 1;
        while j < y.n() && blank_or_comment(y.line(j)) {
            j += 1;
        }
        if j >= y.n() || leading_spaces(y.line(j)) <= n || tab_in_indent(y.line(j)) {
            return Ok((String::new(), k + 1));
        }
        let line = y.line(j);
        let body = line.trim_start_matches(' ');
        return match body.as_bytes()[0] {
            b'"' | b'\'' => quoted_value(y, y.off(body)),
            _ if !plain_start(body.as_bytes()) => Err(format!(
                "line {}: a nested collection or tagged value, which gh-paced does not read",
                j + 1
            )),
            _ => plain_value(y, j, body, n),
        };
    }
    match v.as_bytes()[0] {
        b'|' | b'>' => block_scalar(y, k, n, v),
        b'"' | b'\'' => quoted_value(y, y.off(v)),
        c if !plain_start(v.as_bytes()) => Err(format!(
            "line {}: a value starting with `{}`, which gh-paced does not read",
            k + 1,
            c as char
        )),
        _ => plain_value(y, k, v, n),
    }
}

/// A quoted value starting at byte `p`: only a comment may follow it on its last line.
fn quoted_value(y: &Yaml, p: usize) -> Result<(String, usize), String> {
    let (value, end) = quoted(y, p)?;
    let last = y.line_of(end - 1);
    let line_end = y.off(y.line(last)) + y.line(last).len();
    let tail = &y.text[end..line_end];
    let t = tail.trim_start_matches([' ', '\t']);
    if !t.is_empty() && !(t.starts_with('#') && t.len() < tail.len()) {
        return Err(y.at(end, "text after a quoted value"));
    }
    Ok((value, last + 1))
}

/// A plain value whose first line (line `k`) is `first`, folded with its more-indented
/// continuation lines.
fn plain_value(y: &Yaml, k: usize, first: &str, n: usize) -> Result<(String, usize), String> {
    let at = |j: usize, m: &str| format!("line {}: {m}", j + 1);
    let check = |j: usize, s: &str| -> Result<(), String> {
        if s.contains(": ") || s.contains(":\t") || s.ends_with(':') {
            return Err(at(j, "a `: ` inside a plain value"));
        }
        Ok(())
    };
    let stripped = strip_comment(first);
    let mut saw_comment = stripped.len() != first.len();
    let first = stripped.trim_end_matches([' ', '\t']);
    check(k, first)?;
    let mut value = first.to_string();
    let mut blanks = 0;
    let mut j = k + 1;
    while j < y.n() {
        let line = y.line(j);
        let t = line.trim_start_matches([' ', '\t']);
        if t.is_empty() {
            blanks += 1;
            j += 1;
            continue;
        }
        if tab_in_indent(line) {
            return Err(at(j, "a tab in the indentation"));
        }
        if leading_spaces(line) <= n {
            break;
        }
        if t.starts_with('#') {
            saw_comment = true;
            j += 1;
            continue;
        }
        if saw_comment {
            return Err(at(j, "a plain value continues after a comment"));
        }
        let c = strip_comment(t);
        saw_comment = c.len() != t.len();
        let c = c.trim_end_matches([' ', '\t']);
        check(j, c)?;
        if blanks == 0 {
            value.push(' ');
        } else {
            value.extend(std::iter::repeat_n('\n', blanks));
        }
        blanks = 0;
        value.push_str(c);
        j += 1;
    }
    Ok((value, j))
}

/// A single- or double-quoted scalar starting at byte `p` (the quote), folded across lines as
/// YAML does. Returns its value and the byte after the closing quote.
fn quoted(y: &Yaml, mut p: usize) -> Result<(String, usize), String> {
    let b = y.b;
    let q = b[p];
    let single = q == b'\'';
    let start = p;
    p += 1;
    let mut out: Vec<u8> = Vec::new();
    let unclosed = || y.at(start, "a quote that is never closed");
    loop {
        if p >= b.len() {
            return Err(unclosed());
        }
        if y.col0(p)
            && (b[p..].starts_with(b"---") || b[p..].starts_with(b"..."))
            && matches!(b.get(p + 3), None | Some(b' ' | b'\t' | b'\n'))
        {
            return Err(y.at(p, "a document marker inside a quoted value"));
        }
        let mut leading_blanks = false;
        while p < b.len() && !matches!(b[p], b' ' | b'\t' | b'\n') {
            if single && b[p] == b'\'' && b.get(p + 1) == Some(&b'\'') {
                out.push(b'\'');
                p += 2;
                continue;
            }
            if b[p] == q {
                break;
            }
            if !single && b[p] == b'\\' {
                if b.get(p + 1) == Some(&b'\n') {
                    p += 2;
                    leading_blanks = true;
                    break;
                }
                let (c, used) = escape(y, p)?;
                let mut buf = [0u8; 4];
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                p += used;
                continue;
            }
            out.push(b[p]);
            p += 1;
        }
        if p >= b.len() {
            return Err(unclosed());
        }
        if b[p] == q {
            let s = String::from_utf8(out).map_err(|_| y.at(start, "invalid UTF-8"))?;
            return Ok((s, p + 1));
        }
        let mut spaces: Vec<u8> = Vec::new();
        let mut first_break = false;
        let mut breaks = 0usize;
        while p < b.len() && matches!(b[p], b' ' | b'\t' | b'\n') {
            if b[p] != b'\n' {
                if !leading_blanks {
                    spaces.push(b[p]);
                }
            } else if !leading_blanks {
                spaces.clear();
                first_break = true;
                leading_blanks = true;
            } else {
                breaks += 1;
            }
            p += 1;
        }
        if leading_blanks {
            if first_break && breaks == 0 {
                out.push(b' ');
            } else {
                out.extend(std::iter::repeat_n(b'\n', breaks));
            }
        } else {
            out.extend(spaces);
        }
    }
}

/// The escape sequence starting at the backslash at byte `p` of a double-quoted scalar: the
/// character and the number of bytes used.
fn escape(y: &Yaml, p: usize) -> Result<(char, usize), String> {
    let b = y.b;
    let Some(&e) = b.get(p + 1) else {
        return Err(y.at(p, "a backslash at the end of the file"));
    };
    let simple = match e {
        b'0' => Some('\0'),
        b'a' => Some('\x07'),
        b'b' => Some('\x08'),
        b't' | b'\t' => Some('\t'),
        b'n' => Some('\n'),
        b'v' => Some('\x0b'),
        b'f' => Some('\x0c'),
        b'r' => Some('\r'),
        b'e' => Some('\x1b'),
        b' ' => Some(' '),
        b'"' => Some('"'),
        b'/' => Some('/'),
        b'\'' => Some('\''),
        b'\\' => Some('\\'),
        b'N' => Some('\u{85}'),
        b'_' => Some('\u{a0}'),
        b'L' => Some('\u{2028}'),
        b'P' => Some('\u{2029}'),
        _ => None,
    };
    if let Some(c) = simple {
        return Ok((c, 2));
    }
    let len = match e {
        b'x' => 2,
        b'u' => 4,
        b'U' => 8,
        _ => return Err(y.at(p, "an unknown escape in a double-quoted value")),
    };
    let digits = b
        .get(p + 2..p + 2 + len)
        .filter(|d| d.iter().all(u8::is_ascii_hexdigit))
        .ok_or_else(|| y.at(p, "a bad hexadecimal escape"))?;
    let code = u32::from_str_radix(std::str::from_utf8(digits).unwrap_or("x"), 16)
        .map_err(|_| y.at(p, "a bad hexadecimal escape"))?;
    let c = char::from_u32(code).ok_or_else(|| y.at(p, "an escape that is not a character"))?;
    Ok((c, 2 + len))
}

/// A block scalar (`|` or `>`) whose header `header` is on line `k`, in a mapping indented `n`.
fn block_scalar(y: &Yaml, k: usize, n: usize, header: &str) -> Result<(String, usize), String> {
    let at = |j: usize, m: &str| format!("line {}: {m}", j + 1);
    let literal = header.starts_with('|');
    let h = &header.as_bytes()[1..];
    let mut chomp = 0i8;
    let mut incr = 0usize;
    let mut i = 0;
    while i < h.len() && i < 2 {
        match h[i] {
            b'+' | b'-' if chomp == 0 => chomp = if h[i] == b'+' { 1 } else { -1 },
            d @ b'1'..=b'9' if incr == 0 => incr = usize::from(d - b'0'),
            _ => break,
        }
        i += 1;
    }
    let tail = &header[1 + i..];
    let t = tail.trim_start_matches([' ', '\t']);
    if !t.is_empty() && !(t.starts_with('#') && t.len() < tail.len()) {
        return Err(at(k, "a block scalar header gh-paced does not read"));
    }
    let mut indent = if incr > 0 { n + incr } else { 0 };
    let last = y.n() - 1;
    // Consume line breaks and indentation as libyaml's `scan_block_scalar_breaks` does.
    let breaks = |mut j: usize, indent: &mut usize| -> Result<(usize, usize), String> {
        let mut max_indent = 0;
        let mut count = 0;
        while j < y.n() {
            let lb = y.line(j).as_bytes();
            let mut c = 0;
            while (*indent == 0 || c < *indent) && c < lb.len() && lb[c] == b' ' {
                c += 1;
            }
            max_indent = max_indent.max(c);
            if (*indent == 0 || c < *indent) && lb.get(c) == Some(&b'\t') {
                return Err(at(j, "a tab in the indentation of a block scalar"));
            }
            if c < lb.len() || j == last {
                break;
            }
            count += 1;
            j += 1;
        }
        if *indent == 0 {
            *indent = max_indent.max(n + 1);
        }
        Ok((count, j))
    };
    let (mut trailing, mut j) = breaks(k + 1, &mut indent)?;
    let mut s = String::new();
    let mut leading_break = false;
    let mut leading_blank = false;
    while j < y.n() {
        let line = y.line(j);
        if leading_spaces(line) < indent {
            break;
        }
        let cur = &line[indent..];
        if j == last && cur.is_empty() {
            break;
        }
        let trailing_blank = cur.starts_with([' ', '\t']);
        if !literal && leading_break && !leading_blank && !trailing_blank {
            if trailing == 0 {
                s.push(' ');
            }
        } else if leading_break {
            s.push('\n');
        }
        leading_break = false;
        s.extend(std::iter::repeat_n('\n', trailing));
        trailing = 0;
        leading_blank = trailing_blank;
        s.push_str(cur);
        if j == last {
            j += 1;
            break;
        }
        leading_break = true;
        let (t, next) = breaks(j + 1, &mut indent)?;
        trailing = t;
        j = next;
    }
    if chomp != -1 && leading_break {
        s.push('\n');
    }
    if chomp == 1 {
        s.extend(std::iter::repeat_n('\n', trailing));
    }
    Ok((s, j))
}

/// Skip blanks, line breaks and comments inside a flow mapping.
fn flow_gap(y: &Yaml, mut p: usize) -> Result<usize, String> {
    let b = y.b;
    loop {
        match b.get(p) {
            None => return Err(y.at(p, "a `{` that is never closed")),
            Some(b' ' | b'\t' | b'\n') => p += 1,
            Some(b'#') if matches!(b[p - 1], b' ' | b'\t' | b'\n') => {
                while p < b.len() && b[p] != b'\n' {
                    p += 1;
                }
            }
            Some(&c) => {
                if y.col0(p) && c != b'}' {
                    return Err(y.at(p, "flow mapping text at the left margin"));
                }
                return Ok(p);
            }
        }
    }
}

/// A scalar in a flow mapping starting at byte `p`: quoted or plain.
fn flow_scalar(y: &Yaml, p: usize) -> Result<(String, usize), String> {
    let b = y.b;
    match b[p] {
        b'"' | b'\'' => return quoted(y, p),
        b'-' if !matches!(b.get(p + 1), None | Some(b' ' | b'\t' | b'\n')) => {}
        c if indicator(c) => {
            return Err(y.at(
                p,
                &format!(
                    "`{}` in a flow mapping, which gh-paced does not read",
                    c as char
                ),
            ))
        }
        _ => {}
    }
    if b[p..].starts_with(b"<<") {
        return Err(y.at(p, "a merge key (`<<`)"));
    }
    let ends = |q: usize| match b.get(q) {
        None | Some(b',' | b'[' | b']' | b'{' | b'}') => true,
        Some(b':') => matches!(
            b.get(q + 1),
            None | Some(b' ' | b'\t' | b'\n' | b',' | b'?' | b'[' | b']' | b'{' | b'}')
        ),
        _ => false,
    };
    let mut out: Vec<u8> = Vec::new();
    let mut p = p;
    loop {
        while p < b.len() && !matches!(b[p], b' ' | b'\t' | b'\n') && !ends(p) {
            out.push(b[p]);
            p += 1;
        }
        let q = p;
        let mut spaces: Vec<u8> = Vec::new();
        let mut breaks = 0usize;
        while p < b.len() && matches!(b[p], b' ' | b'\t' | b'\n') {
            if b[p] == b'\n' {
                breaks += 1;
                spaces.clear();
            } else if breaks == 0 {
                spaces.push(b[p]);
            }
            p += 1;
        }
        if ends(p) || (p > q && b[p] == b'#') {
            p = q;
            break;
        }
        if breaks > 0 && y.col0(p) {
            return Err(y.at(p, "flow mapping text at the left margin"));
        }
        match breaks {
            0 => out.extend(spaces),
            1 => out.push(b' '),
            _ => out.extend(std::iter::repeat_n(b'\n', breaks - 1)),
        }
    }
    let s = String::from_utf8(out).map_err(|_| y.at(p, "invalid UTF-8"))?;
    Ok((s, p))
}

/// An `aliases` mapping in flow form, `{` at byte `p`. Returns the pairs and the next line.
fn flow_map(y: &Yaml, mut p: usize) -> Result<(Vec<(String, String)>, usize), String> {
    let b = y.b;
    let mut out: Vec<(String, String)> = Vec::new();
    p += 1;
    loop {
        p = flow_gap(y, p)?;
        if b[p] == b'}' {
            p += 1;
            break;
        }
        let key_at = p;
        let (key, after) = flow_scalar(y, p)?;
        if y.line_of(after - 1) != y.line_of(key_at) {
            // YAML takes a key that spans lines differently; do not guess the alias name.
            return Err(y.at(key_at, "an alias name that spans lines"));
        }
        p = flow_gap(y, after)?;
        let mut value = String::new();
        if b[p] == b':' {
            p = flow_gap(y, p + 1)?;
            if !matches!(b[p], b',' | b'}') {
                let (v, after) = flow_scalar(y, p)?;
                value = v;
                p = flow_gap(y, after)?;
            }
        }
        if out.iter().any(|(name, _)| *name == key) {
            return Err(y.at(key_at, &format!("the alias `{key}` is set twice")));
        }
        out.push((key, value));
        match b[p] {
            b',' => p += 1,
            b'}' => {
                p += 1;
                break;
            }
            _ => return Err(y.at(p, "expected `,` or `}` in the aliases mapping")),
        }
    }
    let line = y.line_of(p - 1);
    let line_end = y.off(y.line(line)) + y.line(line).len();
    let tail = &y.text[p..line_end];
    let t = tail.trim_start_matches([' ', '\t']);
    if !t.is_empty() && !(t.starts_with('#') && t.len() < tail.len()) {
        return Err(y.at(p, "text after the aliases mapping"));
    }
    Ok((out, line + 1))
}

/// The value of a top-level `key: value` line of a gh `config.yml` (such as `editor`), or `None`
/// when the key is absent. A key that is only indented, inside another mapping, does not count.
pub fn top_level_value(text: &str, key: &str) -> Option<String> {
    text.lines()
        .filter(|l| !l.starts_with([' ', '\t']))
        .filter_map(|l| split_key(strip_comment(l).trim_end()))
        .find(|(k, _)| k == key)
        .map(|(_, rest)| scalar(rest.trim()))
}

/// Drop a comment (`#` at the start or after a blank) from a line.
fn strip_comment(line: &str) -> &str {
    let b = line.as_bytes();
    for (i, &c) in b.iter().enumerate() {
        if c == b'#' && (i == 0 || matches!(b[i - 1], b' ' | b'\t')) {
            return &line[..i];
        }
    }
    line
}

/// Split `key: value` (the key plain or quoted).
fn split_key(line: &str) -> Option<(String, &str)> {
    if line.starts_with('\'') || line.starts_with('"') {
        let (key, used) = quoted_line(line)?;
        let rest = line[used..].trim_start().strip_prefix(':')?;
        return Some((key, rest));
    }
    let i = line
        .find(": ")
        .or_else(|| line.strip_suffix(':').map(str::len))?;
    let rest = line.get(i + 1..).unwrap_or("");
    Some((line[..i].trim().to_string(), rest))
}

/// A YAML scalar value: quoted, or plain with any trailing comment removed.
fn scalar(text: &str) -> String {
    if text.starts_with('\'') || text.starts_with('"') {
        if let Some((v, _)) = quoted_line(text) {
            return v;
        }
    }
    strip_comment(text).trim().to_string()
}

/// A single- or double-quoted YAML scalar at the start of `text`, within one line: its value and
/// the bytes used.
fn quoted_line(text: &str) -> Option<(String, usize)> {
    let y = Yaml::new(text);
    let (v, end) = quoted(&y, 0).ok()?;
    (!text[..end].contains('\n')).then_some((v, end))
}

/// Split `s` into words the way gh splits an alias expansion (github.com/google/shlex): blanks
/// separate words; `'...'` is literal; inside `"..."` a backslash takes the next character
/// literally; outside quotes a backslash does the same; `#` at the start of a word begins a
/// comment to the end of the line. `None` when a quote or escape is left open (gh refuses the
/// alias then).
pub fn split(s: &str) -> Option<Vec<String>> {
    #[derive(PartialEq)]
    enum St {
        Start,
        Word,
        Comment,
    }
    let mut out = Vec::new();
    let mut word = String::new();
    let mut st = St::Start;
    let mut chars = s.chars();
    while let Some(ch) = chars.next() {
        match st {
            St::Comment => {
                if ch == '\n' {
                    st = St::Start;
                }
                continue;
            }
            St::Start if ch == '#' => {
                st = St::Comment;
                continue;
            }
            _ => {}
        }
        match ch {
            ' ' | '\t' | '\r' | '\n' => {
                if st == St::Word {
                    out.push(std::mem::take(&mut word));
                }
                st = St::Start;
            }
            '\\' => {
                word.push(chars.next()?);
                st = St::Word;
            }
            '\'' => {
                loop {
                    match chars.next()? {
                        '\'' => break,
                        c => word.push(c),
                    }
                }
                st = St::Word;
            }
            '"' => {
                loop {
                    match chars.next()? {
                        '"' => break,
                        '\\' => word.push(chars.next()?),
                        c => word.push(c),
                    }
                }
                st = St::Word;
            }
            c => {
                word.push(c);
                st = St::Word;
            }
        }
    }
    if st == St::Word {
        out.push(word);
    }
    Some(out)
}

/// Expand one ordinary alias as gh does. gh walks the arguments in order: while the expansion
/// still contains a `$`, argument N replaces every `$N`; once no `$` is left, each remaining
/// argument is appended. So `issue comment $1` given `7 extra` becomes
/// `issue comment 7 extra`. The text is then split into words. `None` when gh would refuse it
/// (a `$` and a digit left over, or an open quote).
pub fn expand(expansion: &str, args: &[String]) -> Option<Vec<String>> {
    let mut text = expansion.to_string();
    let mut extra = Vec::new();
    for (i, a) in args.iter().enumerate() {
        if text.contains('$') {
            text = text.replace(&format!("${}", i + 1), a);
        } else {
            extra.push(a.clone());
        }
    }
    let bytes = text.as_bytes();
    if bytes
        .windows(2)
        .any(|w| w[0] == b'$' && w[1].is_ascii_digit())
    {
        return None;
    }
    let mut words = split(&text)?;
    words.extend(extra);
    Some(words)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_string).collect()
    }

    fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
        list.iter()
            .map(|(n, e)| (n.to_string(), e.to_string()))
            .collect()
    }

    fn aliases(list: &[(&str, &str)]) -> GhAliases {
        GhAliases::from_pairs(pairs(list), Some(Vec::new()))
    }

    fn expanded(r: Resolution) -> Vec<String> {
        match r {
            Resolution::Expanded { argv, .. } => argv,
            other => panic!("expected an expansion, got {other:?}"),
        }
    }

    fn refused(r: &Resolution) -> bool {
        matches!(r, Resolution::Refused { .. })
    }

    /// The aliases block exactly as gh 2.97 wrote it for these `gh alias set` calls.
    const CONFIG: &str = "\
# Aliases allow you to create nicknames for gh commands
aliases:
    co: pr checkout
    upload: api -X POST repos/o/r/issues/$1/comments --input -
    sh1: '!echo hi | cat'
    q1: 'issue comment $1 --body ''it''''s: a #test'''
    q2: api -F \"body=@-\" \"repos/o/r/issues/$1/comments\"
    nest: upload
    issue publish: api -X POST x --input -
    \"dq\": \"api --input \\x2d x\"
# The path to a unix socket
http_unix_socket:
";

    fn get<'a>(a: &'a [(String, String)], k: &str) -> Option<&'a str> {
        a.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str())
    }

    #[test]
    fn reads_the_aliases_gh_writes() {
        let a = parse_aliases(CONFIG).unwrap();
        assert_eq!(get(&a, "co"), Some("pr checkout"));
        assert_eq!(
            get(&a, "upload"),
            Some("api -X POST repos/o/r/issues/$1/comments --input -")
        );
        assert_eq!(get(&a, "sh1"), Some("!echo hi | cat"));
        assert_eq!(
            get(&a, "q1"),
            Some("issue comment $1 --body 'it''s: a #test'")
        );
        assert_eq!(
            get(&a, "q2"),
            Some("api -F \"body=@-\" \"repos/o/r/issues/$1/comments\"")
        );
        assert_eq!(get(&a, "issue publish"), Some("api -X POST x --input -"));
        assert_eq!(get(&a, "dq"), Some("api --input - x"));
        assert_eq!(a.len(), 8, "{a:?}");
        assert!(parse_aliases("version: 1\naliases: {}\n")
            .unwrap()
            .is_empty());
        assert!(parse_aliases("version: 1\n").unwrap().is_empty());
        assert!(parse_aliases("aliases:\nversion: 1\n").unwrap().is_empty());
        assert!(parse_aliases("aliases: ~\n").unwrap().is_empty());
    }

    #[test]
    fn a_file_without_settings_has_ghs_default_alias() {
        for text in ["", "# only a comment\n", "---\n", "\u{feff}\n\n"] {
            assert_eq!(
                parse_aliases(text).unwrap(),
                pairs(&[("co", "pr checkout")]),
                "{text:?}"
            );
        }
    }

    #[test]
    fn reads_flow_mappings() {
        let a = parse_aliases(
            "version: 1\naliases: {upload: 'issue comment 7 --body-file -', co: pr checkout}\n",
        )
        .unwrap();
        assert_eq!(get(&a, "upload"), Some("issue comment 7 --body-file -"));
        assert_eq!(get(&a, "co"), Some("pr checkout"));
        let a = parse_aliases(
            "aliases: {\n  # a comment\n  \"x y\": \"api -X POST\n    z\",\n  w: run watch\n    99,\n  url: api https://h/x:y,\n  empty:,\n}\neditor: vim\n",
        )
        .unwrap();
        assert_eq!(get(&a, "x y"), Some("api -X POST z"));
        assert_eq!(get(&a, "w"), Some("run watch 99"));
        assert_eq!(get(&a, "url"), Some("api https://h/x:y"));
        assert_eq!(get(&a, "empty"), Some(""));
        assert_eq!(a.len(), 4);
    }

    #[test]
    fn reads_block_scalars() {
        let a = parse_aliases(
            "aliases:\n  lit: |\n    issue comment 7\n    --body-file -\n  fold: >-\n    issue\n    comment\n\n    7\n  keep: |+\n    a\n\n  strip: |-\n    b\n  ind: |4\n        c\n  next: d\n",
        )
        .unwrap();
        assert_eq!(get(&a, "lit"), Some("issue comment 7\n--body-file -\n"));
        assert_eq!(get(&a, "fold"), Some("issue comment\n7"));
        assert_eq!(get(&a, "keep"), Some("a\n\n"));
        assert_eq!(get(&a, "strip"), Some("b"));
        // The indentation indicator counts from the mapping's own indentation (2 + 4).
        assert_eq!(get(&a, "ind"), Some("  c\n"));
        assert_eq!(get(&a, "next"), Some("d"));
        // Plain values fold their continuation lines; a blank line is a line break.
        let a = parse_aliases("aliases:\n  p: issue\n    comment 7\n\n    x\n").unwrap();
        assert_eq!(get(&a, "p"), Some("issue comment 7\nx"));
        // A value on the line after its key.
        let a = parse_aliases("aliases:\n  p:\n    issue comment\n  q:\n").unwrap();
        assert_eq!(get(&a, "p"), Some("issue comment"));
        assert_eq!(get(&a, "q"), Some(""));
    }

    #[test]
    fn folds_quoted_values() {
        let a = parse_aliases(
            "aliases:\n  s: 'issue\n    comment   \n\n    7'\n  d: \"api \\\n    -X\\tPOST\"\n  e: \"a\\u0041\\x42\"  # comment\n",
        )
        .unwrap();
        assert_eq!(get(&a, "s"), Some("issue comment\n7"));
        assert_eq!(get(&a, "d"), Some("api -X\tPOST"));
        assert_eq!(get(&a, "e"), Some("aAB"));
        let a = parse_aliases("aliases:\r\n  co: pr checkout\r\n").unwrap();
        assert_eq!(get(&a, "co"), Some("pr checkout"));
    }

    #[test]
    fn refuses_yaml_it_cannot_read() {
        for text in [
            "aliases:\n  co: !!str pr checkout\n",
            "aliases:\n  co: &a pr checkout\n  x: *a\n",
            "aliases:\n  <<: {co: pr checkout}\n",
            "base: &b {co: pr checkout}\naliases: *b\n",
            "aliases:\n  co: pr checkout\n  co: issue list\n",
            "aliases:\n  co: pr checkout\n  \"co\": issue list\n",
            "aliases: {co: a, co: b}\n",
            "aliases:\n\tco: pr checkout\n",
            "aliases:\n  co:\n    nested: map\n",
            "aliases:\n  co:\n    - a\n",
            "aliases:\n  co: [a, b]\n",
            "aliases:\n  co: \"bad \\q escape\"\n",
            "aliases:\n  co: pr checkout\r  x: y\n",
            "editor: vim\n---\naliases:\n  co: a\n",
            "  aliases:\n    co: a\n",
            "aliases:\n  co: a\n   x: b\n",
            "aliases:\n    co: a\n  x: b\n",
            "aliases: [co]\n",
            "aliases: x\n",
            "aliases:\n  co: 'unclosed\n",
            "aliases: {co: a\n",
            "editor: 'multi\n  line'\naliases:\n  co: a\n",
            "aliases:\n  co: a: b\n",
            "aliases:\n  co: a\n  # comment\n    b\n",
            "aliases\n",
            "? aliases\n: {co: a}\n",
            "aliases: {co: {x: y}}\n",
            "aliases: {\nco: a}\n",
            "aliases: {a\n  b: c}\n",
            "aliases: {'a\n  b': c}\n",
            // Characters libyaml reads differently from a line splitter, or rejects.
            "aliases:\n  co: pr checkout\u{85}  w: run watch 9\n",
            "aliases:\n  co: pr checkout\u{2028}  w: run watch 9\n",
            "aliases:\n  co: pr checkout\u{2029}  w: run watch 9\n",
            "\u{feff}\u{feff}aliases:\n  co: pr checkout\n",
            "aliases:\n\u{feff}  co: pr checkout\n",
            "aliases:\n  co: pr\u{1}checkout\n",
            "aliases:\n  co: pr\u{7f}checkout\n",
            "aliases:\n  co: pr\u{9b}checkout\n",
            "aliases:\n  co: pr\u{fffe}checkout\n",
            // A sequence entry, a lone `-`, and document markers after the first setting.
            "aliases:\n  - co\n",
            "aliases:\n  co: - a\n",
            "aliases:\n  co: -\n",
            "aliases:\n  co:\n    - a\n",
            "aliases:\n  co: a\n... x: y\n",
            "aliases:\n  co: a\n--- # c\nx: y\n",
        ] {
            assert!(parse_aliases(text).is_err(), "{text:?} should be an error");
        }
    }

    /// libyaml starts a plain scalar at `-`, `?` or `:` followed by a character other than a
    /// space or tab, in block context; gh's configuration reader takes such keys and values.
    #[test]
    fn reads_plain_scalars_starting_with_dash_question_or_colon() {
        let text = "aliases:\n  pv: -R o/r pr view\n  ?review: pr list\n  :review: issue list\n  \
                    w:\n    -R o/r\n    run watch\n  q: ?x\n  c: :x\n";
        assert_eq!(
            parse_aliases(text).unwrap(),
            pairs(&[
                ("pv", "-R o/r pr view"),
                ("?review", "pr list"),
                (":review", "issue list"),
                ("w", "-R o/r run watch"),
                ("q", "?x"),
                ("c", ":x"),
            ])
        );
        // A single leading byte order mark and a tab are still read.
        assert_eq!(
            parse_aliases("\u{feff}aliases:\n  co: pr\tcheckout\n").unwrap(),
            pairs(&[("co", "pr\tcheckout")])
        );
    }

    /// cobra names a command after its usage text up to the first space, so gh runs the alias
    /// `issue 'publish hidden'` as `gh issue publish`.
    #[test]
    fn an_alias_word_with_a_space_runs_as_its_first_word() {
        let a = aliases(&[
            ("a", "issue publish"),
            ("issue 'publish hidden'", "run watch 99 --interval 1"),
        ]);
        assert_eq!(
            expanded(a.resolve(&argv("a"))),
            argv("run watch 99 --interval 1")
        );
        assert_eq!(
            expanded(a.resolve(&argv("issue publish"))),
            argv("run watch 99 --interval 1")
        );
        assert_eq!(a.resolve(&argv("issue list")), Resolution::NotAlias);
        // The first word is a built-in command's or cobra's own: which one runs is not known.
        let clash = aliases(&[("issue 'view all'", "issue list"), ("'help me'", "pr list")]);
        assert!(refused(&clash.resolve(&argv("issue view 1"))));
        assert!(refused(&clash.resolve(&argv("help"))));
        // An empty first word: cobra never takes it as a command, so the alias never runs.
        let empty = aliases(&[("issue ' x'", "run watch 9")]);
        assert!(empty.is_empty());
        assert_eq!(empty.resolve(&argv("issue x")), Resolution::NotAlias);
    }

    /// gh registers extensions before aliases and rejects an alias whose name finds a runnable
    /// command, so an installed extension runs instead of an alias of the same name; when gh may
    /// register no extension at all, which one runs is not known.
    #[test]
    fn an_extension_runs_instead_of_an_alias_of_its_name() {
        let listed = |name: &str, certain| {
            GhAliases::place(
                pairs(&[(name, "pr list")]),
                Some(Extensions {
                    words: vec!["myext".to_string()],
                    certain,
                }),
            )
        };
        let sure = listed("myext", true);
        assert!(sure.is_empty());
        assert_eq!(sure.resolve(&argv("myext x")), Resolution::NotAlias);
        assert!(refused(&listed("myext", false).resolve(&argv("myext x"))));
        // `'myext now'` adds a second command named `myext`, beside the extension.
        assert!(refused(
            &listed("'myext now'", true).resolve(&argv("myext x"))
        ));
    }

    #[test]
    fn a_refusal_does_not_repeat_the_expansion() {
        let a = aliases(&[("post", "issue comment $1 --body CANARY_PRIVATE_BODY")]);
        match a.resolve(&argv("post")) {
            Resolution::Refused { reason, config } => {
                assert!(!config);
                assert!(reason.contains("`post`"), "{reason}");
                assert!(!reason.contains("CANARY_PRIVATE_BODY"), "{reason}");
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    /// A lookup variable that is not UTF-8 is a path gh uses and gh-paced cannot: every command
    /// is refused as unreadable configuration, never resolved as if it were unset.
    #[test]
    fn a_lookup_variable_that_is_not_utf8_makes_the_configuration_unreadable() {
        use std::os::unix::ffi::OsStrExt;
        for bad in LOOKUP_VARS {
            let env = |k: &str| (k == "HOME").then(|| "/nonexistent-gh-paced".to_string());
            let raw = |k: &str| {
                if k == bad {
                    Some(std::ffi::OsStr::from_bytes(b"/x\xff").to_os_string())
                } else {
                    env(k).map(Into::into)
                }
            };
            let a = GhAliases::load_checked(&env, &raw);
            assert!(a.unreadable().is_some_and(|w| w.contains(bad)), "{bad}");
            for line in ["up", "pr view 1", "help"] {
                assert!(matches!(
                    a.resolve(&argv(line)),
                    Resolution::Refused { config: true, .. }
                ));
            }
            assert_eq!(a.resolve(&argv("--version")), Resolution::NotAlias);
        }
        let env = |k: &str| (k == "HOME").then(|| "/nonexistent-gh-paced".to_string());
        let raw = |k: &str| env(k).map(Into::into);
        let a = GhAliases::load_checked(&env, &raw);
        assert!(a.unreadable().is_none());
        assert_eq!(expanded(a.resolve(&argv("co 3"))), argv("pr checkout 3"));
    }

    #[test]
    fn splits_like_gh() {
        assert_eq!(
            split(r#"a 'b c' "d \" e" f\ g h#i #comment"#).unwrap(),
            vec!["a", "b c", "d \" e", "f g", "h#i"]
        );
        assert_eq!(split(r#"x "" y"#).unwrap(), vec!["x", "", "y"]);
        assert_eq!(split("'open"), None);
        assert_eq!(split("trailing\\"), None);
    }

    #[test]
    fn expands_like_gh() {
        assert_eq!(
            expand("pr checkout", &argv("12 --force")).unwrap(),
            argv("pr checkout 12 --force")
        );
        // While a `$` is left, an argument goes only where its `$N` puts it; once none is
        // left, the rest are appended (gh checks for `$` before each argument).
        assert_eq!(
            expand("issue comment $1 --body-file -", &argv("7")).unwrap(),
            argv("issue comment 7 --body-file -")
        );
        assert_eq!(
            expand("issue comment $1 --body-file -", &argv("7 extra")).unwrap(),
            argv("issue comment 7 --body-file - extra")
        );
        assert_eq!(
            expand("api repos/$1/$2 --input -", &argv("o r --verbose")).unwrap(),
            argv("api repos/o/r --input - --verbose")
        );
        // A substituted argument is split into words with the rest of the text.
        assert_eq!(
            expand("issue comment $1", &["7 --body-file big.md".to_string()]).unwrap(),
            argv("issue comment 7 --body-file big.md")
        );
        assert_eq!(expand("issue comment $2", &argv("7")), None);
    }

    #[test]
    fn finds_the_command_word_like_cobra() {
        let fw = |s: &str, root: bool| first_words(&argv(s), root);
        assert_eq!(fw("upload 7", true), vec![Some(0)]);
        assert_eq!(fw("-R o/r upload", true), vec![Some(2)]);
        assert_eq!(fw("--repo o/r upload", false), vec![Some(2)]);
        assert_eq!(fw("--help upload", true), vec![Some(1)]);
        assert_eq!(fw("--version upload", true), vec![Some(1)]);
        assert_eq!(fw("-- upload", true), vec![None]);
        assert_eq!(fw("--x=1 upload", true), vec![Some(1)]);
        assert_eq!(fw("-abc upload", true), vec![Some(1)]);
        // A flag whose arity is unknown: either it takes `a` (word `b`) or it does not.
        assert_eq!(fw("--flag a b", true), vec![Some(1), Some(2)]);
        // A value flag with at most one token after it ends the search.
        assert_eq!(fw("-R o/r", true), vec![None]);
        assert_eq!(fw("-x a", true), vec![None, Some(1)]);
        assert_eq!(fw("", true), vec![None]);
    }

    #[test]
    fn resolves_nested_compound_and_shell_aliases() {
        let a = GhAliases::from_pairs(parse_aliases(CONFIG).unwrap(), Some(Vec::new()));
        assert_eq!(
            expanded(a.resolve(&argv("upload 7"))),
            argv("api -X POST repos/o/r/issues/7/comments --input -")
        );
        assert_eq!(
            a.resolve(&argv("nest 7")),
            Resolution::Expanded {
                argv: argv("api -X POST repos/o/r/issues/7/comments --input -"),
                chain: vec!["nest".to_string(), "upload".to_string()],
            }
        );
        assert_eq!(
            expanded(a.resolve(&argv("issue publish --help"))),
            argv("api -X POST x --input - --help")
        );
        assert_eq!(
            a.resolve(&argv("sh1 x")),
            Resolution::Shell {
                argv: argv("sh1 x"),
                chain: vec!["sh1".to_string()],
            }
        );
        assert_eq!(a.resolve(&argv("pr view 1")), Resolution::NotAlias);
        assert_eq!(
            a.resolve(&argv("issue comment 7 --body upload")),
            Resolution::NotAlias
        );
        // gh would refuse the expansion: `$1` has no argument.
        assert!(refused(&a.resolve(&argv("upload"))));
    }

    #[test]
    fn a_loop_or_deep_nesting_is_refused() {
        let looped = aliases(&[("a", "b"), ("b", "a")]);
        assert!(refused(&looped.resolve(&argv("a"))));
        let deep = aliases(&[
            ("a1", "a2"),
            ("a2", "a3"),
            ("a3", "a4"),
            ("a4", "a5"),
            ("a5", "a6"),
            ("a6", "pr list"),
        ]);
        assert!(refused(&deep.resolve(&argv("a1"))));
        assert_eq!(expanded(deep.resolve(&argv("a2"))), argv("pr list"));
    }

    #[test]
    fn finds_an_alias_after_flags_like_cobra() {
        let a = aliases(&[("issue upload", "issue comment 7 --body-file -")]);
        // cobra skips `-R o/r` while looking for the next command word.
        assert_eq!(
            expanded(a.resolve(&argv("issue -R o/r upload x"))),
            argv("issue comment 7 --body-file - -R o/r x")
        );
        assert_eq!(
            expanded(a.resolve(&argv("-R o/r issue upload"))),
            argv("issue comment 7 --body-file - -R o/r")
        );
        // A flag gh-paced cannot classify makes the position uncertain: refused.
        assert!(refused(&a.resolve(&argv("issue --flag upload x"))));
        // After `--`, cobra stops looking: `upload` is an argument of `issue`.
        assert_eq!(a.resolve(&argv("issue -- upload")), Resolution::NotAlias);
        // A group's cobra alias reaches the same place.
        let cs = aliases(&[("codespace mine", "codespace list")]);
        assert_eq!(
            expanded(cs.resolve(&argv("cs mine"))),
            argv("codespace list")
        );
    }

    #[test]
    fn names_gh_rejects_are_dropped() {
        let a = aliases(&[
            ("pr list", "api -X POST x --input -"),
            ("ls", "pr list"),
            ("-x", "pr list"),
            ("issue -R upload", "pr list"),
            ("pr view x", "pr list"),
            ("bad", "nosuchcommand"),
            ("empty", ""),
        ]);
        assert_eq!(a.len(), 1, "{a:?}");
        assert_eq!(a.resolve(&argv("pr list")), Resolution::NotAlias);
        assert_eq!(expanded(a.resolve(&argv("ls"))), argv("pr list"));
        assert_eq!(a.resolve(&argv("bad")), Resolution::NotAlias);
    }

    #[test]
    fn uncertain_names_are_refused() {
        // Two names that gh splits to the same command word.
        let a = aliases(&[("co", "pr checkout"), ("'co'", "issue list")]);
        assert!(refused(&a.resolve(&argv("co 1"))));
        // An alias named like an extension gh may not register, or when extensions cannot be
        // listed.
        let ext = GhAliases::place(
            pairs(&[("up", "pr list"), ("issue up", "issue list")]),
            Some(Extensions {
                words: vec!["up".to_string()],
                certain: false,
            }),
        );
        assert!(refused(&ext.resolve(&argv("up"))));
        assert_eq!(expanded(ext.resolve(&argv("issue up"))), argv("issue list"));
        let unlisted = GhAliases::from_pairs(pairs(&[("up", "pr list")]), None);
        assert!(refused(&unlisted.resolve(&argv("up"))));
        // cobra adds `help` after aliases load.
        let help = aliases(&[("help", "pr list")]);
        assert!(refused(&help.resolve(&argv("help"))));
        // A parent word gh-paced does not know as a gh command.
        let odd = aliases(&[("newcmd up", "pr list")]);
        assert!(refused(&odd.resolve(&argv("newcmd up"))));
        assert_eq!(odd.resolve(&argv("pr view 1")), Resolution::NotAlias);
    }

    /// With a configuration gh-paced cannot read, every command word is refused: gh's own
    /// commands (one renamed alias can add a second `pr`), `help` (added after the aliases) and
    /// an installed extension (an alias of its name runs when gh registers no extensions) may all
    /// be aliases there. A line without a command word still runs.
    #[test]
    fn an_unreadable_configuration_refuses_every_command() {
        let a = GhAliases {
            unreadable: Some("bad YAML".to_string()),
            extensions: Some(vec!["myext".to_string()]),
            ..GhAliases::default()
        };
        for line in [
            "pr view 1",
            "api repos/o/r",
            "myext x",
            "help",
            "help run",
            "__complete pr",
            "auth git-credential get",
            "-R o/r pr view 1",
            "upload",
            "issue upload",
        ] {
            assert!(
                matches!(
                    a.resolve(&argv(line)),
                    Resolution::Refused { config: true, .. }
                ),
                "{line}"
            );
        }
        for line in ["", "--version", "--help"] {
            assert_eq!(a.resolve(&argv(line)), Resolution::NotAlias, "{line:?}");
        }
    }

    #[test]
    fn finds_gh_config_like_gh() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| {
                pairs
                    .iter()
                    .find(|(n, _)| *n == k)
                    .map(|(_, v)| v.to_string())
            }
        };
        assert_eq!(
            config_file(&env(&[("GH_CONFIG_DIR", "/c"), ("HOME", "/h")])),
            Some(PathBuf::from("/c/config.yml"))
        );
        assert_eq!(
            config_file(&env(&[("XDG_CONFIG_HOME", "/x"), ("HOME", "/h")])),
            Some(PathBuf::from("/x/gh/config.yml"))
        );
        assert_eq!(
            config_file(&env(&[("HOME", "/h")])),
            Some(PathBuf::from("/h/.config/gh/config.yml"))
        );
        // Without HOME, gh falls back to a path relative to the working directory.
        assert_eq!(
            config_file(&env(&[])),
            Some(PathBuf::from(".config/gh/config.yml"))
        );
        assert_eq!(
            extensions_dir(&env(&[("XDG_DATA_HOME", "/d"), ("HOME", "/h")])),
            PathBuf::from("/d/gh/extensions")
        );
        assert_eq!(
            extensions_dir(&env(&[("HOME", "/h")])),
            PathBuf::from("/h/.local/share/gh/extensions")
        );
    }

    #[test]
    fn loads_aliases_and_extensions_from_disk() {
        let dir = std::env::temp_dir().join(format!("gh-paced-alias-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("cfg")).unwrap();
        std::fs::create_dir_all(dir.join("data/gh/extensions/gh-myext")).unwrap();
        let cfg = dir.join("cfg").to_string_lossy().into_owned();
        let data = dir.join("data").to_string_lossy().into_owned();
        let env = move |k: &str| match k {
            "GH_CONFIG_DIR" => Some(cfg.clone()),
            "XDG_DATA_HOME" => Some(data.clone()),
            _ => None,
        };
        // No configuration file: gh's default alias.
        let a = GhAliases::load(&env);
        assert_eq!(expanded(a.resolve(&argv("co 3"))), argv("pr checkout 3"));
        std::fs::write(
            dir.join("cfg/config.yml"),
            "aliases: {myext: pr list, up: pr list}\n",
        )
        .unwrap();
        let a = GhAliases::load(&env);
        assert!(a.unreadable().is_none());
        // A directory extension: gh registers it, and it runs instead of the alias.
        assert_eq!(a.resolve(&argv("myext")), Resolution::NotAlias);
        assert_eq!(expanded(a.resolve(&argv("up"))), argv("pr list"));
        assert_eq!(a.resolve(&argv("co 3")), Resolution::NotAlias);
        // A binary extension's manifest, or a regular file, can make gh register none.
        let manifest = dir.join("data/gh/extensions/gh-bin/manifest.yml");
        std::fs::create_dir_all(manifest.parent().unwrap()).unwrap();
        std::fs::write(&manifest, "owner: o\n").unwrap();
        assert!(refused(&GhAliases::load(&env).resolve(&argv("myext"))));
        std::fs::remove_file(&manifest).unwrap();
        assert_eq!(
            GhAliases::load(&env).resolve(&argv("myext")),
            Resolution::NotAlias
        );
        std::fs::write(dir.join("data/gh/extensions/gh-file"), "").unwrap();
        assert!(refused(&GhAliases::load(&env).resolve(&argv("myext"))));
        std::fs::remove_file(dir.join("data/gh/extensions/gh-file")).unwrap();
        std::fs::write(
            dir.join("cfg/config.yml"),
            "aliases:\n  up: !!str pr list\n",
        )
        .unwrap();
        let a = GhAliases::load(&env);
        assert!(a.unreadable().is_some());
        // Unreadable: even the registered extension may be an alias gh runs instead.
        for line in ["up", "myext", "pr view 1"] {
            assert!(matches!(
                a.resolve(&argv(line)),
                Resolution::Refused { config: true, .. }
            ));
        }
        std::fs::write(dir.join("cfg/config.yml"), b"aliases:\n  up: \xff\n").unwrap();
        assert!(GhAliases::load(&env).unreadable().is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
