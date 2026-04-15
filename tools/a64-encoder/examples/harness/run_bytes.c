/* a64-harness/run_bytes.c
 *
 * Read AArch64 machine code from stdin, execute it, exit with its i32
 * return value. Minimal JIT-executor for verifying a64-encoder output
 * on real hardware.
 *
 * Contract:
 *   - Input bytes are a callable AArch64 function following AAPCS64
 *     (result in X0, RET at end). Up to 64 KiB.
 *   - We allocate one page with mmap(PROT_READ|WRITE|EXEC), copy the
 *     bytes in, flush caches, and call through a function pointer.
 *   - Exit code is the low 8 bits of X0 (Linux exit() truncates).
 *     Tests encode small positive values so this is unambiguous.
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>

#define CODE_CAP (64 * 1024)

typedef int (*fn_t)(void);

int main(void) {
    unsigned char buf[CODE_CAP];
    size_t total = 0;
    ssize_t n;
    while ((n = read(0, buf + total, CODE_CAP - total)) > 0) {
        total += (size_t)n;
        if (total == CODE_CAP) break;
    }
    if (total == 0) {
        fprintf(stderr, "run_bytes: no input\n");
        return 127;
    }

    /* Round up to page for mmap. */
    long page = sysconf(_SC_PAGESIZE);
    size_t alloc = ((total + page - 1) / page) * page;

    void *mem = mmap(NULL, alloc,
                     PROT_READ | PROT_WRITE | PROT_EXEC,
                     MAP_ANONYMOUS | MAP_PRIVATE, -1, 0);
    if (mem == MAP_FAILED) {
        perror("run_bytes: mmap");
        return 126;
    }
    memcpy(mem, buf, total);

    /* AArch64 requires an explicit instruction-cache flush after
       writing code that we're about to execute. __clear_cache is
       the portable GCC builtin for this. */
    __builtin___clear_cache((char *)mem, (char *)mem + total);

    fn_t fn = (fn_t)mem;
    int rv = fn();
    return rv & 0xFF;
}
