/* Test-only J2534 DLL for the Windows helper. It has no hardware access. */
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <stdint.h>
#include <string.h>
#include <wchar.h>

#ifndef FIXTURE_MODE
#define FIXTURE_MODE 0
#endif

#define API __declspec(dllexport) __stdcall
#define J1850_VPW 1u
#define J1850_PWM 2u
#define ISO_9141 3u
#define KWP 4u
#define RAW_CAN 5u
#define ISO_TP 6u
#define EXTENDED 0x100u
#define RX_START 0x02u
#define PADDING_ERROR 0x10u
#define FUNCTIONAL 0x18DB33F1u
#define FIXTURE_PROBE_FAILURE (FIXTURE_MODE >= 25 && FIXTURE_MODE <= 27)
#define FIXTURE_11BIT (FIXTURE_MODE == 13 || FIXTURE_MODE == 14 || FIXTURE_MODE == 16 || FIXTURE_MODE == 18 || FIXTURE_MODE == 21 || FIXTURE_PROBE_FAILURE)
#define FIXTURE_KLINE (FIXTURE_MODE == 17 || FIXTURE_MODE == 19 || FIXTURE_MODE == 22 || FIXTURE_MODE == 23 || FIXTURE_MODE == 24)
#define FIXTURE_KWP (FIXTURE_MODE == 22 || FIXTURE_MODE == 23)
#define FIXTURE_J1850 (FIXTURE_MODE == 28 || FIXTURE_MODE == 29)

typedef struct {
    uint32_t protocol, rx_status, tx_flags, timestamp, data_size, extra_data_index;
    uint8_t data[4128];
} Message;

static HMODULE module;
static int device_open;
static uint32_t protocol;
static uint32_t filters[8];
static unsigned filter_count, slot, samples, request_len;
static unsigned readiness_reads;
static unsigned stored_code_reads;
static unsigned health_bitmap_reads, health_rpm_reads;
static unsigned monitor_discovery_reads;
static unsigned live_rpm_requests;
static unsigned oxygen_reads;
static unsigned frame_three_trigger_reads;
static int verified_pidmap;
static int pwm_node_configured, pwm_response_configured;
static DWORD last_kline_init;
static DWORD last_kline_wait;
static uint8_t request[16];
static DWORD request_started;

BOOL WINAPI DllMain(HINSTANCE instance, DWORD reason, LPVOID unused) {
    (void)unused;
    if (reason == DLL_PROCESS_ATTACH) module = instance;
    return TRUE;
}

static void mark(const wchar_t *suffix) {
    wchar_t path[MAX_PATH];
    DWORD length = GetModuleFileNameW(module, path, MAX_PATH);
    size_t tail = wcslen(suffix) + 1;
    if (length == 0 || length + tail > MAX_PATH) return;
    memcpy(path + length, suffix, tail * sizeof(wchar_t));
    HANDLE file = CreateFileW(path, FILE_APPEND_DATA, FILE_SHARE_READ | FILE_SHARE_WRITE,
        NULL, OPEN_ALWAYS, FILE_ATTRIBUTE_NORMAL, NULL);
    if (file != INVALID_HANDLE_VALUE) {
        DWORD written;
        WriteFile(file, "1", 1, &written, NULL);
        CloseHandle(file);
    }
}

static uint32_t id(const Message *message) {
    return ((uint32_t)message->data[0] << 24) | ((uint32_t)message->data[1] << 16)
        | ((uint32_t)message->data[2] << 8) | message->data[3];
}

uint32_t API PassThruOpen(const void *name, uint32_t *device) {
    mark(L".open");
    if (FIXTURE_MODE == 1) Sleep(60000);
    if (name != NULL || device_open) return 7;
    device_open = 1;
    *device = 11;
    return 0;
}

uint32_t API PassThruClose(uint32_t device) {
    if (device != 11) return 0x1a;
    mark(L".close");
    device_open = 0;
    protocol = 0;
    return 0;
}

uint32_t API PassThruConnect(uint32_t device, uint32_t kind, uint32_t flags,
    uint32_t baud, uint32_t *channel) {
    if (!device_open || device != 11 || protocol != 0) return 3;
    if (FIXTURE_KLINE) {
        if (flags != 0 || baud != 10400 || (kind != ISO_9141 && kind != KWP)) return 3;
    } else if (FIXTURE_J1850) {
        if (flags != 0 || kind != (FIXTURE_MODE == 28 ? J1850_VPW : J1850_PWM)
            || baud != (kind == J1850_VPW ? 10400u : 41600u)) return 3;
    } else if (baud != 500000 || (FIXTURE_11BIT
        ? (flags != 0 || kind != ISO_TP)
        : (flags != EXTENDED || (kind != RAW_CAN && kind != ISO_TP)))) {
        return 3;
    }
    protocol = kind;
    filter_count = 0;
    verified_pidmap = 0;
    pwm_node_configured = pwm_response_configured = 0;
    *channel = 20 + kind;
    mark(L".channel");
    return 0;
}

uint32_t API PassThruDisconnect(uint32_t channel) {
    if (protocol == 0 || channel != 20 + protocol) return 2;
    mark(L".disconnect");
    if (FIXTURE_MODE == 27) return 1;
    protocol = 0;
    return 0;
}

