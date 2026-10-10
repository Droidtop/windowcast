# Draft upstream issue: pairing fails with "Incorrect PIN" for about one salt in sixteen

**For:** moonlight-qt (seen with 6.2.0, Arch Linux's package; the code
below is `app/backend/nvpairingmanager.cpp` in the source of that
release). **Status:** draft for the owner to review and file; not filed.

## Title

Pairing randomly reports "Incorrect PIN" for the correct PIN: the AES key
is truncated at its first zero byte

## Summary

`NvPairingManager::pair` builds the PIN key by constructing a `QByteArray`
from the hash's `constData()`:

```cpp
QByteArray aesKey = QCryptographicHash::hash(saltedPin, hashAlgo).constData();
aesKey.truncate(16);
```

(`nvpairingmanager.cpp:230-231`). `QByteArray(const char *)` copies up to
the first NUL, so when the first 16 bytes of `SHA-256(salt + PIN)` contain a
zero byte, the key is shorter than 16 bytes. `encrypt()` and `decrypt()`
then hand `key.data()` to `EVP_EncryptInit`/`EVP_DecryptInit` for
AES-128, which reads 16 bytes: the key's bytes, the terminating NUL and
whatever follows in memory. The client challenge is encrypted with a key
the host cannot know, the host's answer does not verify, and Moonlight
reports "Incorrect PIN" (`nvpairingmanager.cpp:334-338`).

The salt is 16 random bytes, so the chance that 16 bytes of the hash hold
at least one zero is 1 - (255/256)^16, about 6%: roughly one pairing
attempt in sixteen fails with the right PIN, against any host. Pairing
again (new salt) normally works, so users see it as an occasional
mistyped PIN.

## Reproduction

1. Pair `moonlight pair HOST --pin 1234` against any GameStream host many
   times (we used a test host that enters the PIN automatically; Sunshine
   with its PIN entry works the same way).
2. With the Qt info log on, each attempt logs the `getservercert` request,
   including `salt=...`.
3. Every attempt that ends in `Incorrect PIN` has a salt for which
   `SHA-256(salt bytes + PIN as ASCII)` has a zero byte in its first 16
   bytes; no successful attempt does. For example (PIN 4721):
   - salt `b953128048a727d7afc1c8493d9d43a3`: key
     `56370bb441`**`00`**`1b33889b0c9c9a444dea`
   - salt `8f497315a62006094416a7308091ed84`: key
     `340eb51e54f6cd2d6c37034f`**`00`**`538ad7`

   Shell check for a logged salt:
   `(echo -n SALT | xxd -r -p; echo -n PIN) | sha256sum | cut -c1-32`.
4. In our runs, 2 of 14 attempts failed this way, and replaying a failing
   exchange offline with the full 16-byte key showed the host's challenge
   response was correct.

## Suggested fix

Keep the hash as a byte array instead of going through a C string:

```cpp
QByteArray aesKey = QCryptographicHash::hash(saltedPin, hashAlgo);
aesKey.truncate(16);
```

`encrypt()`/`decrypt()` could also assert `key.size() == 16` so a short key
fails loudly instead of reading past the buffer.

## Notes

- The read past the end of the short key is also undefined behaviour,
  which may make the failure look nondeterministic for a given salt.
- Found while testing windowcast's GameStream host against stock
  moonlight-qt in CI; windowcast's test now pairs again on this error.
