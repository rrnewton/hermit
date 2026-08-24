#include <stdio.h>
#include <unistd.h>
int main(void){
  uid_t before = getuid();
  int rc = setuid(1234);
  uid_t after = getuid();
  printf("before=%d setuid_rc=%d after=%d\n", (int)before, rc, (int)after);
  printf("branch.setuid_claimed_success=%s\n", rc == 0 ? "yes" : "no");
  printf("branch.uid_actually_changed=%s\n", after == 1234 ? "yes" : "no");
  printf("branch.COHERENT=%s\n", (rc == 0) == (after == 1234) ? "yes" : "NO-SILENT-NOOP");
  return 0;
}
