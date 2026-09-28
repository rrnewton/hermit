.text
.globl fixture_kernel_entry
.type fixture_kernel_entry,@function
fixture_kernel_entry:
    ud2
.size fixture_kernel_entry, .-fixture_kernel_entry
.globl pe_private_crt_entry
.hidden pe_private_crt_entry
.type pe_private_crt_entry,@function
pe_private_crt_entry:
    ud2
.size pe_private_crt_entry, .-pe_private_crt_entry
.section .tdata,"awT",@progbits
    .quad 0x8765432101234567
.data
    .quad 0x123456789abcdef0
.bss
    .balign 32
    .skip 8192
.section .note.GNU-stack,"",@progbits
