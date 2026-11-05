//! Stop recovery advice carries the generation proved at the refusal site.

use std::path::Path;

use super::{AgentError, StopOptions};

/// Recovery supported by the exact state that refused a stop operation.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum RecoveryAction {
    /// Inspect the registry when no mutating recovery was proved safe to propose.
    #[default]
    Doctor,
    /// Retry stop with a captured generation and a record digest when required.
    Stop {
        /// Registered name proved by the refused operation.
        name: String,
        /// Exact registry generation captured before the refusal.
        token: String,
        /// SHA-256 of an adopted record that has no saved shell identity.
        record_sha256: Option<String>,
        /// Preserve the caller's request to retire an agentcloud viewer without halting.
        skip_cloud_halt: bool,
    },
    /// Archive the captured adopted generation after independently proving its harness dead.
    RetireDeadAdoption {
        /// Registered name from the exact private record snapshot.
        name: String,
        /// Exact registry generation captured before the refusal.
        token: String,
        /// SHA-256 of the original raw record bytes.
        record_sha256: String,
    },
    /// Inspect the rename that reserves both names; its CLI cannot assert this token.
    Rename {
        /// Original registered name in the validated journal.
        old: String,
        /// Destination registered name in the validated journal.
        new: String,
        /// Exact generation reserved by the journal.
        token: String,
    },
    /// Inspect the pending move; its CLI cannot assert this captured token.
    Move {
        /// Registered name from the validated move intent.
        name: String,
        /// Exact generation reserved by the intent.
        token: String,
    },
    /// Finish publication or cleanup of the recorded revive transaction.
    Revive {
        /// Registered name from the validated revive journal.
        name: String,
        /// Original generation reserved by the journal.
        token: String,
    },
}

impl RecoveryAction {
    fn token(&self) -> Option<&str> {
        match self {
            Self::Doctor => None,
            Self::Stop { token, .. }
            | Self::RetireDeadAdoption { token, .. }
            | Self::Rename { token, .. }
            | Self::Move { token, .. }
            | Self::Revive { token, .. } => Some(token),
        }
    }

    pub(super) fn for_assertions(self, options: &StopOptions) -> Self {
        if !self.valid_retirement_authority() {
            return Self::Doctor;
        }
        let digest = match &self {
            Self::Stop { record_sha256, .. } => record_sha256.as_deref(),
            Self::RetireDeadAdoption { record_sha256, .. } => Some(record_sha256.as_str()),
            _ => None,
        };
        if options
            .expected_token
            .as_deref()
            .is_some_and(|expected| self.token() != Some(expected))
            || options
                .expected_record_sha256
                .as_deref()
                .is_some_and(|expected| digest != Some(expected))
        {
            Self::Doctor
        } else {
            self
        }
    }

    fn arguments(&self) -> Vec<String> {
        if !self.valid_retirement_authority() {
            return vec!["doctor".to_owned()];
        }
        match self {
            Self::Doctor => vec!["doctor".to_owned()],
            Self::Stop {
                name,
                token,
                record_sha256,
                skip_cloud_halt,
            } => {
                let mut arguments = vec![
                    "stop".to_owned(),
                    name.clone(),
                    format!("--expected-token={token}"),
                ];
                if let Some(digest) = record_sha256 {
                    arguments.push("--recover-legacy-adoption".to_owned());
                    arguments.push(format!("--expected-record-sha256={digest}"));
                }
                if *skip_cloud_halt {
                    arguments.push("--skip-cloud-halt".to_owned());
                }
                arguments
            }
            Self::RetireDeadAdoption {
                name,
                token,
                record_sha256,
            } => vec![
                "stop".to_owned(),
                name.clone(),
                "--retire-dead-adoption".to_owned(),
                format!("--expected-token={token}"),
                format!("--expected-record-sha256={record_sha256}"),
            ],
            Self::Rename { .. } | Self::Move { .. } => vec!["doctor".to_owned()],
            Self::Revive { name, token } => vec![
                "revive".to_owned(),
                name.clone(),
                format!("--expected-token={token}"),
            ],
        }
    }

    fn valid_retirement_authority(&self) -> bool {
        match self {
            Self::RetireDeadAdoption {
                name,
                token,
                record_sha256,
            } => {
                super::name_pattern(name)
                    && !token.is_empty()
                    && token.len() <= 80
                    && token.bytes().all(|byte| {
                        byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'
                    })
                    && valid_record_digest(record_sha256)
            }
            _ => true,
        }
    }
}

