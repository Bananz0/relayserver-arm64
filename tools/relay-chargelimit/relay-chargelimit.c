// relay-chargelimit: keeps the battery of an always-plugged relay phone between two levels.
//
// Above `max` the charger input is cut (ExternalConnected=NO), so the phone runs from the
// battery until it falls to `max`. Between `min` and `max` the input stays on but charging
// is inhibited (IsCharging=NO): the charger powers the phone and the battery level holds.
// At or below `min` normal charging resumes until `max` is reached again. Unplugged, at a
// very low level, when disabled, or on exit, everything is put back to normal.
//
// Settings: /var/mobile/chargelimit.conf (key=value: enabled, max, min), re-read every loop.
// State for relay-sensors / Home Assistant: /var/mobile/chargelimit.state (one JSON object).
//
// Usage: relay-chargelimit          run as a daemon (launchd job dev.copper.chargelimit)
//        relay-chargelimit reset    restore normal charging and exit (used by prerm)
//
// Same mechanism as ChargeLimiter on iOS <= 12; verified on iOS 10.3.3 (A6) and 12.5.8 (A8).

#include <CoreFoundation/CoreFoundation.h>
#include <mach/mach.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

typedef mach_port_t io_object_t;
extern const mach_port_t kIOMasterPortDefault;
extern CFMutableDictionaryRef IOServiceMatching(const char *name);
extern io_object_t IOServiceGetMatchingService(mach_port_t master, CFDictionaryRef matching);
extern kern_return_t IORegistryEntryCreateCFProperties(io_object_t e, CFMutableDictionaryRef *p, CFAllocatorRef a,
                                                       uint32_t o);
extern kern_return_t IORegistryEntrySetCFProperties(io_object_t e, CFTypeRef props);

#define CONF_PATH "/var/mobile/chargelimit.conf"
#define STATE_PATH "/var/mobile/chargelimit.state"
#define LOOP_SECONDS 30
#define REASSERT_SECONDS 300
#define FAILSAFE_LEVEL 15

static io_object_t g_ps;
static volatile sig_atomic_t g_stop;

static int dict_long(CFDictionaryRef d, CFStringRef k, long long *out) {
    CFTypeRef v = CFDictionaryGetValue(d, k);
    if (v && CFGetTypeID(v) == CFNumberGetTypeID()) return CFNumberGetValue(v, kCFNumberLongLongType, out);
    if (v && CFGetTypeID(v) == CFBooleanGetTypeID()) {
        *out = CFBooleanGetValue(v);
        return 1;
    }
    return 0;
}

// Battery level in percent (iOS 10 reports mAh, iOS 11+ percent) and whether a charger is
// physically attached. ExternalConnected cannot be used for the latter: it reads back as 1
// even while this program has cut the input.
static int read_battery(int *level, int *plugged) {
    CFMutableDictionaryRef p = NULL;
    if (IORegistryEntryCreateCFProperties(g_ps, &p, kCFAllocatorDefault, 0) != KERN_SUCCESS || !p) return 0;
    long long cur = 0, max = 0, capable = 0;
    int ok = dict_long(p, CFSTR("CurrentCapacity"), &cur) && dict_long(p, CFSTR("MaxCapacity"), &max) && max > 0;
    if (ok) *level = max == 100 ? (int)cur : (int)cur * 100 / (int)max;
    *plugged = dict_long(p, CFSTR("ExternalChargeCapable"), &capable) ? capable != 0 : 1;
    CFRelease(p);
    return ok;
}

static int apply(int inflow, int charging) {
    const void *k[] = {CFSTR("ExternalConnected"), CFSTR("IsCharging"), CFSTR("PredictiveChargingInhibit")};
    const void *v[] = {inflow ? kCFBooleanTrue : kCFBooleanFalse, charging ? kCFBooleanTrue : kCFBooleanFalse,
                       kCFBooleanFalse};
    CFDictionaryRef d = CFDictionaryCreate(NULL, k, v, 3, &kCFTypeDictionaryKeyCallBacks, &kCFTypeDictionaryValueCallBacks);
    kern_return_t r = IORegistryEntrySetCFProperties(g_ps, d);
    CFRelease(d);
    return r == KERN_SUCCESS;
}

