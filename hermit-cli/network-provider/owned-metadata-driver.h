/* Included only by the production driver after its command/collector types. */
#include "owned-metadata.h"
static int owned_full_link(struct ap_session *s,u32 at,struct bpf_link_info *out) {
    struct bpf_link_info bound;if(owned_link_info(s,at,&bound))return -1;
    memset(out,0,sizeof(*out));u32 size=sizeof(*out);
    if(bpf_obj_get_info_by_fd(bpf_link__fd(s->links[at]),out,&size))return -1;
    if(size<sizeof(*out) || out->type!=bound.type || out->id!=bound.id || out->prog_id!=bound.prog_id)
        return unavailable();
    return 0;
}
static int owned_metadata_locked(struct ap_session *s,struct ap_program_id id,
                                 struct ap_owned_metadata *out) {
    if(!s->object || s->links_count>AP_LINKS)return unavailable();
    struct ap_owned_metadata v={.identity=id,.source=AP_OWNED_DIRECT_FD,.version=1};
    unsigned matches=0;
    if(id.kind==0) {
        struct bpf_map *m=NULL;
        while((m=bpf_object__next_map(s->object,m))) {
            int fd=bpf_map__fd(m);if(fd<0)continue;
            struct bpf_map_info info={0};u32 size=sizeof(info);
            if(bpf_obj_get_info_by_fd(fd,&info,&size))return -1;
            if(size<sizeof(info) || !info.id || !info.type)return unavailable();
            if(info.id==id.id) {v.value.map=info;matches++;}
        }
    } else if(id.kind==1) {
        struct bpf_program *p=NULL;u32 at=0;
        while((p=bpf_object__next_program(s->object,p))) {
            int fd=bpf_program__fd(p);
            if(fd>=0) {
                struct bpf_prog_info info={0};u32 size=sizeof(info);
                if(bpf_obj_get_info_by_fd(fd,&info,&size))return -1;
                if(size<sizeof(info) || !info.id || !info.type)return unavailable();
                if(at<s->links_count && s->link_identity[at].program_id &&
                   (info.id!=s->link_identity[at].program_id || info.type!=s->link_identity[at].program_type))
                    return unavailable();
                if(info.id==id.id) {v.value.program=info;matches++;}
            }
            at++;
        }
        if(!matches) {
            /* Only a previously bound pair may replace a released redundant
             * program FD. Re-query its original held link on EVERY call. */
            for(u32 i=0;i<s->links_count;i++) {
                const struct ap_link_identity *bound=&s->link_identity[i];
                if(bound->program_id!=id.id)continue;
                struct bpf_link_info link;
                if(!bound->program_type)return unavailable();
                if(owned_full_link(s,i,&link))return -1;
                if(link.prog_id!=id.id)return unavailable();
                if(matches && v.value.linked_program.program_type!=bound->program_type)return unavailable();
                v.source=AP_OWNED_PROGRAM_LINK;
                v.value.linked_program.program_type=bound->program_type;
                v.value.linked_program.link=link;
                matches=1; /* Classic may own multiple links of this exact program. */
            }
        }
    } else if(id.kind==2) {
        for(u32 i=0;i<s->links_count;i++) {
            struct bpf_link_info link;if(owned_full_link(s,i,&link))return -1;
            if(link.id==id.id) {v.value.link=link;matches++;}
        }
    } else return invalid();
    if(matches!=1)return unavailable();
    *out=v;return 0;
}
int ap_owned_object_info(struct ap_session *s,struct ap_program_id id,struct ap_owned_metadata *out) {
    /* Partial ap_open sessions also retain real objects. No ready claim is
     * made here; serialize with submit/read/ACK but do not require ready. */
    if(!s || !out || !id.id || id.kind>2)return invalid();
    if(atomic_flag_test_and_set_explicit(&s->command_busy,memory_order_acquire)) {errno=EBUSY;return -1;}
    int rc=owned_metadata_locked(s,id,out),error=errno;
    leave_commands(s);errno=error;return rc;
}
int ap_original_read_fault_snapshot(struct ap_session *s,u64 command,u32 map_id,
                                    struct ap_stream_fault_state *out) {
    if(!command || !map_id || !out)return invalid();
    if(enter_commands(s))return -1;
    int rc=-1;
#ifdef AP_FTRACE_PROVIDER
    struct ap_pending_command *p=&s->pending[ap_command_slot(command)];
    if(p->state!=AP_SLOT_COLLECTED || !p->original_collected ||
       p->original_receipt.command!=command || p->original_receipt.operation!=AP_ORIGINAL_READ ||
       memcmp(&p->original_receipt.original.selection,&p->original_selection,sizeof(p->original_selection)) ||
       p->submitted.command!=command || p->submitted.provider!=s->incarnation ||
       p->submitted.operation!=AP_ORIGINAL_READ || !p->original_selected ||
       !ap_original_selection_matches(&p->submitted,&p->original_selection) ||
       !p->original_selection.file || !p->original_selection.user_address ||
       !p->submitted.expected_object) {invalid();goto done;}
    int fd=bpf_object__find_map_fd_by_name(s->object,"stream_copy_faults");
    struct bpf_map_info info={0};u32 size=sizeof(info);
    if(fd<0 || bpf_obj_get_info_by_fd(fd,&info,&size))goto done;
    if(size<sizeof(info) || info.id!=map_id || info.type!=BPF_MAP_TYPE_ARRAY ||
       info.key_size!=sizeof(u32) || info.value_size!=sizeof(*out) ||
       info.max_entries!=AP_COMMANDS || memcmp(info.name,"stream_copy_fau",15)) {
        unavailable();goto done;
    }
    u32 slot=ap_command_slot(command);
    struct ap_stream_fault_state first={0},second={0};
    if(bpf_map_lookup_elem(fd,&slot,&first) || bpf_map_lookup_elem(fd,&slot,&second))goto done;
    if(memcmp(&first,&second,sizeof(first)) || first.provider!=s->incarnation ||
       first.command!=command || first.call!=p->submitted.expected_object ||
       first.task!=p->original_selection.task || first.start!=p->original_selection.task_start ||
       first.file!=p->original_selection.file || first.file!=p->original_receipt.selected_file ||
       first.pointer!=p->original_receipt.selection.word || first.ubuf!=p->original_selection.user_address) {
        unavailable();goto done;
    }
    *out=first;rc=0;
#else
    (void)s;unavailable();
#endif
#ifdef AP_FTRACE_PROVIDER
done:
#endif
    {int error=errno;leave_commands(s);errno=error;}return rc;
}
