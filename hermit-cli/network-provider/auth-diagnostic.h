#ifndef AP_AUTH_DIAGNOSTIC_H
#define AP_AUTH_DIAGNOSTIC_H
#include "provider.h"
_Static_assert(sizeof(struct ap_setter_rejection)==sizeof(struct ap_command_result),"ABI7 reserved slot overlay must stay136B");
/* Actual writer boundary: preserve the old136-byte diagnostic and all64
 * original command bytes. This never authorizes an operation. The complete
 * ABI10 command remains owned in task storage and the pending command. */
static __attribute__((always_inline)) inline void ap_setter_diagnostic_raw(
    struct ap_setter_rejection *d,const struct ap_task_command *raw) {
    d->raw_present=raw!=0;
    if(raw)__builtin_memcpy(&d->raw,raw,sizeof(d->raw));
}
enum ap_auth_mismatch {
    AP_AUTH_ABSENT=1, AP_AUTH_ZERO_PROVIDER=2, AP_AUTH_PROVIDER=4,
    AP_AUTH_OPERATION=8, AP_AUTH_OBJECT=16, AP_AUTH_LEVEL=32,
    AP_AUTH_OPTION=64, AP_AUTH_BEFORE=128, AP_AUTH_AFTER=256,
    AP_AUTH_ZERO_COMMAND=512, AP_AUTH_TIMEOUT=1024,
};
/* Shared scalar authorization and first-failure diagnostic. The exact bounded
 * READY reservation is checked separately by ap_command_reservation_matches.
 * Ticket range is deliberately not confused with physical slot capacity. */
static __attribute__((always_inline)) inline u64 ap_authorization_mismatch(
    const struct ap_task_command *c,u64 provider,u64 object,u64 generation,s32 level,s32 option) {
    if(!c)return AP_AUTH_ABSENT;
    u64 mask=0;
    if(!c->provider)mask|=AP_AUTH_ZERO_PROVIDER;
    if(c->provider!=provider)mask|=AP_AUTH_PROVIDER;
    if(c->operation!=AP_SETTER)mask|=AP_AUTH_OPERATION;
    if(c->expected_object!=object)mask|=AP_AUTH_OBJECT;
    if(c->expected_level!=level)mask|=AP_AUTH_LEVEL;
    if(c->expected_option!=option)mask|=AP_AUTH_OPTION;
    if(c->generation_before!=generation)mask|=AP_AUTH_BEFORE;
    if(c->generation_after!=generation+1)mask|=AP_AUTH_AFTER;
    if(!c->command)mask|=AP_AUTH_ZERO_COMMAND;
    if(c->expected_timeout_ticks)mask|=AP_AUTH_TIMEOUT;
    return mask;
}
#endif
