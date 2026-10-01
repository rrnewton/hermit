#!/bin/bash
read -r u < /proc/sys/kernel/random/uuid; echo "UUID=$u"
