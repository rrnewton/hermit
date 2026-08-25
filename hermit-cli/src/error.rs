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
/// parent -- so there is no cross-version compatibility surface. `serde(default)`
/// is belt-and-braces so an older record still reads as an ordinary `Error`.
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
        Self {
            error,
            context,
            kind: FailureKind::Error,
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
