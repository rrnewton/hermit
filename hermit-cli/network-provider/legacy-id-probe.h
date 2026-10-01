static int bpf_read(enum bpf_cmd command,union bpf_attr *attr) {
    /* Deliberately closed read-only diagnostic vocabulary. */
    if(command!=BPF_MAP_GET_FD_BY_ID && command!=BPF_PROG_GET_FD_BY_ID &&
       command!=BPF_LINK_GET_FD_BY_ID && command!=BPF_OBJ_GET_INFO_BY_FD &&
       command!=BPF_MAP_LOOKUP_ELEM)return refuse();
    return (int)syscall(SYS_bpf,command,attr,sizeof(*attr));
}
static int object_fd(struct ap_program_id id) {
    union bpf_attr a={0};
    if(id.kind==0){a.map_id=id.id;return bpf_read(BPF_MAP_GET_FD_BY_ID,&a);}
    if(id.kind==1){a.prog_id=id.id;return bpf_read(BPF_PROG_GET_FD_BY_ID,&a);}
    if(id.kind==2){a.link_id=id.id;return bpf_read(BPF_LINK_GET_FD_BY_ID,&a);}
    return refuse();
}
