#!/usr/bin/env python3
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

import http.server
import ssl
import sys

httpd = http.server.HTTPServer(("0.0.0.0", 0), http.server.SimpleHTTPRequestHandler)
context = ssl.create_default_context(ssl.Purpose.CLIENT_AUTH)
context.load_cert_chain(certfile="/var/facebook/x509_identities/server.pem")
context.load_verify_locations(cafile="/var/facebook/rootcanal/ca.pem")
httpd.socket = context.wrap_socket(
    httpd.socket,
    server_side=True,
)

requests = -1
if len(sys.argv) > 1:
    requests = int(sys.argv[1])
    sys.stderr.write("[server] Answering only " + str(requests) + " requests.\n")


print(f"{httpd.server_name}:{httpd.server_port}", flush=True)
if requests > 0:
    for _i in range(0, requests):
        httpd.handle_request()
else:
    httpd.serve_forever()
