/* The kernel and the loader both enter observer.S through PT_INTERP.
 * Reaching this program's entry would be an observer failure.
 */
	.text
	.globl _start
	.type _start,@function
_start:
	mov $231, %eax
	mov $99, %edi
	syscall
	ud2
	.size _start, .-_start
	.section .note.GNU-stack,"",@progbits
