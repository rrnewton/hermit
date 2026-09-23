/* SPDX-License-Identifier: GPL-2.0 */
/* Included by the maintained provider after its identity/command helpers.
 * This source is not selected until its complete composed artifact is built,
 * checked against running BTF, and the actual installation consumer is joined. */
#include "fd-effects.h"

struct ap_fd_file { u64 identity; };
struct ap_fd_table { u64 identity; };
struct ap_fd_call {
    u64 command, raw_table, new_file, install_begin;
};
struct ap_fd_replace {
    u64 table, file, previous_file, begin;
    u32 fd, old_seen;
};
struct ap_fd_install_call { u64 table, file, raw_file, begin, accept_command; u32 fd; };
ARRAY(fd_accepts,struct ap_fd_accept,AP_COMMANDS);
ARRAY(fd_status,struct ap_fd_status,1);
HASH(fd_journal,u64,struct ap_fd_event,AP_FD_JOURNAL);
HASH(fd_files,u64,struct ap_fd_file,AP_FD_FILES);
HASH(fd_tables,u64,struct ap_fd_table,AP_FD_TABLES);
HASH(fd_calls,struct ap_invocation_key,struct ap_fd_call,AP_CALLS);
HASH(fd_replacements,struct ap_invocation_key,struct ap_fd_replace,AP_CALLS);
HASH(fd_install_calls,struct ap_invocation_key,struct ap_fd_install_call,AP_CALLS);

INLINE struct ap_fd_status *fd_stats(void) {
    u32 zero=0;return lookup(&fd_status,&zero);
}
INLINE void fd_problem(u64 problem) {
    struct ap_fd_status *s=fd_stats();
    if(s)__sync_fetch_and_or(&s->problem,problem);
}
INLINE struct ap_invocation_key fd_actor(void) { return invocation(0,0); }
INLINE u64 fd_table(struct files_struct *files,int create) {
    u64 key=(u64)files;struct ap_fd_table *known=lookup(&fd_tables,&key);
    if(known)return known->identity;
    if(!create)return 0;
    struct ap_fd_status *s=fd_stats();if(!s)return 0;
    u64 identity=__sync_fetch_and_add(&s->next_table,1)+1;
    if(!identity) { fd_problem(AP_FD_CAPACITY);return 0; }
    struct ap_fd_table fresh={.identity=identity};
    if(update(&fd_tables,&key,&fresh,BPF_NOEXIST)) {
        known=lookup(&fd_tables,&key);
        if(!known)fd_problem(AP_FD_CAPACITY);
        return known?known->identity:0;
    }
    return identity;
}
/* Every caller has an actual borrowed kernel file reference here. The mapping
 * is retired at __fput, before allocation reuse; it is not an owning reference.
 * fd_install fexit MUST NOT call this: its argument reference was consumed. */
INLINE u64 fd_file(struct file *file) {
    if(!file)return 0;
    u64 key=(u64)file;struct ap_fd_file *known=lookup(&fd_files,&key);
    if(known)return known->identity;
    struct ap_fd_status *s=fd_stats();if(!s)return 0;
    u64 identity=__sync_fetch_and_add(&s->next_file,1)+1;
    if(!identity) { fd_problem(AP_FD_CAPACITY);return 0; }
    struct ap_fd_file fresh={.identity=identity};
    if(update(&fd_files,&key,&fresh,BPF_NOEXIST)) {
        known=lookup(&fd_files,&key);
        if(!known)fd_problem(AP_FD_CAPACITY);
        return known?known->identity:0;
    }
    return identity;
}
INLINE u64 fd_event(u64 kind,u64 table,s32 fd,u64 file,u64 previous,
                    u64 dependency,u64 command,s32 returned) {
    struct ap_fd_status *s=fd_stats();if(!s)return 0;
    u64 sequence=__sync_fetch_and_add(&s->next_event,1)+1;
    if(!sequence) { fd_problem(AP_FD_CAPACITY);return 0; }
    /* Never reuse a journal key. Exact deletion of an immutable completed hash
     * entry permits a new, different key without a whole-value clear racing a
     * producer's payload stores (an array-slot memset would not be safe here). */
    struct ap_fd_event pending={.sequence=sequence,.complete=2};
    if(update(&fd_journal,&sequence,&pending,BPF_NOEXIST)) {
        fd_problem(AP_FD_CAPACITY);return 0;
    }
    struct ap_fd_event *e=lookup(&fd_journal,&sequence);
    if(!e) { fd_problem(AP_FD_MISSING);return 0; }
    e->sequence=sequence;e->kind=kind;e->task=pid_tgid();
    e->task_start=CORE(current_task()->start_boottime);
    e->table=table;e->file=file;e->previous_file=previous;
    e->dependency=dependency;e->accept_command=command;e->fd=fd;e->returned=returned;
    __sync_val_compare_and_swap(&e->complete,2,1);
    return sequence;
}
INLINE struct ap_fd_accept *fd_accept(struct ap_fd_call *call) {
    if(!call)return 0;
    u32 slot=ap_command_slot(call->command);
    struct ap_fd_accept *a=lookup(&fd_accepts,&slot);
    return a && a->command==call->command?a:0;
}

