/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Build helper and finalizer for Hermit's release installation package.
//!
//! Default staging includes the core LiteInst resources. Full installations opt
//! into `third-party-backends` to also stage DynamoRIO, SaBRe and e9patch. Both
//! paths require the ordinary `stage-detcore-liteinst` finalizer after Cargo
//! releases the target; selecting a package never bypasses its artifact proof.
