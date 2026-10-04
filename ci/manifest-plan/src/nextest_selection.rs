//! Test names a Nextest selection uses to choose or exclude tests.
//!
//! `ci/run-nextest-counted.sh` lists the population with the same selection
//! arguments it runs, and `ci/nextest-test-results.rs` refuses unless the test
//! identities that executed equal the identities that listing selected. That
//! equality cannot see a selector naming a test that no longer exists: after
//! `b` is renamed, `test(=a) | test(=b)` lists and runs only `a`, and both sides
//! agree. This module extracts every name a selection writes down, with whether
//! it adds tests (`Selects`) or removes them (`Excludes`), so the writer can
//! refuse when a selecting name chooses no listed test at the same head.
//!
//! A name is judged together with the rest of its conjunction. In
//! `(package(=a) & test(=x)) | (package(=b) & test(=x))` the first `x` names
//! only package `a`'s test: when that test is renamed, package `b`'s `x` must
//! not answer for it, so each reference carries the conditions its conjunction
//! places on the same test.
//!
//! The parser accepts Nextest 0.9.100's command-line vocabulary and filterset
//! grammar. An option, predicate argument, or regular expression it cannot
//! interpret is an error rather than a skipped check: a selecting reference
//! that cannot be checked would silently reopen the case above.

/// The fields of one listed test that a selection can constrain.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TestIdentity<'a> {
    pub package: &'a str,
    pub binary_name: &'a str,
    pub kind: &'a str,
    pub test: &'a str,
}

/// Whether a name adds tests to the selection or removes them from it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Polarity {
    Selects,
    Excludes,
}

/// How one written name is compared with a full Nextest test name.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NameMatcher {
    Exact(String),
    Contains(String),
    Prefix(String),
    Suffix(String),
    Glob(String),
}

impl std::fmt::Display for NameMatcher {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Exact(value) => write!(formatter, "named exactly {value:?}"),
            Self::Contains(value) => write!(formatter, "name containing {value:?}"),
            Self::Prefix(value) => write!(formatter, "name starting with {value:?}"),
            Self::Suffix(value) => write!(formatter, "name ending with {value:?}"),
            Self::Glob(pattern) => write!(formatter, "name matching glob {pattern:?}"),
        }
    }
}

impl NameMatcher {
    pub fn matches(&self, name: &str) -> bool {
        match self {
            Self::Exact(value) => name == value,
            Self::Contains(value) => name.contains(value.as_str()),
            Self::Prefix(value) => name.starts_with(value.as_str()),
            Self::Suffix(value) => name.ends_with(value.as_str()),
            Self::Glob(pattern) => glob_matches(pattern.as_bytes(), name.as_bytes()),
        }
    }
}

/// The field of a listed test a filterset predicate compares.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Field {
    Test,
    Package,
    Binary,
    Kind,
}

impl Field {
    fn of<'a>(self, test: &TestIdentity<'a>) -> &'a str {
        match self {
            Self::Test => test.test,
            Self::Package => test.package,
            Self::Binary => test.binary_name,
            Self::Kind => test.kind,
        }
    }
}

/// A condition the rest of a conjunction places on the tests one name can
/// choose.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Scope {
    /// `all()` (true) or `none()` (false).
    Constant(bool),
    /// The field matches one of the matchers, or none of them when negated.
    Field {
        field: Field,
        matchers: Vec<NameMatcher>,
        negated: bool,
    },
    And(Vec<Scope>),
    Or(Vec<Scope>),
}

impl Scope {
    pub fn admits(&self, test: &TestIdentity<'_>) -> bool {
        match self {
            Self::Constant(value) => *value,
            Self::Field {
                field,
                matchers,
                negated,
            } => {
                matchers
                    .iter()
                    .any(|matcher| matcher.matches(field.of(test)))
                    != *negated
            }
            Self::And(terms) => terms.iter().all(|term| term.admits(test)),
            Self::Or(terms) => terms.iter().any(|term| term.admits(test)),
        }
    }
}

/// One name the selection writes down.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NameReference {
    pub polarity: Polarity,
    pub matcher: NameMatcher,
    /// Every condition the surrounding conjunction places on the same test.
    /// Empty for command-line names and for a `test()` atom that stands alone.
    pub scope: Vec<Scope>,
    /// The reference as the selection wrote it, for refusal messages.
    pub written: String,
}

impl NameReference {
    /// Whether this name, within its conjunction, refers to `test`.
    pub fn refers_to(&self, test: &TestIdentity<'_>) -> bool {
        self.matcher.matches(test.test) && self.scope.iter().all(|term| term.admits(test))
    }

