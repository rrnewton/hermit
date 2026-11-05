//! The YAML subset `.herdr-run.yaml` is written in, parsed by herdr-run itself.
//!
//! A policy file must mean the same thing to every reader. Two general-purpose YAML libraries do
//! not agree with each other -- on tabs, directives, tags, explicit-tag semantics, aliases, nesting
//! depth, and every error message -- so herdr-run reads its configuration with this small parser,
//! and the other edition implements the identical rules. The golden cases in
//! `testdata/yaml_cases.json` are run by both editions' unit tests.
//!
//! # What is accepted
//!
//! * UTF-8 text. A leading byte-order mark is ignored; CRLF and CR line ends read as LF.
//! * One document: optional `%YAML 1.1`/`%YAML 1.2` directive lines, an optional `---` line, the
//!   content, and an optional `...` line. Comments start with `#` at a line start or after a space.
//! * Block mappings, block sequences (including the compact forms), and flow collections, which
//!   may span lines.
//! * Single-line plain, single-quoted, and double-quoted scalars (with every YAML escape).
//! * The tags `!!str`, `!!int`, `!!float`, `!!bool`, `!!null` on scalars, `!!seq` on a sequence
//!   and `!!map` on a mapping. `!!int`, `!!bool`, and `!!null` accept exactly the spellings plain
//!   scalars resolve to below; `!!float` accepts any decimal number, including an integer spelling
//!   with leading zeros (`!!float 007` is `7.0`); `!!str` accepts anything.
//! * Plain scalars resolve by herdr-run's own rule. It is close to the YAML 1.2 core schema but not
//!   the same, and this is the whole of it:
//!   - null: empty, `~`, `null`, `Null`, `NULL`;
//!   - bool: `true`, `True`, `TRUE`, `false`, `False`, `FALSE`;
//!   - integer: `[-+]?(0|[1-9][0-9]*)` (decimal WITHOUT leading zeros), `[-+]?0o[0-7]+`,
//!     `[-+]?0x[0-9a-fA-F]+`, and `[-+]?0b[01]+`. The core schema reads `007` as 7, has no `0b`
//!     form, and does not sign `0o`/`0x`; here `007` is a string and `0b101` is 5;
//!   - float: `[-+]?(\.[0-9]+|[0-9]+\.[0-9]*)([eE][-+]?[0-9]+)?` or `[-+]?[0-9]+[eE][-+]?[0-9]+`;
//!     the `.inf` and `.nan` spellings are refused rather than read;
//!   - anything else (`yes`, `on`, `1_000`, `007`) is a string.
//!
//! # What is refused
//!
//! Anchors and aliases, block scalars, explicit keys, multi-line scalars, every other tag, `%TAG`
//! and unknown directives, other `%YAML` versions, a second document, non-string and duplicate
//! keys, merge keys, non-finite numbers, a float that overflows, a tab outside a quoted scalar or
//! a comment, characters YAML does not allow in a stream, a raw U+0085, U+2028, or U+2029 anywhere
//! (YAML 1.1 reads each as a line break and YAML 1.2 does not), and collections nested more than
//! 128 levels deep. An integer whose magnitude exceeds 10^38 reads as 10^38.

use std::fmt;
use std::sync::OnceLock;

use regex::Regex;

use crate::textfmt::quote;

/// Deepest collection nesting accepted.
pub const MAX_DEPTH: usize = 128;

/// Integers are clamped to this magnitude; larger values exceed every configuration bound anyway.
pub const INTEGER_CLAMP: i128 = 100_000_000_000_000_000_000_000_000_000_000_000_000;

const TAB_MESSAGE: &str = "tab characters are only allowed inside quoted scalars and comments";
const FLOW_INDICATORS: &[char] = &[',', '[', ']', '{', '}'];
/// A `:` that starts a later line than its plain flow-mapping key and is followed by text.
const SPLIT_COLON_MESSAGE: &str = "a ':' on a later line than its plain key must be followed by a space: YAML readers disagree on whether it starts the value or continues the key";
const SCALAR_TAGS: &[&str] = &["!!str", "!!int", "!!float", "!!bool", "!!null"];
const ALL_TAGS: &[&str] = &[
    "!!str", "!!int", "!!float", "!!bool", "!!null", "!!seq", "!!map",
];
const NULL_TOKENS: &[&str] = &["", "~", "null", "Null", "NULL"];
const TRUE_TOKENS: &[&str] = &["true", "True", "TRUE"];
const FALSE_TOKENS: &[&str] = &["false", "False", "FALSE"];
const NONFINITE_TOKENS: &[&str] = &[
    ".inf", ".Inf", ".INF", "+.inf", "+.Inf", "+.INF", "-.inf", "-.Inf", "-.INF", ".nan", ".NaN",
    ".NAN",
];

/// One decoded configuration value. Mappings keep their document order.
#[derive(Clone, Debug, PartialEq)]
pub enum Yaml {
    /// `null`, `~`, or an empty value.
    Null,
    /// `true` or `false`.
    Bool(bool),
    /// An integer, clamped to +/-10^38.
    Int(i128),
    /// A finite floating-point number.
    Float(f64),
    /// A string.
    Str(String),
    /// A sequence.
    Seq(Vec<Yaml>),
    /// A mapping with string keys, in document order.
    Map(Vec<(String, Yaml)>),
}

impl Yaml {
    /// Name this value's type in the JSON vocabulary the configuration messages use.
    #[must_use]
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::Null => "null",
            Self::Bool(_) => "bool",
            Self::Int(_) | Self::Float(_) => "number",
            Self::Str(_) => "string",
            Self::Seq(_) => "array",
            Self::Map(_) => "object",
        }
    }
}

/// A configuration document outside the accepted subset, with a 1-based position.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct YamlError {
    /// What is wrong, without the position.
    pub message: String,
    /// 1-based line.
    pub line: usize,
    /// 1-based column, counted in characters.
    pub column: usize,
}

impl fmt::Display for YamlError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} at line {} column {}",
            self.message, self.line, self.column
        )
    }
}

type Parsed<T> = std::result::Result<T, YamlError>;

/// A scalar before resolution: its text, whether it was plain, and where it started.
struct Scalar {
    text: String,
    plain: bool,
    line: usize,
    column: usize,
}

/// An inline node before a tag is applied.
enum Inline {
    Scalar(Scalar),
    Collection(Yaml),
}

