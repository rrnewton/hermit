/* SPDX-License-Identifier: GPL-2.0 */
#include "keeper-channel.h"
/* Built/staged as an explicit immutable helper resource. It links libbpf here,
 * outside the ordinary Hermit binary and outside the guest PID namespace. */
int main(void) { return ug_keeper_main(); }
