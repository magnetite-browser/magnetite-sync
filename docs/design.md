# Design

Magnetite sync keeps spaces, pinned tabs, favorites and settings identical
across a person's devices without the server learning anything about them.
It is optional; the browser talks to a sync server only after someone turns
sync on.

## Sync chain

- The first device generates a 32-byte random seed and shows it once as 24
  BIP39 words (the sync code). Other devices join by entering the words.
- HKDF-SHA256 derives three things from the seed: an Ed25519 signing key, the
  encryption key and opaque collection ids. The words and the seed never leave
  the devices.
- The chain's id on the server is the signing key's public half. There is no
  email, password, user name or device name anywhere.

## Signed requests

`Authorization: Magnetite-Sig <pubkey-hex>.<unix-ms>.<signature>`: an Ed25519
signature over `magnetite-sync/v2`, the method, the server's host, the path
below `/v2`, the time and the SHA-256 of the body. The server accepts ±5
minutes and checks the host against the `Host` header, so a request captured
on one server is refused by any other. A replay can only repeat the identical
request within that window, which versioned writes turn into a harmless
conflict. For 10 minutes after a chain is deleted, the server refuses
requests for it, so a replay cannot bring it back.

The server checks every signature before it looks the chain up: only the
key holder learns whether a chain exists. A collection write sends its
body's hash ahead of the body (`Content-Digest`, RFC 9530); the server
checks the signature against that hash, then the chain, then reads the body
and compares it with the hash. Without the key, a large upload is refused
before a byte of it is read.

## Encryption

XChaCha20-Poly1305 with a random nonce; the collection id and the version are
bound as associated data, so the server cannot move data between collections
or versions. Plaintext is padded to a power of two (at least 1 KiB), so sizes
do not reveal small changes.

## Data model

Opaque, versioned collections. A write is an optimistic `PUT` with
`base_version`; the server answers 409 with the current version on conflict.
Merging is whole-collection, last writer wins. Versions only go up on a
device: a write counts only when the server acknowledges the exact version it
was encrypted for, and a server copy older than the newest version the device
has seen is refused.

## Transport

`https://` only, plain `http://` for loopback addresses; no redirects;
connect and request timeouts. Per-chain storage and write limits, a daily
budget of new chains and a cap on chains protect the server's disk.

## What the server keeps

The public key, the day of creation and last use, how often the chain wrote
in the current minute, collection ids, versions, nonces and ciphertext; for
ten minutes after a deletion, the deleted chain's id. Chains that never
stored anything are deleted after 7 days, chains unused for 400 days
(configurable) too. `DELETE /v2/chain` erases everything. Postgres reuses
the space of deleted rows later, so deleted ciphertext can stay on disk (and
in backups) until then, encrypted on the device as it always is. No request
or IP logging.

## What follows

- A stolen database yields nothing useful and nothing to guess.
- Losing every device and the 24 words loses the synced data; there is no
  recovery by design.
- A malicious server cannot read, forge or roll back data on a device, but it
  can withhold updates, and a device that has just joined takes whatever
  authentic version the server offers first.
- It can also fork devices: accept two different writes of the same version,
  one from each device, and show each device its own. They then never see
  each other's changes.
- Padding hides small changes, not which collection is which: the settings
  collection keeps its small fixed size while the sidebar grows, so the
  server can tell them apart and see roughly how large a sidebar is.
- Keys are not zeroised in memory, and the key file on a device (the sync
  code) can end up in backups of its profile; whoever reads it holds the
  chain.

The client is part of the Magnetite browser,
<https://github.com/magnetite-browser/magnetite>.
