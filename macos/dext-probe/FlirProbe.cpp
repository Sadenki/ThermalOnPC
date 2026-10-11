#include <os/log.h>
#include <DriverKit/IOLib.h>
#include <DriverKit/IOBufferMemoryDescriptor.h>
#include <DriverKit/OSCollections.h>
#include <stdio.h>
#include <USBDriverKit/IOUSBHostDevice.h>
#include <USBDriverKit/IOUSBHostInterface.h>
#include <USBDriverKit/IOUSBHostPipe.h>
#include <USBDriverKit/USBDriverKitDefs.h>
#include <USBDriverKit/IOUSBHostFamilyDefinitions.h>
#include "FlirProbe.h"

// Every message is also appended to a report published as the "FlirProbeLog"
// registry property (read it with: ioreg -r -c IOUserService -l).
static char gReport[8192];
static size_t gReportLen;
#define LOG(fmt, ...) do { \
    os_log(OS_LOG_DEFAULT, "FlirProbe: " fmt, ##__VA_ARGS__); \
    if (gReportLen < sizeof(gReport) - 1) { \
        int _n = snprintf(gReport + gReportLen, sizeof(gReport) - gReportLen, fmt " | ", ##__VA_ARGS__); \
        if (_n > 0) gReportLen += (size_t)_n; \
        if (gReportLen > sizeof(gReport) - 1) gReportLen = sizeof(gReport) - 1; \
    } \
} while (0)

struct FlirProbe_IVars {
    IOUSBHostDevice *device;
    IOUSBHostInterface *ifaces[3];
};

bool FlirProbe::init()
{
    if (!super::init()) return false;
    ivars = IONewZero(FlirProbe_IVars, 1);
    return ivars != nullptr;
}

void FlirProbe::free()
{
    IOSafeDeleteNULL(ivars, FlirProbe_IVars, 1);
    super::free();
}

// Tries a short read on `ep` through a freshly copied pipe.
static void probePipe(IOUSBHostInterface *iface, uint8_t ep, IOBufferMemoryDescriptor *buf, const char *when)
{
    IOUSBHostPipe *pipe = nullptr;
    kern_return_t r = iface->CopyPipe(ep, &pipe);
    if (r != kIOReturnSuccess || !pipe) {
        LOG("%s: CopyPipe(0x%02x) failed 0x%x", when, ep, r);
        return;
    }
    uint32_t n = 0;
    r = pipe->IO(buf, 16384, &n, 100);
    LOG("%s: 0x%02x IO -> 0x%x, %u bytes", when, ep, r, n);
    pipe->release();
}

static void publish(IOService *svc)
{
    OSDictionary *d = OSDictionary::withCapacity(1);
    OSString *str = OSString::withCString(gReport);
    if (d && str) {
        d->setObject("FlirProbeLog", str);
        kern_return_t r = svc->SetProperties(d);
        os_log(OS_LOG_DEFAULT, "FlirProbe: SetProperties -> 0x%x", r);
        IOLog("FlirProbe REPORT: %s\n", gReport);
    }
    if (str) str->release();
    if (d) d->release();
}

kern_return_t IMPL(FlirProbe, Start)
{
    kern_return_t r = Start(provider, SUPERDISPATCH);
    if (r != kIOReturnSuccess) return r;
    LOG("start");

    IOUSBHostDevice *dev = OSDynamicCast(IOUSBHostDevice, provider);
    if (!dev) { LOG("provider is not an IOUSBHostDevice"); return kIOReturnError; }
    ivars->device = dev;
    r = dev->Open(this, 0, 0);
    LOG("device Open -> 0x%x", r);
    if (r != kIOReturnSuccess) return r;
    uint8_t cfgValue = 1;
    const IOUSBConfigurationDescriptor *cd = dev->CopyConfigurationDescriptor((uint8_t)0);
    if (cd) {
        cfgValue = cd->bConfigurationValue;
        LOG("config 0: value=%u interfaces=%u total=%u", cd->bConfigurationValue, cd->bNumInterfaces, cd->wTotalLength);
        IOUSBHostFreeDescriptor(cd);
    } else {
        LOG("CopyConfigurationDescriptor(0) failed");
    }
    r = dev->SetConfiguration(cfgValue, false);
    LOG("SetConfiguration(%u) -> 0x%x", cfgValue, r);

    for (int attempt = 0; attempt < 20 && (!ivars->ifaces[1] || !ivars->ifaces[2]); attempt++) {
    if (attempt) IOSleep(100);
    uintptr_t it = 0;
    r = dev->CreateInterfaceIterator(&it);
    if (attempt == 0 || r) LOG("CreateInterfaceIterator -> 0x%x (attempt %d)", r, attempt);
    IOUSBHostInterface *iface = nullptr;
    while (r == kIOReturnSuccess && dev->CopyInterface(it, &iface) == kIOReturnSuccess && iface) {
        const IOUSBConfigurationDescriptor *cfg = iface->CopyConfigurationDescriptor();
        const IOUSBInterfaceDescriptor *id = cfg ? iface->GetInterfaceDescriptor(cfg) : nullptr;
        uint8_t num = id ? id->bInterfaceNumber : 0xff;
        if (cfg) IOUSBHostFreeDescriptor(cfg);
        kern_return_t o = iface->Open(this, 0, 0);
        LOG("interface %u Open -> 0x%x", num, o);
        if (num < 3 && !ivars->ifaces[num]) ivars->ifaces[num] = iface;
        else iface->release();
        iface = nullptr;
    }
    if (r == kIOReturnSuccess) dev->DestroyInterfaceIterator(it);
    }
    if (!ivars->ifaces[1] || !ivars->ifaces[2]) { LOG("interfaces 1/2 missing"); publish(this); RegisterService(); return kIOReturnSuccess; }

    IOBufferMemoryDescriptor *in = nullptr, *two = nullptr;
    dev->CreateIOBuffer(kIOMemoryDirectionIn, 16384, &in);
    dev->CreateIOBuffer(kIOMemoryDirectionOutIn, 2, &two);
    if (two) {
        IOAddressSegment seg;
        two->GetAddressRange(&seg);
        memset((void *)seg.address, 0, 2);
        two->SetLength(2);
    }
    probePipe(ivars->ifaces[2], 0x85, in, "initial");

    // Pipe copied before any switch, to see whether it survives one.
    IOUSBHostPipe *early85 = nullptr;
    ivars->ifaces[2]->CopyPipe(0x85, &early85);

    struct { uint16_t value, iface; bool data; } steps[] = {{0, 2, false}, {0, 1, false}, {1, 1, false}, {1, 2, true}};
    for (auto &s : steps) {
        uint16_t done = 0;
        IOMemoryDescriptor *d = s.data ? two : nullptr;
        uint16_t len = s.data ? 2 : 0;
        r = dev->DeviceRequest(this, 0x01, 0x0b, s.value, s.iface, len, d, &done, 500);
        LOG("dev SET_INTERFACE v=%u i=%u -> 0x%x", s.value, s.iface, r);
        if (r == kIOReturnNotPermitted) {
            r = ivars->ifaces[s.iface]->DeviceRequest(0x01, 0x0b, s.value, s.iface, len, d, &done, 500);
            LOG("iface SET_INTERFACE v=%u i=%u -> 0x%x", s.value, s.iface, r);
        }
        if (r == kIOReturnNotPermitted) {
            r = ivars->ifaces[s.iface]->SelectAlternateSetting((uint8_t)s.value);
            LOG("SelectAlternateSetting(%u) i=%u -> 0x%x", s.value, s.iface, r);
        }
        probePipe(ivars->ifaces[1], 0x83, in, "after");
        probePipe(ivars->ifaces[2], 0x85, in, "after");
    }
    if (early85) {
        uint64_t total = 0; kern_return_t last = 0;
        for (int i = 0; i < 20; i++) {
            uint32_t n = 0;
            last = early85->IO(in, 16384, &n, 100);
            if (last == kIOReturnSuccess) total += n;
            else if (last == kUSBHostReturnPipeStalled) early85->ClearStall(true);
        }
        LOG("early 0x85 pipe: %llu bytes, last 0x%x", total, last);
        early85->release();
    }

    // Read 0x85 for ~5 s.
    IOUSBHostPipe *pipe = nullptr;
    r = ivars->ifaces[2]->CopyPipe(0x85, &pipe);
    if (r == kIOReturnSuccess && pipe) {
        uint64_t total = 0; uint32_t errors = 0, first = 1;
        for (int i = 0; i < 50; i++) {
            uint32_t n = 0;
            kern_return_t e = pipe->IO(in, 16384, &n, 100);
            if (e == kIOReturnSuccess) {
                if (first && n >= 8) {
                    IOAddressSegment seg; in->GetAddressRange(&seg);
                    const uint8_t *b = (const uint8_t *)seg.address;
                    LOG("first 0x85 data: %02x %02x %02x %02x %02x %02x %02x %02x", b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]);
                    first = 0;
                }
                total += n;
            } else if (e != kIOReturnTimeout && e != kUSBHostReturnPipeStalled) {
                if (errors++ == 0) LOG("0x85 IO error 0x%x", e);
                IOSleep(100);
            } else if (e != kIOReturnTimeout) {
                pipe->ClearStall(true);
                if (errors++ == 0) LOG("0x85 stalled");
            }
        }
        LOG("RESULT 0x85: %llu bytes, %u errors", total, errors);
        pipe->release();
    } else {
        LOG("RESULT 0x85: no pipe (0x%x)", r);
    }
    if (in) in->release();
    if (two) two->release();

    publish(this);
    RegisterService();
    return kIOReturnSuccess;
}

kern_return_t IMPL(FlirProbe, Stop)
{
    LOG("stop");
    for (auto &i : ivars->ifaces) {
        if (i) { i->Close(this, 0); i->release(); i = nullptr; }
    }
    if (ivars->device) ivars->device->Close(this, 0);
    return Stop(provider, SUPERDISPATCH);
}
