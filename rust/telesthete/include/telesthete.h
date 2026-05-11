#ifndef TELESTHETE_H
#define TELESTHETE_H

#pragma once

#include <stdint.h>
#include <stddef.h>

/**
 * Stream flags (mirror of [`telesthete::wire::StreamFlags`]).
 * Producers OR these together for the `flags` argument of
 * [`tlt_send_dmabuf`].
 */
#define TLT_FLAG_INIT 1

#define TLT_FLAG_KEYFRAME 2

#define TLT_FLAG_END_FRAME 4

#define TLT_FLAG_FRAGMENT_CONT 8

#define TLT_FLAG_DMABUF 16

#define TLT_FLAG_WITH_FENCE 32

#define TLT_FLAG_REUSE 64

/**
 * Error return codes. `0` = success.
 */
#define TLT_OK 0

#define TLT_ERR_NULL_HANDLE -1

#define TLT_ERR_HEADER_WRITE -2

#define TLT_ERR_DESCRIPTOR_WRITE -3

#define TLT_ERR_PLANE_COUNT -4

#define TLT_ERR_FD_COUNT -5

#define TLT_ERR_SEND -10

/**
 * Maximum plane count supported by the wire (mirror of
 * [`telesthete::wire::MAX_PLANES`]).
 */
#define TLT_MAX_PLANES 4

/**
 * Maximum fd count per packet — 4 planes + 1 optional fence.
 */
#define TLT_MAX_FDS 5

/**
 * Opaque producer handle. Treat as `void*` from C.
 */
typedef struct TltProducer TltProducer;

/**
 * Plane descriptor. Matches the on-wire `dmabuf` plane layout in
 * SPEC.md §5.4.
 */
typedef struct TltPlane {
  uint32_t offset;
  uint32_t stride;
  /**
   * Index into the `fds` array passed to [`tlt_send_dmabuf`].
   */
  uint8_t fd_index;
} TltPlane;

/**
 * Open a producer using the default local PSK and the default target
 * resolution (XDG_RUNTIME_DIR/telesthete/<band>.sock with /tmp
 * fallback). Returns NULL on failure.
 *
 * `target_path` may be NULL to use the default; otherwise a
 * nul-terminated UTF-8 filesystem path overrides the target.
 *
 * # Safety
 * `target_path` if non-NULL must point to a valid nul-terminated
 * C string for the duration of this call. The returned pointer must
 * be freed with [`tlt_close`].
 */
struct TltProducer *tlt_open(const char *target_path);

/**
 * Open with a custom PSK. `psk` may be empty (psk_len = 0) for a
 * fixed local profile; see SPEC.md §3.4. `target_path` follows the
 * same NULL-means-default rule as [`tlt_open`].
 *
 * # Safety
 * `psk` must point to `psk_len` valid bytes. `target_path` rules
 * match [`tlt_open`].
 */
struct TltProducer *tlt_open_with_psk(const uint8_t *psk,
                                      uintptr_t psk_len,
                                      const char *target_path);

/**
 * Send a dmabuf-backed frame.
 *
 * `flags` is the bitwise OR of `TLT_FLAG_*` constants. The `DMABUF`
 * flag is forced on by this function — callers should not include
 * it themselves but it is not an error to.
 *
 * `planes` describes the on-wire plane table; `fds` is the array
 * the kernel duplicates via SCM_RIGHTS. With `TLT_FLAG_WITH_FENCE`,
 * the last fd in `fds` is the sync_file release fence.
 *
 * # Safety
 * `handle` must be a valid pointer from [`tlt_open`] /
 * [`tlt_open_with_psk`]. `planes` must point to `plane_count` valid
 * `TltPlane` structs. `fds` must point to `fd_count` valid file
 * descriptors that remain live for the call's duration; the kernel
 * duplicates them during sendmsg, the caller retains ownership.
 */
int32_t tlt_send_dmabuf(struct TltProducer *handle,
                        uint16_t channel_id,
                        uint32_t frame_id,
                        uint32_t flags,
                        uint32_t width,
                        uint32_t height,
                        uint32_t fourcc,
                        uint64_t modifier,
                        const struct TltPlane *planes,
                        uint8_t plane_count,
                        const int *fds,
                        uint8_t fd_count);

/**
 * Tear down a producer handle. NULL is a no-op.
 *
 * # Safety
 * `handle` must be a pointer from [`tlt_open`] / [`tlt_open_with_psk`]
 * and not previously freed.
 */
void tlt_close(struct TltProducer *handle);

#endif  /* TELESTHETE_H */
