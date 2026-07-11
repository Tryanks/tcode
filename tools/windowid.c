// Prints "windowid ownername windowname" for every on-screen normal-layer
// window whose owner name contains argv[1] (case-sensitive).
// Build: clang tools/windowid.c -o target/windowid -framework CoreGraphics -framework CoreFoundation
#include <CoreFoundation/CoreFoundation.h>
#include <CoreGraphics/CoreGraphics.h>
#include <stdio.h>
#include <string.h>

int main(int argc, char **argv) {
    const char *needle = argc > 1 ? argv[1] : "";
    CFArrayRef list = CGWindowListCopyWindowInfo(
        kCGWindowListOptionAll, kCGNullWindowID);
    if (!list) return 1;
    for (CFIndex i = 0; i < CFArrayGetCount(list); i++) {
        CFDictionaryRef w = CFArrayGetValueAtIndex(list, i);
        CFNumberRef layerRef = CFDictionaryGetValue(w, kCGWindowLayer);
        int layer = -1;
        if (layerRef) CFNumberGetValue(layerRef, kCFNumberIntType, &layer);
        if (layer != 0) continue;
        CFStringRef owner = CFDictionaryGetValue(w, kCGWindowOwnerName);
        char ownerBuf[256] = {0};
        if (owner) CFStringGetCString(owner, ownerBuf, sizeof(ownerBuf), kCFStringEncodingUTF8);
        if (needle[0] && !strstr(ownerBuf, needle)) continue;
        CFNumberRef numRef = CFDictionaryGetValue(w, kCGWindowNumber);
        long num = 0;
        if (numRef) CFNumberGetValue(numRef, kCFNumberLongType, &num);
        CFStringRef name = CFDictionaryGetValue(w, kCGWindowName);
        char nameBuf[256] = {0};
        if (name) CFStringGetCString(name, nameBuf, sizeof(nameBuf), kCFStringEncodingUTF8);
        printf("%ld %s %s\n", num, ownerBuf, nameBuf);
    }
    CFRelease(list);
    return 0;
}
