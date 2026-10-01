:: Run1...
First run errored during --verify, not continuing to a second. Stdout:
root pid=3 ppid=1 tid=3
grandchild pid=8 ppid=7 tid=8
child pid=7 ppid=3 tid=7
child grandchild=8 waited=-1 exit=0
root child=7 waited=-1 exit=0
exec-child pid=9 ppid=3 tid=9
exec-proc stat=9/3 status=9/3 tracer=1
root exec=9 waited=-1 exit=0
waitid-child pid=10 ppid=3 tid=10

Stderr:

Error: First run during --verify exited in error
