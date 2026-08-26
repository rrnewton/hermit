/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use serde::Deserialize;
use serde::Serialize;

pub type Error = anyhow::Error;

pub use anyhow::Context;

/// A serializable error. This is useful for sending an error to the parent
/// process. This works by converting an error into a string via its `Display`
/// implementation. Although we lose type information in the process of
/// converting to a string, this preserves the error message and its error chain.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, Eq, PartialEq)]
pub enum FailureKind {
    /// The child reported a failure it decided to report.
    #[default]
    Error,
    /// The child PANICKED and the panic was caught and reported.
    ///
    /// ⚠️ THIS IS THE ONE BIT THAT PROSE CANNOT CARRY. A caught panic and a
    /// reported error arrive at the parent as the same shape -- a message and a
    /// chain of causes -- so without a discriminant the parent must either
    /// match on English or call them the same thing. It called them the same
    /// thing, which is why a tracer panic and a bad flag were indistinguishable.
    Panic,
    /// The child stopped because a FAIL-CLOSED POLICY refused the run.
    ///
    /// ⚠️ A REFUSAL AND A REPORTED ERROR ARE THE SAME SHAPE HERE TOO, which is
    /// the identical argument that justified `Panic`. Record mode sets
    /// `exit_on_unsupported_syscall` and `shutdown_on_unsupported_syscall:
    /// false`, so it returns a typed `UnsupportedSyscallError` through Reverie
    /// instead of calling `unrecoverable_shutdown`. That path never produces an
    /// exit status of its own, so the status channel -- which is what carries
    /// `HERMIT_POLICY_REFUSAL_EXIT` on the run path -- has nothing to say, and
    /// the refusal arrived indistinguishable from "hermit broke": exit 125,
    /// `class=container-child-exit`. Two configurations of one policy reported
    /// opposite things about the same decision.
    PolicyRefusal,
}

/// ⚠️ THE `kind` FIELD IS THE THIRD AND LAST OF ONE CHAIN OF FLATTENINGS.
/// The same property -- the failure's CLASS -- was destroyed at three different
/// boundaries, in series on one path:
///
///   1. `main`'s `unwrap_or_else` mapped EVERY error to one exit code;
///   2. `with_container` flattened reverie's typed `RunError::ExitStatus` into
///      opaque prose with `.context(..)?`;
///   3. THIS type crosses a process boundary carrying only strings.
///
/// They are three distinct boundaries with three mechanisms, but they answer one
/// question, so one discriminant serves all three. Preserving it here is what
/// lets the parent classify without parsing English.
///
/// Extending a serialized type is safe HERE specifically because the writer and
/// the reader are the SAME BINARY IMAGE -- hermit's container child and its own
/// parent, over a fork-local pipe -- so there is no cross-version compatibility
/// surface.
///
/// ⚠️ `serde(default)` BELOW IS NOT A COMPATIBILITY GUARANTEE, and must not be
/// read as one. reverie serializes this with `bincode::config::legacy()`
/// (`reverie-process/src/container.rs:861`), which is NOT self-describing: a
/// record written by an older binary with two fields would fail to decode
/// outright rather than defaulting the third. The attribute is harmless and
/// keeps the field optional at the Rust level; the property that actually makes
/// this safe is the same-image one above.
#[derive(Debug, Clone, Serialize, Deserialize, Eq, PartialEq)]
pub struct SerializableError {
    /// The main error.
    error: String,
    /// The chain of causes. This is empty if there are no associated causes.
    context: Vec<String>,
    /// What CLASS of failure this was. See [`FailureKind`].
    #[serde(default)]
    kind: FailureKind,
}

impl SerializableError {
    /// The class this failure was reported as.
    pub fn kind(&self) -> FailureKind {
        self.kind
    }

    /// Re-tag an error as a caught panic. Called at the catch site, which is the
    /// only place that still knows a panic is what happened.
    pub fn into_panic(mut self) -> Self {
        self.kind = FailureKind::Panic;
        self
    }
}

impl From<Error> for SerializableError {
    fn from(err: Error) -> Self {
        let error = err.to_string();
        let context = err.chain().skip(1).map(ToString::to_string).collect();
        // ⚠️ CLASSIFIED HERE BECAUSE THIS IS THE LAST PLACE THE TYPE EXISTS.
        // Everything below this line is strings: `From<SerializableError> for
        // Error` rebuilds the chain with `Error::msg`, so a downcast on the far
        // side can only ever fail. Detecting the refusal after the boundary
        // would mean matching on English, which is the thing `kind` exists to
        // avoid.
        let kind = if err
            .chain()
            .any(|cause| cause.is::<detcore::UnsupportedSyscallError>())
        {
            FailureKind::PolicyRefusal
        } else {
            FailureKind::Error
        };
        Self {
            error,
            context,
            kind,
        }
    }
}

impl From<SerializableError> for Error {
    fn from(mut err: SerializableError) -> Self {
        if let Some(root_cause) = err.context.pop() {
            let mut error = Self::msg(root_cause);

            while let Some(context) = err.context.pop() {
                error = error.context(context);
            }

            error.context(err.error)
        } else {
            Self::msg(err.error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn into_serializable_error() {
        let error = Error::msg("root cause")
            .context("a")
            .context("b")
            .context("c");

        assert_eq!(
            SerializableError::from(error),
            SerializableError {
                error: "c".into(),
                context: vec!["b".into(), "a".into(), "root cause".into(),],
                kind: FailureKind::Error,
            }
        );
    }

    #[test]
    fn from_serializable_error() {
        let error = Error::from(SerializableError {
            error: "c".into(),
            context: vec!["b".into(), "a".into(), "root cause".into()],
            kind: FailureKind::Error,
        });

        assert_eq!(
            error
                .chain()
                .map(ToString::to_string)
                .collect::<Vec<String>>(),
            ["c", "b", "a", "root cause"]
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<String>>(),
        )
    }
}
