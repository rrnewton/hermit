/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

#include <assert.h>
#include "native_callback_scope.h"

static void refused_entry(native_callback_scope_t* state, unsigned kind,
                          unsigned entries) {
  native_callback_scope_t before = *state;
  assert(!native_callback_scope_enter(state, kind, entries));
  assert(state->epoch == before.epoch && state->kind == before.kind &&
         state->phase == before.phase && state->active == before.active &&
         state->publication_active == before.publication_active);
}

static void refused_leave(native_callback_scope_t* state, unsigned entries) {
  native_callback_scope_t before = *state;
  assert(!native_callback_scope_leave(state, entries));
  assert(state->epoch == before.epoch && state->kind == before.kind &&
         state->phase == before.phase && state->active == before.active &&
         state->publication_active == before.publication_active);
}

int main(void) {
  native_callback_scope_t state = {0};
  assert(!native_callback_scope_describable(&state, 1));
  refused_leave(&state, 1);
  refused_entry(&state, NATIVE_CALLBACK_ORDINARY, 1);
  refused_entry(&state, 0, 0);
  refused_entry(&state, 4, 0);

  /* A first instruction/registration can own RPC without a prior syscall. */
  assert(native_callback_scope_enter(&state, NATIVE_CALLBACK_ORDINARY, 0));
  assert(state.epoch == 1 && state.kind == NATIVE_CALLBACK_ORDINARY);
  assert(native_callback_scope_describable(&state, 1));
  assert(!native_callback_scope_describable(&state, 0));
  assert(!native_callback_scope_describable(&state, 2));
  refused_entry(&state, NATIVE_CALLBACK_SYSCALL, 0);
  refused_leave(&state, 2);
  assert(native_callback_scope_leave(&state, 1));
  assert(!state.active && state.kind == 0 && state.epoch == 1);
  /* Evidence-only callbacks with one Rust entry cannot resurrect the owner. */
  assert(!native_callback_scope_describable(&state, 1));
  refused_leave(&state, 1);

  assert(native_callback_scope_enter(&state, NATIVE_CALLBACK_SYSCALL, 0));
  assert(state.epoch == 2 && state.kind == NATIVE_CALLBACK_SYSCALL);
  for (unsigned phase = 1; phase <= 2; ++phase) {
    state.phase = phase;
    refused_leave(&state, 1);
    refused_entry(&state, NATIVE_CALLBACK_ORDINARY, 0);
  }
  state.phase = 3;
  state.publication_active = true;
  refused_leave(&state, 1);
  state.publication_active = false;
  assert(native_callback_scope_leave(&state, 1));
  assert(state.phase == 3);
  assert(native_callback_scope_enter(&state, NATIVE_CALLBACK_ORDINARY, 0));
  assert(state.epoch == 3 && state.phase == 3);
  assert(native_callback_scope_leave(&state, 1));
  assert(state.phase == 3);
  assert(native_callback_scope_enter(&state, NATIVE_CALLBACK_OWNED, 0));
  assert(state.epoch == 4 && state.phase == 0);
  assert(native_callback_scope_leave(&state, 1));

  /* A stale copied live lease cannot become fresh merely by having no borrow. */
  state.phase = 1;
  refused_entry(&state, NATIVE_CALLBACK_ORDINARY, 0);
  state.phase = 2;
  refused_entry(&state, NATIVE_CALLBACK_OWNED, 0);
  state.phase = 0;
  state.publication_active = true;
  refused_entry(&state, NATIVE_CALLBACK_SYSCALL, 0);
  state.publication_active = false;
  state.epoch = UINT64_MAX;
  refused_entry(&state, NATIVE_CALLBACK_ORDINARY, 0);
  assert(!native_callback_scope_describable(&state, 1));
  return 0;
}