uint32_t API PassThruStartMsgFilter(uint32_t channel, uint32_t type, Message *mask,
    Message *pattern, Message *flow, uint32_t *filter) {
    const uint32_t header_size = FIXTURE_J1850 ? 2u : FIXTURE_KLINE ? 1u : 4u;
    if (channel != 20 + protocol || mask->protocol != protocol || pattern->protocol != protocol
        || mask->data_size != header_size || pattern->data_size != header_size || filter_count >= 8) return 7;
    if (FIXTURE_KLINE) {
        if (type != 1 || flow != NULL || mask->data[0] != 0 || pattern->data[0] != 0) return 7;
    } else if (FIXTURE_J1850) {
        if (type != 1 || flow != NULL || mask->data[0] != 0 || mask->data[1] != 0xff
            || pattern->data[0] != 0 || pattern->data[1] != 0x6b) return 7;
    } else if (protocol == RAW_CAN) {
        if (type != 1 || flow != NULL || id(mask) != 0x1FFFFF00u || id(pattern) != 0x18DAF100u) return 7;
    } else if (FIXTURE_11BIT) {
        const uint32_t response = id(pattern);
        if (type != 3 || !flow || id(mask) != 0x7ffu || response < 0x7e8u || response > 0x7efu
            || id(flow) != response - 8) return 7;
    } else {
        uint32_t response = id(pattern);
        if (type != 3 || !flow || id(mask) != UINT32_MAX
            || (response & 0xFFFFFF00u) != 0x18DAF100u
            || id(flow) != (0x18DA00F1u | ((response & 0xffu) << 8))) return 7;
    }
    filters[filter_count++] = id(pattern);
    *filter = filter_count;
    return 0;
}

uint32_t API PassThruStopMsgFilter(uint32_t channel, uint32_t filter) {
    (void)filter;
    return channel == 20 + protocol ? 0 : 2;
}

uint32_t API PassThruStartPeriodicMsg(uint32_t channel, Message *message, uint32_t *id_out,
    uint32_t interval) {
    const uint8_t keepalive[] = {0x68, 0x6a, 0xf1, 1, 0};
    const uint8_t tester_present[] = {0xc1, 0x33, 0xf1, 0x3e};
    if (FIXTURE_J1850) mark(L".unexpected-keepalive");
    if (!FIXTURE_KLINE || !verified_pidmap || channel != 20 + protocol
        || message->protocol != protocol || interval != 2000) {
        return 7;
    }
    if (protocol == KWP) {
        if (message->data_size != sizeof(tester_present)
            || memcmp(message->data, tester_present, sizeof(tester_present))) return 7;
    } else if (message->data_size != sizeof(keepalive)
        || memcmp(message->data, keepalive, sizeof(keepalive))) return 7;
    *id_out = 1;
    mark(L".keepalive");
    return 0;
}

uint32_t API PassThruStopPeriodicMsg(uint32_t channel, uint32_t id_value) {
    if (!FIXTURE_KLINE || channel != 20 + protocol || id_value != 1) return 7;
    mark(L".stop-keepalive");
    return 0;
}

uint32_t API PassThruIoctl(uint32_t handle, uint32_t command, void *input, void *output) {
    if (FIXTURE_J1850 && protocol != 0 && handle == 20 + protocol) {
        if (command == 4 || command == 5) mark(L".unexpected-init");
        if (protocol == J1850_PWM && command == 2) {
            typedef struct { uint32_t parameter, value; } Config;
            typedef struct { uint32_t count; Config *entries; } ConfigList;
            ConfigList *settings = input;
            if (!settings || !settings->entries || settings->count != 1 || output
                || settings->entries[0].parameter != 4 || settings->entries[0].value != 0xf1) return 7;
            pwm_node_configured = 1;
            mark(L".pwm-node");
            return 0;
        }
        if (protocol == J1850_PWM && command == 0x0c) {
            typedef struct { uint32_t count; uint8_t *bytes; } ByteArray;
            ByteArray *address = input;
            if (!pwm_node_configured || !address || !address->bytes || address->count != 1
                || address->bytes[0] != 0x6b || output) return 7;
            pwm_response_configured = 1;
            mark(L".pwm-response");
            return 0;
        }
    }
    if (FIXTURE_KLINE && (protocol == ISO_9141 || protocol == KWP) && handle == 20 + protocol && command == 4) {
        typedef struct { uint32_t count; uint8_t *bytes; } ByteArray;
        ByteArray *address = input;
        ByteArray *keywords = output;
        if (!address || !keywords || address->count != 1 || address->bytes[0] != 0x33
            || keywords->count < 2) return 7;
        mark(protocol == ISO_9141 ? L".slow-iso" : L".slow-kwp");
        if (last_kline_wait != 0 && GetTickCount() - last_kline_init + 20 < last_kline_wait) return 7;
        keywords->count = 2;
        keywords->bytes[0] = FIXTURE_MODE == 23 ? 0x8f : 8;
        keywords->bytes[1] = FIXTURE_MODE == 23 ? 0xe9 : 8;
        last_kline_init = GetTickCount();
        last_kline_wait = 5000;
        return 0;
    }
    if (FIXTURE_KLINE && protocol == KWP && handle == 20 + protocol && command == 5) {
        const uint8_t start[] = {0xc1, 0x33, 0xf1, 0x81};
        Message *request = input;
        Message *response = output;
        if (!request || !response || request->protocol != KWP
            || request->data_size != sizeof(start)
            || memcmp(request->data, start, sizeof(start))) return 7;
        mark(L".fast-kwp");
        last_kline_init = GetTickCount();
        last_kline_wait = 2600;
        if (FIXTURE_MODE != 22) return 9;
        const uint8_t reply[] = {0x83, 0xf1, 0x10, 0xc1, 0x8f, 0xe9};
        memset(response, 0, sizeof(*response));
        response->protocol = KWP;
        memcpy(response->data, reply, sizeof(reply));
        response->data_size = response->extra_data_index = sizeof(reply);
        return 0;
    }
    if (command == 3 && handle == 11) {
        if (FIXTURE_MODE == 8) return 1;
        if (FIXTURE_MODE == 9) {
            *(uint32_t *)output = 0;
            return 0;
        }
        if (FIXTURE_MODE == 4 && protocol == 0) {
            mark(L".passive");
            Sleep(60000);
        }
        *(uint32_t *)output = 12600 + 100 * (samples++ % 6);
        return 0;
    }
    if (protocol != 0 && handle == 20 + protocol && command == 8) {
        slot = 100;
        return 0;
    }
    return 1;
}

