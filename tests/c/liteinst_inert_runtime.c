/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * Exports the in-guest Detcore runtime's initializer but registers no
 * constructor, so preloading it would run the guest without installing
 * Detcore. Hermit must refuse it before dispatch.
 */
void detcore_liteinst_initialize(void) {}