#[derive(Clone, Copy)]
struct Tag {
    name: &'static str,
    line: usize,
    column: usize,
}

#[derive(Clone, Copy)]
struct Opener {
    line: usize,
    column: usize,
    kind: &'static str,
}

fn regex(slot: &'static OnceLock<Regex>, pattern: &str) -> &'static Regex {
    slot.get_or_init(|| Regex::new(pattern).expect("valid YAML scalar pattern"))
}

fn int_dec() -> &'static Regex {
    static SLOT: OnceLock<Regex> = OnceLock::new();
    regex(&SLOT, r"^[-+]?(?:0|[1-9][0-9]*)$")
}

fn int_prefixed() -> &'static Regex {
    static SLOT: OnceLock<Regex> = OnceLock::new();
    regex(&SLOT, r"^([-+]?)0(?:o([0-7]+)|x([0-9a-fA-F]+)|b([01]+))$")
}

fn float_plain() -> &'static Regex {
    static SLOT: OnceLock<Regex> = OnceLock::new();
    regex(
        &SLOT,
        r"^(?:[-+]?(?:\.[0-9]+|[0-9]+\.[0-9]*)(?:[eE][-+]?[0-9]+)?|[-+]?[0-9]+[eE][-+]?[0-9]+)$",
    )
}

fn float_tagged() -> &'static Regex {
    static SLOT: OnceLock<Regex> = OnceLock::new();
    regex(
        &SLOT,
        r"^[-+]?(?:\.[0-9]+|[0-9]+(?:\.[0-9]*)?)(?:[eE][-+]?[0-9]+)?$",
    )
}

fn clamp(negative: bool, magnitude: Option<u128>) -> i128 {
    let magnitude = match magnitude {
        Some(value) if value <= INTEGER_CLAMP as u128 => value as i128,
        _ => INTEGER_CLAMP,
    };
    if negative {
        -magnitude
    } else {
        magnitude
    }
}

fn accumulate(digits: &str, base: u32) -> Option<u128> {
    let mut value: u128 = 0;
    for digit in digits.chars() {
        let digit = u128::from(digit.to_digit(base)?);
        value = value.checked_mul(u128::from(base))?.checked_add(digit)?;
    }
    Some(value)
}

/// Resolve an integer spelling (see the module documentation), clamped to +/-10^38, or `None`.
fn integer(text: &str) -> Option<i128> {
    if int_dec().is_match(text) {
        let negative = text.starts_with('-');
        let digits = text.trim_start_matches(['+', '-']);
        if digits.chars().count() > 39 {
            return Some(clamp(negative, None));
        }
        return Some(clamp(negative, accumulate(digits, 10)));
    }
    let captures = int_prefixed().captures(text)?;
    let negative = captures.get(1).is_some_and(|sign| sign.as_str() == "-");
    let (digits, base) = if let Some(octal) = captures.get(2) {
        (octal.as_str(), 8)
    } else if let Some(hex) = captures.get(3) {
        (hex.as_str(), 16)
    } else {
        (captures.get(4)?.as_str(), 2)
    };
    let digits = digits.trim_start_matches('0');
    let digits = if digits.is_empty() { "0" } else { digits };
    if digits.chars().count() > 200 {
        return Some(clamp(negative, None));
    }
    Some(clamp(negative, accumulate(digits, base)))
}

fn collection_kind(tag: &str) -> &'static str {
    match tag {
        "!!seq" => "sequence",
        "!!map" => "mapping",
        _ => "scalar",
    }
}

fn escape_for(code: char) -> Option<char> {
    Some(match code {
        '0' => '\u{0}',
        'a' => '\u{7}',
        'b' => '\u{8}',
        't' | '\t' => '\t',
        'n' => '\n',
        'v' => '\u{b}',
        'f' => '\u{c}',
        'r' => '\r',
        'e' => '\u{1b}',
        ' ' => ' ',
        '"' => '"',
        '/' => '/',
        '\\' => '\\',
        'N' => '\u{85}',
        '_' => '\u{a0}',
        'L' => '\u{2028}',
        'P' => '\u{2029}',
        _ => return None,
    })
}

/// NEXT LINE, LINE SEPARATOR, and PARAGRAPH SEPARATOR. YAML 1.1 breaks a line at each of them and
/// YAML 1.2 at none, so a file holding one raw means different things to different readers: a deny
/// entry written `-c<U+2028>` is `-c` to one and a longer name nothing matches to the other. They
/// are refused anywhere in the file, comments included; a double-quoted escape (`\N`, `\L`, `\P`)
/// still writes one explicitly, and the configuration rules then decide.
fn ambiguous_break(character: char) -> bool {
    matches!(character, '\u{85}' | '\u{2028}' | '\u{2029}')
}

fn acceptable(character: char) -> bool {
    let code = u32::from(character);
    character == '\t'
        || (0x20..=0x7e).contains(&code)
        || (0xa0..=0xd7ff).contains(&code)
        || (0xe000..=0xfffd).contains(&code)
        || (0x10000..=0x10ffff).contains(&code)
}

struct Parser {
    lines: Vec<Vec<char>>,
}

impl Parser {
    fn new(text: &str) -> Parsed<Self> {
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        let lines: Vec<Vec<char>> = normalized
            .split('\n')
            .map(|line| line.chars().collect())
            .collect();
        for (number, line) in lines.iter().enumerate() {
            for (column, character) in line.iter().enumerate() {
                if ambiguous_break(*character) {
                    return Err(YamlError {
                        message: format!(
                            "U+{:04X} is not allowed: YAML 1.1 reads it as a line break and YAML 1.2 does not",
                            u32::from(*character)
                        ),
                        line: number + 1,
                        column: column + 1,
                    });
                }
                if !acceptable(*character) {
                    return Err(YamlError {
                        message: format!("unacceptable character U+{:04X}", u32::from(*character)),
                        line: number + 1,
                        column: column + 1,
                    });
                }
            }
        }
        Ok(Self { lines })
    }

    fn count(&self) -> usize {
        self.lines.len()
    }

    fn error(message: impl Into<String>, line: usize, column: usize) -> YamlError {
        YamlError {
            message: message.into(),
            line: line + 1,
            column: column + 1,
        }
    }

    fn char_at(&self, line: usize, column: usize) -> Option<char> {
        self.lines[line].get(column).copied()
    }

