/* Exact adapter ABI required by the integrated, artifact-authenticating loader. */
#include <stddef.h>
#include "provider.h"
_Static_assert(sizeof(void *)==8,"Linux x86_64 adapter");
_Static_assert(sizeof(struct ap_identity)==24,"identity ABI");
_Static_assert(sizeof(struct ap_raw_state)==40,"raw state ABI");
_Static_assert(offsetof(struct ap_raw_state,lowat)==16,"lowat ABI");
_Static_assert(offsetof(struct ap_raw_state,window_clamp)==32,"clamp ABI");
_Static_assert(offsetof(struct ap_raw_state,userlocks)==36,"userlocks ABI");
_Static_assert(sizeof(struct ap_creation)==240,"creation ABI");
_Static_assert(offsetof(struct ap_creation,listener)==8,"listener ABI");
_Static_assert(offsetof(struct ap_creation,child)==32,"child ABI");
_Static_assert(offsetof(struct ap_creation,listener_generation)==56,"generation ABI");
_Static_assert(offsetof(struct ap_creation,overlap)==80,"overlap ABI");
_Static_assert(offsetof(struct ap_creation,listener_before)==88,"before ABI");
_Static_assert(offsetof(struct ap_creation,listener_after)==128,"after ABI");
_Static_assert(offsetof(struct ap_creation,child_created)==168,"child state ABI");
_Static_assert(offsetof(struct ap_creation,local)==208,"local ABI");
_Static_assert(offsetof(struct ap_creation,peer)==216,"peer ABI");
_Static_assert(offsetof(struct ap_creation,cookie_at_creation)==224,"cookie ABI");
_Static_assert(offsetof(struct ap_creation,phase)==232,"creation phase ABI");
_Static_assert(sizeof(struct ap_command_result)==128,"command ABI");
_Static_assert(offsetof(struct ap_command_result,identity)==32,"command identity ABI");
_Static_assert(offsetof(struct ap_command_result,creation)==56,"command creation ABI");
_Static_assert(offsetof(struct ap_command_result,state)==72,"command state ABI");
_Static_assert(offsetof(struct ap_command_result,returned)==112,"command result ABI");
_Static_assert(offsetof(struct ap_command_result,phase)==120,"command phase ABI");
_Static_assert(sizeof(struct ap_status)==88,"status ABI");
_Static_assert(sizeof(struct ap_program_id)==8,"identifier ABI");
u64 ap_adapter_abi_version(void) { return 0x4150525553540001ULL; }
