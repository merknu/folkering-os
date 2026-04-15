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
 *
 * Modes:
 *   run_bytes             Default — read bytes on stdin, execute, exit.
 *   run_bytes --addrs     Print the absolute addresses of the callable
 *                         helpers (helper_*) in this binary and exit 0.
 *                         Used by Phase 4A so the host can bake the
 *                         (ASLR-affected) runtime address into a JIT
 *                         Call() via MOVZ/MOVK + BLR.
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <unistd.h>

#define CODE_CAP (64 * 1024)

typedef int (*fn_t)(void);

/* ── Callable helpers (Phase 4A) ───────────────────────────────────
 *
 * These live in this binary so ASLR places them at a fresh address
 * each run. The host queries --addrs once per invocation, bakes the
 * address into the JIT via MOVZ/MOVK chains into X16, and the
 * emitted code dispatches via BLR X16.
 *
 * noinline + used keeps them in the output under -O2 and prevents
 * the compiler from collapsing any future callers into constant
 * loads.
 */

__attribute__((noinline, used))
int helper_return_42(void) {
    return 42;
}

__attribute__((noinline, used))
int helper_add_five(int x) {
    return x + 5;
}

__attribute__((noinline, used))
int helper_multiply_two(int x) {
    return x * 2;
}

int main(int argc, char **argv) {
    if (argc > 1 && strcmp(argv[1], "--addrs") == 0) {
        printf("helper_return_42=%p\n",     (void *)helper_return_42);
        printf("helper_add_five=%p\n",      (void *)helper_add_five);
        printf("helper_multiply_two=%p\n",  (void *)helper_multiply_two);
        return 0;
    }

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