SEC("fentry/__sys_accept4") int fd_accept_enter(u64 *ctx) {
    struct ap_task_command *c=command();
    if(!c || c->operation!=AP_ACCEPT_EFFECT)return 0;
    struct ap_command_result *r=claim_result(c);if(!r)return 0;
    u32 slot=ap_command_slot(c->command);
    struct ap_fd_accept *a=lookup(&fd_accepts,&slot);
    if(!a || a->command!=c->command || a->phases) { fd_problem(AP_FD_DUPLICATE);return 0; }
    a->task=pid_tgid();a->task_start=CORE(current_task()->start_boottime);
    a->table=fd_table(CORE(current_task()->files),1);
    a->requested_fd=(s32)ctx[0];a->flags=(s32)ctx[3];
    a->phases=AP_FD_ENTERED;
    if(a->requested_fd!=c->expected_level || a->flags!=c->expected_option || !a->table)
        a->problem|=AP_FD_IDENTITY;
    struct ap_invocation_key key=fd_actor();
    struct ap_fd_call call={.command=c->command,.raw_table=(u64)CORE(current_task()->files)};
    if(update(&fd_calls,&key,&call,BPF_NOEXIST))fd_problem(AP_FD_DUPLICATE);
    r->identity.provider=incarnation();
    return 0;
}

/* inet_accept receives the actual listener socket selected by fdget, after
 * security_socket_accept. It is not a lookup of the numeric FD after return. */
