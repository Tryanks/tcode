#ifndef Tcode_BridgingHeader_h
#define Tcode_BridgingHeader_h

#include <stddef.h>
#include <stdint.h>

typedef struct GpuiIosTouch {
    uint64_t identifier;
    double x;
    double y;
    double predicted_x;
    double predicted_y;
    float force;
    uint8_t has_prediction;
    uint8_t has_force;
} GpuiIosTouch;

void tcode_ios_start(void);

void gpui_ios_init(void);
uint8_t gpui_ios_attach_view(void *view, float width, float height, float scale,
                             uint8_t dark);
void gpui_ios_detach_view(void *view);
void gpui_ios_request_frame(void);

void gpui_ios_touches_began(const GpuiIosTouch *touches, size_t count);
void gpui_ios_touches_moved(const GpuiIosTouch *touches, size_t count);
void gpui_ios_touches_ended(const GpuiIosTouch *touches, size_t count);
void gpui_ios_touches_cancelled(const GpuiIosTouch *touches, size_t count);

void gpui_ios_resize(float width, float height, float scale);
void gpui_ios_scale_factor_changed(float scale);
void gpui_ios_safe_area_changed(float top, float right, float bottom,
                                float left);
void gpui_ios_keyboard_frame_changed(float height);
void gpui_ios_appearance_changed(uint8_t dark);

void gpui_ios_lifecycle_active(void);
void gpui_ios_lifecycle_inactive(void);
void gpui_ios_lifecycle_background(void);
void gpui_ios_lifecycle_foreground(void);
void gpui_ios_memory_warning(void);

void gpui_ios_insert_text(const uint8_t *bytes, size_t length);
void gpui_ios_set_marked_text(const uint8_t *bytes, size_t length,
                              size_t selection_start,
                              size_t selection_length);
void gpui_ios_unmark_text(void);
void gpui_ios_delete_backward(void);
void gpui_ios_key_event(const uint8_t *key_bytes, size_t key_length,
                        const uint8_t *character_bytes,
                        size_t character_length, uint32_t modifier_bits,
                        uint8_t down, uint8_t repeat);
void gpui_ios_open_url_received(const uint8_t *bytes, size_t length);

#endif
