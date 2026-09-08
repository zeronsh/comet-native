// Native process counters for resource-profile.mjs. CPU time uses Mach ticks;
// footprint includes charged compressed/GPU memory that RSS alone misses.
#include <libproc.h>
#include <mach/mach_time.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/resource.h>

int main(int argc, char **argv) {
    mach_timebase_info_data_t timebase;
    mach_timebase_info(&timebase);
    printf("[");
    int emitted = 0;
    for (int i = 1; i < argc; i++) {
        int pid = atoi(argv[i]);
        struct rusage_info_v4 usage = {0};
        if (pid <= 0 || proc_pid_rusage(pid, RUSAGE_INFO_V4, (rusage_info_t *)&usage)) continue;
        double seconds = (double)(usage.ri_user_time + usage.ri_system_time)
            * timebase.numer / timebase.denom / 1e9;
        printf("%s{\"pid\":%d,\"cpuSeconds\":%.9f,\"rssMiB\":%.6f,"
               "\"footprintMiB\":%.6f,\"lifetimePeakFootprintMiB\":%.6f,"
               "\"idleWakeups\":%llu}",
               emitted++ ? "," : "", pid, seconds,
               usage.ri_resident_size / 1048576.0, usage.ri_phys_footprint / 1048576.0,
               usage.ri_lifetime_max_phys_footprint / 1048576.0, usage.ri_pkg_idle_wkups);
    }
    puts("]");
}
