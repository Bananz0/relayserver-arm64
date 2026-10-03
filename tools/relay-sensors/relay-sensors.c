// relay-sensors: one-shot hardware sensor reader for the relayserver package.
//
// Prints a single JSON object on stdout and exits. It runs as a separate
// process so a crash or entitlement denial in the private IOKit/IOHID calls
// can never take down the relay daemon; relayserver just gets no data.
// Every reading that cannot be obtained on a given device is emitted as null.
//
// Build (armv7s, iOS 10; arm64 the same with arm64-clang):
//   armv7s-clang -O2 -o relay-sensors relay-sensors.c -framework IOKit -framework CoreFoundation -lobjc
//   ldid -Ssensors-entitlements.xml relay-sensors
//
// armv7s has no __floatdidf in libSystem, so 64-bit integers are never
// converted to floating point here; values are narrowed to int first.

#include <CoreFoundation/CoreFoundation.h>
#include <dlfcn.h>
#include <ifaddrs.h>
#include <mach/mach.h>
#include <net/if.h>
#include <net/if_var.h>
#include <notify.h>
#include <objc/message.h>
#include <objc/runtime.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/socket.h>
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
extern CFTypeRef IOHIDServiceClientCopyProperty(IOHIDServiceClientRef service, CFStringRef key);
extern double IOHIDEventGetFloatValue(IOHIDEventRef event, int32_t field);

#define kIOHIDEventTypeAmbientLightSensor 12
#define kIOHIDEventTypeTemperature 15
#define kIOHIDEventTypePower 25
#define kIOHIDEventFieldAmbientLightSensorLevel (kIOHIDEventTypeAmbientLightSensor << 16)
#define kIOHIDEventFieldTemperatureLevel (kIOHIDEventTypeTemperature << 16)
#define kIOHIDEventFieldPowerMeasurement (kIOHIDEventTypePower << 16)
#define kHIDPage_AppleVendor 0xff00
#define kHIDPage_AppleVendorPowerSensor 0xff08
#define kHIDUsage_AppleVendor_AmbientLightSensor 4
#define kHIDUsage_PowerSensor_Current 2
#define kHIDUsage_PowerSensor_Voltage 3

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

static void json_string(const char *key, const char *s) {
    json_key(key);
    putchar('"');
    for (; *s; s++) {
        if (*s == '"' || *s == '\\')
            printf("\\%c", *s);
        else if ((unsigned char)*s < 0x20)
            printf("\\u%04x", (unsigned char)*s);
        else
            putchar(*s);
    }
    putchar('"');
}

static void json_open(const char *key) {
    json_key(key);
    printf("{");
    first_field = 1;
}