static void read_conf(int *enabled, int *max, int *min) {
    *enabled = 1;
    *max = 80;
    *min = 70;
    FILE *f = fopen(CONF_PATH, "r");
    if (!f) return;
    char line[128];
    while (fgets(line, sizeof line, f)) {
        int v;
        if (sscanf(line, " enabled = %d", &v) == 1) *enabled = v;
        else if (sscanf(line, " max = %d", &v) == 1) *max = v;
        else if (sscanf(line, " min = %d", &v) == 1) *min = v;
    }
    fclose(f);
    // Keep the window sane whatever is in the file.
    if (*max > 100) *max = 100;
    if (*max < 30) *max = 30;
    if (*min > *max - 5) *min = *max - 5;
    if (*min < FAILSAFE_LEVEL + 5) *min = FAILSAFE_LEVEL + 5;
}

static void write_state(const char *state, int level, int max, int min, int enabled) {
    FILE *f = fopen(STATE_PATH ".tmp", "w");
    if (!f) return;
    fprintf(f, "{\"state\":\"%s\",\"level\":%d,\"max\":%d,\"min\":%d,\"enabled\":%s,\"updated\":%ld}\n", state, level, max,
            min, enabled ? "true" : "false", (long)time(NULL));
    fclose(f);
    rename(STATE_PATH ".tmp", STATE_PATH);
}

static void on_signal(int sig) {
    (void)sig;
    g_stop = 1;
}

int main(int argc, char **argv) {
    g_ps = IOServiceGetMatchingService(kIOMasterPortDefault, IOServiceMatching("IOPMPowerSource"));
    if (!g_ps) {
        fprintf(stderr, "relay-chargelimit: no IOPMPowerSource\n");
        return 1;
    }
    if (argc > 1 && !strcmp(argv[1], "reset")) {
        int ok = apply(1, 1);
        write_state("stopped", -1, 0, 0, 0);
        printf("normal charging restored (%s)\n", ok ? "ok" : "write failed");
        return ok ? 0 : 1;
    }

    signal(SIGTERM, on_signal);
    signal(SIGINT, on_signal);
    signal(SIGHUP, on_signal);
    setvbuf(stdout, NULL, _IOLBF, 0);

    int latch_charging = 0, last_inflow = -1, last_charging = -1;
    time_t last_apply = 0;
    const char *last_state = "";

    while (!g_stop) {
        int enabled, max, min, level = -1, plugged = 1;
        read_conf(&enabled, &max, &min);
        int have = read_battery(&level, &plugged);

        int inflow = 1, charging = 1;
        const char *state;
        if (!enabled) {
            state = "disabled";
            latch_charging = 0;
        } else if (!have) {
            state = "unknown";
        } else if (!plugged) {
            state = "unplugged";
            latch_charging = 0;
        } else if (level <= FAILSAFE_LEVEL) {
            state = "charging";
            latch_charging = 1;
        } else {
            if (level <= min) latch_charging = 1;
            if (latch_charging && level >= max) latch_charging = 0;
            if (latch_charging) {
                state = "charging";
            } else if (level > max) {
                state = "draining";
                inflow = 0;
                charging = 0;
            } else {
                state = "holding";
                charging = 0;
            }
        }

        time_t now = time(NULL);
        if (inflow != last_inflow || charging != last_charging || now - last_apply >= REASSERT_SECONDS) {
            if (apply(inflow, charging)) {
                last_inflow = inflow;
                last_charging = charging;
                last_apply = now;
            }
        }
        if (strcmp(state, last_state)) {
            printf("%ld %s level=%d max=%d min=%d\n", (long)now, state, level, max, min);
            last_state = state;
        }
        write_state(state, level, max, min, enabled);

        for (int i = 0; i < LOOP_SECONDS && !g_stop; i++) sleep(1);
    }

    apply(1, 1);
    write_state("stopped", -1, 0, 0, 0);
    printf("%ld stopped, normal charging restored\n", (long)time(NULL));
    return 0;
}
