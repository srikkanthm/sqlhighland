# Oracle Native Network Encryption (ANO/NNE) — investigation & port plan

Status: **implemented and verified** (AES encryption), 2026-09-14. The user's
target database sets `SQLNET.ENCRYPTION_SERVER=REQUIRED` and they have no
control over it, so TLS is unavailable; the pure-Rust thin driver (like
`python-oracledb` thin) does not implement ANO and refused with
`NSI_NA_REQUIRED`. Fork commit **`f1435e7`** now implements the handshake and
AES-CBC packet encryption for plain TCP.

Result: connects, queries, and cancels against a local Oracle 19c with
`SQLNET.ENCRYPTION_SERVER=required` (`localhost:1522`), and still connects to
the non-encrypted instance (`localhost:1521`). Integrity/checksum is not yet
negotiated (the client offers "none"); add it if a server requires it.

## 1. Why SQL Developer connects

Oracle's **JDBC Thin driver ships a 100% Java implementation of ANO**: it
negotiates AES/RC4 encryption + MD5/SHA checksums with Diffie-Hellman, with
client level defaulting to `ACCEPTED`. So when the server requires encryption,
JDBC satisfies it transparently — no `sqlnet.ora` on the client. The Rust thin
driver never got that implementation, hence the failure.

## 2. Reference implementation

