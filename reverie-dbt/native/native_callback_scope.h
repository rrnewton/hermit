/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#ifndef REVERIE_DBT_NATIVE_CALLBACK_SCOPE_H
#define REVERIE_DBT_NATIVE_CALLBACK_SCOPE_H

#include <stdbool.h>
#include <stdint.h>

/* RPC ownership is broader than SDK capture eligibility. ORDINARY must never
 * acquire a syscall or detached-signal admission right from its numeric kind. */
enum {
  NATIVE_CALLBACK_SYSCALL = 1,
  NATIVE_CALLBACK_OWNED = 2,
  NATIVE_CALLBACK_ORDINARY = 3,
};

typedef struct {
  uint64_t epoch;
  uint32_t kind;
  uint32_t phase;
  bool active;
  bool publication_active;
} native_callback_scope_t;

/* Called once per actual application-to-Tool borrow, never for helper work.
 * Refusal is nonmutating; the bridge terminates instead of entering Rust. */
static inline bool native_callback_scope_enter(
    native_callback_scope_t* scope, uint32_t kind, uint32_t runtime_entries) {
  if (runtime_entries != 0 || scope->active || scope->phase == 1 ||
      scope->phase == 2 || scope->publication_active ||
      scope->epoch == UINT64_MAX ||
      (kind != NATIVE_CALLBACK_ORDINARY && kind != NATIVE_CALLBACK_SYSCALL &&
       kind != NATIVE_CALLBACK_OWNED))
    return false;
  /* Only the original syscall/owned entry has supplied the existing dispatcher
   * retirement opportunity. Ordinary cleanup preserves phase-3 release intent;
   * this mirror never retires the SDK's physical guard itself. */
  if (kind != NATIVE_CALLBACK_ORDINARY)
    scope->phase = 0;
  scope->kind = kind;
  ++scope->epoch;
  scope->active = true;
  return true;
}

static inline bool native_callback_scope_describable(
    const native_callback_scope_t* scope, uint32_t runtime_entries) {
  return runtime_entries == 1 && scope->active && scope->epoch != 0;
}

/* Called before the final evidence/runtime borrow is dropped. A live exclusion
 * cannot escape through a completed ordinary RPC or a callback return. */
static inline bool native_callback_scope_leave(
    native_callback_scope_t* scope, uint32_t runtime_entries) {
  if (!native_callback_scope_describable(scope, runtime_entries) ||
      scope->phase == 1 || scope->phase == 2 || scope->publication_active)
    return false;
  scope->active = false;
  scope->kind = 0;
  return true;
}

#endif
