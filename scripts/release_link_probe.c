/* Links libreplicant_client like a release consumer that never calls replicant_get_version.
   CI then checks the stripped binary still carries the version marker. */
#include <stdio.h>
#include <stdint.h>
#include "replicant.h"

const char* sqlite3_libversion(void); /* bundled SQLite: entonal-common Migration.cpp links these */

int main(void) {
    printf("abi %u sqlite %s\n", (unsigned)replicant_abi_version(), sqlite3_libversion());
    return 0;
}