    pub fn refers_to_any(&self, tests: &[TestIdentity<'_>]) -> bool {
        tests.iter().any(|test| self.refers_to(test))
    }
}

// Options that take the next argument (or an attached `=value`) and never name
// tests. Filterset options are handled separately.
const VALUE_OPTIONS: &[&str] = &[
    "-p",
    "--package",
    "--exclude",
    "--bin",
    "--example",
    "--test",
    "--bench",
    "-F",
    "--features",
    "--build-jobs",
    "--cargo-profile",
    "--target",
    "--target-dir",
    "--manifest-path",
    "--config",
    "--run-ignored",
    "--partition",
    "--platform-filter",
    "-j",
    "--jobs",
    "--test-threads",
    "--retries",
    "--max-fail",
    "--no-tests",
    "--failure-output",
    "--success-output",
    "--status-level",
    "--final-status-level",
    "--message-format",
    "--message-format-version",
    "--archive-file",
    "--archive-format",
    "--extract-to",
    "--cargo-metadata",
    "--workspace-remap",
    "--binaries-metadata",
    "--target-dir-remap",
    "--config-file",
    "--tool-config-file",
    "-P",
    "--profile",
    "--color",
];

const FILTERSET_OPTIONS: &[&str] = &["-E", "--filterset", "--filter-expr"];

const FLAG_OPTIONS: &[&str] = &[
    "-v",
    "--verbose",
    "--workspace",
    "--all",
    "--lib",
    "--bins",
    "--examples",
    "--tests",
    "--benches",
    "--all-targets",
    "--all-features",
    "--no-default-features",
    "-r",
    "--release",
    "--unit-graph",
    "--timings",
    "--frozen",
    "--locked",
    "--offline",
    "--cargo-quiet",
    "--cargo-verbose",
    "--ignore-rust-version",
    "--future-incompat-report",
    "--ignore-default-filter",
    "--no-run",
    "--fail-fast",
    "--ff",
    "--no-fail-fast",
    "--nff",
    "--no-capture",
    "--nocapture",
    "--hide-progress-bar",
    "--no-output-indent",
    "--no-input-handler",
    "--extract-overwrite",
    "--persist-extract-tempdir",
    "--override-version-check",
];

/// Value of an option written as `--name=value` or, for one-letter options,
/// `-Xvalue`.
fn attached_value<'a>(arg: &'a str, option: &str) -> Option<&'a str> {
    let rest = arg.strip_prefix(option)?;
    if option.starts_with("--") {
        rest.strip_prefix('=')
    } else if rest.is_empty() {
        None
    } else {
        Some(rest.strip_prefix('=').unwrap_or(rest))
    }
}

/// Every name the Nextest arguments use to choose or exclude tests.
///
/// Positional filters and filterset `test()` atoms under an even number of
/// negations select; `--skip` and negated atoms exclude.
pub fn selection_references(args: &[String]) -> Result<Vec<NameReference>, String> {
    let mut filtersets = Vec::new();
    let mut names = Vec::new();
    let mut skips = Vec::new();
    let mut exact = false;
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        index += 1;
        if arg == "--" {
            break;
        }
        if FILTERSET_OPTIONS.contains(&arg) {
            let value = args
                .get(index)
                .ok_or_else(|| format!("Nextest option {arg} requires a filterset"))?;
            filtersets.push(value.clone());
            index += 1;
        } else if let Some(value) = FILTERSET_OPTIONS
            .iter()
            .find_map(|option| attached_value(arg, option))
        {
            filtersets.push(value.to_string());
        } else if VALUE_OPTIONS.contains(&arg) {
            if args.get(index).is_none() {
                return Err(format!("Nextest option {arg} requires a value"));
            }
            index += 1;
        } else if FLAG_OPTIONS.contains(&arg)
            || arg.starts_with("--timings=")
            || VALUE_OPTIONS
                .iter()
                .any(|option| attached_value(arg, option).is_some())
        {
        } else if arg.starts_with('-') {
            return Err(format!(
                "cannot tell whether Nextest option {arg} takes a value, so the test names this selection uses are unknown"
            ));
        } else {
            names.push(arg.to_string());
        }
    }
    while index < args.len() {
        let arg = args[index].as_str();
        index += 1;
        match arg {
            "--exact" => exact = true,
            "--ignored" | "--include-ignored" => {}
            "--skip" => {
                let value = args
                    .get(index)
                    .ok_or_else(|| "test-filter option --skip requires a name".to_string())?;
                skips.push(value.clone());
                index += 1;
            }
            _ => {
                if let Some(value) = arg.strip_prefix("--skip=") {
                    skips.push(value.to_string());
                } else if arg.starts_with('-') {
                    return Err(format!(
                        "unsupported test-filter option {arg} after the Nextest -- separator"
                    ));
                } else {
                    names.push(arg.to_string());
                }
            }
        }
    }
    let name_matcher = |value: &str| {
        if exact {
            NameMatcher::Exact(value.to_string())
        } else {
            NameMatcher::Contains(value.to_string())
        }
    };
    let mut references = Vec::new();
    for name in names {
        if name.is_empty() {
            return Err("an empty Nextest name filter selects every test".into());
        }
        references.push(NameReference {
            polarity: Polarity::Selects,
            matcher: name_matcher(&name),
            scope: Vec::new(),
            written: if exact {
                format!("name filter {name:?} (--exact)")
            } else {
                format!("name filter {name:?}")
            },
        });
    }
    for skip in skips {
        references.push(NameReference {
            polarity: Polarity::Excludes,
            matcher: name_matcher(&skip),
            scope: Vec::new(),
            written: format!("--skip {skip:?}"),
        });
    }
    for filterset in filtersets {
        references.extend(filterset_references(&filterset)?);
    }
    Ok(references)
}

