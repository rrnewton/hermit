#!/bin/bash
# Copyright (c) Meta Platforms, Inc. and affiliates.
# All rights reserved.
#
# This source code is licensed under the BSD-style license found in the
# LICENSE file in the root directory of this source tree.

# Performs parallel CPU work to exercise scheduling under instrumentation.
# Four workers keep multiple child processes runnable while limiting repeated
# CI cost. Each worker still crosses scheduling boundaries under Hermit.

if [[ "$HERMIT_MODE" = "chaosreplay" ]] ||
   [[ "$HERMIT_MODE" = "tracereplay" ]];
then
    # TODO(T100400409): Reenable after performance improvements
    echo "Skipping par_work in unsupported mode. Re-enable when it is fixed." >&2
    exit 0
fi

PARALLELISM=4

function work() {
    name=$1
    echo "Start $name"
    python3 <<EOF
a=1
b=1
for x in range(0, 5000):
    tmp=a
    a+=b
    b=tmp
print("Finished $name", hash(a))
EOF
}

for((t=0; t<PARALLELISM; t++)); do
   work "task_$t" &
done
wait
