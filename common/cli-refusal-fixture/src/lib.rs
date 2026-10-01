/* Copyright (c) Meta Platforms, Inc. and affiliates. */
//! Exact no-fork adapter bytes for the maintained CLI refusal integration test.
//! This crate is a dev dependency only; it grants no execution authority.

/// Compiled by this dev-only crate, never by the production Hermit build script.
pub const EXECUTABLE: &[u8] = include_bytes!(env!("HERMIT_STARTUP_STDERR_FIXTURE"));
