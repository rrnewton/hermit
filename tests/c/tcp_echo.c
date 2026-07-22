/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

/*
 * A minimal self-contained TCP echo server + client.
 *
 * The parent process is the server: it creates a listening socket bound to an
 * ephemeral loopback port, accepts one connection, reads a message, and echoes
 * it back. The forked child is the client: it connects, sends a message, and
 * verifies the echo.
 *
 * This exercises the full socket lifecycle -- socket, setsockopt, bind, listen,
 * getsockname, accept, connect, send (sendto), recv (recvfrom), close -- which
 * is the record/replay coverage this test guards. It prints EXIT-SUCCESS on
 * success so it can double as a run-mode success-marker workload.
 */

#include <arpa/inet.h>
#include <netinet/in.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/wait.h>
#include <unistd.h>

static const char MSG[] = "hello, hermit";

int main(void) {
    int listen_fd = socket(AF_INET, SOCK_STREAM, 0);
    if (listen_fd < 0) {
        perror("socket");
        return 1;
    }

    int one = 1;
    setsockopt(listen_fd, SOL_SOCKET, SO_REUSEADDR, &one, sizeof(one));

    struct sockaddr_in addr;
    memset(&addr, 0, sizeof(addr));
    addr.sin_family = AF_INET;
    addr.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    addr.sin_port = 0; /* let the kernel choose an ephemeral port */

    if (bind(listen_fd, (struct sockaddr*)&addr, sizeof(addr)) != 0) {
        perror("bind");
        return 1;
    }
    if (listen(listen_fd, 1) != 0) {
        perror("listen");
        return 1;
    }

    /* Discover the bound port so the client knows where to connect. */
    socklen_t addr_len = sizeof(addr);
    if (getsockname(listen_fd, (struct sockaddr*)&addr, &addr_len) != 0) {
        perror("getsockname");
        return 1;
    }

    pid_t pid = fork();
    if (pid < 0) {
        perror("fork");
        return 1;
    }

    if (pid == 0) {
        /* Child: the client. */
        int fd = socket(AF_INET, SOCK_STREAM, 0);
        if (fd < 0) {
            perror("client socket");
            _exit(1);
        }
        if (connect(fd, (struct sockaddr*)&addr, sizeof(addr)) != 0) {
            perror("connect");
            _exit(1);
        }
        if (send(fd, MSG, sizeof(MSG) - 1, 0) != (ssize_t)(sizeof(MSG) - 1)) {
            perror("send");
            _exit(1);
        }
        char buf[64] = {0};
        ssize_t n = recv(fd, buf, sizeof(buf), 0);
        if (n != (ssize_t)(sizeof(MSG) - 1) || memcmp(buf, MSG, (size_t)n) != 0) {
            fprintf(stderr, "client: echo mismatch (n=%zd)\n", n);
            _exit(1);
        }
        close(fd);
        _exit(0);
    }

    /* Parent: the server. */
    struct sockaddr_in peer;
    socklen_t peer_len = sizeof(peer);
    int conn = accept(listen_fd, (struct sockaddr*)&peer, &peer_len);
    if (conn < 0) {
        perror("accept");
        return 1;
    }
    char buf[64] = {0};
    ssize_t n = recv(conn, buf, sizeof(buf), 0);
    if (n <= 0) {
        perror("server recv");
        return 1;
    }
    if (send(conn, buf, (size_t)n, 0) != n) {
        perror("server send");
        return 1;
    }
    close(conn);
    close(listen_fd);

    int status = 0;
    if (waitpid(pid, &status, 0) != pid) {
        perror("waitpid");
        return 1;
    }
    if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) {
        fprintf(stderr, "client failed: status=%d\n", status);
        return 1;
    }

    printf("EXIT-SUCCESS\n");
    return 0;
}
