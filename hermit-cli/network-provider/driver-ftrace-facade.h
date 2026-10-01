/* SPDX-License-Identifier: MIT */
/* Host model only. Include every libc declaration before redirecting driver
 * calls. No real libbpf is linked. The mandatory undefined-symbol fence in the
 * maintained-runner proposal must run BEFORE this control is executable.
 * Unexpected calls must either hit df_unexpected or fail that closed fence. */
#ifndef HERMIT_DRIVER_FTRACE_FACADE_H
#define HERMIT_DRIVER_FTRACE_FACADE_H
#define _GNU_SOURCE
#include <assert.h>
#include <errno.h>
#include <fcntl.h>
#include <linux/bpf.h>
#include <linux/btf.h>
#include <poll.h>
#include <stdarg.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <unistd.h>
#if !defined(AP_FTRACE_PROVIDER) || defined(__BPF__)
#error "host control requires AP_FTRACE_PROVIDER, never __BPF__"
#endif
#ifdef NDEBUG
#error "driver controls require assertions"
#endif
static FILE *df_fopen(const char *,const char *);
static size_t df_fread(void *,size_t,size_t,FILE *);
static int df_fseek(FILE *,long,int);
static int df_ferror(FILE *);
static int df_fgetc(FILE *);
static char *df_fgets(char *,int,FILE *);
static int df_fclose(FILE *);
static long df_sysconf(int);
static pid_t df_getpid(void);
static long df_syscall(long,...);
static int df_poll(struct pollfd *,nfds_t,int);
static int df_close(int);
static int df_getsockopt(int,int,int,void *,socklen_t *);
static void *df_calloc(size_t,size_t);
static void *df_realloc(void *,size_t);
static void df_free(void *);
#define fopen df_fopen
#define fread df_fread
#define fseek df_fseek
#define ferror df_ferror
#define fgetc df_fgetc
#define fgets df_fgets
#define fclose df_fclose
#define sysconf df_sysconf
#define getpid df_getpid
#define syscall df_syscall
#define poll df_poll
#define close df_close
#define getsockopt df_getsockopt
#define calloc df_calloc
#define realloc df_realloc
#define free df_free
#endif