/// The `test()` atoms of one Nextest filterset with their polarity and the
/// conditions their conjunctions place on the same test.
pub fn filterset_references(expression: &str) -> Result<Vec<NameReference>, String> {
    let mut parser = Parser {
        text: expression,
        position: 0,
    };
    let tree = parser.union()?;
    parser.skip_space();
    if parser.position != expression.len() {
        return Err(parser.error("unexpected text"));
    }
    let mut references = Vec::new();
    collect_references(&normalize(&tree, false), &mut Vec::new(), &mut references)?;
    Ok(references)
}

/// A filterset as written: `-` is an intersection with a negated right side.
enum Expression {
    Or(Vec<Expression>),
    And(Vec<Expression>),
    Not(Box<Expression>),
    Predicate { name: String, argument: String },
}

/// A filterset with every negation pushed onto a predicate, so an `And`'s
/// children are exactly the conditions that hold together for one test.
enum Normal<'e> {
    Or(Vec<Normal<'e>>),
    And(Vec<Normal<'e>>),
    Predicate {
        name: &'e str,
        argument: &'e str,
        negated: bool,
    },
}

impl std::fmt::Display for Normal<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let join = |formatter: &mut std::fmt::Formatter<'_>, terms: &[Normal<'_>], separator| {
            for (index, term) in terms.iter().enumerate() {
                if index > 0 {
                    formatter.write_str(separator)?;
                }
                write!(formatter, "{term}")?;
            }
            Ok(())
        };
        match self {
            Self::Or(terms) => {
                formatter.write_str("(")?;
                join(formatter, terms, " | ")?;
                formatter.write_str(")")
            }
            Self::And(terms) => join(formatter, terms, " & "),
            Self::Predicate {
                name,
                argument,
                negated,
            } => {
                let not = if *negated { "not " } else { "" };
                write!(formatter, "{not}{name}({argument})")
            }
        }
    }
}

fn normalize(expression: &Expression, negated: bool) -> Normal<'_> {
    match expression {
        Expression::Not(inner) => normalize(inner, !negated),
        Expression::Or(terms) | Expression::And(terms) => {
            let terms = terms.iter().map(|term| normalize(term, negated)).collect();
            // De Morgan: a negated intersection is a union of negations.
            if matches!(expression, Expression::And(_)) != negated {
                Normal::And(terms)
            } else {
                Normal::Or(terms)
            }
        }
        Expression::Predicate { name, argument } => Normal::Predicate {
            name,
            argument,
            negated,
        },
    }
}

/// Records each `test()` atom with the siblings of every intersection above
/// it, which are the conditions the same test must also meet.
fn collect_references<'e>(
    node: &'e Normal<'e>,
    context: &mut Vec<&'e Normal<'e>>,
    references: &mut Vec<NameReference>,
) -> Result<(), String> {
    match node {
        Normal::Or(terms) => {
            for term in terms {
                collect_references(term, context, references)?;
            }
        }
        Normal::And(terms) => {
            for (index, term) in terms.iter().enumerate() {
                let depth = context.len();
                context.extend(
                    terms
                        .iter()
                        .enumerate()
                        .filter(|(other, _)| *other != index)
                        .map(|(_, sibling)| sibling),
                );
                let result = collect_references(term, context, references);
                context.truncate(depth);
                result?;
            }
        }
        Normal::Predicate {
            name: "test",
            argument,
            negated,
        } => {
            let polarity = if *negated {
                Polarity::Excludes
            } else {
                Polarity::Selects
            };
            let atom = format!("test({argument})");
            let matchers = test_matchers(argument, polarity, &atom)?;
            if matchers.is_empty() {
                return Ok(());
            }
            let scope = match context
                .iter()
                .map(|term| scope_of(term, polarity))
                .collect::<Result<Vec<_>, _>>()
            {
                Ok(scope) => scope,
                // An exclusion is only reported, so one whose scope cannot be
                // evaluated is not reported. A selecting one must be checkable.
                Err(reason) => {
                    return match polarity {
                        Polarity::Excludes => Ok(()),
                        Polarity::Selects => Err(format!(
                            "cannot determine which tests {atom} selects: {reason}"
                        )),
                    };
                }
            };
            let written = if context.is_empty() {
                atom
            } else {
                let conditions = context
                    .iter()
                    .map(|term| term.to_string())
                    .collect::<Vec<_>>();
                format!("{atom} with {}", conditions.join(" & "))
            };
            for matcher in matchers {
                references.push(NameReference {
                    polarity,
                    matcher,
                    scope: scope.clone(),
                    written: written.clone(),
                });
            }
        }
        Normal::Predicate { .. } => {}
    }
    Ok(())
}