We are porting from the pure-Go driver [`sijms/go-ora`](https://github.com/sijms/go-ora),
which implements NNE without an Oracle Client. Key files:
`v2/advanced_nego/{advanced_nego,comm,encrypt_service,data_integrity_service,default_service}.go`,
`v2/network/security/general.go`, `v2/network/data_packet.go`,
`v2/connection.go` (the hook), `v2/network/accept_packet.go`. Local copies of
these were fetched to `/tmp/go-ora-ano/` during investigation.

## 3. Protocol notes (from go-ora)

### 3.1 Negotiation framing

- Header (13 bytes): `DEADBEEF` (u32) · total length (u16) · version
  `0x0B200200` (u32) · service count (u16) · error flags (u8).
- Per service: service id (u16: 1=auth, 2=encrypt, 3=integrity, 4=supervisor),
  sub-packet count (u16), status (u32).
- Sub-packets: type (u16) + length (u16) + payload. Types:
  0=string, 1=bytes, 2=ub1, 3=ub2, 4=ub4, 5=version(u32), 6=status(u16),
  7=array.
- The ANO exchange is carried in ordinary TNS **DATA** packets and happens
  before encryption is activated.

### 3.2 Diffie-Hellman (integrity service, id 3)

Server sends (sub-packet 8): `dhGenLen` (u16), `dhPrimLen` (u16),
generator bytes, prime bytes, server public key bytes, IV bytes. Client
computes `publicKey = gen^priv mod prime`, `sharedKey = serverPub^priv mod
prime` (fixed-width to `ceil(dhGenLen/8)`), and returns its public key. The
`sharedKey` + server `IV` become the session key material.

### 3.3 Algorithms

- Encryption: `1`=RC4_40, `2`=DES56C, `6`=RC4_256, `8`=RC4_56, `10`=RC4_128,
  **`15`=AES128, `16`=AES192, `17`=AES256**. Client offers
  `RC4_40,RC4_56,RC4_128,RC4_256,DES56C,AES128,AES192,AES256`; server picks.
- Integrity: `1`=MD5, `3`=SHA1, `4`=SHA512, `5`=SHA256, `6`=SHA384.

### 3.4 Crypto construction

- AES (16/24/32-byte key from `sharedKey`): AES-CBC with an **all-zero IV**.
  Encrypt pads the plaintext with N zero bytes to a multiple of 16 and appends
  one byte `N+1`; decrypt reads that trailing byte, requires `len-1` to be a
  multiple of 16, strips padding.
- Integrity uses the negotiated hash plus a keystream derived from
  `sharedKey`/`IV`:
  - MD5/SHA1: `keyGen = RC4(last5(sharedKey) || 0xFF || IV)`; derive 5 bytes;
    `encryptor = RC4(k5 || 90)`, `decryptor = RC4(k5 || 180)`.
  - SHA256/384/512: `keyGen = AES-CBC(sharedKey[:5] || 0xFF … , IV[:16])`;
    run it over 32 zero bytes to get `key(16)` + `iv(16)`; then keystreams are
    AES-CBC with `key[5]=90` / `key[5]=180`. `Compute(input) = H(input ||
    keystream(hash_size))`; `Validate` recomputes over `input` minus trailing
    hash bytes.

### 3.5 Wire transform (per DATA packet body)

- Send: if hash → `data ||= H(data)`; if encrypt → `data = Encrypt(data)`;
  if either → append a single `0x00` "folding key" byte.
- Receive: strip the trailing folding byte, `Decrypt`, then `Validate`
  (reverse order).
- Only **DATA** packets are transformed; CONNECT/ACCEPT/MARKER/control are not.

### 3.6 Trigger

In go-ora, after ACCEPT: if `ACFL0 & 1 != 0 && ACFL0 & 4 == 0 && ACFL1 & 8 == 0`
(accept payload bytes 14/15), run `Write → Read → StartServices`. The client
must **not** send `NSI_DISABLE_NA` when it wants ANO. `StartServices` order is
integrity(3) → encrypt(2) → auth(1) → supervisor(4).

## 4. Rust port plan

New module in the fork, e.g. `src/advanced_nego/`:

- `mod.rs` — negotiation driver (header/subpacket codec, service list, the
  `write → read → start` sequence).
- `dh.rs` — Diffie-Hellman with `num-bigint` (server supplies group params).
- `crypt.rs` — AES-CBC cryptor + integrity hashes (keystream derivation).
- Wire into `Client`/`Transport`:
  - `messages/connect.rs`: stop setting `NSI_DISABLE_NA` (advertise NA) when
    ANO is enabled; store `ACFL0`/`ACFL1`.
  - `client/mod.rs`: after `connect_phase_one`, run the ANO negotiation before
    the TTC protocol/auth messages.
  - transport: transform `PACKET_TYPE_DATA` bodies on send/receive once the
    cryptor/hash are active.
- Crates: `aes`, `cbc` (already present), add `rc4`, `md-5`, `sha1`, `num-bigint`,
  `num-traits`.
- Keep it behind the fork; SQLHighland needs no API change (it becomes
  transparent).

## 5. Test strategy

We need a server that requires NA. The user's Oracle 19.19 container
`highlanddb` (localhost:1521) currently has NA off, and flipping it on would
break the existing `live`/`cancel_live` tests. Options:

1. A **second** Oracle container (e.g. `highlanddb-ano`) with
   `SQLNET.ENCRYPTION_SERVER=REQUIRED` and `SQLNET.ENCRYPTION_TYPES_SERVER=(AES256,AES192,AES128)`
   in `sqlnet.ora`, mapped to a different port.
2. Temporarily toggle NA on the existing container during development (breaks
   other tests while enabled).

Verification: connect with the ported driver to the NA-required instance and
compare a trivial query with `sqlplus`/SQL Developer; cover AES256/192/128 and
at least one integrity algorithm (SHA256), plus no-integrity.

## 6. Progress checklist

- [x] NA-required test Oracle: `highlanddb-ano` on `localhost:1522`.
- [x] Diffie-Hellman + unit tests (`src/encryption.rs`).
- [x] AES-CBC cryptor + unit tests (round-trip across sizes).
- [x] Negotiation codec + service lists (`src/advanced_nego.rs`).
- [x] Client hook: advertise ANO (drop `NSI_DISABLE_NA`), capture
      `ACFL0/ACFL1`, run the handshake after ACCEPT, install the cryptor.
- [x] Transport DATA transform (pad → encrypt → folding byte on send;
      strip → decrypt on receive).
- [x] End-to-end against the NA server: connect, `SELECT 1 FROM dual`, and
      cancel (`cancel_live`) all pass; the non-NA instance still works.
- [x] Integrity/checksum negotiation (SHA-256/384/512, AES-keystream);
      required by some servers and applied per DATA packet.
- [ ] Explicit AES128/AES192 runs (the test server picks AES256).
- [x] Oracle 10G (O3LOGON) password verifier support for legacy accounts
      (see below).
- [x] Repin fork + docs. The interim `NSI_NA_REQUIRED` hard error is now a
      targeted failure only when the server requires NA but does not offer the
      ANO handshake.

### Oracle 10G password verifiers (O3LOGON)

A connection can complete the ANO handshake yet still fail with
`ORA-01017: invalid username/password; logon denied` even when the password is
correct. This happens when the account exposes **only an Oracle 10G password
verifier** — typically because `sec_case_sensitive_logon=FALSE` and/or
`SQLNET.ALLOWED_LOGIN_VERSION_SERVER=10` — which disables the 11G/12C verifiers
that O5LOGON (used by both python-oracledb thin and this driver) relies on. The
server signals this by reporting verifier type `0x939` (2361) in the
`AUTH_VFR_DATA` flags; the JDBC *thin* driver (SQL Developer) silently falls
back to the 10G exchange, which is why SQL Developer can connect.

The fork now implements that fallback in `src/messages/auth.rs`
(`generate_verifier_10g`): the 8-byte DES-derived verifier
(`encryption::oracle10g_verifier`, ported from Oracle's `O3LOGON`) seeds a
16-byte AES-128 key used to wrap the session keys, the combo key is derived
with **PBKDF2-HMAC-SHA512** (`AUTH_PBKDF2_CSK_SALT`/`AUTH_PBKDF2_SDER_COUNT`,
keyLen 16) when the server supplies those fields — matching JDBC's modern
`O5Logon` path — and the password is encrypted with AES-128-CBC/PKCS#5. The
legacy XOR/MD5 fold is only used if the PBKDF2 fields are absent. Algorithm
ported from the decompiled `oracle.security.o5logon`/`o3logon` JDBC helper
classes; the verifier is unit-tested against passlib's known vector
(`username`/`password` → `872805F3F4C83365`). Diagnose with the connection
trace: `auth verifier_type=Some(2361)` indicates the 10G path, and
`auth 10g: combo key via PBKDF2 (keylen=16)` confirms the modern combo path.

### Debug trace

The driver writes a per-connection trace to `~/sqlhighland-ano-debug.log`
(ACCEPT flags, the ANO handshake, and auth pair lengths/verifier type) **only
when `SQLHIGHLAND_ANO_TRACE` is set**; normal runs never touch the home
directory. To capture it, launch the app from a terminal:

```
SQLHIGHLAND_ANO_TRACE=1 /Applications/SQLHighland.app/Contents/MacOS/SQLHighland
```

Caveat: the 10G verifier is weak (case-insensitive, DES-based) and is removed
from Oracle 21c onward. Where possible, have the account password reset so that
11G/12C verifiers are generated instead.

### Known limitation: cancel on a checksummed session

An interrupt (Cancel) on a session that negotiated a checksum leaves Oracle's
per-request checksum state desynced (the re-keying isn't observable from the
client). The post-interrupt packet fails integrity validation, the driver
reports an unrecoverable error and closes the transport. The error is
poisoning, so SQLHighland drops the session and reconnects on the next run.
Servers that only require *encryption* (no checksum) are unaffected — Cancel
works normally there.

## 7. Risks

- Exact DH parameter sizes and padding are server-supplied, but the cryptor
  edge cases (padding byte, folding key, empty payloads) must match exactly.
- `num-bigint` modexp performance is fine at session setup only.
- Recursion/handshake ordering vs. TLS: ANO and TCPS are alternatives; don't
  run both.

## 8. Known issue: intermittent TTC desync

On some ANO servers a run can intermittently fail with
`internal error: unknown TTC message type 42 …` and then succeed on the next
run. This is a response-stream desync (the driver's poison path drops the
session and reconnects). It is tracked in
[`docs/TTC_DESYNC.md`](TTC_DESYNC.md), which lists the candidate causes
(marker/timeout recovery leaving a response partially consumed, ANO per-packet
framing) and the diagnostics (`SQLHIGHLAND_ANO_TRACE`, `RSO_DEBUG_PACKETS`).