uint32_t API PassThruWriteMsgs(uint32_t channel, Message *message, uint32_t *count,
    uint32_t timeout) {
    (void)timeout;
    if (protocol == 0 || channel != 20 + protocol || *count != 1
        || message->protocol != protocol) return 7;
    if (FIXTURE_KLINE) {
        if (message->data_size <= 3 || message->data_size > 66) return 7;
        const uint8_t expected[] = {
            protocol == KWP ? (uint8_t)(0xc0u | (message->data_size - 3)) : 0x68,
            protocol == KWP ? 0x33 : 0x6a,
            0xf1
        };
        if (memcmp(message->data, expected, sizeof(expected))) return 7;
        if (FIXTURE_MODE == 23 && protocol == ISO_9141) mark(L".wrong-header");
    } else if (FIXTURE_J1850) {
        const uint8_t expected[] = {protocol == J1850_PWM ? 0x61 : 0x68, 0x6a, 0xf1};
        if (!filter_count || message->data_size < 4 || message->data_size > 10
            || message->tx_flags || memcmp(message->data, expected, sizeof(expected))
            || (protocol == J1850_PWM && (!pwm_node_configured || !pwm_response_configured))) return 7;
    } else if (id(message) != (FIXTURE_11BIT ? 0x7dfu : FUNCTIONAL)) {
        return 7;
    }
    if (protocol == RAW_CAN) {
        const uint8_t expected[] = {2, 1, 0, 0, 0, 0, 0, 0};
        if (message->data_size != 12 || memcmp(message->data + 4, expected, sizeof(expected))) return 7;
    } else {
        const uint32_t header_size = (FIXTURE_KLINE || FIXTURE_J1850) ? 3u : 4u;
        if (message->data_size <= header_size || message->data_size > header_size + sizeof(request)) return 7;
        memset(request, 0, sizeof(request));
        request_len = message->data_size - header_size;
        memcpy(request, message->data + header_size, request_len);
        if (request[0] == 4) mark(L".clear");
        if (request[0] == 1 && request[1] == 0) mark(L".pidmap-00");
        if (FIXTURE_MODE == 25 || FIXTURE_MODE == 27) {
            *count = 0;
            return 9;
        }
        if (request[0] == 1 && request[1] == 0x20) mark(L".pidmap-20");
        if (request[0] == 1 && request[1] == 0x40) mark(L".pidmap-40");
        if (request[0] == 1 && request_len > 2) mark(L".group-live");
        if (FIXTURE_MODE >= 10 && request[0] == 1 && request_len == 2) {
            if (request[1] == 5) mark(L".single-05");
            if (request[1] == 12) mark(L".single-0c");
            if (request[1] == 13) mark(L".single-0d");
            if (FIXTURE_MODE == 20 && request[1] == 12) live_rpm_requests++;
        }
        if (FIXTURE_MODE == 14 && request_len == 2 && request[0] == 1 && request[1] == 1) {
            readiness_reads++;
        }
        if (FIXTURE_MODE == 15 && request_len == 2 && request[0] == 1) {
            if (request[1] == 0) health_bitmap_reads++;
            if (request[1] == 0x0c) health_rpm_reads++;
        }
        if ((FIXTURE_MODE == 16 || FIXTURE_KLINE) && request_len == 1 && request[0] == 3) {
            stored_code_reads++;
            if (FIXTURE_KLINE && readiness_reads < stored_code_reads) return 7;
        }
        if ((FIXTURE_MODE == 16 || FIXTURE_KLINE) && request_len == 2 && request[0] == 1 && request[1] == 1) {
            if (FIXTURE_KLINE) readiness_reads++;
            mark(L".code-count");
        }
        if ((FIXTURE_MODE == 13 || FIXTURE_MODE == 21) && request[0] == 2 && request_len == 3) {
            switch (request[1]) {
                case 0: mark(L".frame-map-00"); break;
                case 0x20: mark(L".frame-map-20"); break;
                case 0x40: mark(L".frame-map-40"); break;
                case 0x60: mark(L".frame-map-60"); break;
                case 2:
                    mark(L".frame-trigger");
                    if (FIXTURE_MODE == 13 && request[2] == 3) frame_three_trigger_reads++;
                    break;
                default: mark(L".frame-data"); break;
            }
        }
        if (FIXTURE_MODE == 18 && request[0] == 6) {
            mark(L".mode-06");
            if (request_len != 2) return 7;
            if (request[1] == 0) monitor_discovery_reads++;
        }
        if (FIXTURE_MODE == 19 && request[0] == 9) mark(L".mode-09");
        if (FIXTURE_MODE == 24 && request[0] == 5 && request_len == 3) {
            mark(L".oxygen");
            if (request[1] == 5 && request[2] == 0x10) oxygen_reads++;
            if (request[1] == 0x20) mark(L".oxygen-continuation");
        }
    }
    slot = 0;
    request_started = GetTickCount();
    return 0;
}

