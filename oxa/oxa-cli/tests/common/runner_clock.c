// Fixture only: preparation cannot consume the controlled deadline interval.
// Resume real runner time at the child's first write, or measure elapsed time
// from acceptance's marker. Every child's clock stays real.
#define _GNU_SOURCE
#include <stdlib.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

static pid_t runner;
static struct timespec started;
static const char *first_write;
static int elapsed_from_marker;

__attribute__((constructor)) static void freeze_runner_wall_clock(void) {
    runner = getpid();
    first_write = getenv("OXA_TEST_RUNNER_STARTED");
    elapsed_from_marker = getenv("OXA_TEST_RUNNER_ELAPSED") != NULL;
    if (!first_write || syscall(SYS_clock_gettime, CLOCK_REALTIME, &started) != 0)
        _exit(91);
    // Interposition stays in this runner; neither Git nor the real worker loads it.
    unsetenv("LD_PRELOAD");
}

int clock_gettime(clockid_t clock, struct timespec *value) {
    if (clock != CLOCK_REALTIME || getpid() != runner)
        return (int)syscall(SYS_clock_gettime, clock, value);
    struct stat marker;
    if (stat(first_write, &marker) != 0) {
        *value = started;
        return 0;
    }
    int result = (int)syscall(SYS_clock_gettime, clock, value);
    if (result != 0 || !elapsed_from_marker)
        return result;
    // Acceptance gets the original interval: count only time since its first write.
    // Read the file's timestamp even if the runner next checks after acceptance ends.
    value->tv_sec += started.tv_sec - marker.st_mtim.tv_sec;
    value->tv_nsec += started.tv_nsec - marker.st_mtim.tv_nsec;
    if (value->tv_nsec < 0) {
        value->tv_sec--;
        value->tv_nsec += 1000000000L;
    } else if (value->tv_nsec >= 1000000000L) {
        value->tv_sec++;
        value->tv_nsec -= 1000000000L;
    }
    return 0;
}
