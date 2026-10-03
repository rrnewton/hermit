/* Exact adapter ABI required by the integrated, artifact-authenticating loader. */
#include <stddef.h>
#include "provider.h"
#include "stream-copy.h"
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
_Static_assert(sizeof(struct ap_command_result)==136,"command ABI");
_Static_assert(offsetof(struct ap_command_result,identity)==32,"command identity ABI");
_Static_assert(offsetof(struct ap_command_result,creation)==56,"command creation ABI");
_Static_assert(offsetof(struct ap_command_result,state)==72,"command state ABI");
_Static_assert(offsetof(struct ap_command_result,returned)==112,"command result ABI");
_Static_assert(offsetof(struct ap_command_result,phase)==120,"command phase ABI");
_Static_assert(sizeof(struct ap_status)==88,"status ABI");
_Static_assert(sizeof(struct ap_program_id)==8,"identifier ABI");
#ifndef AP_NATIVE_COPY_VERSION
#define AP_NATIVE_COPY_VERSION AP_STREAM_COPY_VERSION
#endif
/* Declaring ABI8 is not enough to manufacture a V5 producer. The legacy
 * header has no frontier grammar; a package selecting5 must fail compilation
 * until the reviewed V5 header/producer is included in this exact artifact. */
#if AP_NATIVE_COPY_VERSION != AP_STREAM_COPY_VERSION
#ifndef AP_STREAM_COPY_VERSION_FRONTIER
#error "Selected copy grammar has no compiled native producer"
#elif AP_NATIVE_COPY_VERSION != AP_STREAM_COPY_VERSION_FRONTIER
#error "Selected copy grammar is not a declared native producer grammar"
#endif
#endif
/* ABI10 adds a distinct632-byte blocking TX capture and72-byte internal
 * command. Older hosts must reject before resolving any operation. */
#if AP_NATIVE_COPY_VERSION == 4ULL || AP_NATIVE_COPY_VERSION == 5ULL
u64 ap_adapter_abi_version(void) { return 0x415052555354000aULL; }
#else
#error "No authenticated adapter ABI for this copy grammar"
#endif
u64 ap_adapter_copy_version(void) { return AP_NATIVE_COPY_VERSION; }
u64 ap_adapter_task_command_size(void) { return sizeof(struct ap_task_command); }
#include "fd-enrollment.h"
_Static_assert(sizeof(struct ap_fd_enrollment)==112,"enrollment ABI");
_Static_assert(offsetof(struct ap_fd_enrollment,phases)==72,"enrollment phases ABI");
_Static_assert(offsetof(struct ap_fd_enrollment,slots)==88,"enrollment counts ABI");
_Static_assert(offsetof(struct ap_fd_enrollment,ptrace_return)==104,"enrollment native result ABI");

_Static_assert(sizeof(struct ap_fd_event)==112,"dispatch-bearing event ABI v4");
_Static_assert(offsetof(struct ap_fd_event,mode)==88,"event inode mode ABI");
_Static_assert(offsetof(struct ap_fd_event,status_flags)==92,"event status ABI");
_Static_assert(offsetof(struct ap_fd_event,device_major)==96,"event device major ABI");
_Static_assert(offsetof(struct ap_fd_event,device_minor)==100,"event device minor ABI");
_Static_assert(offsetof(struct ap_fd_event,source_ioctl_dispatch)==104,"event source dispatch ABI");

_Static_assert(sizeof(struct ap_original_selection)==104,"original selection ABI");
_Static_assert(offsetof(struct ap_original_selection,ready)==80,"original selection publication");
_Static_assert(sizeof(struct ap_original_result)==320,"original result ABI");
_Static_assert(sizeof(struct ap_stream_tx_summary)==64,"original Sendto summary ABI");
_Static_assert(sizeof(struct ap_stream_tx_capture)==624,"original Sendto capture ABI");
_Static_assert(offsetof(struct ap_stream_tx_capture,returned)==40,"Sendto actual return ABI");
_Static_assert(offsetof(struct ap_stream_tx_capture,summary)==48,"Sendto summary offset ABI");
_Static_assert(offsetof(struct ap_stream_tx_capture,bytes)==112,"Sendto accepted prefix ABI");
_Static_assert(sizeof(struct ap_original_epoll_installation)==40,"original epoll installation overlay ABI");
_Static_assert(offsetof(struct ap_original_epoll_installation,status_flags)==24,"original epoll OFD flag offset");
_Static_assert(offsetof(struct ap_original_epoll_installation,descriptor_flags)==28,"original epoll descriptor flag offset");
_Static_assert(offsetof(struct ap_original_epoll_installation,profiled)==32,"original epoll positive profile witness offset");
_Static_assert(offsetof(struct ap_original_result,address)==104,"kernel sockaddr bytes ABI");
_Static_assert(offsetof(struct ap_original_result,complete)==288,"original completion ABI");