static uint32_t reply(Message *message, uint32_t *count, uint32_t source,
    const uint8_t *bytes, uint32_t length) {
    memset(message, 0, sizeof(*message));
    message->protocol = protocol;
    const uint32_t header_size = (FIXTURE_KLINE || FIXTURE_J1850) ? 3u : 4u;
    if (FIXTURE_KLINE) {
        message->data[0] = protocol == KWP ? (uint8_t)(0x80 | length) : 0x48;
        message->data[1] = protocol == KWP ? 0xf1 : 0x6b;
        message->data[2] = (uint8_t)source;
    } else if (FIXTURE_J1850) {
        message->data[0] = protocol == J1850_PWM
            ? (source == 0x10 ? 0x41 : 0x61)
            : (source == 0x10 ? 0x48 : 0x68);
        message->data[1] = 0x6b;
        message->data[2] = (uint8_t)source;
    } else {
        message->rx_status = FIXTURE_11BIT ? 0 : EXTENDED;
        message->data[0] = (uint8_t)(source >> 24);
        message->data[1] = (uint8_t)(source >> 16);
        message->data[2] = (uint8_t)(source >> 8);
        message->data[3] = (uint8_t)source;
    }
    memcpy(message->data + header_size, bytes, length);
    message->data_size = header_size + length;
    message->extra_data_index = message->data_size;
    if (FIXTURE_J1850 && protocol == J1850_PWM) {
        message->data[message->data_size++] = 0xf1; /* J2534 exposes the PWM IFR after the payload. */
    }
    *count = 1;
    return 0;
}

static uint32_t freeze_frame_reply(Message *message, uint32_t *count, uint32_t source) {
    const uint8_t pid = request[1], frame = request[2];
    const int engine = source == 0x7e8u;
    if (frame == 0 && engine && pid == 2) {
        /* The observed vehicle reported no freeze frame with this payload. */
        const uint8_t trigger[] = {0x42, 2, 0, 0, 0};
        return reply(message, count, source, trigger, sizeof(trigger));
    }
    if (frame == 3) {
        if (pid == 2) {
            if (engine && frame_three_trigger_reads > 1) {
                *count = 0;
                return 0x10;
            }
            const uint8_t trigger[] = {0x42, 2, frame, engine ? 1 : 0, engine ? 0x33 : 0};
            return reply(message, count, source, trigger, sizeof(trigger));
        }
        if (pid == 0) {
            const uint8_t engine_map[] = {0x42, 0, frame, 0x48, 0, 0, 1};
            const uint8_t empty_map[] = {0x42, 0, frame, 0x40, 8, 0, 0};
            return reply(message, count, source, engine ? engine_map : empty_map, sizeof(engine_map));
        }
        if (engine && (pid == 0x20 || pid == 0x40 || pid == 0x60)) {
            const uint8_t bitmap[] = {0x42, pid, frame, 0, pid == 0x20 ? 0 : 2, 0, pid == 0x60 ? 0 : 1};
            return reply(message, count, source, bitmap, sizeof(bitmap));
        }
        if (pid == 5) {
            const uint8_t coolant[] = {0x42, 5, frame, engine ? 128 : 0};
            return reply(message, count, source, coolant, sizeof(coolant));
        }
        if (engine && (pid == 0x4f || pid == 0x6f)) {
            const uint8_t raw[] = {0x42, pid, frame, 0x01, 0xab, 0xcd};
            return reply(message, count, source, raw, sizeof(raw));
        }
    }
    *count = 0;
    return 0x10;
}

static uint32_t kline_code_reply(Message *message, uint32_t *count, uint32_t source) {
    if (request[0] == 1 && request[1] == 0) {
        const uint8_t bitmap[] = {0x41, 0, 0x80, FIXTURE_KWP ? 0x10 : 0,
            FIXTURE_MODE == 24 && source == 0x10 ? 0x20 : 0,
            FIXTURE_MODE == 24 && source == 0x18 ? 8 : 0};
        verified_pidmap = 1;
        return reply(message, count, source, bitmap, sizeof(bitmap));
    }
    if (request[0] == 1 && request[1] == 1) {
        const uint8_t readiness[] = {0x41, 1, source == 0x18 && readiness_reads != 2 ? 1 : 0, 7, 0, 0};
        return reply(message, count, source, readiness, sizeof(readiness));
    }
    if (FIXTURE_KWP && request[0] == 1 && request[1] == 0x0c && source == 0x10) {
        const uint8_t rpm[] = {0x41, 0x0c, 0x0c, 0x80};
        return reply(message, count, source, rpm, sizeof(rpm));
    }
    if (request[0] == 3 && source == 0x18 && readiness_reads != 2) {
        const uint8_t stored[] = {0x43, readiness_reads == 1 ? 1 : 0, readiness_reads == 1 ? 0x33 : 0, 0, 0, 0, 0};
        return reply(message, count, source, stored, sizeof(stored));
    }
    if (request[0] == 7) {
        const uint8_t pending[] = {0x47, 0, 0, 0, 0, 0, 0};
        return reply(message, count, source, pending, sizeof(pending));
    }
    if (request[0] == 10) {
        const uint8_t unsupported[] = {0x7f, 10, 0x11};
        return reply(message, count, source, unsupported, sizeof(unsupported));
    }
    *count = 0;
    return 0x10;
}

