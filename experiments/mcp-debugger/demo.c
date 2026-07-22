/*
 * Tiny debuggable guest for the MCP-debugger proof of concept.
 *
 * Compiled with `-g -O0` so a source-level breakpoint inside `add` resolves
 * cleanly. The POC agent does not hard-code a line number: it locates the
 * marker comment below at run time, so this file can be edited freely as long
 * as that marker stays on the `return sum;` line (where `sum` has already been
 * computed to 42). The marker token is spelled with a leading '@' so it only
 * matches the code line, never this description.
 */
#include <stdio.h>

int add(int a, int b) {
    int sum = a + b;
    return sum;  /* @BREAK-HERE */
}

int main() {
    int x = 41;
    int y = add(x, 1);
    printf("result=%d\n", y);
    return 0;
}