    fn is_blank(&self, line: usize) -> bool {
        let text = &self.lines[line];
        match text.iter().position(|character| *character != ' ') {
            None => true,
            Some(index) => text[index] == '#',
        }
    }

    fn indent(&self, line: usize) -> usize {
        self.lines[line]
            .iter()
            .take_while(|character| **character == ' ')
            .count()
    }

    fn is_marker(&self, line: usize, marker: &str) -> bool {
        let text = &self.lines[line];
        let marker: Vec<char> = marker.chars().collect();
        text.starts_with(&marker) && (text.len() == 3 || matches!(text[3], ' ' | '\t'))
    }

    fn is_document_boundary(&self, line: usize) -> bool {
        self.is_marker(line, "---") || self.is_marker(line, "...")
    }

    fn next_content(&self, mut line: usize) -> Option<usize> {
        while line < self.count() {
            if !self.is_blank(line) {
                return Some(line);
            }
            line += 1;
        }
        None
    }

    fn skip_spaces(&self, line: usize, mut column: usize) -> Parsed<usize> {
        let text = &self.lines[line];
        while column < text.len() {
            match text[column] {
                ' ' => column += 1,
                '\t' => return Err(Self::error(TAB_MESSAGE, line, column)),
                _ => break,
            }
        }
        Ok(column)
    }

    fn comment_or_end(&self, line: usize, column: usize) -> bool {
        let text = &self.lines[line];
        column >= text.len() || (text[column] == '#' && column > 0 && text[column - 1] == ' ')
    }

    fn check_trailing(&self, line: usize, column: usize) -> Parsed<()> {
        let column = self.skip_spaces(line, column)?;
        if self.comment_or_end(line, column) {
            return Ok(());
        }
        Err(Self::error("unexpected text after a value", line, column))
    }

    fn is_dash(&self, line: usize, column: usize) -> bool {
        let text = &self.lines[line];
        column < text.len()
            && text[column] == '-'
            && (column + 1 == text.len() || matches!(text[column + 1], ' ' | '\t'))
    }

    // ---- tags and scalars ----------------------------------------------------------------------

    fn parse_tag(&self, line: usize, column: usize, flow: bool) -> Parsed<(Tag, usize)> {
        let text = &self.lines[line];
        let mut end = column;
        while end < text.len()
            && !matches!(text[end], ' ' | '\t')
            && !(flow && FLOW_INDICATORS.contains(&text[end]))
        {
            end += 1;
        }
        let tag: String = text[column..end].iter().collect();
        let Some(name) = ALL_TAGS.iter().find(|candidate| **candidate == tag) else {
            return Err(Self::error(
                format!("unsupported tag {}", quote(&tag)),
                line,
                column,
            ));
        };
        Ok((Tag { name, line, column }, end))
    }

    fn quote_end(text: &[char], column: usize) -> Option<usize> {
        let quote_char = text[column];
        let mut index = column + 1;
        while index < text.len() {
            let character = text[index];
            if quote_char == '\'' {
                if character == '\'' {
                    if index + 1 < text.len() && text[index + 1] == '\'' {
                        index += 2;
                        continue;
                    }
                    return Some(index + 1);
                }
            } else {
                if character == '\\' {
                    index += 2;
                    continue;
                }
                if character == '"' {
                    return Some(index + 1);
                }
            }
            index += 1;
        }
        None
    }

    fn parse_quoted(&self, line: usize, column: usize) -> Parsed<(Scalar, usize)> {
        let text = &self.lines[line];
        let quote_char = text[column];
        let mut index = column + 1;
        let mut out = String::new();
        loop {
            if index >= text.len() {
                return Err(Self::error(
                    "quoted scalars must not span lines",
                    line,
                    column,
                ));
            }
            let character = text[index];
            if quote_char == '\'' {
                if character == '\'' {
                    if index + 1 < text.len() && text[index + 1] == '\'' {
                        out.push('\'');
                        index += 2;
                        continue;
                    }
                    index += 1;
                    break;
                }
                out.push(character);
                index += 1;
                continue;
            }
            if character == '"' {
                index += 1;
                break;
            }
            if character != '\\' {
                out.push(character);
                index += 1;
                continue;
            }
            if index + 1 >= text.len() {
                return Err(Self::error(
                    "quoted scalars must not span lines",
                    line,
                    column,
                ));
            }
            let code = text[index + 1];
            if let Some(replacement) = escape_for(code) {
                out.push(replacement);
                index += 2;
                continue;
            }
            let width = match code {
                'x' => 2,
                'u' => 4,
                'U' => 8,
                _ => {
                    return Err(Self::error(
                        format!("invalid escape {}", quote(&format!("\\{code}"))),
                        line,
                        index,
                    ))
                }
            };
            let end = (index + 2 + width).min(text.len());
            let digits: String = text[index + 2..end].iter().collect();
            let sequence: String = text[index..end].iter().collect();
            if digits.chars().count() != width
                || !digits.chars().all(|digit| digit.is_ascii_hexdigit())
            {
                return Err(Self::error(
                    format!("invalid escape {}", quote(&sequence)),
                    line,
                    index,
                ));
            }
            let value = u32::from_str_radix(&digits, 16).unwrap_or(u32::MAX);
            let Some(decoded) = char::from_u32(value) else {
                return Err(Self::error(
                    format!(
                        "escape {} is not a valid Unicode scalar value",
                        quote(&sequence)
                    ),
                    line,
                    index,
                ));
            };
            out.push(decoded);
            index += 2 + width;
        }
        Ok((
            Scalar {
                text: out,
                plain: false,
                line,
                column,
            },
            index,
        ))
    }

    fn plain_start_error(&self, line: usize, column: usize, flow: bool) -> Parsed<()> {
        let character = self.char_at(line, column).unwrap_or('\0');
        let following = self.char_at(line, column + 1);
        let safe_follow = match following {
            None | Some(' ' | '\t') => false,
            Some(next) => !(flow && FLOW_INDICATORS.contains(&next)),
        };
        if matches!(character, '&' | '*') {
            return Err(Self::error(
                "anchors and aliases are not supported",
                line,
                column,
            ));
        }
        if matches!(character, '|' | '>') {
            return Err(Self::error(
                "block scalars (| and >) are not supported",
                line,
                column,
            ));
        }
        // In a flow collection `?x` is a plain scalar to a YAML 1.2 reader and an explicit key `x`
        // to a YAML 1.1 reader, so it is refused there rather than read one way.
        if character == '?' && (flow || !safe_follow) {
            return Err(Self::error(
                "explicit mapping keys (?) are not supported",
                line,
                column,
            ));
        }
        if ",[]{}#%@`!".contains(character)
            || (matches!(character, '-' | ':') && !safe_follow)
            || (character == ':' && flow)
        {
            return Err(Self::error(
                format!(
                    "a plain scalar cannot start with {}",
                    quote(&character.to_string())
                ),
                line,
                column,
            ));
        }
        Ok(())
    }