static uint32_t j1850_reply(Message *message, uint32_t *count, unsigned index) {
    const uint8_t mode = request[0], pid = request[1];
    const uint32_t source = index == 0 ? 0x10u : 0x18u;
    if (mode == 1 && pid == 0 && index < 2) {
        const uint8_t supported[] = {0x41, 0, 0x80, 0x10, 0, 0};
        return reply(message, count, source, supported, sizeof(supported));
    }
    if (mode == 1 && pid == 1 && index < 2) {
        const uint8_t status[] = {0x41, 1, index == 0 ? 0x82 : 0, 7, 5, 4};
        return reply(message, count, source, status, sizeof(status));
    }
    if (mode == 1 && pid == 0x41 && index < 2) {
        const uint8_t status[] = {0x41, 0x41, 0, 7, 5, 4};
        return reply(message, count, source, status, sizeof(status));
    }
    if (mode == 1 && pid == 0x0c && index < 2) {
        const uint8_t rpm[] = {0x41, 0x0c, index == 0 ? 0x0c : 0x19, 0x80};
        return reply(message, count, source, rpm, sizeof(rpm));
    }
    if (mode == 3 && index < 2) {
        const uint8_t codes[] = {0x43, 1, index == 0 ? 0x33 : 0x71, 0, 0, 0, 0};
        return reply(message, count, 0x10, codes, sizeof(codes));
    }
    if (mode == 7 && index < 2) {
        const uint8_t pending[] = {0x47, 0, 0, 0, 0, 0, 0};
        return reply(message, count, source, pending, sizeof(pending));
    }
    if (mode == 2 && pid == 2 && index < 2) {
        const uint8_t trigger[] = {0x42, 2, request[2], index == 0 ? 1 : 0, index == 0 ? 0x33 : 0};
        return reply(message, count, source, trigger, sizeof(trigger));
    }
    if (mode == 2 && pid == 0 && index == 0) {
        const uint8_t supported[] = {0x42, 0, request[2], 0x08, 0, 0, 0};
        return reply(message, count, source, supported, sizeof(supported));
    }
    if (mode == 2 && pid == 5 && index == 0) {
        const uint8_t coolant[] = {0x42, 5, request[2], 128};
        return reply(message, count, source, coolant, sizeof(coolant));
    }
    *count = 0;
    return 0x10;
}

static uint32_t legacy_monitor_reply(Message *message, uint32_t *count, unsigned index) {
    const uint8_t pid = request[1], sensor = request[2];
    const uint32_t source = index == 0 ? 0x10u : 0x18u;
    const int first_ecu = source == 0x10u;
    if (request[0] == 1 && pid == 1) {
        const uint8_t readiness[] = {0x41, 1, 0, 7, 0x20, first_ecu ? 0 : 0x20};
        return reply(message, count, source, readiness, sizeof(readiness));
    }
    if (request[0] == 1 && ((pid == 0x13 && first_ecu) || (pid == 0x1d && !first_ecu))) {
        const uint8_t locations[] = {0x41, pid, 0x10};
        return reply(message, count, source, locations, sizeof(locations));
    }
    if (request[0] == 5 && sensor == 0x10) {
        if (pid == 0) {
            const uint8_t support[] = {0x45, 0, sensor, first_ecu ? 0x88 : 0x08, 0, 0, first_ecu ? 1 : 0};
            return reply(message, count, source, support, sizeof(support));
        }
        if (pid == 0x20 && first_ecu) {
            const uint8_t support[] = {0x45, 0x20, sensor, 0x80, 0, 0, 0};
            return reply(message, count, source, support, sizeof(support));
        }
        if (pid == 1 && first_ecu) {
            const uint8_t threshold[] = {0x45, 1, sensor, 90};
            return reply(message, count, source, threshold, sizeof(threshold));
        }
        if (pid == 5 && (first_ecu || oxygen_reads == 1)) {
            const uint8_t timing[] = {0x45, 5, sensor, first_ecu ? 0x12 : 0x2a, 0, 0x19};
            return reply(message, count, source, timing, sizeof(timing));
        }
        if (pid == 0x21 && first_ecu) {
            const uint8_t timing[] = {0x45, 0x21, sensor, 0x2a, 0, 0x19};
            return reply(message, count, source, timing, sizeof(timing));
        }
        if (pid == 9 && first_ecu) {
            const uint8_t other_sensor[] = {0x45, 9, 2, 2, 0, 4};
            return reply(message, count, source, other_sensor, sizeof(other_sensor));
        }
    }
    if (request[0] == 6 && first_ecu) {
        if (pid == 0) {
            const uint8_t supported[] = {0x46, 0, 0xff, 0x08, 0, 0, 0};
            return reply(message, count, source, supported, sizeof(supported));
        }
        if (pid == 5) {
            const uint8_t test[] = {0x46, 5, 1, 0, 2, 0, 3};
            return reply(message, count, source, test, sizeof(test));
        }
    }
    *count = 0;
    return 0x10;
}

