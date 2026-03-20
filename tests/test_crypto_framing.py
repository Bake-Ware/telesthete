"""
Integration test for crypto + framing

Tests the full encrypt-pack-unpack-decrypt cycle
"""

import sys
import os

# Add parent directory to path so we can import telesthete
sys.path.insert(0, os.path.join(os.path.dirname(__file__), '..'))

from telesthete.protocol.crypto import BandCrypto
from telesthete.protocol.framing import pack_packet, unpack_packet, ChannelType


def test_full_packet_flow():
    """
    Test the complete flow:
    1. Encrypt a payload
    2. Pack into wire format
    3. Unpack from wire format
    4. Decrypt payload
    """
    print("Testing full packet flow: encrypt -> pack -> unpack -> decrypt")
    print("=" * 60)

    # Setup
    psk = "test-passphrase-123"
    crypto = BandCrypto(psk)

    # Original data
    channel_type = ChannelType.STREAM
    channel_id = 42
    sequence = 1000
    plaintext = b"Hello from Machine A!"
    associated_data = bytes([channel_type, channel_id >> 8, channel_id & 0xFF])

    print(f"\nOriginal data:")
    print(f"  Band ID: {crypto.band_id.hex()}")
    print(f"  Channel type: {channel_type} (STREAM)")
    print(f"  Channel ID: {channel_id}")
    print(f"  Sequence: {sequence}")
    print(f"  Plaintext: {plaintext}")
    print(f"  AAD: {associated_data.hex()}")

    # Step 1: Encrypt
    ciphertext = crypto.encrypt(sequence, plaintext, associated_data)
    print(f"\nEncrypted:")
    print(f"  Ciphertext length: {len(ciphertext)} bytes")
    print(f"  Ciphertext: {ciphertext.hex()[:64]}...")

    # Step 2: Pack
    packet_bytes = pack_packet(
        crypto.band_id,
        channel_type,
        channel_id,
        sequence,
        ciphertext
    )
    print(f"\nPacked:")
    print(f"  Packet length: {len(packet_bytes)} bytes")
    print(f"  Packet (hex): {packet_bytes.hex()[:64]}...")

    # Step 3: Simulate network transmission
    print(f"\n>>> Simulating network transmission >>>")
    received_bytes = packet_bytes  # In real life, this goes over UDP

    # Step 4: Unpack
    packet = unpack_packet(received_bytes)
    print(f"\nUnpacked:")
    print(f"  Band ID: {packet.band_id.hex()}")
    print(f"  Channel type: {packet.channel_type}")
    print(f"  Channel ID: {packet.channel_id}")
    print(f"  Sequence: {packet.sequence}")
    print(f"  Ciphertext length: {len(packet.ciphertext)} bytes")

    # Step 5: Verify band_id matches (peer would check this)
    if packet.band_id != crypto.band_id:
        print("\nERROR: Band ID mismatch! Packet is for different Band.")
        return False

    # Step 6: Decrypt
    aad_received = bytes([packet.channel_type, packet.channel_id >> 8, packet.channel_id & 0xFF])
    decrypted = crypto.decrypt(packet.sequence, packet.ciphertext, aad_received)
    print(f"\nDecrypted:")
    print(f"  Plaintext: {decrypted}")

    # Verify
    if decrypted == plaintext:
        print(f"\n[PASS] SUCCESS: Decrypted plaintext matches original!")
        return True
    else:
        print(f"\n[FAIL] FAILURE: Decrypted plaintext does NOT match!")
        print(f"  Expected: {plaintext}")
        print(f"  Got: {decrypted}")
        return False


def test_wrong_psk():
    """
    Test that a peer with wrong PSK can't decrypt
    """
    print("\n\nTesting wrong PSK rejection")
    print("=" * 60)

    # Sender
    sender_psk = "correct-passphrase"
    sender_crypto = BandCrypto(sender_psk)

    # Receiver with WRONG PSK
    receiver_psk = "wrong-passphrase"
    receiver_crypto = BandCrypto(receiver_psk)

    print(f"Sender band_id: {sender_crypto.band_id.hex()}")
    print(f"Receiver band_id: {receiver_crypto.band_id.hex()}")

    # Sender encrypts and packs
    plaintext = b"Secret message"
    sequence = 1
    aad = b"\x01\x00\x00"  # channel_type=1, channel_id=0
    ciphertext = sender_crypto.encrypt(sequence, plaintext, aad)
    packet_bytes = pack_packet(sender_crypto.band_id, 1, 0, sequence, ciphertext)

    # Receiver unpacks
    packet = unpack_packet(packet_bytes)

    # Receiver checks band_id
    if packet.band_id != receiver_crypto.band_id:
        print(f"\n[PASS] SUCCESS: Receiver correctly rejected packet (band_id mismatch)")
        print(f"  Receiver would drop this packet without attempting decryption")
        return True
    else:
        print(f"\n[FAIL] FAILURE: Band IDs matched (should not happen with different PSKs)")
        return False


def test_packet_corruption():
    """
    Test that corrupted packets are detected
    """
    print("\n\nTesting packet corruption detection")
    print("=" * 60)

    psk = "test-passphrase"
    crypto = BandCrypto(psk)

    # Create valid packet
    plaintext = b"Important data"
    sequence = 5
    aad = b"\x01\x00\x00"
    ciphertext = crypto.encrypt(sequence, plaintext, aad)
    packet_bytes = pack_packet(crypto.band_id, 1, 0, sequence, ciphertext)

    # Corrupt a byte in the ciphertext
    corrupted = bytearray(packet_bytes)
    corrupted[-10] ^= 0xFF  # Flip bits in ciphertext
    corrupted = bytes(corrupted)

    print(f"Original packet: {len(packet_bytes)} bytes")
    print(f"Corrupted byte at position {len(corrupted) - 10}")

    # Try to decrypt
    packet = unpack_packet(corrupted)
    try:
        decrypted = crypto.decrypt(packet.sequence, packet.ciphertext, aad)
        print(f"\n[FAIL] FAILURE: Corrupted packet was accepted! Decrypted: {decrypted}")
        return False
    except Exception as e:
        print(f"\n[PASS] SUCCESS: Corrupted packet was rejected")
        print(f"  Error: {type(e).__name__}: {e}")
        return True


if __name__ == "__main__":
    print("Telesthete Crypto + Framing Integration Tests")
    print("=" * 60)

    results = []
    results.append(("Full packet flow", test_full_packet_flow()))
    results.append(("Wrong PSK rejection", test_wrong_psk()))
    results.append(("Corruption detection", test_packet_corruption()))

    print("\n\n" + "=" * 60)
    print("TEST SUMMARY")
    print("=" * 60)

    for name, passed in results:
        status = "[PASS]" if passed else "[FAIL]"
        print(f"{status}: {name}")

    all_passed = all(result for _, result in results)
    print(f"\n{'All tests passed!' if all_passed else 'Some tests failed!'}")
    sys.exit(0 if all_passed else 1)