    fn parse_plain(&self, line: usize, column: usize, flow: bool) -> Parsed<(Scalar, usize)> {
        self.plain_start_error(line, column, flow)?;
        let text = &self.lines[line];
        let mut index = column;
        while index < text.len() {
            let character = text[index];
            if character == '\t' {
                return Err(Self::error(TAB_MESSAGE, line, index));
            }
            if character == ':' {
                let following = text.get(index + 1).copied();
                let ends = match following {
                    None | Some(' ' | '\t') => true,
                    Some(next) => flow && FLOW_INDICATORS.contains(&next),
                };
                if ends {
                    break;
                }
            }
            if character == '#' && index > column && text[index - 1] == ' ' {
                break;
            }
            if flow && FLOW_INDICATORS.contains(&character) {
                break;
            }
            index += 1;
        }
        let mut end = index;
        while end > column && text[end - 1] == ' ' {
            end -= 1;
        }
        Ok((
            Scalar {
                text: text[column..end].iter().collect(),
                plain: true,
                line,
                column,
            },
            end,
        ))
    }

    // ---- resolution ----------------------------------------------------------------------------

    fn resolve(&self, scalar: Scalar, tag: Option<Tag>) -> Parsed<Yaml> {
        let Some(tag) = tag else {
            if !scalar.plain {
                return Ok(Yaml::Str(scalar.text));
            }
            return self.resolve_plain(scalar);
        };
        let text = scalar.text.as_str();
        match tag.name {
            "!!str" => return Ok(Yaml::Str(scalar.text)),
            "!!null" => {
                if matches!(text, "~" | "null" | "Null" | "NULL")
                    || (text.is_empty() && scalar.plain)
                {
                    return Ok(Yaml::Null);
                }
            }
            "!!bool" => {
                if TRUE_TOKENS.contains(&text) {
                    return Ok(Yaml::Bool(true));
                }
                if FALSE_TOKENS.contains(&text) {
                    return Ok(Yaml::Bool(false));
                }
            }
            "!!int" => {
                if let Some(value) = integer(text) {
                    return Ok(Yaml::Int(value));
                }
            }
            "!!float" => {
                if NONFINITE_TOKENS.contains(&text) {
                    return Err(Self::error(
                        "non-finite numbers are not supported",
                        scalar.line,
                        scalar.column,
                    ));
                }
                if float_tagged().is_match(text) {
                    return Self::finite(text, &scalar);
                }
            }
            other => {
                return Err(Self::error(
                    format!("{other} can only tag a {}", collection_kind(other)),
                    tag.line,
                    tag.column,
                ))
            }
        }
        Err(Self::error(
            format!("{} is not a valid {} value", quote(text), tag.name),
            scalar.line,
            scalar.column,
        ))
    }

    fn resolve_plain(&self, scalar: Scalar) -> Parsed<Yaml> {
        let text = scalar.text.as_str();
        if NULL_TOKENS.contains(&text) {
            return Ok(Yaml::Null);
        }
        if TRUE_TOKENS.contains(&text) {
            return Ok(Yaml::Bool(true));
        }
        if FALSE_TOKENS.contains(&text) {
            return Ok(Yaml::Bool(false));
        }
        if let Some(value) = integer(text) {
            return Ok(Yaml::Int(value));
        }
        if NONFINITE_TOKENS.contains(&text) {
            return Err(Self::error(
                "non-finite numbers are not supported",
                scalar.line,
                scalar.column,
            ));
        }
        if float_plain().is_match(text) {
            return Self::finite(text, &scalar);
        }
        Ok(Yaml::Str(scalar.text))
    }

    fn finite(text: &str, scalar: &Scalar) -> Parsed<Yaml> {
        let value = text.parse::<f64>().unwrap_or(f64::INFINITY);
        if !value.is_finite() {
            return Err(Self::error(
                "number out of range",
                scalar.line,
                scalar.column,
            ));
        }
        Ok(Yaml::Float(value))
    }

    fn tag_collection(value: Yaml, tag: Option<Tag>) -> Parsed<Yaml> {
        let Some(tag) = tag else {
            return Ok(value);
        };
        let kind = if matches!(value, Yaml::Seq(_)) {
            "!!seq"
        } else {
            "!!map"
        };
        if tag.name == kind {
            return Ok(value);
        }
        if SCALAR_TAGS.contains(&tag.name) {
            return Err(Self::error(
                format!("{} can only tag a scalar", tag.name),
                tag.line,
                tag.column,
            ));
        }
        Err(Self::error(
            format!("{} can only tag a {}", tag.name, collection_kind(tag.name)),
            tag.line,
            tag.column,
        ))
    }

    fn apply(&self, inline: Inline, tag: Option<Tag>) -> Parsed<Yaml> {
        match inline {
            Inline::Scalar(scalar) => self.resolve(scalar, tag),
            Inline::Collection(value) => Self::tag_collection(value, tag),
        }
    }

    // ---- keys ----------------------------------------------------------------------------------

    fn find_key_colon(&self, line: usize, column: usize) -> Option<usize> {
        let text = &self.lines[line];
        let mut index = column;
        if index < text.len() && text[index] == '!' {
            while index < text.len() && !matches!(text[index], ' ' | '\t') {
                index += 1;
            }
            while index < text.len() && text[index] == ' ' {
                index += 1;
            }
        }
        if index >= text.len() {
            return None;
        }
        let character = text[index];
        if character == '!' {
            return None;
        }
        if matches!(character, '"' | '\'') {
            let mut end = Self::quote_end(text, index)?;
            while end < text.len() && text[end] == ' ' {
                end += 1;
            }
            if end < text.len()
                && text[end] == ':'
                && (end + 1 == text.len() || matches!(text[end + 1], ' ' | '\t'))
            {
                return Some(end);
            }
            return None;
        }
        if "[{&*|>%@`#,]}".contains(character) {
            return None;
        }
        if matches!(character, '-' | '?' | ':')
            && (index + 1 == text.len() || matches!(text[index + 1], ' ' | '\t'))
        {
            return None;
        }
        let start = index;
        while index < text.len() {
            let character = text[index];
            if character == ':'
                && (index + 1 == text.len() || matches!(text[index + 1], ' ' | '\t'))
            {
                return Some(index);
            }
            if character == '#' && index > start && matches!(text[index - 1], ' ' | '\t') {
                return None;
            }
            index += 1;
        }
        None
    }