static uint32_t kline_information_reply(Message *message, uint32_t *count, unsigned index) {
    if (request[1] == 0 && index < 2) {
        const uint8_t bitmap[] = {0x49, 0, 1, index == 0 ? 0x70 : 0x0c, 0, 0, 0};
        return reply(message, count, index == 0 ? 0x10 : 0x18, bitmap, sizeof(bitmap));
    }
    if (request[1] == 2 && index < 5) {
        const uint8_t vin[] = "\0\0\0JTDBR32E720123456";
        uint8_t packet[] = {0x49, 2, (uint8_t)(index + 1), 0, 0, 0, 0};
        memcpy(packet + 3, vin + index * 4, 4);
        return reply(message, count, 0x10, packet, sizeof(packet));
    }
    if (request[1] == 3 && index == 0) {
        const uint8_t messages[] = {0x49, 3, 8};
        return reply(message, count, 0x10, messages, sizeof(messages));
    }
    if (request[1] == 4 && index < 4) {
        const uint8_t calibration[] = "KLINE-CAL-000001";
        uint8_t packet[] = {0x49, 4, (uint8_t)(index + 1), 0, 0, 0, 0};
        memcpy(packet + 3, calibration + index * 4, 4);
        return reply(message, count, 0x10, packet, sizeof(packet));
    }
    if (request[1] == 5 && index == 0) {
        const uint8_t messages[] = {0x49, 5, 1};
        return reply(message, count, 0x18, messages, sizeof(messages));
    }
    if (request[1] == 6 && index == 0) {
        const uint8_t cvn[] = {0x49, 6, 1, 0x12, 0x34, 0x56, 0x78};
        return reply(message, count, 0x18, cvn, sizeof(cvn));
    }
    *count = 0;
    return 0x10;
}

