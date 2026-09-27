/* SPDX-License-Identifier: GPL-2.0 */
/* Explicit successor build; the legacy translation unit remains available
 * for its historical controls. Only the reviewed grouped loader may load it. */
#define AP_GROUPED_PROVIDER 1
#include "provider.bpf.c"