    fn key_text(&self, scalar: Scalar, tag: Option<Tag>) -> Parsed<String> {
        if let Some(tag) = tag {
            if !SCALAR_TAGS.contains(&tag.name) {
                return Err(Self::error(
                    format!("{} can only tag a {}", tag.name, collection_kind(tag.name)),
                    tag.line,
                    tag.column,
                ));
            }
        }
        let (line, column) = (scalar.line, scalar.column);
        match self.resolve(scalar, tag)? {
            Yaml::Str(key) => Ok(key),
            _ => Err(Self::error("mapping keys must be strings", line, column)),
        }
    }

    fn parse_block_key(
        &self,
        line: usize,
        mut column: usize,
        colon: usize,
    ) -> Parsed<(String, usize, usize)> {
        let text = &self.lines[line];
        let mut tag = None;
        if text[column] == '!' {
            let (parsed, after) = self.parse_tag(line, column, false)?;
            tag = Some(parsed);
            column = self.skip_spaces(line, after)?;
        }
        let scalar = if matches!(text[column], '"' | '\'') {
            self.parse_quoted(line, column)?.0
        } else {
            let raw = &text[column..colon];
            if let Some(offset) = raw.iter().position(|character| *character == '\t') {
                return Err(Self::error(TAB_MESSAGE, line, column + offset));
            }
            let mut end = raw.len();
            while end > 0 && raw[end - 1] == ' ' {
                end -= 1;
            }
            Scalar {
                text: raw[..end].iter().collect(),
                plain: true,
                line,
                column,
            }
        };
        let key_column = scalar.column;
        let key = self.key_text(scalar, tag)?;
        Ok((key, key_column, colon + 1))
    }

    fn insert(mapping: &[(String, Yaml)], key: &str, line: usize, column: usize) -> Parsed<()> {
        if mapping.iter().any(|(existing, _)| existing == key) {
            return Err(Self::error(
                format!("duplicate mapping key {}", quote(key)),
                line,
                column,
            ));
        }
        if key == "<<" {
            return Err(Self::error(
                "YAML merge keys are not supported",
                line,
                column,
            ));
        }
        Ok(())
    }

    // ---- block structure -----------------------------------------------------------------------

    fn check_depth(depth: usize, line: usize, column: usize) -> Parsed<()> {
        if depth > MAX_DEPTH {
            return Err(Self::error(
                format!("collections nested more than {MAX_DEPTH} levels deep"),
                line,
                column,
            ));
        }
        Ok(())
    }

    fn parse_block_node(
        &self,
        line: usize,
        column: usize,
        owner: isize,
        depth: usize,
    ) -> Parsed<(Yaml, usize)> {
        if self.is_dash(line, column) {
            return self.parse_block_sequence(line, column, depth);
        }
        if let Some(colon) = self.find_key_colon(line, column) {
            return self.parse_block_mapping(line, column, colon, depth);
        }
        self.parse_inline_value(line, column, owner, depth, None)
    }

    fn parse_inline_value(
        &self,
        line: usize,
        mut column: usize,
        owner: isize,
        depth: usize,
        mut tag: Option<Tag>,
    ) -> Parsed<(Yaml, usize)> {
        let length = self.lines[line].len();
        if tag.is_none() && self.char_at(line, column) == Some('!') {
            let (parsed, after) = self.parse_tag(line, column, false)?;
            tag = Some(parsed);
            column = self.skip_spaces(line, after)?;
            if column >= length || self.comment_or_end(line, column) {
                let value = self.resolve(
                    Scalar {
                        text: String::new(),
                        plain: true,
                        line,
                        column,
                    },
                    tag,
                )?;
                self.check_continuation(line, owner, true)?;
                return Ok((value, line + 1));
            }
        }
        let (inline, end_line, end_column, plain) = self.parse_inline(line, column, depth)?;
        let value = self.apply(inline, tag)?;
        self.check_trailing(end_line, end_column)?;
        self.check_continuation(end_line, owner, plain)?;
        Ok((value, end_line + 1))
    }

    fn check_continuation(&self, line: usize, owner: isize, plain: bool) -> Parsed<()> {
        let Some(following) = self.next_content(line + 1) else {
            return Ok(());
        };
        if self.is_document_boundary(following) {
            return Ok(());
        }
        let indent = self.indent(following);
        if indent as isize > owner {
            let message = if plain {
                "plain scalars must not span lines"
            } else {
                "unexpected indentation"
            };
            return Err(Self::error(message, following, indent));
        }
        Ok(())
    }

    fn parse_value(
        &self,
        line: usize,
        column: usize,
        owner: isize,
        depth: usize,
        in_sequence: bool,
    ) -> Parsed<(Yaml, usize)> {
        let length = self.lines[line].len();
        let mut column = self.skip_spaces(line, column)?;
        let mut tag = None;
        if self.char_at(line, column) == Some('!') {
            let (parsed, after) = self.parse_tag(line, column, false)?;
            tag = Some(parsed);
            column = self.skip_spaces(line, after)?;
        }
        if matches!(self.char_at(line, column), Some('&' | '*')) {
            return Err(Self::error(
                "anchors and aliases are not supported",
                line,
                column,
            ));
        }
        if column >= length || self.comment_or_end(line, column) {
            return self.parse_following(line, owner, depth, in_sequence, tag);
        }
        if self.is_dash(line, column) {
            if !in_sequence {
                return Err(Self::error(
                    "block sequence entries are not allowed here",
                    line,
                    column,
                ));
            }
            let (value, following) = self.parse_block_sequence(line, column, depth)?;
            return Ok((Self::tag_collection(value, tag)?, following));
        }
        if let Some(colon) = self.find_key_colon(line, column) {
            if !in_sequence {
                return Err(Self::error(
                    "mapping values are not allowed here",
                    line,
                    colon,
                ));
            }
            let (value, following) = self.parse_block_mapping(line, column, colon, depth)?;
            return Ok((Self::tag_collection(value, tag)?, following));
        }
        self.parse_inline_value(line, column, owner, depth, tag)
    }

