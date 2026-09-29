// relay-sensors: one-shot hardware sensor reader for the relayserver package.
//
// Prints a single JSON object on stdout and exits. It runs as a separate
// process so a crash or entitlement denial in the private IOKit/IOHID calls
// can never take down the relay daemon; relayserver just gets no data.
//
// Build (armv7s, iOS 10):
//   armv7s-clang -O2 -o relay-sensors relay-sensors.c -framework IOKit -framework CoreFoundation
//   ldid -Ssensors-entitlements.xml relay-sensors

#include <CoreFoundation/CoreFoundation.h>
#include <mach/mach.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/sysctl.h>
#include <sys/time.h>
#include <time.h>

// ---- IOKit (public on macOS, present-but-headerless on iOS) ----
typedef mach_port_t io_object_t;
typedef io_object_t io_service_t;
typedef io_object_t io_registry_entry_t;
typedef UInt32 IOOptionBits;
extern const mach_port_t kIOMasterPortDefault;
extern CFMutableDictionaryRef IOServiceMatching(const char *name);
extern io_service_t IOServiceGetMatchingService(mach_port_t master, CFDictionaryRef matching);
extern kern_return_t IORegistryEntryCreateCFProperties(io_registry_entry_t entry, CFMutableDictionaryRef *props,
                                                       CFAllocatorRef alloc, IOOptionBits options);
extern kern_return_t IOObjectRelease(io_object_t object);

// ---- IOHID event system (private) ----
typedef struct __IOHIDEventSystemClient *IOHIDEventSystemClientRef;
typedef struct __IOHIDServiceClient *IOHIDServiceClientRef;
typedef struct __IOHIDEvent *IOHIDEventRef;
extern IOHIDEventSystemClientRef IOHIDEventSystemClientCreate(CFAllocatorRef alloc);
extern void IOHIDEventSystemClientSetMatching(IOHIDEventSystemClientRef client, CFDictionaryRef match);
extern CFArrayRef IOHIDEventSystemClientCopyServices(IOHIDEventSystemClientRef client);
extern IOHIDEventRef IOHIDServiceClientCopyEvent(IOHIDServiceClientRef service, int64_t type, int32_t options,
                                                 int64_t timestamp);
extern double IOHIDEventGetFloatValue(IOHIDEventRef event, int32_t field);

#define kIOHIDEventTypeAmbientLightSensor 12
#define kIOHIDEventFieldAmbientLightSensorLevel (kIOHIDEventTypeAmbientLightSensor << 16)
#define kHIDPage_AppleVendor 0xff00
#define kHIDUsage_AppleVendor_AmbientLightSensor 4

static int first_field = 1;

static void json_key(const char *key) {
    printf("%s\"%s\":", first_field ? "" : ",", key);
    first_field = 0;
}

static void json_null(const char *key) {
    json_key(key);
    printf("null");
}

static void json_long(const char *key, long long v) {
    json_key(key);
    printf("%lld", v);
}

static void json_double(const char *key, double v) {
    json_key(key);
    printf("%.2f", v);
}

static void json_bool(const char *key, int v) {
    json_key(key);
    printf(v ? "true" : "false");
}

static int dict_long(CFDictionaryRef d, const char *name, long long *out) {
    CFStringRef k = CFStringCreateWithCString(NULL, name, kCFStringEncodingUTF8);
    CFTypeRef v = CFDictionaryGetValue(d, k);
    CFRelease(k);
    if (v && CFGetTypeID(v) == CFNumberGetTypeID())
        return CFNumberGetValue((CFNumberRef)v, kCFNumberLongLongType, out);
    if (v && CFGetTypeID(v) == CFBooleanGetTypeID()) {
        *out = CFBooleanGetValue((CFBooleanRef)v);
        return 1;
    }
    return 0;
}

static void emit_long(CFDictionaryRef d, const char *name, const char *key) {
    long long v;
    if (dict_long(d, name, &v))
        json_long(key, v);
    else
        json_null(key);
}

static void emit_bool(CFDictionaryRef d, const char *name, const char *key) {
    long long v;
    if (dict_long(d, name, &v))
        json_bool(key, v != 0);
    else
        json_null(key);
}

