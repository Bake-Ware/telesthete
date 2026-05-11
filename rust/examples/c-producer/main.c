/* Telesthete C producer reference — sends one dmabuf-shaped frame
 * over AF_UNIX. Uses a memfd as a stand-in for a real dmabuf fd so
 * the example compiles + runs without a GPU; a real producer would
 * pass a dmabuf fd from EGL_EXT_image_dma_buf_export, VK_EXT_external
 * _memory_fd, or a similar source.
 *
 * Build: see Makefile in this directory.
 * Run:   ./tlt-c-producer
 *
 * Exit: 0 on success, non-zero on error. If no consumer is bound to
 * the target socket the send call returns TLT_ERR_SEND and the program
 * exits non-zero — that's expected when running standalone. Pair with
 * the `consumer-rs` example to see a full round-trip.
 */
#define _GNU_SOURCE  /* memfd_create + MFD_CLOEXEC on glibc */
#include <fcntl.h>
#include <sys/mman.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

#include "telesthete.h"

/* DRM fourcc 'XR24' — packed BGRX 8:8:8:8 little-endian. */
#define FOURCC_XR24  0x34325258u
/* DRM_FORMAT_MOD_LINEAR. */
#define MOD_LINEAR   0ull

int main(void) {
    const uint32_t W = 1920, H = 1080;
    const uint32_t STRIDE = W * 4;
    const size_t SIZE = (size_t)STRIDE * H;

    /* Stand-in for a dmabuf fd: memfd of the right size. The wire
     * accepts any fd the kernel can pass via SCM_RIGHTS; the consumer
     * decides whether to actually import it as a GPU buffer. */
    int fd = memfd_create("tlt-demo", MFD_CLOEXEC);
    if (fd < 0) { perror("memfd_create"); return 1; }
    if (ftruncate(fd, SIZE) != 0) { perror("ftruncate"); close(fd); return 1; }

    TltProducer *p = tlt_open(NULL);
    if (!p) {
        fprintf(stderr, "tlt_open returned NULL\n");
        close(fd);
        return 2;
    }

    TltPlane plane = { .offset = 0, .stride = STRIDE, .fd_index = 0 };
    int fds[1] = { fd };

    int32_t rc = tlt_send_dmabuf(
        p,
        /* channel_id = */ 1,
        /* frame_id   = */ 1,
        /* flags      = */ TLT_FLAG_KEYFRAME | TLT_FLAG_END_FRAME,
        W, H,
        FOURCC_XR24,
        MOD_LINEAR,
        &plane, 1,
        fds, 1
    );
    fprintf(stderr, "tlt_send_dmabuf -> %d\n", rc);

    tlt_close(p);
    close(fd);
    return rc == TLT_OK ? 0 : 3;
}