    fn parse_following(
        &self,
        line: usize,
        owner: isize,
        depth: usize,
        in_sequence: bool,
        tag: Option<Tag>,
    ) -> Parsed<(Yaml, usize)> {
        if let Some(following) = self.next_content(line + 1) {
            if !self.is_document_boundary(following) {
                let indent = self.indent(following);
                if indent as isize > owner {
                    if tag.is_some() && self.lines[following][indent] == '!' {
                        return Err(Self::error(
                            "a node can have only one tag",
                            following,
                            indent,
                        ));
                    }
                    let (value, after) = self.parse_block_node(following, indent, owner, depth)?;
                    let Some(tag) = tag else {
                        return Ok((value, after));
                    };
                    if matches!(value, Yaml::Seq(_) | Yaml::Map(_)) {
                        return Ok((Self::tag_collection(value, Some(tag))?, after));
                    }
                    return Err(Self::error(
                        "a tag must be on the same line as its scalar",
                        tag.line,
                        tag.column,
                    ));
                }
                if !in_sequence && indent as isize == owner && self.is_dash(following, indent) {
                    let (value, after) = self.parse_block_sequence(following, indent, depth)?;
                    return Ok((Self::tag_collection(value, tag)?, after));
                }
            }
        }
        let Some(tag) = tag else {
            return Ok((Yaml::Null, line + 1));
        };
        let value = self.resolve(
            Scalar {
                text: String::new(),
                plain: true,
                line,
                column: tag.column,
            },
            Some(tag),
        )?;
        Ok((value, line + 1))
    }

    fn parse_block_mapping(
        &self,
        mut line: usize,
        mut column: usize,
        mut colon: usize,
        depth: usize,
    ) -> Parsed<(Yaml, usize)> {
        Self::check_depth(depth, line, column)?;
        let mut mapping: Vec<(String, Yaml)> = Vec::new();
        let indent = column;
        loop {
            let (key, key_column, after) = self.parse_block_key(line, column, colon)?;
            Self::insert(&mapping, &key, line, key_column)?;
            let (value, following) =
                self.parse_value(line, after, indent as isize, depth + 1, false)?;
            mapping.push((key, value));
            line = following;
            let Some(next_line) = self.next_content(line) else {
                return Ok((Yaml::Map(mapping), line));
            };
            if self.is_document_boundary(next_line) {
                return Ok((Yaml::Map(mapping), line));
            }
            let next_indent = self.indent(next_line);
            if next_indent < indent {
                return Ok((Yaml::Map(mapping), line));
            }
            if next_indent > indent {
                return Err(Self::error(
                    "unexpected indentation",
                    next_line,
                    next_indent,
                ));
            }
            if self.is_dash(next_line, indent) {
                return Err(Self::error(
                    "block sequence entries are not allowed here",
                    next_line,
                    indent,
                ));
            }
            let Some(found) = self.find_key_colon(next_line, indent) else {
                return Err(Self::error(
                    "expected a mapping key followed by ':'",
                    next_line,
                    indent,
                ));
            };
            line = next_line;
            column = indent;
            colon = found;
        }
    }

    fn parse_block_sequence(
        &self,
        mut line: usize,
        mut column: usize,
        depth: usize,
    ) -> Parsed<(Yaml, usize)> {
        Self::check_depth(depth, line, column)?;
        let mut items = Vec::new();
        let indent = column;
        loop {
            let (value, following) =
                self.parse_value(line, column + 1, indent as isize, depth + 1, true)?;
            items.push(value);
            line = following;
            let Some(next_line) = self.next_content(line) else {
                return Ok((Yaml::Seq(items), line));
            };
            if self.is_document_boundary(next_line) {
                return Ok((Yaml::Seq(items), line));
            }
            let next_indent = self.indent(next_line);
            if next_indent < indent {
                return Ok((Yaml::Seq(items), line));
            }
            if next_indent > indent {
                return Err(Self::error(
                    "unexpected indentation",
                    next_line,
                    next_indent,
                ));
            }
            if !self.is_dash(next_line, indent) {
                return Ok((Yaml::Seq(items), line));
            }
            line = next_line;
            column = indent;
        }
    }

    // ---- inline and flow nodes -----------------------------------------------------------------

    fn parse_inline(
        &self,
        line: usize,
        column: usize,
        depth: usize,
    ) -> Parsed<(Inline, usize, usize, bool)> {
        match self.char_at(line, column) {
            Some('[') => {
                let (items, end_line, end_column) =
                    self.parse_flow_sequence(line, column, depth)?;
                Ok((Inline::Collection(items), end_line, end_column, false))
            }
            Some('{') => {
                let (mapping, end_line, end_column) =
                    self.parse_flow_mapping(line, column, depth)?;
                Ok((Inline::Collection(mapping), end_line, end_column, false))
            }
            Some('"' | '\'') => {
                let (scalar, end) = self.parse_quoted(line, column)?;
                Ok((Inline::Scalar(scalar), line, end, false))
            }
            _ => {
                let (scalar, end) = self.parse_plain(line, column, false)?;
                Ok((Inline::Scalar(scalar), line, end, true))
            }
        }
    }

    fn skip_flow(
        &self,
        mut line: usize,
        mut column: usize,
        opener: Opener,
    ) -> Parsed<(usize, usize)> {
        loop {
            if line >= self.count() {
                return Err(Self::error(
                    format!("unterminated flow {}", opener.kind),
                    opener.line,
                    opener.column,
                ));
            }
            let text = &self.lines[line];
            if column >= text.len() {
                line += 1;
                column = 0;
                if line < self.count() && self.is_document_boundary(line) {
                    return Err(Self::error(
                        format!("unterminated flow {}", opener.kind),
                        opener.line,
                        opener.column,
                    ));
                }
                continue;
            }
            match text[column] {
                ' ' => column += 1,
                '\t' => return Err(Self::error(TAB_MESSAGE, line, column)),
                '#' if column == 0 || text[column - 1] == ' ' => column = text.len(),
                _ => return Ok((line, column)),
            }
        }
    }