pub(super) fn valid_record_digest(digest: &str) -> bool {
    digest.len() == 64
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// A stop refusal and the recovery action proved before that refusal.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StopFailure {
    /// The original failure, including its established process exit code.
    pub error: Box<AgentError>,
    /// Recovery captured by the operation, with doctor as the conservative default.
    pub recovery: RecoveryAction,
}

impl std::fmt::Display for StopFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.error.fmt(formatter)
    }
}

impl std::error::Error for StopFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.error.as_ref())
    }
}

/// CLI options captured before stop executes; this never reads registry state.
#[derive(Clone, Debug)]
pub(crate) struct StopContext {
    prefix: Option<Vec<String>>,
}

impl StopContext {
    pub(crate) fn new(registry: &Path, herdr_bin: &Path) -> Self {
        let absolute = if registry.is_absolute() {
            Some(registry.to_path_buf())
        } else {
            std::env::current_dir()
                .ok()
                .map(|directory| directory.join(registry))
        };
        Self::from_absolute(absolute.as_deref(), herdr_bin)
    }

    fn from_absolute(registry: Option<&Path>, herdr_bin: &Path) -> Self {
        let text = |path: &Path| {
            path.to_str()
                .filter(|value| !value.is_empty() && !value.contains('\0'))
                .map(str::to_owned)
        };
        let prefix = registry
            .and_then(text)
            .zip(text(herdr_bin))
            .map(|(registry, herdr_bin)| {
                vec![
                    "agentctl".to_owned(),
                    format!("--registry={registry}"),
                    format!("--herdr-bin={herdr_bin}"),
                ]
            });
        Self { prefix }
    }