_Static_assert(sizeof(struct ap_original_terminal)==480,"dead original cleanup ABI");
_Static_assert(sizeof(struct ap_native_birth)==192,"native birth ABI");
_Static_assert(offsetof(struct ap_native_birth,clear_child_tid)==184,"actual clear-child-TID ABI");
_Static_assert(sizeof(struct ap_native_birth_terminal)==352,"native birth terminal ABI");
_Static_assert(offsetof(struct ap_native_birth,ready)==144,"native birth publication ABI");
_Static_assert(sizeof(struct ap_fd_call)==424,"existing shared call map ABI");

_Static_assert(sizeof(struct ap_task_command)==72,"ABI10 task command");
_Static_assert(offsetof(struct ap_task_command,original_count)==56,"full Read request count");
_Static_assert(offsetof(struct ap_command_result,original_count)==128,"full Read result count");
_Static_assert(offsetof(struct ap_original_selection,original_count)==96,"full Read selection count");
_Static_assert(sizeof(struct ap_setter_rejection)==136,"full command diagnostic overlay");

_Static_assert(sizeof(struct ap_epoll_ctl_copy)==136,"copy-only epoll receipt ABI");
_Static_assert(sizeof(struct ap_epoll_ctl_terminal)==296,"copy-only terminal ABI");

#include "stream-copy.h"
_Static_assert(sizeof(struct ap_stream_copy_record)==584,"Read observation record ABI");
_Static_assert(offsetof(struct ap_stream_copy_record,bytes)==72,"Read observation payload ABI");
_Static_assert(sizeof(struct ap_stream_copy_unit)==72,"Version4 copy unit ABI");
_Static_assert(offsetof(struct ap_stream_copy_unit,disposition)==64,"Version4 disposition ABI");
_Static_assert(sizeof(struct ap_stream_copy_summary)==64,"Read observation summary ABI");
_Static_assert(sizeof(struct ap_stream_copy_manifest)==120,"Read observation manifest ABI");
_Static_assert(offsetof(struct ap_stream_copy_manifest,summary)==56,"Read observation manifest summary ABI");

_Static_assert(sizeof(struct ap_original_epoll_ctl)==112,"original ctl observation overlay");
_Static_assert(offsetof(struct ap_original_epoll_ctl,ctl_returned)==96,"immutable ctl selection prefix");
_Static_assert(sizeof(struct ap_original_epoll_ctl)<=sizeof(((struct ap_original_result *)0)->address),"same original Call union bound");

_Static_assert(offsetof(struct ap_task_command,expected_timeout_ticks)==64,"ABI10 timeout intent");
_Static_assert(sizeof(struct ap_stream_tx_blocking_summary)==72,"blocking TX summary ABI");
_Static_assert(sizeof(struct ap_stream_tx_blocking_capture)==632,"blocking TX capture ABI");
_Static_assert(offsetof(struct ap_stream_tx_blocking_capture,bytes)==120,"blocking TX bytes ABI");
_Static_assert(sizeof(struct ap_task_command_prefix)==64,"unchanged diagnostic prefix");
_Static_assert(offsetof(struct ap_setter_rejection,raw)==64,"unchanged diagnostic prefix offset");
_Static_assert(offsetof(struct ap_setter_rejection,raw_present)==128,"unchanged diagnostic presence offset");
_Static_assert(offsetof(struct ap_setter_rejection,raw_level)==132,"unchanged diagnostic actual level offset");