    fn parse_flow_node(
        &self,
        mut line: usize,
        mut column: usize,
        depth: usize,
        opener: Opener,
    ) -> Parsed<(Yaml, usize, usize, bool)> {
        let mut tag = None;
        if self.char_at(line, column) == Some('!') {
            let (parsed, after) = self.parse_tag(line, column, true)?;
            tag = Some(parsed);
            (line, column) = self.skip_flow(line, after, opener)?;
        }
        match self.char_at(line, column) {
            Some(',' | ']' | '}') => {
                if tag.is_none() {
                    return Err(Self::error(
                        "expected a value in a flow collection",
                        line,
                        column,
                    ));
                }
                let value = self.resolve(
                    Scalar {
                        text: String::new(),
                        plain: true,
                        line,
                        column,
                    },
                    tag,
                )?;
                Ok((value, line, column, false))
            }
            Some('[') => {
                let (items, end_line, end_column) =
                    self.parse_flow_sequence(line, column, depth)?;
                Ok((
                    Self::tag_collection(items, tag)?,
                    end_line,
                    end_column,
                    false,
                ))
            }
            Some('{') => {
                let (mapping, end_line, end_column) =
                    self.parse_flow_mapping(line, column, depth)?;
                Ok((
                    Self::tag_collection(mapping, tag)?,
                    end_line,
                    end_column,
                    false,
                ))
            }
            Some('"' | '\'') => {
                let (scalar, end) = self.parse_quoted(line, column)?;
                Ok((self.resolve(scalar, tag)?, line, end, false))
            }
            _ => {
                let (scalar, end) = self.parse_plain(line, column, true)?;
                Ok((self.resolve(scalar, tag)?, line, end, true))
            }
        }
    }

    fn flow_separator_error(
        line: usize,
        column: usize,
        crossed: bool,
        plain: bool,
        expected: &str,
    ) -> YamlError {
        if plain && crossed {
            return Self::error("plain scalars must not span lines", line, column);
        }
        Self::error(expected, line, column)
    }

    fn parse_flow_sequence(
        &self,
        mut line: usize,
        mut column: usize,
        depth: usize,
    ) -> Parsed<(Yaml, usize, usize)> {
        Self::check_depth(depth, line, column)?;
        let opener = Opener {
            line,
            column,
            kind: "sequence",
        };
        let mut items = Vec::new();
        column += 1;
        loop {
            (line, column) = self.skip_flow(line, column, opener)?;
            if self.char_at(line, column) == Some(']') {
                return Ok((Yaml::Seq(items), line, column + 1));
            }
            let (value, end_line, end_column, plain) =
                self.parse_flow_node(line, column, depth + 1, opener)?;
            items.push(value);
            (line, column) = self.skip_flow(end_line, end_column, opener)?;
            match self.char_at(line, column) {
                Some(',') => {
                    column += 1;
                }
                Some(']') => return Ok((Yaml::Seq(items), line, column + 1)),
                Some(':') => {
                    return Err(Self::error(
                        "a flow sequence entry cannot be a mapping",
                        line,
                        column,
                    ))
                }
                _ => {
                    return Err(Self::flow_separator_error(
                        line,
                        column,
                        line != end_line,
                        plain,
                        "expected ',' or ']' in a flow sequence",
                    ))
                }
            }
        }
    }

    fn parse_flow_mapping(
        &self,
        mut line: usize,
        mut column: usize,
        depth: usize,
    ) -> Parsed<(Yaml, usize, usize)> {
        Self::check_depth(depth, line, column)?;
        let opener = Opener {
            line,
            column,
            kind: "mapping",
        };
        let mut mapping: Vec<(String, Yaml)> = Vec::new();
        column += 1;
        loop {
            (line, column) = self.skip_flow(line, column, opener)?;
            let mut character = self.char_at(line, column);
            if character == Some('}') {
                return Ok((Yaml::Map(mapping), line, column + 1));
            }
            let mut tag = None;
            if character == Some('!') {
                let (parsed, after) = self.parse_tag(line, column, true)?;
                tag = Some(parsed);
                (line, column) = self.skip_flow(line, after, opener)?;
                character = self.char_at(line, column);
            }
            if matches!(character, Some('[' | '{')) {
                return Err(Self::error("mapping keys must be strings", line, column));
            }
            if matches!(character, Some(',' | '}')) {
                return Err(Self::error(
                    "expected a value in a flow collection",
                    line,
                    column,
                ));
            }
            let (scalar, key_end, mut plain) = if matches!(character, Some('"' | '\'')) {
                let (scalar, end) = self.parse_quoted(line, column)?;
                (scalar, end, false)
            } else {
                let (scalar, end) = self.parse_plain(line, column, true)?;
                (scalar, end, true)
            };
            let (key_line, key_column) = (scalar.line, scalar.column);
            let key = self.key_text(scalar, tag)?;
            Self::insert(&mapping, &key, key_line, key_column)?;
            let mut last_line = line;
            (line, column) = self.skip_flow(line, key_end, opener)?;
            let mut value = Yaml::Null;
            match self.char_at(line, column) {
                Some(':') => {
                    let text_follows = match self.char_at(line, column + 1) {
                        None | Some(' ' | '\t') => false,
                        Some(next) => !FLOW_INDICATORS.contains(&next),
                    };
                    if plain && line != last_line && text_follows {
                        // `{x\n :y}`: other readers continue the plain key (`x :y`); refused
                        // rather than read as `x: y`.
                        return Err(Self::error(SPLIT_COLON_MESSAGE, line, column));
                    }
                    (line, column) = self.skip_flow(line, column + 1, opener)?;
                    if matches!(self.char_at(line, column), Some(',' | '}')) {
                        plain = false;
                        last_line = line;
                    } else {
                        let (parsed, end_line, end_column, was_plain) =
                            self.parse_flow_node(line, column, depth + 1, opener)?;
                        value = parsed;
                        plain = was_plain;
                        last_line = end_line;
                        (line, column) = self.skip_flow(end_line, end_column, opener)?;
                    }
                }
                Some(',' | '}') => {}
                _ => {
                    return Err(Self::flow_separator_error(
                        line,
                        column,
                        line != last_line,
                        plain,
                        "expected ':' after a flow mapping key",
                    ))
                }
            }
            mapping.push((key, value));
            match self.char_at(line, column) {
                Some(',') => {
                    column += 1;
                }
                Some('}') => return Ok((Yaml::Map(mapping), line, column + 1)),
                _ => {
                    return Err(Self::flow_separator_error(
                        line,
                        column,
                        line != last_line,
                        plain,
                        "expected ',' or '}' in a flow mapping",
                    ))
                }
            }
        }
    }