/// The condition one conjunct places on a listed test, from the predicates a
/// listing identity can answer.
fn scope_of(node: &Normal<'_>, polarity: Polarity) -> Result<Scope, String> {
    let all = |terms: &[Normal<'_>]| {
        terms
            .iter()
            .map(|term| scope_of(term, polarity))
            .collect::<Result<Vec<_>, _>>()
    };
    match node {
        Normal::Or(terms) => Ok(Scope::Or(all(terms)?)),
        Normal::And(terms) => Ok(Scope::And(all(terms)?)),
        // Whether an exclusion removes anything does not depend on the other
        // exclusions: in `not (test(foo) | test(foobar))` both remove `foobar`.
        // Counting each against the other would report both as removing
        // nothing. A selecting name is still checked against every exclusion,
        // because a name every one of whose tests is excluded chooses nothing.
        Normal::Predicate {
            name: "test",
            negated: true,
            ..
        } if polarity == Polarity::Excludes => Ok(Scope::Constant(true)),
        Normal::Predicate {
            name,
            argument,
            negated,
        } => {
            let (field, default) = match *name {
                "all" | "none" if argument.is_empty() => {
                    return Ok(Scope::Constant((*name == "all") != *negated));
                }
                "test" => (Field::Test, DefaultMatcher::Contains),
                "package" => (Field::Package, DefaultMatcher::Glob),
                "binary" => (Field::Binary, DefaultMatcher::Glob),
                "kind" => (Field::Kind, DefaultMatcher::Equal),
                _ => {
                    return Err(format!(
                        "the condition {name}({argument}) is not interpreted"
                    ));
                }
            };
            let matchers = argument_matchers(argument, default)
                .map_err(|reason| format!("the condition {name}({argument}): {reason}"))?;
            Ok(Scope::Field {
                field,
                matchers,
                negated: *negated,
            })
        }
    }
}

struct Parser<'a> {
    text: &'a str,
    position: usize,
}

impl Parser<'_> {
    fn error(&self, message: &str) -> String {
        format!(
            "cannot parse Nextest filterset {:?} at byte {}: {message}",
            self.text, self.position
        )
    }

    fn rest(&self) -> &str {
        &self.text[self.position..]
    }

    fn skip_space(&mut self) {
        let trimmed = self.rest().trim_start();
        self.position = self.text.len() - trimmed.len();
    }

    fn eat(&mut self, token: &str) -> bool {
        self.skip_space();
        if self.rest().starts_with(token) {
            self.position += token.len();
            true
        } else {
            false
        }
    }

    fn eat_keyword(&mut self, keyword: &str) -> bool {
        self.skip_space();
        let Some(after) = self.rest().strip_prefix(keyword) else {
            return false;
        };
        if after
            .chars()
            .next()
            .is_some_and(|next| next.is_ascii_alphanumeric() || next == '_')
        {
            return false;
        }
        self.position += keyword.len();
        true
    }

    fn union(&mut self) -> Result<Expression, String> {
        let mut terms = vec![self.intersection()?];
        while self.eat("|") || self.eat("+") || self.eat_keyword("or") {
            terms.push(self.intersection()?);
        }
        Ok(if terms.len() == 1 {
            terms.remove(0)
        } else {
            Expression::Or(terms)
        })
    }

    // Nextest gives `&`, `and`, and set difference `-` one precedence level,
    // binding tighter than union and associating left, so a chain of them is
    // one intersection in which each `-` negates only its right side.
    fn intersection(&mut self) -> Result<Expression, String> {
        let mut terms = vec![self.unary()?];
        loop {
            if self.eat("&") || self.eat_keyword("and") {
                terms.push(self.unary()?);
            } else if self.eat("-") {
                terms.push(Expression::Not(Box::new(self.unary()?)));
            } else {
                break;
            }
        }
        Ok(if terms.len() == 1 {
            terms.remove(0)
        } else {
            Expression::And(terms)
        })
    }

    fn unary(&mut self) -> Result<Expression, String> {
        if self.eat("!") || self.eat_keyword("not") {
            return Ok(Expression::Not(Box::new(self.unary()?)));
        }
        if self.eat("(") {
            let inner = self.union()?;
            if !self.eat(")") {
                return Err(self.error("expected )"));
            }
            return Ok(inner);
        }
        self.predicate()
    }

    fn predicate(&mut self) -> Result<Expression, String> {
        self.skip_space();
        let name_length = self
            .rest()
            .find(|character: char| !(character.is_ascii_alphanumeric() || character == '_'))
            .unwrap_or(self.rest().len());
        if name_length == 0 {
            return Err(self.error("expected a predicate"));
        }
        let name = self.rest()[..name_length].to_string();
        self.position += name_length;
        if !self.eat("(") {
            return Err(self.error("expected ( after a predicate name"));
        }
        self.skip_space();
        let argument = if self.rest().starts_with('/') {
            self.regex_argument()?
        } else {
            let end = self
                .rest()
                .find(')')
                .ok_or_else(|| self.error("unterminated predicate argument"))?;
            let argument = self.rest()[..end].trim().to_string();
            self.position += end;
            argument
        };
        if !self.eat(")") {
            return Err(self.error("expected ) after a predicate argument"));
        }
        Ok(Expression::Predicate { name, argument })
    }

    /// A `/regex/` argument, honouring `\/` inside it, returned with its slashes.
    fn regex_argument(&mut self) -> Result<String, String> {
        let start = self.position;
        let mut escaped = false;
        for (offset, character) in self.rest().char_indices().skip(1) {
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '/' {
                let end = self.position + offset + 1;
                if self.text[end..].trim_start().starts_with(')') {
                    self.position = end;
                    return Ok(self.text[start..end].to_string());
                }
            }
        }
        Err(self.error("unterminated regular expression"))
    }
}

/// How a predicate compares a bare argument: Nextest's `test()` matches a
/// substring, `package()` and `binary()` a glob, and `kind()` equality.
#[derive(Clone, Copy)]
enum DefaultMatcher {
    Contains,
    Glob,
    Equal,
}