static void json_close(void) {
    printf("}");
    first_field = 0;
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

static int cf_to_cstr(CFTypeRef v, char *buf, size_t n) {
    return v && CFGetTypeID(v) == CFStringGetTypeID() &&
           CFStringGetCString((CFStringRef)v, buf, n, kCFStringEncodingUTF8);
}

// Battery gauge via the IOPMPowerSource registry entry (AppleARMPMUCharger).
static void emit_battery(void) {
    io_service_t svc = IOServiceGetMatchingService(kIOMasterPortDefault, IOServiceMatching("IOPMPowerSource"));
    CFMutableDictionaryRef props = NULL;
    if (!svc || IORegistryEntryCreateCFProperties(svc, &props, kCFAllocatorDefault, 0) != KERN_SUCCESS || !props) {
        if (svc) IOObjectRelease(svc);
        json_null("battery");
        return;
    }
    IOObjectRelease(svc);

    json_open("battery");

    long long v;
    if (dict_long(props, "Temperature", &v))  // hundredths of a degree C
        json_double("temperature_c", (int)v / 100.0);
    else
        json_null("temperature_c");
    emit_long(props, "Voltage", "voltage_mv");
    if (dict_long(props, "InstantAmperage", &v) || dict_long(props, "Amperage", &v))
        json_long("current_ma", v);
    else
        json_null("current_ma");
    emit_long(props, "CycleCount", "cycle_count");

    // iOS 11+ reports CurrentCapacity/MaxCapacity as percent (MaxCapacity == 100);
    // iOS 10 reports both in mAh.
    long long cur = 0, max = 0;
    if (dict_long(props, "CurrentCapacity", &cur) && dict_long(props, "MaxCapacity", &max) && max > 0)
        json_long("level_pct", max == 100 ? cur : (int)cur * 100 / (int)max);
    else
        json_null("level_pct");

    // Health: iOS 11+ exposes NominalChargeCapacity, which is what Settings shows as
    // Maximum Capacity. AppleRawMaxCapacity can disagree wildly on a worn gauge, so it
    // is only the fallback (iOS 10 has nothing better).
    long long design = 0, full = 0;
    int has_design = dict_long(props, "DesignCapacity", &design);
    int has_full = dict_long(props, "NominalChargeCapacity", &full);
    if (!has_full) has_full = dict_long(props, "AppleRawMaxCapacity", &full);
    has_design ? json_long("design_capacity_mah", design) : json_null("design_capacity_mah");
    has_full ? json_long("max_capacity_mah", full) : json_null("max_capacity_mah");
    if (has_design && has_full && design > 0)
        json_double("health_pct", 100.0 * (int)full / (int)design);
    else
        json_null("health_pct");

    emit_bool(props, "IsCharging", "is_charging");
    emit_bool(props, "ExternalConnected", "external_connected");
    emit_bool(props, "FullyCharged", "fully_charged");

    CFDictionaryRef adapter = CFDictionaryGetValue(props, CFSTR("AdapterDetails"));
    char name[64];
    if (adapter && CFGetTypeID(adapter) == CFDictionaryGetTypeID()) {
        emit_long(adapter, "Watts", "adapter_watts");
        if (cf_to_cstr(CFDictionaryGetValue(adapter, CFSTR("Description")), name, sizeof name))
            json_string("adapter_name", name);
        else
            json_null("adapter_name");
    } else {
        json_null("adapter_watts");
        json_null("adapter_name");
    }
    json_close();
    CFRelease(props);
}

// One pass over every HID service: ambient light, named temperature sensors and the
// charger input rails. iOS 10 lists the temperature services but returns 0 for them,
// and reports the power rails unscaled, so implausible values are dropped.
static void emit_hid(void) {
    double lux = 0, soc_sum = 0, cpu_max = 0, nand = 0, camera = 0, vbus_v = 0, vbus_a = 0;
    int has_lux = 0, soc_n = 0, has_cpu = 0, has_nand = 0, has_camera = 0, has_v = 0, has_a = 0;
    long als_services = 0;

    IOHIDEventSystemClientRef client = IOHIDEventSystemClientCreate(kCFAllocatorDefault);
    CFArrayRef services = client ? IOHIDEventSystemClientCopyServices(client) : NULL;
    if (services) {
        for (CFIndex i = 0; i < CFArrayGetCount(services); i++) {
            IOHIDServiceClientRef svc = (IOHIDServiceClientRef)CFArrayGetValueAtIndex(services, i);
            long long page = 0, usage = 0;
            CFTypeRef p = IOHIDServiceClientCopyProperty(svc, CFSTR("PrimaryUsagePage"));
            CFTypeRef u = IOHIDServiceClientCopyProperty(svc, CFSTR("PrimaryUsage"));
            CFTypeRef prod = IOHIDServiceClientCopyProperty(svc, CFSTR("Product"));
            if (p && CFGetTypeID(p) == CFNumberGetTypeID()) CFNumberGetValue(p, kCFNumberLongLongType, &page);
            if (u && CFGetTypeID(u) == CFNumberGetTypeID()) CFNumberGetValue(u, kCFNumberLongLongType, &usage);
            char product[96] = "";
            cf_to_cstr(prod, product, sizeof product);
            if (p) CFRelease(p);
            if (u) CFRelease(u);
            if (prod) CFRelease(prod);

            if (page == kHIDPage_AppleVendor && usage == kHIDUsage_AppleVendor_AmbientLightSensor) {
                als_services++;
                IOHIDEventRef ev = IOHIDServiceClientCopyEvent(svc, kIOHIDEventTypeAmbientLightSensor, 0, 0);
                if (ev) {
                    lux = IOHIDEventGetFloatValue(ev, kIOHIDEventFieldAmbientLightSensorLevel);
                    has_lux = 1;
                    CFRelease(ev);
                }
                continue;
            }
            if (!product[0]) continue;

            int is_temp = !strncmp(product, "PMGR SOC Die Temp Sensor", 24) || !strncmp(product, "TCC Temp Sensor", 15) ||
                          !strcmp(product, "NAND CH0 temp") || !strcmp(product, "PMU RCAM");
            if (is_temp) {
                IOHIDEventRef ev = IOHIDServiceClientCopyEvent(svc, kIOHIDEventTypeTemperature, 0, 0);
                if (!ev) continue;
                double t = IOHIDEventGetFloatValue(ev, kIOHIDEventFieldTemperatureLevel);
                CFRelease(ev);
                if (t <= 1.0 || t > 130.0) continue;
                if (product[0] == 'P' && product[1] == 'M' && product[2] == 'G') {
                    soc_sum += t;
                    soc_n++;
                } else if (product[0] == 'T') {
                    if (!has_cpu || t > cpu_max) cpu_max = t;
                    has_cpu = 1;
                } else if (product[0] == 'N') {
                    nand = t;
                    has_nand = 1;
                } else {
                    camera = t;
                    has_camera = 1;
                }
                continue;
            }

            if (page == kHIDPage_AppleVendorPowerSensor && !strcmp(product, "Charger vbus")) {
                IOHIDEventRef ev = IOHIDServiceClientCopyEvent(svc, kIOHIDEventTypePower, 0, 0);
                if (!ev) continue;
                double x = IOHIDEventGetFloatValue(ev, kIOHIDEventFieldPowerMeasurement);
                CFRelease(ev);
                if (usage == kHIDUsage_PowerSensor_Voltage && x >= 3.0 && x <= 20.0) {
                    vbus_v = x;
                    has_v = 1;
                } else if (usage == kHIDUsage_PowerSensor_Current && x >= 0.0 && x <= 5.0) {
                    vbus_a = x;
                    has_a = 1;
                }
            }
        }
        CFRelease(services);
    }
    if (client) CFRelease(client);

    // The light sensor is powered only while the display is on; with the screen off it
    // returns a stale 0, which would look like darkness in Home Assistant.
    uint64_t blanked = 0;
    int token;
    if (notify_register_check("com.apple.springboard.hasBlankedScreen", &token) == NOTIFY_STATUS_OK) {
        notify_get_state(token, &blanked);
        notify_cancel(token);
    }
    json_long("als_services", als_services);
    has_lux && !blanked ? json_double("illuminance_lx", lux) : json_null("illuminance_lx");
    soc_n ? json_double("soc_temp_c", soc_sum / soc_n) : json_null("soc_temp_c");
    has_cpu ? json_double("cpu_temp_c", cpu_max) : json_null("cpu_temp_c");
    has_nand ? json_double("nand_temp_c", nand) : json_null("nand_temp_c");
    has_camera ? json_double("camera_temp_c", camera) : json_null("camera_temp_c");
    if (has_v && has_a)
        json_double("charger_input_w", vbus_v * vbus_a);
    else
        json_null("charger_input_w");
}

static int notify_state(const char *name, uint64_t *state) {
    int token;
    if (notify_register_check(name, &token) != NOTIFY_STATUS_OK) return 0;
    int ok = notify_get_state(token, state) == NOTIFY_STATUS_OK;
    notify_cancel(token);
    return ok;
}

// SpringBoard/system state published through notifyd.
static void emit_device_state(void) {
    uint64_t s;
    notify_state("com.apple.springboard.hasBlankedScreen", &s) ? json_bool("screen_on", s == 0) : json_null("screen_on");
    notify_state("com.apple.springboard.lockstate", &s) ? json_bool("locked", s != 0) : json_null("locked");
    notify_state("com.apple.springboard.ringerstate", &s) ? json_bool("ringer_on", s != 0) : json_null("ringer_on");
    notify_state("com.apple.system.lowpowermode", &s) ? json_bool("low_power_mode", s != 0) : json_null("low_power_mode");
    notify_state("com.apple.system.thermalpressurelevel", &s) ? json_long("thermal_pressure", (long long)s)
                                                              : json_null("thermal_pressure");

    void *bbs = dlopen("/System/Library/PrivateFrameworks/BackBoardServices.framework/BackBoardServices", RTLD_LAZY);
    float (*brightness)(void) = bbs ? (float (*)(void))dlsym(bbs, "BKSDisplayBrightnessGetCurrent") : NULL;
    brightness ? json_long("brightness_pct", (long long)(brightness() * 100.0f + 0.5f)) : json_null("brightness_pct");

    float media = -1, ringer = -1;
    dlopen("/System/Library/PrivateFrameworks/Celestial.framework/Celestial", RTLD_LAZY);
    Class cls = objc_getClass("AVSystemController");
    if (cls) {
        id ctl = ((id (*)(Class, SEL))objc_msgSend)(cls, sel_registerName("sharedAVSystemController"));
        SEL get = sel_registerName("getVolume:forCategory:");
        if (ctl) {
            if (!((BOOL (*)(id, SEL, float *, id))objc_msgSend)(ctl, get, &media, (id)CFSTR("Audio/Video"))) media = -1;
            if (!((BOOL (*)(id, SEL, float *, id))objc_msgSend)(ctl, get, &ringer, (id)CFSTR("Ringtone"))) ringer = -1;
        }
    }
    media >= 0 ? json_long("volume_media_pct", (long long)(media * 100.0f + 0.5f)) : json_null("volume_media_pct");
    ringer >= 0 ? json_long("volume_ringer_pct", (long long)(ringer * 100.0f + 0.5f)) : json_null("volume_ringer_pct");
}

// Current Wi-Fi association via the private MobileWiFi framework.
static void emit_wifi(void) {
    char ssid[96] = "", bssid[32] = "";
    long long rssi = 0, channel = 0;
    int has_rssi = 0, has_channel = 0;

    void *h = dlopen("/System/Library/PrivateFrameworks/MobileWiFi.framework/MobileWiFi", RTLD_LAZY);
    void *(*mgr_create)(CFAllocatorRef, int) = h ? dlsym(h, "WiFiManagerClientCreate") : NULL;
    CFArrayRef (*copy_devices)(void *) = h ? dlsym(h, "WiFiManagerClientCopyDevices") : NULL;
    void *(*copy_network)(void *) = h ? dlsym(h, "WiFiDeviceClientCopyCurrentNetwork") : NULL;
    CFStringRef (*get_ssid)(void *) = h ? dlsym(h, "WiFiNetworkGetSSID") : NULL;
    CFTypeRef (*get_prop)(void *, CFStringRef) = h ? dlsym(h, "WiFiNetworkGetProperty") : NULL;

    if (mgr_create && copy_devices && copy_network) {
        void *mgr = mgr_create(kCFAllocatorDefault, 0);
        CFArrayRef devices = mgr ? copy_devices(mgr) : NULL;
        if (devices && CFArrayGetCount(devices) > 0) {
            void *net = copy_network((void *)CFArrayGetValueAtIndex(devices, 0));
            if (net) {
                if (get_ssid) cf_to_cstr(get_ssid(net), ssid, sizeof ssid);
                if (get_prop) {
                    CFTypeRef v = get_prop(net, CFSTR("RSSI"));
                    if (v && CFGetTypeID(v) == CFNumberGetTypeID())
                        has_rssi = CFNumberGetValue(v, kCFNumberLongLongType, &rssi);
                    v = get_prop(net, CFSTR("CHANNEL"));
                    if (v && CFGetTypeID(v) == CFNumberGetTypeID())
                        has_channel = CFNumberGetValue(v, kCFNumberLongLongType, &channel);
                    cf_to_cstr(get_prop(net, CFSTR("BSSID")), bssid, sizeof bssid);
                }
            }
        }
        // The network and manager objects are left to process exit: this helper is
        // one-shot, and their exact CF ownership on every iOS version is not documented.
        if (devices) CFRelease(devices);
    }

    ssid[0] ? json_string("wifi_ssid", ssid) : json_null("wifi_ssid");
    has_rssi ? json_long("wifi_rssi_dbm", rssi) : json_null("wifi_rssi_dbm");
    has_channel ? json_long("wifi_channel", channel) : json_null("wifi_channel");
    bssid[0] ? json_string("wifi_bssid", bssid) : json_null("wifi_bssid");

    // en0 byte counters. if_data counters are 32-bit and wrap at 4 GiB.
    unsigned int rx = 0, tx = 0;
    int has_counters = 0;
    struct ifaddrs *ifa;
    if (getifaddrs(&ifa) == 0) {
        for (struct ifaddrs *p = ifa; p; p = p->ifa_next)
            if (p->ifa_addr && p->ifa_addr->sa_family == AF_LINK && p->ifa_data && !strcmp(p->ifa_name, "en0")) {
                struct if_data *d = (struct if_data *)p->ifa_data;
                rx = d->ifi_ibytes;
                tx = d->ifi_obytes;
                has_counters = 1;
            }
        freeifaddrs(ifa);
    }
    has_counters ? json_long("wifi_rx_bytes", rx) : json_null("wifi_rx_bytes");
    has_counters ? json_long("wifi_tx_bytes", tx) : json_null("wifi_tx_bytes");
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
    if (getloadavg(load, 3) == 3) {
        json_double("load_1m", load[0]);
        json_double("load_5m", load[1]);
        json_double("load_15m", load[2]);
    } else {
        json_null("load_1m");
        json_null("load_5m");
        json_null("load_15m");
    }

    // /private/var holds apps and data; / is the system partition, which the 32-bit
    // devices keep almost full.
    struct statfs fs;
    if (statfs("/private/var", &fs) == 0) {
        json_long("data_free_mb", (long long)(((uint64_t)fs.f_bavail * fs.f_bsize) >> 20));
        json_long("data_total_mb", (long long)(((uint64_t)fs.f_blocks * fs.f_bsize) >> 20));
    } else {
        json_null("data_free_mb");
        json_null("data_total_mb");
    }
    if (statfs("/", &fs) == 0)
        json_long("system_free_mb", (long long)(((uint64_t)fs.f_bavail * fs.f_bsize) >> 20));
    else
        json_null("system_free_mb");
}

int main(void) {
    printf("{");
    emit_battery();
    emit_hid();
    emit_device_state();
    emit_wifi();
    emit_system();
    printf("}\n");
    return 0;
}