SEC("fentry/inet_accept") int fd_listener_selected(u64 *ctx) {
    struct ap_invocation_key key=fd_actor();struct ap_fd_call *call=lookup(&fd_calls,&key);
    struct ap_fd_accept *a=fd_accept(call);if(!a)return 0;
    struct socket *socket=(struct socket *)ctx[0];struct sock *sk=CORE(socket->sk);
    struct ap_object *o=object(sk);struct ap_task_command *c=command();
    if(!o || !c || o->creation || o->object!=c->expected_object) { a->problem|=AP_FD_IDENTITY;return 0; }
    a->listener=identity(sk,o->object);a->phases|=AP_FD_LISTENER;
    return 0;
}
SEC("fexit/inet_csk_accept") int fd_child_dequeued(u64 *ctx) {
    struct ap_invocation_key key=fd_actor();struct ap_fd_call *call=lookup(&fd_calls,&key);
    struct ap_fd_accept *a=fd_accept(call);if(!a)return 0;
    struct sock *child=(struct sock *)ctx[2];
    if(!child || (u64)child>=(u64)-4095)return 0;
    struct ap_object *o=object(child);
    if(!o || !o->creation || a->phases&AP_FD_DEQUEUED) { a->problem|=AP_FD_IDENTITY;return 0; }
    a->child=identity(child,o->object);a->creation=o->creation;a->cookie=o->cookie;
    a->phases|=AP_FD_DEQUEUED;
    return 0;
}
SEC("fexit/do_accept") int fd_new_file_returned(u64 *ctx) {
    struct ap_invocation_key key=fd_actor();struct ap_fd_call *call=lookup(&fd_calls,&key);
    struct ap_fd_accept *a=fd_accept(call);if(!a)return 0;
    struct file *file=(struct file *)ctx[5];
    if((u64)file>=(u64)-4095) { a->do_accept_errno=-(s64)(u64)file;return 0; }
    if(!file || !(a->phases&AP_FD_DEQUEUED) || call->new_file) { a->problem|=AP_FD_OUTCOME;return 0; }
    call->new_file=(u64)file;a->file=fd_file(file);a->phases|=AP_FD_FILE_RETURNED;
    return 0;
}
SEC("fentry/fd_install") int fd_install_enter(u64 *ctx) {
    struct files_struct *files=CORE(current_task()->files);u64 table=fd_table(files,0);
    if(!table)return 0;
    struct file *file=(struct file *)ctx[1];u64 file_id=fd_file(file);
    struct ap_invocation_key key=fd_actor();struct ap_fd_call *call=lookup(&fd_calls,&key);
    struct ap_fd_accept *a=fd_accept(call);
    struct ap_fd_install_call install={.table=table,.file=file_id,.raw_file=(u64)file,.fd=ctx[0]};
    if(a) {
        if(call->new_file!=(u64)file || a->file!=file_id || call->raw_table!=(u64)files || a->phases&AP_FD_INSTALL_ENTERED) {
            a->problem|=AP_FD_IDENTITY;
        } else install.accept_command=a->command;
    }
    install.begin=fd_event(AP_FD_INSTALL_BEGIN,table,ctx[0],file_id,0,0,install.accept_command,0);
    if(update(&fd_install_calls,&key,&install,BPF_NOEXIST)) { fd_problem(AP_FD_DUPLICATE);return 0; }
    if(a && install.accept_command) {
        a->returned_fd=(s32)ctx[0];a->phases|=AP_FD_INSTALL_ENTERED;
        a->install_begin=install.begin;call->install_begin=install.begin;
    }
    return 0;
}
SEC("fexit/fd_install") int fd_install_returned(u64 *ctx) {
    struct ap_invocation_key key=fd_actor();struct ap_fd_install_call *install=lookup(&fd_install_calls,&key);
    if(!install)return 0;
    /* Pointer comparison only. The consumed file may already have been freed. */
    if(install->raw_file!=ctx[1] || install->fd!=(u32)ctx[0] || !install->begin) { fd_problem(AP_FD_IDENTITY);return 0; }
    u64 end=fd_event(AP_FD_INSTALL_END,install->table,install->fd,install->file,0,install->begin,install->accept_command,0);
    if(install->accept_command) {
        struct ap_fd_call *call=lookup(&fd_calls,&key);struct ap_fd_accept *a=fd_accept(call);
        if(!a || a->command!=install->accept_command)a=0;
        if(!a)fd_problem(AP_FD_MISSING);
        else { a->install_end=end;a->phases|=AP_FD_INSTALL_RETURNED; }
    }
    remove_key(&fd_install_calls,&key);
    return 0;
}
SEC("fexit/__sys_accept4") int fd_accept_returned(u64 *ctx) {
    struct ap_invocation_key key=fd_actor();struct ap_fd_call *call=lookup(&fd_calls,&key);
    struct ap_fd_accept *a=fd_accept(call);if(!a)return 0;
    struct ap_command_result *r=result(a->command);if(!r)return 0;
    s32 returned=(s32)ctx[4];r->returned=returned;
    if(returned>=0) {
        if(!(a->phases&AP_FD_INSTALL_RETURNED) || returned!=a->returned_fd || !a->file)
            a->problem|=AP_FD_OUTCOME;
    } else if(a->phases&AP_FD_INSTALL_ENTERED) a->problem|=AP_FD_OUTCOME;
    if(a->phases&AP_FD_DEQUEUED) { r->identity=a->child;r->creation=a->creation;r->cookie=a->cookie; }
    a->phases|=AP_FD_SYSCALL_RETURNED;
    remove_key(&fd_calls,&key);
    /* This is the final access to both accept receipt and command result. */
    publish_result(r);return 0;
}

