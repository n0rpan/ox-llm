// Fixture only: preparation cannot consume the child-deadline control's budget.
// Monotonic time and every exec'd child retain their real clocks.
#define _GNU_SOURCE
#include <stdlib.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

static pid_t runner;
static struct timespec started;

__attribute__((constructor)) static void freeze_runner_wall_clock(void) {
    runner = getpid();
    if (syscall(SYS_clock_gettime, CLOCK_REALTIME, &started) != 0)
        _exit(91);
    // Interposition stays in this runner; neither Git nor the real worker loads it.
    unsetenv("LD_PRELOAD");
}

int clock_gettime(clockid_t clock, struct timespec *value) {
    if (clock == CLOCK_REALTIME && getpid() == runner) {
        *value = started;
        return 0;
    }
    return (int)syscall(SYS_clock_gettime, clock, value);
}
