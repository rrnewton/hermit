#!/bin/bash
# child pids + a stack address (ASLR witness), as a real build tree would see
echo "self=$$"
for i in 1 2 3; do ( echo "child$i=$BASHPID" ); done
awk '/\[stack\]/{split($1,a,"-"); print "stack=" a[1]}' /proc/self/maps