/// The matchers one predicate argument stands for, or why it cannot be
/// interpreted. A closed regular-expression alternation yields one matcher per
/// alternative.
fn argument_matchers(
    argument: &str,
    default: DefaultMatcher,
) -> Result<Vec<NameMatcher>, &'static str> {
    let literal = |value: &str| {
        !value.is_empty()
            && !value.contains('\\')
            && value.chars().all(|character| !character.is_control())
    };
    let glob = |pattern: &str| {
        if literal(pattern) && !pattern.contains(['[', ']', '{', '}']) {
            Ok(vec![NameMatcher::Glob(pattern.into())])
        } else {
            Err("only * and ? globs are interpreted")
        }
    };
    if let Some(value) = argument.strip_prefix('=') {
        return if literal(value) {
            Ok(vec![NameMatcher::Exact(value.into())])
        } else {
            Err("escaped or empty exact names are not interpreted")
        };
    }
    if let Some(value) = argument.strip_prefix('~') {
        return if literal(value) {
            Ok(vec![NameMatcher::Contains(value.into())])
        } else {
            Err("escaped or empty substrings are not interpreted")
        };
    }
    if let Some(pattern) = argument.strip_prefix('#') {
        return glob(pattern);
    }
    if let Some(body) = argument
        .strip_prefix('/')
        .and_then(|rest| rest.strip_suffix('/'))
    {
        return regex_matchers(body).ok_or(
            "only a literal, optionally anchored, or an anchored alternation of literals is interpreted",
        );
    }
    match default {
        DefaultMatcher::Glob => glob(argument),
        DefaultMatcher::Contains if literal(argument) => {
            Ok(vec![NameMatcher::Contains(argument.into())])
        }
        DefaultMatcher::Equal if literal(argument) => Ok(vec![NameMatcher::Exact(argument.into())]),
        DefaultMatcher::Contains | DefaultMatcher::Equal => {
            Err("escaped or empty arguments are not interpreted")
        }
    }
}

/// The names a `test()` argument matches. A closed alternation such as
/// `/^(a|b)$/` yields one matcher per alternative, because each alternative is
/// a separate name that must still exist.
fn test_matchers(
    argument: &str,
    polarity: Polarity,
    written: &str,
) -> Result<Vec<NameMatcher>, String> {
    match argument_matchers(argument, DefaultMatcher::Contains) {
        Ok(matchers) => Ok(matchers),
        // An exclusion is only reported, never refused, so one that cannot be
        // interpreted is simply not reported. A selecting one must be checkable.
        Err(_) if polarity == Polarity::Excludes => Ok(Vec::new()),
        Err(reason) => Err(format!(
            "cannot determine which test names {written} selects: {reason}"
        )),
    }
}

/// Interprets `^?(lit|lit|...)$?` or `^?lit$?` where every literal is made of
/// characters a regular expression treats literally.
fn regex_matchers(body: &str) -> Option<Vec<NameMatcher>> {
    let (start_anchor, body) = match body.strip_prefix('^') {
        Some(rest) => (true, rest),
        None => (false, body),
    };
    let (end_anchor, body) = match body.strip_suffix('$') {
        Some(rest) => (true, rest),
        None => (false, body),
    };
    let alternatives: Vec<&str> = match body
        .strip_prefix('(')
        .and_then(|rest| rest.strip_suffix(')'))
    {
        Some(group) => group.split('|').collect(),
        None => vec![body],
    };
    let plain = |value: &str| {
        !value.is_empty()
            && value.chars().all(|character| {
                character.is_ascii_alphanumeric() || matches!(character, '_' | ':' | '-')
            })
    };
    if !alternatives.iter().all(|value| plain(value)) {
        return None;
    }
    // A bare alternation without a group would need to bind each anchor to
    // only one side; reject it rather than guess.
    if alternatives.len() == 1 && body.contains('|') {
        return None;
    }
    Some(
        alternatives
            .into_iter()
            .map(|value| {
                let value = value.to_string();
                match (start_anchor, end_anchor) {
                    (true, true) => NameMatcher::Exact(value),
                    (true, false) => NameMatcher::Prefix(value),
                    (false, true) => NameMatcher::Suffix(value),
                    (false, false) => NameMatcher::Contains(value),
                }
            })
            .collect(),
    )
}

/// `*` matches any run of characters and `?` exactly one.
fn glob_matches(pattern: &[u8], name: &[u8]) -> bool {
    let (mut pattern_index, mut name_index) = (0, 0);
    let mut backtrack: Option<(usize, usize)> = None;
    while name_index < name.len() {
        match pattern.get(pattern_index) {
            Some(b'*') => {
                backtrack = Some((pattern_index, name_index));
                pattern_index += 1;
            }
            Some(&byte) if byte == b'?' || byte == name[name_index] => {
                pattern_index += 1;
                name_index += 1;
            }
            _ => match backtrack {
                Some((star, matched)) => {
                    pattern_index = star + 1;
                    name_index = matched + 1;
                    backtrack = Some((star, matched + 1));
                }
                None => return false,
            },
        }
    }
    pattern[pattern_index..].iter().all(|&byte| byte == b'*')
}

