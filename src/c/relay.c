//
//  Use this file to import your target's public headers that you would like to expose to Swift.
//

#import "absd.h"
#import <mach/mach.h>
#import <sys/sysctl.h>
#import <stdio.h>
#import <stdlib.h>
#import <CoreFoundation/CoreFoundation.h>

extern kern_return_t bootstrap_look_up(mach_port_t bp, const char *service_name, mach_port_t *sp);

mach_port_t ABSD_PORT = MACH_PORT_NULL;
uint32_t NAC_MAGIC = 0x50936603;

static void reset_absd_port(void) {
    if (ABSD_PORT != MACH_PORT_NULL) {
        printf("[relay.c] Resetting stale ABSD_PORT (%u)...\n", ABSD_PORT);
        fflush(stdout);
        mach_port_deallocate(mach_task_self(), ABSD_PORT);
        ABSD_PORT = MACH_PORT_NULL;
    }
}

int nac_init(const void *certificate_bytes, size_t certificate_len, uint64_t *out_ctx, void **out_session_request, size_t *session_requestCnt) {
    printf("[relay.c] nac_init: cert_len=%zu\n", certificate_len);
    fflush(stdout);
    if (ABSD_PORT == MACH_PORT_NULL) {
        printf("[relay.c] Looking up com.apple.absd via bootstrap_look_up...\n");
        fflush(stdout);
        kern_return_t kret = bootstrap_look_up(bootstrap_port, "com.apple.absd", &ABSD_PORT);
        if (kret != KERN_SUCCESS) {
            printf("[relay.c] bootstrap_look_up failed: %d (0x%x)\n", kret, kret);
            fflush(stdout);
            reset_absd_port();
            return kret;
        }
        printf("[relay.c] bootstrap_look_up success: ABSD_PORT = %u (0x%x)\n", ABSD_PORT, ABSD_PORT);
        fflush(stdout);
    }
    
    printf("[relay.c] Calling rawNACInit(port=%u, magic=0x%x)...\n", ABSD_PORT, NAC_MAGIC);
    fflush(stdout);
    mach_msg_type_number_t local_req_cnt = 0;
    int ret = rawNACInit(ABSD_PORT, NAC_MAGIC, (vm_offset_t)certificate_bytes, certificate_len, out_ctx, (vm_offset_t *)out_session_request, &local_req_cnt);
    if (ret != 0) {
        printf("[relay.c] rawNACInit failed: %d (0x%x)\n", ret, ret);
        fflush(stdout);
        reset_absd_port();
        return ret;
    }
    *session_requestCnt = (size_t)local_req_cnt;
    printf("[relay.c] rawNACInit success! context=0x%llx, req_cnt=%zu\n", *out_ctx, *session_requestCnt);
    fflush(stdout);

    return 0;
}

int nac_key_establishment(uint64_t val_ctx, const void *session_response, size_t session_response_len) {
    printf("[relay.c] Calling rawNACKeyEstablishment(port=%u, magic=0x%x, ctx=0x%llx, len=%zu)...\n", ABSD_PORT, NAC_MAGIC, val_ctx, session_response_len);
    fflush(stdout);
    int ret = rawNACKeyEstablishment(ABSD_PORT, NAC_MAGIC, val_ctx, (vm_offset_t)session_response, session_response_len);
    if (ret != 0) {
        printf("[relay.c] rawNACKeyEstablishment failed: %d (0x%x)\n", ret, ret);
        fflush(stdout);
        reset_absd_port();
        return ret;
    }
    printf("[relay.c] rawNACKeyEstablishment success!\n");
    fflush(stdout);
    return 0;
}

int nac_sign(uint64_t val_ctx, const void* data, size_t data_len, void **out_signature, size_t* out_sig_len) {
    printf("[relay.c] Calling rawNACSign(port=%u, magic=0x%x, ctx=0x%llx, data_len=%zu)...\n", ABSD_PORT, NAC_MAGIC, val_ctx, data_len);
    fflush(stdout);
    mach_msg_type_number_t local_sig_cnt = 0;
    int ret = rawNACSign(ABSD_PORT, NAC_MAGIC, val_ctx, (vm_offset_t)data, data_len, (vm_offset_t *)out_signature, &local_sig_cnt);
    if (ret != 0) {
        printf("[relay.c] rawNACSign failed: %d (0x%x)\n", ret, ret);
        fflush(stdout);
        reset_absd_port();
        return ret;
    }
    *out_sig_len = (size_t)local_sig_cnt;
    printf("[relay.c] rawNACSign success! sig_len=%zu\n", *out_sig_len);
    fflush(stdout);
    return 0;
}

extern CFTypeRef MGCopyAnswer(CFStringRef property);

char* mg_copy_answer(const char* firstProperty) {
    if (!firstProperty) return NULL;
    CFStringRef property = CFStringCreateWithCString(kCFAllocatorDefault, firstProperty, kCFStringEncodingUTF8);
    if (!property) return NULL;
    CFTypeRef answer = MGCopyAnswer(property);
    CFRelease(property);
    
    size_t malloc_size = 128;
    char *buildNumberBuf = calloc(1, malloc_size);
    if (!buildNumberBuf) {
        if (answer) CFRelease(answer);
        return NULL;
    }
    if (answer) {
        if (CFGetTypeID(answer) == CFStringGetTypeID()) {
            CFStringGetCString((CFStringRef)answer, buildNumberBuf, malloc_size, kCFStringEncodingUTF8);
        }
        CFRelease(answer);
    }
    return buildNumberBuf;
}

int mg_get_battery_level(void) {
    CFStringRef prop = CFStringCreateWithCString(kCFAllocatorDefault, "BatteryCurrentCapacity", kCFStringEncodingUTF8);
    if (!prop) return -1;
    CFTypeRef ans = MGCopyAnswer(prop);
    CFRelease(prop);
    if (!ans) return -1;
    int level = -1;
    if (CFGetTypeID(ans) == CFNumberGetTypeID()) {
        CFNumberGetValue((CFNumberRef)ans, kCFNumberIntType, &level);
    }
    CFRelease(ans);
    return level;
}

int mg_is_charging(void) {
    CFStringRef prop = CFStringCreateWithCString(kCFAllocatorDefault, "ExternalConnected", kCFStringEncodingUTF8);
    CFTypeRef ans = prop ? MGCopyAnswer(prop) : NULL;
    if (prop) CFRelease(prop);

    if (!ans) {
        prop = CFStringCreateWithCString(kCFAllocatorDefault, "ExternalPowerSourceConnected", kCFStringEncodingUTF8);
        ans = prop ? MGCopyAnswer(prop) : NULL;
        if (prop) CFRelease(prop);
    }

    if (!ans) return 0;
    int charging = 0;
    if (CFGetTypeID(ans) == CFBooleanGetTypeID()) {
        charging = CFBooleanGetValue((CFBooleanRef)ans) ? 1 : 0;
    }
    CFRelease(ans);
    return charging;
}