uint32_t API PassThruReadMsgs(uint32_t channel, Message *message, uint32_t *count,
    uint32_t timeout) {
    if (protocol == 0 || channel != 20 + protocol) return 2;
    if (FIXTURE_MODE == 26) {
        if (slot++ == 0) mark(L".wait-for-reply");
        *count = 0;
        return 0x10;
    }
    if (protocol == ISO_TP && !FIXTURE_11BIT && request[0] == 9 && request[1] == 2 && slot == 2) {
        DWORD elapsed = GetTickCount() - request_started;
        if (elapsed < 280) {
            DWORD remaining = 280 - elapsed;
            Sleep(timeout < remaining ? timeout : remaining);
            *count = 0;
            return 0x10;
        }
    }
    unsigned index = slot++;
    if (FIXTURE_J1850) return j1850_reply(message, count, index);
    if (FIXTURE_MODE == 19 && request[0] == 9) {
        return kline_information_reply(message, count, index);
    }
    if (FIXTURE_MODE == 24 && index < 2 && (request[0] == 5 || request[0] == 6
        || (request[0] == 1 && request[1] != 0))) {
        return legacy_monitor_reply(message, count, index);
    }
    if (FIXTURE_KLINE && index < 2) {
        return kline_code_reply(message, count, index == 0 ? 0x10u : 0x18u);
    }
    if (protocol == RAW_CAN && index < 5) {
        if (FIXTURE_MODE == 8) {
            *count = 0;
            return 0x10;
        }
        const uint32_t sources[] = {0x18DA10F1u, 0x18DAF110u, 0x18DAF127u, 0x18DAF145u, 0x18DAF127u};
        const uint8_t bytes[] = {6, 0x41, 0, 0x80, 0x10, 0, 0, 0};
        return reply(message, count, sources[index], bytes, index == 1 ? 3 : sizeof(bytes));
    }
    if (protocol == ISO_TP && !FIXTURE_11BIT && request[0] == 9 && (request[1] == 2 || request[1] == 4)) {
        /* Receive the start of the second ECU's reply before the first ECU's complete reply. */
        const uint8_t vin_a[] = "\x49\x02\x01" "JTDBR32E720123456";
        const uint8_t vin_b[] = "\x49\x02\x01" "JTDBR32E720123457";
        const uint8_t calibration[] = "\x49\x04\x01" "MFC3-FIXTURE-001";
        if (index == 0) {
            reply(message, count, 0x18DAF145u, vin_b, 0);
            message->rx_status |= RX_START;
            message->extra_data_index = 0;
            return 0;
        }
        if (index == 1) {
            return request[1] == 2
                ? reply(message, count, 0x18DAF127u, vin_a, sizeof(vin_a) - 1)
                : reply(message, count, 0x18DAF127u, calibration, sizeof(calibration) - 1);
        }
        if (index == 2 && request[1] == 2) {
            return reply(message, count, 0x18DAF145u, vin_b, sizeof(vin_b) - 1);
        }
    }
    if (protocol == ISO_TP && index < filter_count) {
        uint32_t source = filters[index];
        const uint32_t engine_source = FIXTURE_11BIT ? 0x7e8u : 0x18DAF127u;
        const uint32_t other_source = FIXTURE_11BIT ? 0x7eau : 0x18DAF145u;
        if (source != engine_source && source != other_source) {
            *count = 0;
            return 0x10;
        }
        if (FIXTURE_MODE == 21 && request[0] != 1 && request[0] != 3
            && request[0] != 2 && request[0] != 7 && request[0] != 9 && request[0] != 10) return 7;
        if (FIXTURE_MODE == 21 && request[0] == 1 && request[1] == 1) {
            const uint8_t status[] = {0x41, 1, source == engine_source ? 0 : 0x81, 7, 1, 0};
            return reply(message, count, source, status, sizeof(status));
        }
        if (FIXTURE_MODE == 21 && request[0] == 1 && request[1] == 0x41) {
            const uint8_t cycle[] = {0x41, 0x41, 0, 7, 1, 1};
            const uint8_t unsupported[] = {0x7f, 1, 0x12};
            return source == engine_source
                ? reply(message, count, source, cycle, sizeof(cycle))
                : reply(message, count, source, unsupported, sizeof(unsupported));
        }
        if (FIXTURE_MODE == 21 && request[0] == 2) {
            if (source == engine_source && request[2] == 0) {
                switch (request[1]) {
                    case 2: {
                        const uint8_t trigger[] = {0x42, 2, 0, 1, 0x33};
                        return reply(message, count, source, trigger, sizeof(trigger));
                    }
                    case 0: {
                        const uint8_t supported[] = {0x42, 0, 0, 0x48, 0, 0, 0};
                        return reply(message, count, source, supported, sizeof(supported));
                    }
                    case 5: {
                        const uint8_t coolant[] = {0x42, 5, 0, 128};
                        return reply(message, count, source, coolant, sizeof(coolant));
                    }
                }
            }
            if (source == other_source && request[1] == 2 && request[2] == 0) {
                const uint8_t empty[] = {0x42, 2, 0, 0, 0};
                return reply(message, count, source, empty, sizeof(empty));
            }
            if (source == other_source && request[1] == 0 && request[2] == 0) {
                const uint8_t supported[] = {0x42, 0, 0, 0x40, 0, 0, 0};
                return reply(message, count, source, supported, sizeof(supported));
            }
            *count = 0;
            return 0x10;
        }
        if (FIXTURE_MODE == 21 && request[0] == 3) {
            if (source == engine_source) {
                const uint8_t empty[] = {0x43, 0};
                return reply(message, count, source, empty, sizeof(empty));
            }
            *count = 0;
            return 0x10;
        }
        if (FIXTURE_MODE == 21 && request[0] == 9 && request[1] == 2) {
            if (source == engine_source) {
                const uint8_t vin[] = "\x49\x02\x01" "JTDBR32E720123456";
                return reply(message, count, source, vin, sizeof(vin) - 1);
            }
            *count = 0;
            return 0x10;
        }
        if (FIXTURE_MODE == 13 && request[0] == 2 && request_len == 3) {
            return freeze_frame_reply(message, count, source);
        }
        if (FIXTURE_MODE == 18 && request[0] == 6) {
            if (request[1] == 0 || request[1] == 0x20 || request[1] == 0x40) {
                const uint8_t bitmap[] = {0x46, request[1],
                    request[1] == 0x40 && monitor_discovery_reads > 1 ? 0 : 0x80,
                    0, 0, request[1] != 0x40};
                if (request[1] != 0x40 || source == engine_source) {
                    const uint32_t length = request[1] == 0x20 && source == other_source ? 3 : sizeof(bitmap);
                    return reply(message, count, source, bitmap, length);
                }
            } else if (source == engine_source) {
                if (request[1] == 1) {
                    const uint8_t result[] = {0x46, 1, 1, 1, 0, 0, 0, 0, 0, 0,
                        1, 2, 1, 0, 40, 0, 0, 0, 60};
                    return reply(message, count, source, result, sizeof(result));
                }
                if (request[1] == 0x21) {
                    const uint8_t result[] = {0x46, 0x21, 1, 1, 0, 80, 0, 0, 0, 60};
                    return reply(message, count, source, result, sizeof(result));
                }
                if (request[1] == 0x41) {
                    const uint8_t result[] = {0x46, 0x41, 1, 0xf0, 0, 40, 0, 0, 0, 60};
                    return reply(message, count, source, result, sizeof(result));
                }
            }
            *count = 0;
            return 0x10;
        }
        if (FIXTURE_MODE == 14 && request[0] == 1 && request_len == 2) {
            if (request[1] == 0) {
                const uint8_t bitmap[] = {0x41, 0x00, 0x80, 0, 0, 0};
                const uint32_t length = source == engine_source ? sizeof(bitmap) : 3;
                return reply(message, count, source, bitmap, length);
            }
            if ((request[1] == 1 && source == (readiness_reads == 1 ? other_source : engine_source))
                || (request[1] == 0x41 && source == engine_source)) {
                const uint8_t status[] = {0x41, request[1], 0, 7, 0x21, 0x20};
                return reply(message, count, source, status, sizeof(status));
            }
            *count = 0;
            return 0x10;
        }
        if (FIXTURE_MODE == 15 && request[0] == 1 && request_len == 2) {
            if (request[1] == 0 && health_bitmap_reads != 2) {
                const uint8_t bitmap[] = {0x41, 0, 8, 0x10, 0, 0};
                const uint8_t rejected[] = {0x7f, 1, 0x12};
                if (health_bitmap_reads == 4 || (health_bitmap_reads == 5 && source != engine_source)) {
                    return reply(message, count, source, rejected, sizeof(rejected));
                }
                if (health_bitmap_reads == 3) {
                    return reply(message, count, source, bitmap, 3);
                }
                if (health_bitmap_reads == 6 && source != engine_source) {
                    *count = 0;
                    return 0x10;
                }
                return reply(message, count, source, bitmap, sizeof(bitmap));
            }
            if (request[1] == 0x0c && source == engine_source && health_rpm_reads != 2) {
                const uint8_t rpm[] = {0x41, 0x0c, 0x0c, 0x80};
                return reply(message, count, source, rpm, sizeof(rpm));
            }
            if (request[1] == 0x0f && source == engine_source) {
                const uint8_t rejected[] = {0x7f, 1, 0x12};
                return reply(message, count, source, rejected, sizeof(rejected));
            }
            *count = 0;
            return 0x10;
        }
        if (FIXTURE_MODE == 16 && request_len == 1) {
            if (request[0] == 3 && (source == engine_source || stored_code_reads == 1)) {
                const uint8_t stored[] = {0x43, 1, source == engine_source ? 1 : 3, source == engine_source ? 0x33 : 0};
                const uint8_t empty[] = {0x43, 0};
                return stored_code_reads == 1
                    ? reply(message, count, source, stored, sizeof(stored))
                    : reply(message, count, source, empty, sizeof(empty));
            }
            if (request[0] == 7 && source == engine_source) {
                const uint8_t pending[] = {0x47, 0};
                return reply(message, count, source, pending, sizeof(pending));
            }
            if (request[0] == 10) {
                const uint8_t permanent[] = {0x4a, 1, 3, 0};
                const uint8_t unsupported[] = {0x7f, 10, 0x11};
                return source == engine_source
                    ? reply(message, count, source, unsupported, sizeof(unsupported))
                    : reply(message, count, source, permanent, sizeof(permanent));
            }
            *count = 0;
            return 0x10;
        }
        if (FIXTURE_MODE == 2 && request[0] == 4) Sleep(60000);
        if (FIXTURE_MODE == 3 && request[0] == 1 && request[1] == 0x0c) ExitProcess(77);
        if (FIXTURE_MODE == 5 && request[0] == 1 && request[1] == 0x0c && index == 1) {
            protocol = 0;
            *count = 0;
            return 2;
        }
        if (request[0] == 3) {
            if (source == 0x18DAF145u) {
                const uint8_t bytes[] = {0x7f, 3, 0x78};
                return reply(message, count, source, bytes, sizeof(bytes));
            }
            const uint8_t bytes[] = {0x43, 1, 1, 0x33};
            return reply(message, count, source, bytes, sizeof(bytes));
        }
        if (request[0] == 7) {
            const uint8_t bytes[] = {0x47, 0};
            reply(message, count, source, bytes, sizeof(bytes));
            if (source == 0x18DAF145u) message->rx_status |= PADDING_ERROR;
            return 0;
        }
        if (request[0] == 1 && request[1] == 0) {
            if (FIXTURE_MODE == 7 && source == 0x18DAF145u) {
                *count = 0;
                return 0x10;
            }
            if (FIXTURE_MODE == 6) {
                const uint8_t engine[] = {0x41, 0, 0x88, 0x10, 0, 1};
                const uint8_t transmission[] = {0x41, 0, 0, 8, 0, 0};
                return reply(message, count, source, source == 0x18DAF127u ? engine : transmission, sizeof(engine));
            }
            if (FIXTURE_MODE >= 10) {
                const uint8_t engine[] = {0x41, 0, 0x08, 0x18, 0, 0};
                const uint8_t transmission[] = {0x41, 0, 0, 0x10, 0, 0};
                if (FIXTURE_MODE == 20 && source == other_source) {
                    return reply(message, count, source, transmission, 3);
                }
                return reply(message, count, source, source == engine_source ? engine : transmission, sizeof(engine));
            }
            const uint8_t bytes[] = {0x41, 0, 0x80, 0x10, 0, 0};
            return reply(message, count, source, bytes, sizeof(bytes));
        }
        if (FIXTURE_MODE == 6 && request[0] == 1 && (request[1] == 0x20 || request[1] == 0x40)) {
            if (source == 0x18DAF145u) {
                *count = 0;
                return 0x10;
            }
            const uint8_t next[] = {0x41, 0x20, 0x80, 0, 0, 1};
            const uint8_t last[] = {0x41, 0x40, 0x40, 0, 0, 0};
            return reply(message, count, source, request[1] == 0x20 ? next : last, sizeof(next));
        }
        if (request[0] == 1 && request_len > 2) {
            const uint8_t group[] = {1, 5, 12, 13};
            const uint8_t engine[] = {0x41, 13, 43, 5, 128, 12, 0x0c, 0x80};
            const uint8_t transmission[] = {0x41, 12, 0x0f, 0xa0};
            if (request_len == sizeof(group) && memcmp(request, group, sizeof(group)) == 0) {
                if (source == 0x18DAF127u) {
                    return reply(message, count, source, engine, sizeof(engine));
                }
                if (FIXTURE_MODE != 12) {
                    const uint32_t length = sizeof(transmission) - (FIXTURE_MODE == 11);
                    return reply(message, count, source, transmission, length);
                }
            }
            *count = 0;
            return 0x10;
        }
        if (FIXTURE_MODE >= 10 && request[0] == 1) {
            if (source == 0x18DAF127u) {
                if (request[1] == 5 || request[1] == 13) {
                    const uint8_t bytes[] = {0x41, request[1], request[1] == 5 ? 128 : 43};
                    return reply(message, count, source, bytes, sizeof(bytes));
                }
                if (request[1] == 12) {
                    const uint8_t bytes[] = {0x41, 12, 0x10, 0x00};
                    return reply(message, count, source, bytes, sizeof(bytes));
                }
            } else if (request[1] == 12 && FIXTURE_MODE != 12
                && (FIXTURE_MODE != 20 || live_rpm_requests == 1)) {
                const uint8_t bytes[] = {0x41, 12, 0x0f, 0xa0};
                return reply(message, count, source, bytes, sizeof(bytes));
            }
            *count = 0;
            return 0x10;
        }
        if (request[0] == 1 && request[1] == 0x0c) {
            const uint8_t bytes[] = {0x41, 0x0c, 0x0c, 0x80};
            return reply(message, count, source, bytes, sizeof(bytes));
        }
        const uint8_t bytes[] = {(uint8_t)(request[0] + 0x40)};
        return reply(message, count, source, bytes, sizeof(bytes));
    }
    *count = 0;
    return 0x10;
}

uint32_t API PassThruGetLastError(char *buffer) {
    strcpy(buffer, FIXTURE_MODE == 25 || FIXTURE_MODE == 27 ? "Only sent 0 of 1 messages" : "scanner fixture");
    return 0;
}
