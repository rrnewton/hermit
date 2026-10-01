#!/bin/bash
mkdir -p /tmp/s176-nixlike/build
echo "built-by-pid-$$ RANDOM=$RANDOM" > /tmp/s176-nixlike/build/out.txt
echo "guest sees: $(cat /tmp/s176-nixlike/build/out.txt)"
