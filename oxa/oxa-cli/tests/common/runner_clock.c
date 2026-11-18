// Fixture only: preparation cannot consume the child-deadline control's budget.
// Resume the runner's wall clock after the real worker's first write so normal
// deadline classification remains active. Every child's clock stays real.
#define _GNU_SOURCE
#include <stdlib.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

static pid_t runner;
static struct timespec started;
static const char *first_write;

__attribute__((constructor)) static void freeze_runner_wall_clock(void) {
    runner = getpid();
    first_write = getenv("OXA_TEST_RUNNER_STARTED");
    if (!first_write || syscall(SYS_clock_gettime, CLOCK_REALTIME, &started) != 0)
        _exit(91);
    // Interposition stays in this runner; neither Git nor the real worker loads it.
    unsetenv("LD_PRELOAD");
}

int clock_gettime(clockid_t clock, struct timespec *value) {
    if (clock == CLOCK_REALTIME && getpid() == runner && access(first_write, F_OK) != 0) {
        *value = started;
        return 0;
    }
    return (int)syscall(SYS_clock_gettime, clock, value);
}