SEC("fexit/file_close_fd_locked") int fd_removed(u64 *ctx) {
    u64 table=fd_table((struct files_struct *)ctx[0],0);if(!table)return 0;
    struct file *file=(struct file *)ctx[2];if(!file)return 0;
    /* The removed reference is still held by the close caller, under file_lock. */
    fd_event(AP_FD_REMOVE,table,ctx[1],fd_file(file),0,0,0,0);return 0;
}
SEC("fentry/do_dup2") int fd_replace_enter(u64 *ctx) {
    u64 table=fd_table((struct files_struct *)ctx[0],0);if(!table)return 0;
    struct ap_invocation_key key=fd_actor();
    struct ap_fd_replace replacement={.table=table,.file=fd_file((struct file *)ctx[1]),.fd=ctx[2]};
    replacement.begin=fd_event(AP_FD_REPLACE_BEGIN,table,ctx[2],replacement.file,0,0,0,0);
    if(update(&fd_replacements,&key,&replacement,BPF_NOEXIST))fd_problem(AP_FD_DUPLICATE);
    return 0;
}
SEC("fentry/filp_close") int fd_replaced_old_file(u64 *ctx) {
    struct ap_invocation_key key=fd_actor();struct ap_fd_replace *r=lookup(&fd_replacements,&key);
    if(!r)return 0;
    /* The first direct filp_close in do_dup2 receives its actual `tofree`.
     * Nested flush closes are retained as unresolved rather than replacing it. */
    if(r->old_seen) { fd_problem(AP_FD_UNKNOWN_TABLE);return 0; }
    r->old_seen=1;r->previous_file=fd_file((struct file *)ctx[0]);
    fd_event(AP_FD_REPLACE_OLD_FILE,r->table,r->fd,r->file,r->previous_file,r->begin,0,0);
    return 0;
}
SEC("fexit/do_dup2") int fd_replace_returned(u64 *ctx) {
    struct ap_invocation_key key=fd_actor();struct ap_fd_replace *r=lookup(&fd_replacements,&key);
    if(!r)return 0;
    s32 returned=(s32)ctx[4];
    if((returned<0 && r->old_seen) || (returned>=0 && (u32)returned!=r->fd))fd_problem(AP_FD_OUTCOME);
    fd_event(AP_FD_REPLACE_END,r->table,r->fd,r->file,r->previous_file,r->begin,0,returned);
    remove_key(&fd_replacements,&key);return 0;
}
SEC("fentry/__fput") int fd_file_retired(u64 *ctx) {
    u64 key=ctx[0];struct ap_fd_file *file=lookup(&fd_files,&key);if(!file)return 0;
    fd_event(AP_FD_FILE_RETIRED,0,-1,file->identity,0,0,0,0);
    remove_key(&fd_files,&key);return 0;
}
/* These paths bypass individual install/remove hooks. Keep the physical cut
 * unresolved until the shared copy/exec/final-table producer is implemented;
 * never claim a complete census from this narrow accepted-install observer. */
SEC("fentry/do_close_on_exec") int fd_exec_unresolved(u64 *ctx) {
    u64 table=fd_table((struct files_struct *)ctx[0],0);if(!table)return 0;
    fd_event(AP_FD_UNRESOLVED_TABLE_MUTATION,table,-1,0,0,0,0,0);
    fd_problem(AP_FD_UNKNOWN_TABLE);return 0;
}
SEC("fentry/dup_fd") int fd_copy_unresolved(u64 *ctx) {
    u64 table=fd_table((struct files_struct *)ctx[0],0);if(!table)return 0;
    fd_event(AP_FD_UNRESOLVED_TABLE_MUTATION,table,-1,0,0,0,0,0);
    fd_problem(AP_FD_UNKNOWN_TABLE);return 0;
}
SEC("fentry/put_files_struct") int fd_table_put_unresolved(u64 *ctx) {
    u64 table=fd_table((struct files_struct *)ctx[0],0);if(!table)return 0;
    fd_event(AP_FD_UNRESOLVED_TABLE_MUTATION,table,-1,0,0,0,0,0);
    fd_problem(AP_FD_UNKNOWN_TABLE);return 0;
}
/* close_files is inlined on the qualified kernel. put_files_struct alone
 * cannot prove the last reference: another put can race its entry snapshot.
 * Retire a watched pointer only at the actual allocator-free boundary; the
 * pointer is only a private lookup key for its already allocated generation. */
SEC("fentry/kmem_cache_free") int fd_table_retired(u64 *ctx) {
    u64 key=ctx[1];struct ap_fd_table *table=lookup(&fd_tables,&key);
    if(!table)return 0;
    fd_event(AP_FD_TABLE_RETIRED,table->identity,-1,0,0,0,0,0);
    remove_key(&fd_tables,&key);return 0;
}