/// Selecting references that, within their conjunctions, choose none of
/// `selected`, the tests Nextest listed to run. Each would have chosen tests
/// when it was written.
pub fn dangling_selections<'a>(
    references: &'a [NameReference],
    selected: &[TestIdentity<'_>],
) -> Vec<&'a NameReference> {
    references
        .iter()
        .filter(|reference| reference.polarity == Polarity::Selects)
        .filter(|reference| !reference.refers_to_any(selected))
        .collect()
}

/// Excluding references that, within their conjunctions, match no test in
/// `universe`, every test the listing enumerated. They remove nothing and are
/// reported, not refused: an exclusion that matches nothing cannot hide a test.
pub fn stale_exclusions<'a>(
    references: &'a [NameReference],
    universe: &[TestIdentity<'_>],
) -> Vec<&'a NameReference> {
    references
        .iter()
        .filter(|reference| reference.polarity == Polarity::Excludes)
        .filter(|reference| !reference.refers_to_any(universe))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    fn written(references: &[NameReference], polarity: Polarity) -> Vec<(String, NameMatcher)> {
        references
            .iter()
            .filter(|reference| reference.polarity == polarity)
            .map(|reference| (reference.written.clone(), reference.matcher.clone()))
            .collect()
    }

    #[test]
    fn exact_union_and_its_complement_have_opposite_polarity() {
        let union = filterset_references("test(=a) | test(=b)").unwrap();
        assert_eq!(
            written(&union, Polarity::Selects),
            [
                ("test(=a)".into(), NameMatcher::Exact("a".into())),
                ("test(=b)".into(), NameMatcher::Exact("b".into())),
            ]
        );
        let complement = filterset_references("not (test(=a) | test(=b))").unwrap();
        assert!(written(&complement, Polarity::Selects).is_empty());
        assert_eq!(written(&complement, Polarity::Excludes).len(), 2);
        let double = filterset_references("not not test(=a)").unwrap();
        assert_eq!(double[0].polarity, Polarity::Selects);
        let bang = filterset_references("!test(=a) and test(=b)").unwrap();
        assert_eq!(
            bang.iter()
                .map(|reference| reference.polarity)
                .collect::<Vec<_>>(),
            [Polarity::Excludes, Polarity::Selects]
        );
    }

    #[test]
    fn difference_negates_only_its_right_side_and_binds_like_and() {
        let references = filterset_references("test(=a) - test(=b) | test(=c)").unwrap();
        assert_eq!(
            references
                .iter()
                .map(|reference| (reference.written.as_str(), reference.polarity))
                .collect::<Vec<_>>(),
            [
                ("test(=a) with not test(=b)", Polarity::Selects),
                ("test(=b) with test(=a)", Polarity::Excludes),
                ("test(=c)", Polarity::Selects),
            ]
        );
        let nested = filterset_references("all() - (test(=a) - test(=b))").unwrap();
        assert_eq!(
            nested
                .iter()
                .map(|reference| reference.polarity)
                .collect::<Vec<_>>(),
            [Polarity::Excludes, Polarity::Selects]
        );
    }

    #[test]
    fn other_predicates_are_parsed_but_name_nothing() {
        let references = filterset_references(
            "package(=detcore-testutils) & test(=tests::isolated) | kind(lib) & binary_id(/a)b/) | all()",
        )
        .unwrap();
        assert_eq!(
            written(&references, Polarity::Selects),
            [(
                "test(=tests::isolated) with package(=detcore-testutils)".into(),
                NameMatcher::Exact("tests::isolated".into())
            )]
        );
        assert_eq!(
            references[0].scope,
            [Scope::Field {
                field: Field::Package,
                matchers: vec![NameMatcher::Exact("detcore-testutils".into())],
                negated: false,
            }]
        );
    }

    fn identity<'a>(package: &'a str, test: &'a str) -> TestIdentity<'a> {
        TestIdentity {
            package,
            binary_name: package,
            kind: "lib",
            test,
        }
    }

    #[test]
    fn a_package_scoped_name_is_not_answered_by_another_packages_test() {
        let references =
            filterset_references("(package(=a) & test(=shared)) | (package(=b) & test(=shared))")
                .unwrap();
        let before = [identity("a", "shared"), identity("b", "shared")];
        assert!(dangling_selections(&references, &before).is_empty());
        // `a::shared` is renamed. Nextest lists and runs only `b::shared`, so
        // the executed and listed sets agree; package `a`'s reference must
        // still be reported rather than satisfied by package `b`'s test.
        let after = [identity("a", "renamed"), identity("b", "shared")];
        assert_eq!(
            dangling_selections(&references, &after[1..])
                .iter()
                .map(|reference| reference.written.as_str())
                .collect::<Vec<_>>(),
            ["test(=shared) with package(=a)"]
        );
        // A conjunct that is itself a union admits any of its alternatives,
        // binary() compares by glob, and kind() by equality.
        let union = filterset_references(
            "(package(=a) | package(=c)) & test(=shared) & binary(b) & not kind(bench)",
        )
        .unwrap();
        assert!(dangling_selections(&union, &after[1..]).len() == 1);
        let in_b = [TestIdentity {
            package: "c",
            binary_name: "b",
            kind: "lib",
            test: "shared",
        }];
        assert!(dangling_selections(&union, &in_b).is_empty());
        let bench = [TestIdentity {
            kind: "bench",
            ..in_b[0]
        }];
        assert_eq!(dangling_selections(&union, &bench).len(), 1);
    }

    #[test]
    fn negation_moves_conditions_by_de_morgan() {
        // not (not x & package(a)) = x | not package(a): `x` stands alone.
        let alone = filterset_references("not (not test(=x) & package(=a))").unwrap();
        assert_eq!(alone[0].polarity, Polarity::Selects);
        assert!(alone[0].scope.is_empty(), "{alone:?}");
        // not (x | package(a)) = not x & not package(a).
        let excluded = filterset_references("not (test(=x) | package(=a))").unwrap();
        assert_eq!(excluded[0].polarity, Polarity::Excludes);
        assert_eq!(excluded[0].written, "test(=x) with not package(=a)");
        // package(a) - x removes `x` only from package `a`.
        let difference = filterset_references("package(=a) - test(=x)").unwrap();
        assert_eq!(difference[0].polarity, Polarity::Excludes);
        assert_eq!(difference[0].written, "test(=x) with package(=a)");
        let universe = [identity("b", "x")];
        assert_eq!(stale_exclusions(&difference, &universe).len(), 1);
        assert!(stale_exclusions(&excluded, &universe).is_empty());
    }

    #[test]
    fn a_bare_binary_argument_is_a_glob_as_in_nextest() {
        let in_tests_time = [TestIdentity {
            package: "hermit-detcore",
            binary_name: "tests_time",
            kind: "test",
            test: "shared",
        }];
        // cargo-nextest 0.9.100 lists the tests of binary `integ_one` for
        // `binary(integ_*)`: a bare binary() argument is a glob, not a name.
        let scoped = filterset_references("binary(tests_*) & test(=shared)").unwrap();
        assert!(dangling_selections(&scoped, &in_tests_time).is_empty());
        let excluded = filterset_references("not binary(tests_*) & test(=shared)").unwrap();
        assert_eq!(
            dangling_selections(&excluded, &in_tests_time)
                .iter()
                .map(|reference| reference.written.as_str())
                .collect::<Vec<_>>(),
            ["test(=shared) with not binary(tests_*)"]
        );
        // kind() stays equality: Nextest lists nothing for `kind(li*)`.
        let kind = filterset_references("kind(te*) & test(=shared)").unwrap();
        assert_eq!(dangling_selections(&kind, &in_tests_time).len(), 1);
    }

    #[test]
    fn overlapping_exclusions_are_each_answered_by_the_test_they_remove() {
        // not (test(foo) | test(foobar)) = not test(foo) & not test(foobar).
        let references = filterset_references("not (test(foo) | test(foobar))").unwrap();
        assert_eq!(
            written(&references, Polarity::Excludes)
                .into_iter()
                .map(|(written, _)| written)
                .collect::<Vec<_>>(),
            [
                "test(foo) with not test(foobar)",
                "test(foobar) with not test(foo)"
            ]
        );
        let universe = [identity("p", "foobar"), identity("p", "other")];
        assert!(stale_exclusions(&references, &universe).is_empty());
        let unrelated = [identity("p", "other")];
        assert_eq!(stale_exclusions(&references, &unrelated).len(), 2);
        // A selecting name is still checked against the exclusions beside it.
        let shadowed = filterset_references("test(foo) & not test(foobar)").unwrap();
        assert_eq!(dangling_selections(&shadowed, &universe[..1]).len(), 1);
        assert_eq!(stale_exclusions(&shadowed, &universe[..1]).len(), 0);
    }

    #[test]
    fn conditions_that_cannot_be_evaluated_refuse_selecting_names_and_drop_exclusions() {
        for expression in [
            "deps(=x) & test(=a)",
            "binary_id(p::t) & test(=a)",
            "package(/^p.*$/) & test(=a)",
            "platform(host) & (test(=a) | test(=b))",
        ] {
            let error = filterset_references(expression).unwrap_err();
            assert!(
                error.contains("cannot determine which tests test(=a) selects: the condition"),
                "{expression}: {error}"
            );
        }
        assert!(
            filterset_references("deps(=x) - test(=a)")
                .unwrap()
                .is_empty()
        );
        // An uninterpretable predicate outside every name's conjunction does
        // not affect any reference.
        assert_eq!(
            filterset_references("deps(=x) | test(=a)").unwrap()[0].scope,
            []
        );
    }

    #[test]
    fn malformed_non_ascii_text_is_an_error_not_a_panic() {
        for malformed in [
            "test(=a) \u{1f4a5}",
            "\u{1f4a5}",
            "test(=a) o\u{1f4a5}",
            "test(=a) a\u{e9}",
            "not\u{1f4a5}",
            "test(=a) |\u{1f4a5}",
        ] {
            assert!(
                filterset_references(malformed).is_err(),
                "accepted {malformed}"
            );
        }
        assert_eq!(
            filterset_references("test(=caf\u{e9})").unwrap()[0].matcher,
            NameMatcher::Exact("caf\u{e9}".into())
        );
    }

    #[test]
    fn closed_regex_alternation_names_each_alternative() {
        let references =
            filterset_references("test(/^(chaos_buck_getpid|chaos_buck_uname)$/)").unwrap();
        assert_eq!(
            references
                .iter()
                .map(|reference| reference.matcher.clone())
                .collect::<Vec<_>>(),
            [
                NameMatcher::Exact("chaos_buck_getpid".into()),
                NameMatcher::Exact("chaos_buck_uname".into()),
            ]
        );
        let prefix =
            filterset_references("test(/^ptrace_completion::tests::real_random_/)").unwrap();
        assert_eq!(
            prefix[0].matcher,
            NameMatcher::Prefix("ptrace_completion::tests::real_random_".into())
        );
        assert_eq!(
            filterset_references("test(/_suffix$/)").unwrap()[0].matcher,
            NameMatcher::Suffix("_suffix".into())
        );
        assert_eq!(
            filterset_references("test(~middle) | test(bare)")
                .unwrap()
                .iter()
                .map(|reference| reference.matcher.clone())
                .collect::<Vec<_>>(),
            [
                NameMatcher::Contains("middle".into()),
                NameMatcher::Contains("bare".into()),
            ]
        );
    }

    #[test]
    fn uninterpretable_selecting_patterns_refuse_and_exclusions_are_dropped() {
        for expression in [
            "test(/^a.*b$/)",
            "test(/^a|b$/)",
            "test(#a[bc])",
            "test(=a\\)b)",
            "test()",
        ] {
            assert!(
                filterset_references(expression).is_err(),
                "accepted {expression}"
            );
        }
        assert!(
            filterset_references("not test(/^a.*b$/)")
                .unwrap()
                .is_empty()
        );
        for malformed in [
            "test(=a",
            "test(=a) |",
            "(test(=a)",
            "test(=a) test(=b)",
            "| test(=a)",
        ] {
            assert!(
                filterset_references(malformed).is_err(),
                "accepted {malformed}"
            );
        }
    }

    #[test]
    fn command_line_names_skips_and_filtersets_are_all_extracted() {
        let references = selection_references(&args(&[
            "--manifest-path",
            "Cargo.toml",
            "--locked",
            "--profile",
            "ci",
            "-p",
            "hermit",
            "--test",
            "cli",
            "-j",
            "1",
            "--no-tests=fail",
            "-E",
            "test(=a)",
            "--filterset=not test(=b)",
            "positional",
            "--",
            "--exact",
            "--ignored",
            "after",
            "--skip",
            "skipped",
        ]))
        .unwrap();
        assert_eq!(
            written(&references, Polarity::Selects),
            [
                (
                    "name filter \"positional\" (--exact)".into(),
                    NameMatcher::Exact("positional".into())
                ),
                (
                    "name filter \"after\" (--exact)".into(),
                    NameMatcher::Exact("after".into())
                ),
                ("test(=a)".into(), NameMatcher::Exact("a".into())),
            ]
        );
        assert_eq!(
            written(&references, Polarity::Excludes),
            [
                (
                    "--skip \"skipped\"".into(),
                    NameMatcher::Exact("skipped".into())
                ),
                ("test(=b)".into(), NameMatcher::Exact("b".into())),
            ]
        );
        let substring = selection_references(&args(&["--", "--skip", "run_kvm_", "java"])).unwrap();
        assert_eq!(substring[0].matcher, NameMatcher::Contains("java".into()));
        assert_eq!(
            substring[1].matcher,
            NameMatcher::Contains("run_kvm_".into())
        );
    }

    #[test]
    fn unknown_options_refuse_instead_of_guessing_whether_they_take_a_value() {
        for unknown in [
            args(&["--mystery", "name"]),
            args(&["--", "--nocapture"]),
            args(&["-E"]),
            args(&["--", "--skip"]),
        ] {
            assert!(
                selection_references(&unknown).is_err(),
                "accepted {unknown:?}"
            );
        }
        assert!(
            selection_references(&args(&["-j4", "-Etest(=a)", "--test=cli"])).unwrap()[0].polarity
                == Polarity::Selects
        );
    }

    #[test]
    fn dangling_and_stale_references_are_judged_against_their_own_sets() {
        let references = selection_references(&args(&[
            "-E",
            "test(=kept) | test(=renamed_away) | test(/^(also_kept|gone)$/)",
            "--",
            "--skip",
            "no_such_test",
            "--skip",
            "kept_skip",
        ]))
        .unwrap();
        let selected = [identity("p", "kept"), identity("p", "also_kept")];
        let universe = [
            identity("p", "kept"),
            identity("p", "also_kept"),
            identity("p", "kept_skip_case"),
        ];
        assert_eq!(
            dangling_selections(&references, &selected)
                .iter()
                .map(|reference| reference.matcher.clone())
                .collect::<Vec<_>>(),
            [
                NameMatcher::Exact("renamed_away".into()),
                NameMatcher::Exact("gone".into()),
            ]
        );
        assert_eq!(
            stale_exclusions(&references, &universe)
                .iter()
                .map(|reference| reference.written.as_str())
                .collect::<Vec<_>>(),
            ["--skip \"no_such_test\""]
        );
    }

    #[test]
    fn globs_match_like_nextest() {
        let glob = NameMatcher::Glob("mem_race::*_detcore".into());
        assert!(glob.matches("mem_race::top_detcore"));
        assert!(!glob.matches("mem_race::top_detcore_extra"));
        assert!(NameMatcher::Glob("a?c".into()).matches("abc"));
        assert!(!NameMatcher::Glob("a?c".into()).matches("ac"));
        assert!(NameMatcher::Glob("*".into()).matches(""));
    }
}
