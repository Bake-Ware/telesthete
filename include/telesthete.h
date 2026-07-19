/* telesthete.h — C ABI for the Rook wire transport (client side).
 * Mirrors spatial_proto.h conventions: opaque handle, borrowed buffers, explicit codes.
 * See spec/TELESTHETE.md §7. */
#ifndef TELESTHETE_H
#define TELESTHETE_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#define TEL_OK 0
#define TEL_ERR_NULL (-1)
#define TEL_ERR_CONNECT (-2)
#define TEL_ERR_DEAD (-3)

typedef struct TelClient TelClient;

/* Per-message receive callback. `msg`/`len` are valid only during the call. */
typedef void (*TelRecvCb)(void *user, uint8_t channel, const uint8_t *msg, size_t len);

/* Connect + PSK handshake. Returns handle or NULL. psk_path may be NULL (default config). */
TelClient *tel_client_connect(const char *host, uint16_t tcp_port, const char *psk_path);

/* Send one spatial-proto message on `channel`; routes TCP/UDP internally. */
int32_t tel_client_send(TelClient *c, uint8_t channel, const uint8_t *msg, size_t len);

/* Drain all ready messages (invokes cb per message). Returns count, or negative on dead. */
int32_t tel_client_poll(TelClient *c, TelRecvCb cb, void *user);

/* 1 if alive, 0 otherwise. */
int32_t tel_client_connected(const TelClient *c);

/* Free the handle. */
void tel_client_free(TelClient *c);

#ifdef __cplusplus
}
#endif
#endif /* TELESTHETE_H */
