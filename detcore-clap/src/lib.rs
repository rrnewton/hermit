/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! The `clap` that detcore-model sees when it is built without `std`, for the
//! Narf kernel target (`x86_64-unknown-none`), where clap does not build. On
//! `target_os = "none"` detcore-model binds this crate as `clap`.
//!
//! Without `std` there is no command line to parse: the host parses it and
//! passes the resulting `Config` in, serialized. So `Parser` here generates
//! nothing. It declares `clap` as its helper attribute, so the `#[clap(...)]`
//! attributes on `Config` and its fields are accepted and ignored, and
//! `config.rs` keeps one definition of each option for both builds. What
//! clap's derive would have generated (`Config::parse_from` and the `Default`
//! built on it) is left to the host build.
//!
//! As a procedural macro, this crate is compiled for the machine running the
//! compiler, whatever the target.

use proc_macro::TokenStream;

/// clap's `Parser` derive, generating nothing; see the crate documentation.
#[proc_macro_derive(Parser, attributes(clap))]
pub fn parser(_item: TokenStream) -> TokenStream {
    TokenStream::new()
}
