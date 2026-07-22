/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

pub fn xfail_dbi(reason: &str) -> bool {
    if std::env::var("HERMIT_BACKEND").as_deref() != Ok("dbi") {
        return false;
    }

    eprintln!("DBI_XFAIL: {reason}");
    true
}