    // ---- the document --------------------------------------------------------------------------

    fn parse_directive(&self, line: usize, seen_yaml: bool) -> Parsed<()> {
        let text = &self.lines[line];
        let mut end = 1;
        while end < text.len() && !matches!(text[end], ' ' | '\t') {
            end += 1;
        }
        let name: String = text[1..end.max(1)].iter().collect();
        if name == "TAG" {
            return Err(Self::error("%TAG directives are not supported", line, 0));
        }
        if name != "YAML" {
            return Err(Self::error(
                format!("unknown directive {}", quote(&format!("%{name}"))),
                line,
                0,
            ));
        }
        if seen_yaml {
            return Err(Self::error("duplicate %YAML directive", line, 0));
        }
        let column = self.skip_spaces(line, end)?;
        let mut version_end = column;
        while version_end < text.len() && !matches!(text[version_end], ' ' | '\t') {
            version_end += 1;
        }
        let version: String = text[column..version_end].iter().collect();
        if version.is_empty() || version.starts_with('#') {
            return Err(Self::error(
                "the %YAML directive needs a version",
                line,
                column,
            ));
        }
        if version != "1.1" && version != "1.2" {
            return Err(Self::error(
                format!("unsupported YAML version {}", quote(&version)),
                line,
                column,
            ));
        }
        self.check_trailing(line, version_end)
    }

    fn check_marker_rest(&self, line: usize, marker: &str) -> Parsed<()> {
        let column = self.skip_spaces(line, 3)?;
        if self.comment_or_end(line, column) {
            return Ok(());
        }
        Err(Self::error(
            format!("content after {marker} is not supported"),
            line,
            column,
        ))
    }

    fn parse(&self) -> Parsed<Yaml> {
        let mut line = 0;
        let mut seen_yaml = false;
        while line < self.count() {
            if self.is_blank(line) {
                line += 1;
                continue;
            }
            if self.lines[line].first() == Some(&'%') {
                self.parse_directive(line, seen_yaml)?;
                seen_yaml = true;
                line += 1;
                continue;
            }
            break;
        }
        if line < self.count() && self.is_marker(line, "---") {
            self.check_marker_rest(line, "'---'")?;
            line += 1;
        } else if seen_yaml {
            let at = if line < self.count() {
                line
            } else {
                self.count() - 1
            };
            return Err(Self::error("a directive must be followed by '---'", at, 0));
        }
        let mut value = Yaml::Null;
        let mut after = line;
        if let Some(first) = self.next_content(line) {
            if !self.is_document_boundary(first) {
                (value, after) = self.parse_block_node(first, self.indent(first), -1, 1)?;
            }
        }
        let Some(rest) = self.next_content(after) else {
            return Ok(value);
        };
        if self.is_marker(rest, "...") {
            self.check_marker_rest(rest, "'...'")?;
            if let Some(trailing) = self.next_content(rest + 1) {
                return Err(Self::error(
                    "multiple documents are not supported",
                    trailing,
                    0,
                ));
            }
            return Ok(value);
        }
        if self.is_marker(rest, "---") {
            return Err(Self::error("multiple documents are not supported", rest, 0));
        }
        Err(Self::error(
            "expected the end of the document",
            rest,
            self.indent(rest),
        ))
    }
}

/// Parse one configuration document, failing with a positioned error outside the subset.
pub fn core_load(text: &str) -> std::result::Result<Yaml, YamlError> {
    Parser::new(text)?.parse()
}

#[cfg(test)]
mod tests {
    use serde_json::Value;

    use super::*;
    use crate::textfmt::{python_float_repr, quote};

    /// The corpus's canonical rendering, written out the same way in the other edition's test.
    fn render(value: &Yaml) -> String {
        match value {
            Yaml::Null => "null".to_owned(),
            Yaml::Bool(true) => "true".to_owned(),
            Yaml::Bool(false) => "false".to_owned(),
            Yaml::Int(number) => number.to_string(),
            Yaml::Float(number) => python_float_repr(*number),
            Yaml::Str(text) => quote(text),
            Yaml::Seq(items) => format!(
                "[{}]",
                items.iter().map(render).collect::<Vec<_>>().join(", ")
            ),
            Yaml::Map(entries) => format!(
                "{{{}}}",
                entries
                    .iter()
                    .map(|(key, item)| format!("{}: {}", quote(key), render(item)))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }

    #[test]
    fn golden_yaml_subset() {
        let document: Value =
            serde_json::from_str(include_str!("../testdata/yaml_cases.json")).expect("cases");
        for case in document["cases"].as_array().expect("case list") {
            let name = case["name"].as_str().expect("name");
            let loaded = core_load(case["text"].as_str().expect("text"));
            match (loaded, case.get("error").and_then(Value::as_str)) {
                (Err(error), Some(expected)) => assert_eq!(error.to_string(), expected, "{name}"),
                (Ok(value), None) => assert_eq!(render(&value), case["value"], "{name}"),
                (Ok(value), Some(expected)) => {
                    panic!(
                        "{name}: accepted {} but expected {expected}",
                        render(&value)
                    )
                }
                (Err(error), None) => panic!("{name}: refused with {error}"),
            }
        }
    }

    #[test]
    fn integers_are_clamped_and_leading_zeros_are_strings() {
        assert_eq!(integer("007"), None);
        assert_eq!(integer("-0"), Some(0));
        assert_eq!(integer("0x1F"), Some(31));
        assert_eq!(integer("-0o17"), Some(-15));
        assert_eq!(integer("+0b101"), Some(5));
        assert_eq!(integer(&"9".repeat(5000)), Some(INTEGER_CLAMP));
        assert_eq!(
            integer(&format!("-{}", "9".repeat(39))),
            Some(-INTEGER_CLAMP)
        );
        assert_eq!(
            integer(&"9".repeat(38)),
            Some(clamp(false, accumulate(&"9".repeat(38), 10)))
        );
    }
}
