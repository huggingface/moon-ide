# ADR 0087 — Companion end-to-end encryption: the relay becomes a blind pipe

Date: 2026-09-10
Status: accepted; hardens [ADR 0031](0031-remote-bridge-relay.md) /
[ADR 0035](0035-public-relay-deployment.md). Implements README
§ "Before wider release" items "scope pairing to IDEs" and "make the
relay untrusted" (minus the PWA-origin caveat below).

## Context

The relay terminates TLS and sees plaintext JSON-RPC: transcripts,
file contents, and — worse — it can _originate_ `call`s to any
enrolled IDE, which drive the coder and therefore arbitrary commands.
The relay also mints the phone pairing code, and the IDE never learns
which phones exist. A compromised relay box owns every IDE behind it,
and any relay-paired phone can drive every enrolled IDE.

## Decision

### The IDE, not the relay, authorizes phones

Each IDE host has a static X25519 identity (keyring
`companion-e2e-identity`) and its own list of authorized phone public
keys (`companion-e2e-devices`). Relay tokens stay, demoted to routing
and DoS control. A phone can drive exactly the IDEs that paired it.

### Pairing: one link, delivered by QR or copy-paste

The IDE mints an invite: a 256-bit single-use secret (15 min TTL,
keyring `companion-e2e-invites`, shared by the host's processes) and
bundles it with a relay routing code into one link:

```
https://<relay>/#pair=<relay-code>&ide=<ide_id>&ws=<slug>&k=<ide-pubkey>&s=<secret>
```

The desktop renders it as a QR and offers "Copy link"; headless
`moon-remote pair` prints it plus a terminal QR (for the ssh case).
There is **no short typed code**: text delivery is copy-paste of the
same link, so both paths carry the same 256 bits. The phone runs
`Noise_IKpsk2_25519_ChaChaPoly_SHA256` against the IDE key from the
link with the secret as PSK; the IDE pins the phone's static key.

### Sessions and transport

Each phone↔(IDE, workspace) session is a `Noise_IK_25519_ChaChaPoly_SHA256`
handshake (prologue binds the workspace slug); the IDE rejects any
initiator static not in its authorized list. Transport uses the Noise
split keys with **explicit** 64-bit nonces (replies and events arrive
out of order through the relay), a 2048-slot replay window per
direction, and encrypted request ids echoed in encrypted results so
the relay cannot swap replies between calls.

Wire shape rides the existing relay frames, opaque to it:

- `call{method:"e2e_pair"|"e2e_open", params:{msg}}` — handshakes.
- `call{method:"e2e_call", params:{sid,n,c}}` → `result{value:{n,c}}`.
- `subscribe{…, params:{sid}}` → `event{event:{sid,n,c}}`.

On the relay path the IDE refuses every plaintext method except
`workspace_launch` (starting an existing workspace; routed by the
relay to any live process of that IDE, so it can't sit inside a
per-process session). The local-carrier `instance.sock` stays lenient:
the local bridge is the same host and user as the IDE.

### Residual risk: the relay serves the PWA

The relay still serves the companion's JavaScript from its own origin,
so a malicious relay can ship JS that exfiltrates the phone's key.
Accepted for now (operator's call); the fix is serving the PWA from a
separate origin (static host built by CI) with the relay as a
cross-origin WebSocket only. Until then "blind pipe" holds against a
relay that forwards bytes, not one that rewrites the app.

## Rejected alternatives

- **Short code + PAKE (CPace/SPAKE2).** Needed only for hand-typed
  codes; copy-paste delivery makes a 256-bit secret free, and a PAKE
  is extra crypto to implement twice (Rust + TS) and keep interoperable.
- **Short code as a Noise PSK.** An active relay MITM can brute-force
  a 32–40-bit PSK offline from one handshake message within the TTL.
- **Signing frames without encryption.** Stops forgery but leaves
  transcripts and file contents readable by the relay.
- **Noise's implicit-nonce transport.** Replies and events are
  reordered by concurrent dispatch; explicit nonces + replay window
  (WireGuard-style) tolerate that.

## Consequences

- Existing relay-paired phones must re-pair once (no migration).
- New deps: `snow` (Rust handshake + stateless transport), `@noble/curves`,
  `@noble/ciphers`, `@noble/hashes` (PWA); interop pinned by
  snow-generated fixtures checked from vitest.
- Revoking a phone on the IDE takes effect within the device-cache TTL
  (30 s) for live sessions.
