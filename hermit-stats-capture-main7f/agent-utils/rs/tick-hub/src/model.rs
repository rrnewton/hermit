//! Declarative reminder, gate, emission, and health-check model.

use indexmap::IndexMap;

/// A cadence at or below this value is checked on every tick.
pub const EVERY_TICK: i64 = 0;

/// What a fired reminder emits.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EmitKind {
    /// A dispatchable `ACTION:` line.
    Action,
    /// An informational `NOTE:` line.
    Note,
}

impl EmitKind {
    /// The stable interchange spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Action => "action",
            Self::Note => "note",
        }
    }

    /// Parse an interchange spelling.
    pub fn from_value(value: &str) -> Option<Self> {
        match value {
            "action" => Some(Self::Action),
            "note" => Some(Self::Note),
            _ => None,
        }
    }
}

/// The condition under which a gate command fires its reminder.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GateWhen {
    /// The command exited zero.
    Success,
    /// The command exited non-zero.
    Failure,
    /// The command wrote non-whitespace stdout.
    Nonempty,
    /// Always fire; the command may exist only to capture values.
    Always,
}

impl GateWhen {
    /// The stable interchange spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Failure => "failure",
            Self::Nonempty => "nonempty",
            Self::Always => "always",
        }
    }

    /// Parse an interchange spelling.
    pub fn from_value(value: &str) -> Option<Self> {
        match value {
            "success" => Some(Self::Success),
            "failure" => Some(Self::Failure),
            "nonempty" => Some(Self::Nonempty),
            "always" => Some(Self::Always),
            _ => None,
        }
    }
}

/// An optional shell command that decides whether a due reminder fires.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Gate {
    /// Shell source passed to `bash -c` by the production runner.
    pub cmd: String,
    /// Which command outcome fires the reminder.
    pub when: GateWhen,
    /// Whether `key=value` stdout lines are captured.
    pub capture: bool,
    /// Whether this gate may start early and overlap other due gate commands.
    pub parallel: bool,
    /// Seconds this gate's command may run before it is terminated and reported as
    /// NO-SIGNAL. `None` uses the runner's default. Appended so existing positional
    /// construction is unaffected.
    pub timeout_secs: Option<i64>,
}

impl Gate {
    /// Construct a success-gated command with capture disabled.
    pub fn new(cmd: impl Into<String>) -> Self {
        Self {
            cmd: cmd.into(),
            when: GateWhen::Success,
            capture: false,
            parallel: false,
            timeout_secs: None,
        }
    }
}

/// The line emitted by a fired reminder.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Emit {
    /// Action or note.
    pub kind: EmitKind,
    /// Action title or note text.
    pub title: String,
    /// Action handler name. Notes do not use this field.
    pub skill: String,
    /// Ordered action fields. Order is observable in the line protocol.
    pub fields: IndexMap<String, String>,
}

impl Emit {
    /// Construct an action emission.
    pub fn action(skill: impl Into<String>, title: impl Into<String>) -> Self {
        Self {
            kind: EmitKind::Action,
            title: title.into(),
            skill: skill.into(),
            fields: IndexMap::new(),
        }
    }

    /// Construct a note emission.
    pub fn note(text: impl Into<String>) -> Self {
        Self {
            kind: EmitKind::Note,
            title: text.into(),
            skill: String::new(),
            fields: IndexMap::new(),
        }
    }
}

/// One recurring responsibility.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Reminder {
    /// Stable reminder and fired-state key.
    pub name: String,
    /// What to emit when the reminder fires.
    pub emit: Emit,
    /// Minimum seconds between completed checks; zero means every tick.
    pub cadence_secs: i64,
    /// State flags that must all be truthy.
    pub requires_flags: Vec<String>,
    /// Optional shell gate.
    pub gate: Option<Gate>,
    /// Reminders whose explicit no-result makes this reminder's silence unevaluable.
    /// Dependencies never prevent this reminder's gate from running or suppress a real emission.
    pub depends_on: Vec<String>,
    /// Optional phase for `cadence_secs`, in seconds past each cadence boundary. When set,
    /// the reminder is due at the largest instant `T <= now` with `T % cadence == offset`,
    /// and only if it has not fired since. Derived from absolute time, so the phase survives
    /// a restart and is identical on replay. `None` keeps the plain elapsed rule.
    pub cadence_offset_secs: Option<i64>,
    /// How long after that instant the reminder may still be offered. Without it a reminder
    /// that did not complete is offered on every later tick, including the other phase's --
    /// the pile-up phasing exists to prevent. It never marks a missed reminder done.
    pub cadence_window_secs: Option<i64>,
}

impl Reminder {
    /// Construct an every-tick reminder without flags or a gate.
    pub fn new(name: impl Into<String>, emit: Emit) -> Self {
        Self {
            name: name.into(),
            emit,
            cadence_secs: EVERY_TICK,
            requires_flags: Vec::new(),
            gate: None,
            depends_on: Vec::new(),
            cadence_offset_secs: None,
            cadence_window_secs: None,
        }
    }
}

/// A freshness probe over a filesystem glob.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HealthCheck {
    /// Stable check name.
    pub name: String,
    /// Filesystem glob; the newest matching mtime is measured.
    pub glob: String,
    /// Maximum healthy age in seconds.
    pub threshold_secs: i64,
    /// Human-readable detail included in the health line.
    pub detail: String,
}

impl HealthCheck {
    /// Construct a health check with no detail string.
    pub fn new(name: impl Into<String>, glob: impl Into<String>, threshold_secs: i64) -> Self {
        Self {
            name: name.into(),
            glob: glob.into(),
            threshold_secs,
            detail: String::new(),
        }
    }
}

/// A complete reminder set.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TickConfig {
    /// Reminders in registration order.
    pub reminders: Vec<Reminder>,
    /// Health checks in registration order.
    pub health_checks: Vec<HealthCheck>,
    /// Free-form documentation that does not affect behavior.
    pub description: String,
}

impl TickConfig {
    /// Return reminders keyed by name. Later duplicates replace earlier values.
    pub fn by_name(&self) -> IndexMap<String, &Reminder> {
        let mut out = IndexMap::new();
        for reminder in &self.reminders {
            out.insert(reminder.name.clone(), reminder);
        }
        out
    }
}
