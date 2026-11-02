#!/bin/sh
exec with-proxy gh pr view 4796 --repo rrnewton/hermit --json body,headRefOid,state