    pub(crate) fn command(&self, recovery: &RecoveryAction) -> String {
        let (mut arguments, recovery) = match &self.prefix {
            Some(prefix) => (prefix.clone(), recovery),
            None => (vec!["agentctl".to_owned()], &RecoveryAction::Doctor),
        };
        arguments.extend(recovery.arguments());
        arguments
            .iter()
            .map(|argument| shell_word(argument))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

pub(crate) fn stop_reason(error: &impl std::fmt::Display) -> String {
    let reason = error.to_string();
    if reason.trim().is_empty() {
        "stop refused without a reason".to_owned()
    } else {
        reason
    }
}

fn shell_word(value: &str) -> String {
    if !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_@%+=:,./-".contains(&byte))
    {
        value.to_owned()
    } else {
        format!("'{}'", value.replace('\'', "'\"'\"'"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_commands_quote_literal_globals_and_generation_assertions() {
        let context = StopContext::new(
            Path::new("/work/record dir/'registry'"),
            Path::new("-herdr $literal 'quoted'"),
        );
        let digest = "a".repeat(64);
        let recovery = RecoveryAction::Stop {
            name: "worker".to_owned(),
            token: "-generation".to_owned(),
            record_sha256: Some(digest.clone()),
            skip_cloud_halt: true,
        };
        assert_eq!(
            context.command(&recovery),
            format!(
                "agentctl '--registry=/work/record dir/'\"'\"'registry'\"'\"'' '--herdr-bin=-herdr $literal '\"'\"'quoted'\"'\"'' stop worker --expected-token=-generation --recover-legacy-adoption --expected-record-sha256={digest} --skip-cloud-halt"
            )
        );
        assert_eq!(
            context.command(&RecoveryAction::Doctor),
            "agentctl '--registry=/work/record dir/'\"'\"'registry'\"'\"'' '--herdr-bin=-herdr $literal '\"'\"'quoted'\"'\"'' doctor"
        );
    }

    #[test]
    fn pending_recovery_tokens_are_checked_even_when_the_cli_has_no_token_flag() {
        for recovery in [
            RecoveryAction::Rename {
                old: "worker".to_owned(),
                new: "reviewer".to_owned(),
                token: "original".to_owned(),
            },
            RecoveryAction::Move {
                name: "worker".to_owned(),
                token: "original".to_owned(),
            },
            RecoveryAction::Revive {
                name: "worker".to_owned(),
                token: "original".to_owned(),
            },
        ] {
            assert_eq!(
                recovery.clone().for_assertions(&StopOptions {
                    expected_token: Some("replacement".to_owned()),
                    ..StopOptions::default()
                }),
                RecoveryAction::Doctor
            );
            assert_eq!(
                recovery.clone().for_assertions(&StopOptions {
                    expected_token: Some("original".to_owned()),
                    ..StopOptions::default()
                }),
                recovery
            );
        }
    }

    #[test]
    fn legacy_digest_assertions_cannot_be_replaced_with_newly_captured_bytes() {
        let digest = "a".repeat(64);
        let recovery = RecoveryAction::Stop {
            name: "foreign".to_owned(),
            token: "original".to_owned(),
            record_sha256: Some(digest.clone()),
            skip_cloud_halt: false,
        };
        assert_eq!(
            recovery.clone().for_assertions(&StopOptions {
                expected_token: Some("original".to_owned()),
                recover_legacy_adoption: true,
                retire_dead_adoption: false,
                expected_record_sha256: Some(digest),
                skip_cloud_halt: false,
            }),
            recovery
        );
        assert_eq!(
            recovery.for_assertions(&StopOptions {
                expected_token: Some("original".to_owned()),
                recover_legacy_adoption: true,
                retire_dead_adoption: false,
                expected_record_sha256: Some("b".repeat(64)),
                skip_cloud_halt: false,
            }),
            RecoveryAction::Doctor
        );
    }

    #[test]
    fn dead_adoption_recovery_has_a_distinct_selector_and_checks_both_assertions() {
        let digest = "a".repeat(64);
        let action = RecoveryAction::RetireDeadAdoption {
            name: "foreign".to_owned(),
            token: "-generation".to_owned(),
            record_sha256: digest.clone(),
        };
        let context = StopContext::new(Path::new("/work/registry"), Path::new("literal-herdr"));
        assert_eq!(
            context.command(&action),
            format!("agentctl --registry=/work/registry --herdr-bin=literal-herdr stop foreign --retire-dead-adoption --expected-token=-generation --expected-record-sha256={digest}")
        );
        assert_eq!(
            action.clone().for_assertions(&StopOptions::default()),
            action
        );
        assert_eq!(
            action.clone().for_assertions(&StopOptions {
                expected_token: Some("-generation".to_owned()),
                expected_record_sha256: Some(digest),
                retire_dead_adoption: true,
                ..StopOptions::default()
            }),
            action
        );
        for options in [
            StopOptions {
                expected_token: Some("replacement".to_owned()),
                ..StopOptions::default()
            },
            StopOptions {
                expected_record_sha256: Some("b".repeat(64)),
                ..StopOptions::default()
            },
            StopOptions {
                expected_record_sha256: Some("bad hash".to_owned()),
                ..StopOptions::default()
            },
        ] {
            assert_eq!(
                action.clone().for_assertions(&options),
                RecoveryAction::Doctor
            );
        }
    }

    #[test]
    fn dead_adoption_recovery_never_formats_malformed_captured_authority() {
        let context = StopContext::new(Path::new("/work/registry"), Path::new("literal-herdr"));
        for (name, token, digest) in [
            ("bad name", "generation".to_owned(), "a".repeat(64)),
            ("foreign", String::new(), "a".repeat(64)),
            ("foreign", "bad\0token".to_owned(), "a".repeat(64)),
            ("foreign", "A".to_owned(), "a".repeat(64)),
            ("foreign", "a".repeat(81), "a".repeat(64)),
            ("foreign", "generation".to_owned(), "A".repeat(64)),
            ("foreign", "generation".to_owned(), "a".repeat(63)),
        ] {
            let action = RecoveryAction::RetireDeadAdoption {
                name: name.to_owned(),
                token,
                record_sha256: digest,
            };
            assert_eq!(
                context.command(&action),
                "agentctl --registry=/work/registry --herdr-bin=literal-herdr doctor"
            );
            assert_eq!(
                action.for_assertions(&StopOptions::default()),
                RecoveryAction::Doctor
            );
        }
    }

    #[test]
    fn stop_reasons_have_a_nonempty_fallback_without_changing_error_codes() {
        for message in ["", " \t\n"] {
            let error = AgentError::Delivery(message.to_owned());
            assert_eq!(stop_reason(&error), "stop refused without a reason");
            assert_eq!(error.exit_code(), 75);
        }
        let error = crate::error::AdapterError::unavailable("control unavailable");
        assert_eq!(stop_reason(&error), "control unavailable");
        assert_eq!(error.exit_code(), 69);
    }

    #[test]
    fn unsafe_cli_context_proposes_only_the_canonical_doctor_command() {
        let context = StopContext::new(Path::new("/work/registry"), Path::new("bad\0herdr"));
        assert_eq!(
            context.command(&RecoveryAction::Revive {
                name: "worker".to_owned(),
                token: "original".to_owned(),
            }),
            "agentctl doctor"
        );
    }
}