// Battery gauge via the IOPMPowerSource registry entry (AppleARMPMUCharger on A6).
static void emit_battery(void) {
    io_service_t svc = IOServiceGetMatchingService(kIOMasterPortDefault, IOServiceMatching("IOPMPowerSource"));
    CFMutableDictionaryRef props = NULL;
    if (!svc || IORegistryEntryCreateCFProperties(svc, &props, kCFAllocatorDefault, 0) != KERN_SUCCESS || !props) {
        if (svc) IOObjectRelease(svc);
        json_key("battery");
        printf("null");
        return;
    }
    IOObjectRelease(svc);

    json_key("battery");
    printf("{");
    first_field = 1;

    long long v;
    if (dict_long(props, "Temperature", &v))  // hundredths of a degree C
        json_double("temperature_c", (int)v / 100.0);  // int cast: no __floatdidf on armv7s
    else
        json_null("temperature_c");
    emit_long(props, "Voltage", "voltage_mv");
    if (dict_long(props, "InstantAmperage", &v) || dict_long(props, "Amperage", &v))
        json_long("current_ma", v);
    else
        json_null("current_ma");
    emit_long(props, "CycleCount", "cycle_count");
    emit_long(props, "CurrentCapacity", "level_pct");

    long long design = 0, raw_max = 0;
    int has_design = dict_long(props, "DesignCapacity", &design);
    int has_raw_max = dict_long(props, "AppleRawMaxCapacity", &raw_max);
    if (!has_raw_max) has_raw_max = dict_long(props, "NominalChargeCapacity", &raw_max);
    has_design ? json_long("design_capacity_mah", design) : json_null("design_capacity_mah");
    has_raw_max ? json_long("max_capacity_mah", raw_max) : json_null("max_capacity_mah");
    if (has_design && has_raw_max && design > 0)
        json_double("health_pct", 100.0 * (int)raw_max / (int)design);
    else
        json_null("health_pct");

    emit_bool(props, "IsCharging", "is_charging");
    emit_bool(props, "ExternalConnected", "external_connected");
    emit_bool(props, "FullyCharged", "fully_charged");
    printf("}");
    first_field = 0;
    CFRelease(props);
}

// Ambient light. The ALS is only powered while the display is on, so with the
// screen off this usually yields the last cached reading or nothing at all.
static void emit_ambient_light(void) {
    IOHIDEventSystemClientRef client = IOHIDEventSystemClientCreate(kCFAllocatorDefault);
    if (!client) {
        json_null("illuminance_lx");
        return;
    }

    int page = kHIDPage_AppleVendor, usage = kHIDUsage_AppleVendor_AmbientLightSensor;
    CFNumberRef n_page = CFNumberCreate(NULL, kCFNumberIntType, &page);
    CFNumberRef n_usage = CFNumberCreate(NULL, kCFNumberIntType, &usage);
    const void *keys[] = {CFSTR("PrimaryUsagePage"), CFSTR("PrimaryUsage")};
    const void *vals[] = {n_page, n_usage};
    CFDictionaryRef match = CFDictionaryCreate(NULL, keys, vals, 2, &kCFTypeDictionaryKeyCallBacks,
                                               &kCFTypeDictionaryValueCallBacks);
    IOHIDEventSystemClientSetMatching(client, match);
    CFRelease(match);
    CFRelease(n_page);
    CFRelease(n_usage);

    int found = 0;
    double lux = 0;
    CFArrayRef services = IOHIDEventSystemClientCopyServices(client);
    if (services) {
        for (CFIndex i = 0; i < CFArrayGetCount(services) && !found; i++) {
            IOHIDServiceClientRef svc = (IOHIDServiceClientRef)CFArrayGetValueAtIndex(services, i);
            IOHIDEventRef ev = IOHIDServiceClientCopyEvent(svc, kIOHIDEventTypeAmbientLightSensor, 0, 0);
            if (ev) {
                lux = IOHIDEventGetFloatValue(ev, kIOHIDEventFieldAmbientLightSensorLevel);
                found = 1;
                CFRelease(ev);
            }
        }
        json_long("als_services", CFArrayGetCount(services));
        CFRelease(services);
    } else {
        json_long("als_services", 0);
    }
    CFRelease(client);

    if (found)
        json_double("illuminance_lx", lux);
    else
        json_null("illuminance_lx");
}

static void emit_system(void) {
    struct timeval boottime;
    size_t len = sizeof(boottime);
    int mib[2] = {CTL_KERN, KERN_BOOTTIME};
    if (sysctl(mib, 2, &boottime, &len, NULL, 0) == 0 && boottime.tv_sec > 0)
        json_long("system_uptime_s", (long long)(time(NULL) - boottime.tv_sec));
    else
        json_null("system_uptime_s");

    uint64_t memsize = 0;
    len = sizeof(memsize);
    if (sysctlbyname("hw.memsize", &memsize, &len, NULL, 0) == 0)
        json_long("mem_total_mb", (long long)(memsize >> 20));
    else
        json_null("mem_total_mb");

    vm_statistics_data_t vm;
    mach_msg_type_number_t count = HOST_VM_INFO_COUNT;
    vm_size_t page_size = 0;
    host_page_size(mach_host_self(), &page_size);
    if (host_statistics(mach_host_self(), HOST_VM_INFO, (host_info_t)&vm, &count) == KERN_SUCCESS && page_size)
        json_long("mem_free_mb", (long long)(((uint64_t)vm.free_count + vm.inactive_count) * page_size >> 20));
    else
        json_null("mem_free_mb");

    double load[3];
    if (getloadavg(load, 3) == 3)
        json_double("load_1m", load[0]);
    else
        json_null("load_1m");
}

int main(void) {
    printf("{");
    emit_battery();
    emit_ambient_light();
    emit_system();
    printf("}\n");
    return 0;
}
