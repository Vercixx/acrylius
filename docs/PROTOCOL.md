# The acrylius protocol, version 1

This document is normative. Where it disagrees with the code, one of them is
wrong and it is worth finding out which.

The vectors in section 13 are asserted by `crates/acrylius-proto/tests/vectors.rs`,
so the document and the code cannot drift apart silently.

## 1. Scope

acrylius links a phone and a computer over a local network. A device offers
capabilities, a peer accepts the ones it understands, and messages flow in both
directions over a session that either side may use once it exists.

The protocol does not interoperate with KDE Connect and is not intended to. It
borrows one idea, capability lists exchanged at connection time, and nothing
else.

### Non-goals

Reaching a device across the internet is out of scope. Use a VPN.

Anonymity is out of scope. A passive observer on the same network learns that
two acrylius devices exist and roughly how much they are saying to each other.

## 2. Conventions

Byte strings are written as lower-case hex. Text is UTF-8.

`b64u(x)` is base64url without padding, with a strict decoder. The decoder
rejects padding characters, bytes outside the alphabet, lengths that the encoder
could not produce (`len % 4 == 1`), and non-canonical trailing bits. A lenient
decoder would let one value be spelled several ways, which turns an identifier
into a set of spellings rather than a value.

`HKDF(salt, ikm, info, n)` is HKDF-SHA256 producing `n` bytes.

All CBOR uses definite-length encoding.

## 3. Identity

A device is a static X25519 key pair. The public key is the identity. A name, an
address, and an mDNS record are all hints.

```
fp(pk)        = b64u(SHA-256("acrylius/v1/fp"  || pk))          43 characters
device_id(pk) = b64u(SHA-256("acrylius/v1/did" || pk)[0..16])   22 characters
```

Both derivations are domain-tagged so that a fingerprint can never be mistaken
for a device id, even if the truncation length changed.

`fp` is what a human compares and what discovery advertises. `device_id` is an
index: short enough for a log line, and backed by 128 bits of a hash over a key
nobody else holds.

A receiver always derives both from the key the handshake authenticated. A
device id that arrives in a payload is a label for logging and is never used to
decide who the sender is. This is what stops two devices colliding onto one
record by claiming the same identifier.

The private key is stored by the host: the iOS Keychain with
`kSecAttrAccessibleWhenUnlockedThisDeviceOnly` and no biometric access control,
or a `0600` file on Linux. The public half is derived on load rather than stored
beside the private half, so an edited key file cannot produce an identity whose
fingerprint disagrees with the key it can prove possession of.

## 4. Discovery

Discovery supplies candidate addresses. It never establishes identity, and a
record that lies costs an attacker nothing, so nothing may be decided from one.

```
service   _acrylius._tcp
port      1971 by default
TXT       v=1
          fp=<fingerprint, 43 characters>
          id=<device id, 22 characters>
          n=<display name>
          pair=0|1
```

The raw static key is deliberately absent from the TXT record. Keeping it
unpublished is what lets a session opener (section 6.2) stay opaque to an
observer.

`pair=1` advertises that a pairing window is currently open. It is a convenience
for a user interface. A device must not treat it as permission for anything.

A device that cannot advertise, which includes iOS, is still fully functional:
it dials and is never dialled.

## 5. Framing

A transport delivers whole messages. Fragmentation, if any, is the transport's
business and is invisible above this line.

Every message carries one leading byte naming its kind:

```
1   pairing handshake message   (Noise XXpsk0)
2   session handshake message   (Noise IKpsk2)
3   transport message           (an encrypted envelope)
```

The tag is outside the encryption and is therefore forgeable. That is
acceptable, and it is why the Noise prologue in section 6 also carries the mode:
the tag chooses a parser, and the prologue is what makes the choice binding.

The TCP transport frames each message as a big-endian `u32` length followed by
that many bytes, capped at 1 MiB. A receiver enforces the cap before allocating,
so a peer cannot announce a large length and make the receiver reserve it
without sending anything.

### 5.1 The BLE transport's framing

A GATT characteristic carries a couple of hundred bytes at a time, so a message
is split across several writes or notifications. Each fragment carries one
header byte:

```
bit 0  MORE    more fragments belong to this message
bit 1  START   this fragment begins a message
2..7           reserved, must be zero
```

A message that fits one fragment is therefore `0x02`; a message in three is
`0x03`, `0x01`, `0x00`. A receiver accumulates until it sees a fragment with
`MORE` clear, and enforces the link's `max_message` *before* keeping the bytes,
for the same reason the TCP transport checks its length prefix first.

`START` is redundant on a link that neither loses nor reorders, which is what a
BLE connection provides for as long as it lives. It is carried anyway because it
costs nothing and makes two otherwise silent failures nameable: a continuation
arriving with no message open, and a message beginning while one is unfinished.
Both are errors, and both mean the link is no longer trustworthy.

A fragment with a reserved bit set is refused rather than ignored. A peer that
sets one is speaking a dialect this version does not have.

The fragment size is whatever the link reports — on BlueZ the `mtu` handed to
`WriteValue`/`StartNotify`, on iOS `maximumWriteValueLength(for:)`. It is never
assumed.

## 6. Handshakes

Both handshakes run with a prologue, which Noise mixes into the handshake hash.
The prologue is not secret, but it is authenticated, so an attacker who flips
the mode byte to push a paired device back into pairing causes a decryption
failure on both sides instead of a downgrade.

```
prologue = "ACR" || version || mode || len(suite) || suite
mode     = 1 for pairing, 2 for session
suite    = the full Noise pattern name below
```

### 6.1 Pairing: `Noise_XXpsk0_25519_ChaChaPoly_SHA256`

The pre-shared key comes from the pairing code:

```
psk = HKDF(salt = "acrylius/pair/v1/psk", ikm = normalized_code, info = "", 32)
```

The PSK is at position 0, not 3, and the difference matters. `psk3` mixes
the key into message 3, which the initiator writes. It therefore proves to the
responder that the initiator knew the code and proves nothing in the other
direction: an initiator talking to the wrong machine completes its side,
displays a code, and waits for a human to approve a peer that never
demonstrated knowing anything. `psk0` mixes the code into the chaining key
before the first message, so every encrypted payload in either direction
depends on it. A wrong code means message 1 does not decrypt, no reply is sent,
and no code appears on either screen.

```
initiator                                responder
  -> e                        payload: empty
  <- e, ee, s, es             payload: Hello
  -> s, se                    payload: Hello
```

Message 1 carries no payload. Message 2 and message 3 carry a `Hello`
(section 7).

When the handshake completes, both sides compute:

```
sas         = the six decimal digits of HKDF("", handshake_hash, "acrylius/pair/v1/sas", 4)
              interpreted big-endian, modulo 1000000, zero-padded, and split 3+3
session_psk = HKDF("", handshake_hash, "acrylius/pair/v1/session-psk", 32)
```

`session_psk` is stored by both sides and never transmitted.

### 6.2 Sessions: `Noise_IKpsk2_25519_ChaChaPoly_SHA256`

```
initiator                                responder
  -> e, es, s, ss             payload: Hello
  <- e, ee, se, psk2          payload: Hello
```

The PSK is `session_psk` from the pairing that established the peer.

Sessions use `psk2` rather than following pairing to `psk0`, and the reason is
mechanical. An IK responder must read message 1 before it knows which peer is
calling and therefore which PSK to answer with. Because `psk2` is mixed at
message 2, message 1's payload does not depend on it: a throwaway handshake
built with an all-zero PSK reads message 1 to exactly the same result. The
responder uses the static key it learns that way to select the real PSK, then
replays the same message 1 into a fresh handshake. Nothing is leaked, and
nothing is trusted: the identity learned by probing only chooses a key, and a
wrong choice still fails at message 2.

Message 1's payload is not forward-secret. It is encrypted under `es` and `ss`,
so anyone who later compromises the responder's static key can decrypt every
message 1 they recorded. It therefore carries identity and capabilities only.
A command must never appear there. The initiator waits for message 2, which is
forward-secret, before sending anything else. On a local network that costs
about four milliseconds.

Message 1 is also replayable, which section 7 addresses.

### 6.3 After the handshake

Both sides enter Noise transport mode. The cipher's own nonce counter provides
replay protection within a session, which is why there is no timestamp, no
nonce cache, and no per-message signature anywhere in this protocol.

A session on a lossy or unordered link would need caller-supplied nonces and a
replay window instead. No such transport exists yet, and an implementation must
refuse rather than pretend a stateful cipher is safe on one.

An implementation should rekey the outgoing cipher after 2^20 messages.

## 7. Hello

Sent in both handshakes, CBOR, array-encoded with these positions:

```
0  v          u8       protocol version, 1
1  ts_ms      u64      sender's wall clock, milliseconds since the Unix epoch
2  device_id  text     the sender's own id, for logging only
3  name       text     display name
4  platform   text     "linux", "ios", advisory
5  caps_out   [text]   capabilities this side may send
6  caps_in    [text]   capabilities this side can handle
```

### Freshness

A session opener can be recorded and sent again later. The responder keeps a
`greatest_seen` timestamp per peer and rejects a `Hello` whose `ts_ms` is not
strictly greater. It also rejects one more than 60 seconds from its own clock in
either direction.

Both checks are needed. The watermark stops replay. The skew bound stops a peer
with a badly wrong clock from setting a watermark so far ahead that it can never
connect again.

`greatest_seen` is persisted. A device that forgot it across a restart would
accept a recorded opener again.

This is the whole of the protocol's replay machinery: one `u64` per peer.

## 8. Pairing

Either side may ask to pair. The other must approve.

```
idle ──open window──▶ open ──handshake completes──▶ awaiting approval ──approve──▶ paired
  ▲                     │                                   │
  │                     ├── window expires ─────────────────┼──▶ idle
  │                     └── N failed handshakes ────────────┘
  └───────────────────────────────────── deny ──────────────┘
```

### Rules

A pairing code is 8 characters from Crockford base32 with `I`, `L`, `O` and `U`
removed, giving 40 bits. Input is normalized before use: separators are
stripped, letters are upper-cased, and `I` and `L` fold onto `1`, `O` onto `0`,
`U` onto `V`. Normalization happens before key derivation, so a user who types
`l` for `1` reaches the same key rather than merely passing the same string
comparison.

The window expiry is measured on a monotonic clock. Changing the system clock
must not extend it.

Three failed handshakes close the window. A correct code that fails to complete
is not a typo, it means the code reached someone who could not complete with it.

Opening a window has no network route. On a desktop it is reachable only over a
`0600` Unix socket whose peer credentials are checked against the daemon's own
uid. "You must be at the machine" is therefore a property of the transport
rather than a rule a handler could forget to apply.

Both sides display the SAS. It is a cross-check, not the security mechanism:
`XXpsk0` already makes a wrong code fail. Showing it costs nothing and catches
implementation errors that a PSK check would mask.

Refusing the SAS closes the link and burns the window. A code that does not
match is a hostile handshake, not a mistake worth retrying.

On approval each side stores the peer's static key, `device_id`, name, platform,
`session_psk`, and `greatest_seen = 0`.

### QR payload

```
acrylius:1?n=<name>&h=<host>&p=<port>&id=<device id>&fp=<fingerprint>&c=<code>
```

The fingerprint in a QR payload is checked against the one the handshake
produces. A mismatch aborts.

## 9. Envelope

Every transport message is a CBOR array, array-encoded with these positions:

```
0  v      u8            protocol version, 1
1  id     u32           sender-assigned, unique within a session
2  re     u32 or nil    the id this message answers
3  cap    text          capability, including its major version
4  ty     text          verb within the capability
5  body   bytes         opaque
6  flags  u8            reserved, 0
7  bulk   u64 or nil    bulk transfer this refers to
```

`body` is an opaque byte string and not inline CBOR. If it were structured, the
core would have to parse a plugin's schema in order to route a message, which
defeats the purpose of having plugins at all. Opaque bodies let a message be
routed, queued, and forwarded by code that understands none of it, let a plugin
choose its own encoding, and make handing a message to an out-of-process plugin
a copy. The cost is a few bytes of double encoding. This is the same layering
COSE uses.

A trailing nil is omitted, so an envelope with no `bulk` encodes as an array of
seven. Adding a field at position 8 in a later version is therefore backward
compatible: an older reader stops at the fields it knows.

## 10. Capabilities

A capability identifier carries its own major version:

```
org.acrylius.clipboard/1
```

Negotiation is a set intersection with no separate version field to compare
wrongly, and `org.acrylius.clipboard/2` is simply a different capability. A peer
that speaks both advertises both.

Negotiation is directional:

```
what A may send to B  =  A.caps_out ∩ B.caps_in
what B may send to A  =  B.caps_out ∩ A.caps_in
```

Conflating the two would let a peer receive a capability it only declared it
could send. A message arriving for a capability outside the negotiated set is
answered with `cap_not_negotiated` and is not delivered.

A device advertises every capability it knows about, including ones it cannot
serve. Being unable to *serve* a capability says nothing about being able to
*use* one: a phone has no desktop session of its own and still wants to lock a
computer's, and an answer arrives under the same capability as the request, so
withdrawing it would break replies as well. A request a device cannot carry out
is answered `not_allowed`.

What a device can actually do is discovered instead from what it announces on
connect — a session state, a catalogue of commands, wake targets, a list of
players. A machine with no commands configured sends no catalogue and a remote
shows no button.

## 11. The version 1 capabilities

Bodies are CBOR, array-encoded, with the positions given. Every request may be
answered with `err` (section 12) instead of the reply named here.

### org.acrylius.ping/1

```
->  ping    body: arbitrary bytes
<-  pong    body: the same bytes, echoed
```

It exists to exercise routing in both directions with no host support at all,
which makes it the thing to reach for when something is wrong and the question
is whether the session itself works.

### org.acrylius.session/1

Sent to a computer. A phone does not offer it in `caps_in`.

```
->  query   body: []
->  lock    body: []
->  unlock  body: []
<-  state   body: [0 locked:bool, 1 session_id:text, 2 type:text, 3 active:bool]
<-  result  body: [0 was_locked:bool, 1 locked:bool, 2 session_id:text]
```

`state` is also sent unsolicited when the lock state changes.

Both verbs are idempotent. Locking an already-locked session returns `result`
with `was_locked = true`, not an error.

`locked` in a `result` is always read back from the system after the operation.
An implementation must not report success because a command exited zero. The
lockers that matter here act on a signal asynchronously, so the exit status says
only that the signal was sent.

Determining `locked` is not simply reading logind's `LockedHint`. That hint is
only as good as the locker maintaining it, and some maintain it not at all:
Noctalia on Hyprland locks the screen while leaving the hint reading `no`. A
hint of `yes` is trusted. A hint of `no`, on an active Wayland session, is
checked against the compositor before it is believed. A compositor that cannot
answer leaves the hint standing, so a probe failure never reads as "unlocked".

Choosing which session to act on differs between the two verbs, and the two
rankings must not be shared. `unlock` prefers a locked session; `lock` prefers
an unlocked one. Ranking locked-first for a lock request would target a session
that is already locked, do nothing, and report success. On a machine with one
session both rankings pick the same thing and the bug is invisible, which is
exactly why it is written down here.

### org.acrylius.wol/1

```
<-  config  body: [0 macs:[text], 1 broadcast:text, 2 port:u16, 3 last_ipv4:text]
->  relay   body: [0 mac:text]
```

The interesting half of waking a computer involves no messages at all. A
sleeping computer is not running the daemon, so the phone sends the magic
packet. `config` exists to tell the phone what to send and where.

A magic packet is `ff` six times followed by the MAC repeated 16 times, 102
bytes, or 108 with a six-byte SecureOn password.

The phone aims at `last_ipv4` first and only then at a broadcast address. A
network interface matches the packet's payload, not its destination address, so
a unicast datagram wakes the machine just as well. This ordering matters because
iOS gates UDP broadcast behind an entitlement a free developer account cannot
have. It requires the router to still hold an ARP entry for a sleeping machine,
which in practice means a DHCP reservation and a static ARP entry.

`relay` asks a computer that is awake to wake a different one. The MAC must be
in that computer's configured allowlist, or the answer is `not_allowed`.
Otherwise the endpoint would be an open UDP relay.

Waking is not authenticated and cannot be. Anyone on the network can send a
magic packet, and waking a machine is not a privileged action.

### org.acrylius.clipboard/1

```
<>  set     body: [0 mime:text, 1 data:bytes, 2 hash:bytes]
<>  get     body: []          the peer answers with `set`
```

`hash` is SHA-256 of `data`.

Loop prevention is mandatory. Each side keeps the hash of the last value it set
locally and the last it received, and forwards a local change only when it
matches neither. Without this, two peers echo a paste at each other forever.

In version 1 `mime` is `text/plain;charset=utf-8` and `data` is capped at
128 KiB. A larger or unrecognised value is answered with `too_large` or
`not_allowed` rather than truncated.

A `set` carrying `re` answers a `get` and is a different thing from an
unsolicited push. An answer is always delivered to whoever asked, regardless of
the receive switch, and is not written to the local clipboard: the caller
asked to see the value, not to take it. Taking ownership of a selection nobody
asked for is how two devices end up fighting over one.

The two directions are separately switchable, and neither is required. This is
not only a preference: iOS cannot read its own pasteboard silently. Since iOS 16
a programmatic read of content that came from another app raises a system
prompt, and only a `PasteButton`, the paste menu, or the keyboard shortcut are
exempt. `UIPasteboard.changeCount` can be polled without a prompt, so an iOS
implementation can notice a change and offer a button, but it cannot sync
phone to computer silently. Computer to phone is unaffected.

### org.acrylius.media/1

Control whatever is playing. Which players a machine has, and what "the active
one" means there, is the host's business; on Linux it is MPRIS.

| direction | ty | body |
|---|---|---|
| →peer | `query` | empty |
| →peer | `play`, `pause`, `playpause`, `next`, `previous`, `stop` | `MediaCommand`, or empty |
| →peer | `seek` | `MediaCommand`, `value` = milliseconds, may be negative |
| →peer | `position` | `MediaCommand`, `value` = milliseconds from the start |
| →peer | `volume` | `MediaCommand`, `value` = 0 to 100 |
| ←peer | `state` | `MediaState` |

```
MediaCommand { 0: player, 1: value }
MediaState   { 0: [MediaPlayer], 1: active, 2: system_volume }
MediaPlayer  { 0: id, 1: name, 2: status, 3: title, 4: artist, 5: album,
               6: length_ms, 7: position_ms, 8: volume_percent,
               9: can_go_next, 10: can_go_previous, 11: can_seek,
               12: can_control }
```

An empty `player` means whichever the peer reports as `active`. A remote whose
buttons stop working because a second player appeared is worse than one that
occasionally guesses, and the guess is visible.

Every command is answered with `state`, never an acknowledgement. A player may
ignore a command, clamp a seek, or stop of its own accord, and reading it back
is the only honest answer.

`position_ms` is a reading, true only at the instant it was taken, and it is
never counted forward by anything that stores or forwards it: a position that
advanced on its own would drift, and would keep advancing after the media
stopped somewhere its holder cannot see.

A screen showing it to a person may estimate between readings — the reported
position plus the time since it arrived, while the player says it is playing —
but only while it is also asking for a fresh one, and never past the track's own
length. An estimate that nothing is correcting is the thing this rule forbids;
one that is corrected every couple of seconds by the side that is looking at it
is how a clock ticks without anyone inventing where a track has got to. A `state` is not broadcast for a position change alone,
because a playing track would otherwise announce itself once a second forever;
it is broadcast for a track change, a pause, a volume move, or a player
appearing or leaving.

`volume` with no `player` named is the **machine's** output volume, reported as
`system_volume`; with one named it is that player's own. That asymmetry is not
tidy and is deliberate. MPRIS gives every player a writable `Volume` property
that a great many of them accept and then ignore — Chromium does exactly this,
while reporting `can_control` true — so a remote offering only the per-player
control has a slider that works for some of what you play and silently does
nothing for the rest. The machine's volume always moves something, and it is
what a person means by "turn it down".

A host that sets a player's volume must read it back and answer `not_allowed`
if it did not move. Reporting a change that did not happen is worse than
refusing one that cannot.

Values outside 0 to 100 are refused before they reach a host, so no host need
range-check again.

Album art is not carried. MPRIS supplies a URL, usually to a file the peer
cannot read, and an image is far past what an envelope should hold.

### org.acrylius.command/1

```
<-  list     body: [0 [[0 id:text, 1 name:text, 2 needs_confirm:bool]]]
->  run      body: [0 id:text]
<-  started  body: [0 run_id:u32]
<-  output   body: [0 run_id:u32, 1 stream:u8, 2 bytes:bytes]     stream: 0 stdout, 1 stderr
<-  exited   body: [0 run_id:u32, 1 code:i32, 2 truncated:bool]
```

One rule makes this a command runner and not a remote shell: the wire carries
an `id` from the computer's own configuration and never a command string. An
id that is not in the allowlist is answered with `not_allowed`.

A command runs as an argv vector with an absolute path and no shell. It has a
timeout, 10 seconds by default, and its captured output is capped, 64 KiB by
default, with `truncated` set when the cap was reached.

### org.acrylius.share/1

Send a file. The envelope is the wrong place for one — a session frame is capped
at 1 MiB and a photo is not — so this capability negotiates a transfer and the
bytes travel beside the session, on their own connection.

| direction | ty | body |
|---|---|---|
| →peer | `offer` | `Offer` |
| ←peer | `accept` | `Accept`, answering the offer's `id` |
| ←peer | `reject` | `Finished`, `ok` false |
| ↔peer | `finished` | `Finished` |

```
Offer    { 0: transfer, 1: name, 2: size, 3: mime }
Accept   { 0: transfer, 1: endpoint }
Finished { 0: transfer, 1: ok, 2: detail }
```

`name` is a name and never a path. A receiver treats it as a suggestion: it
strips every directory component, and it never replaces a file already there —
two photos called the same thing is an ordinary event and losing the first one
is not. `size` is what the sender claims; a receiver that gets fewer bytes than
that has a failed transfer, not a short file, and keeps nothing.

`transfer` is chosen by the sender and is unique only within that session.

**The receiver listens, not the sender.** This is the one place the packet-layer
symmetry does not reach: a phone cannot accept an incoming connection, so it can
only ever be the side that dials. Naming the listener in `accept` puts the
choice with the side that has already agreed to receive.

An `offer` is never accepted automatically unless a host is configured to. It is
announced, and a person answers.

A device with nowhere to put a file answers `not_allowed` immediately instead,
without announcing anything. It still advertises the capability — withdrawing it
would mean nobody could be sent a file *by* that device either, and would break
replies — but a device that cannot ask a person must not sit on an offer, because
there is no later moment at which an answer could arrive.

**Sending and receiving are not the same ability, and a device may have only
one.** Sending needs a file and a socket to dial. Receiving needs a directory, a
listening socket and somebody to ask. A phone has the first and none of the
second, so it offers files freely and refuses every offer made to it. Nothing in
the protocol requires the two to come together.

#### The bulk connection

The bytes go over a separate connection to the `endpoint` in `accept`, framed as
a big-endian `u32` length followed by that many bytes:

```
hello    "ACRB" || transfer as u64be              12 bytes, unencrypted
chunk    ChaCha20-Poly1305(key, nonce, plaintext) up to 64 KiB per chunk
```

The key is never transmitted. Both ends derive it from something only a peer
that completed the session handshake can have:

```
key = HKDF-SHA256(ikm = session handshake hash,
                  info = "acrylius/bulk/v1" || transfer as u64be,
                  salt = none)[0..32]
```

The nonce is the chunk's sequence number, counting from zero, in the last eight
bytes of the twelve. Sequence numbers are never reused under a key, because a
key is used for exactly one transfer.

The hello is in the clear and is only a demultiplexer: it says which transfer is
arriving so the listener can pick a key. It authenticates nothing. A connection
that opens with the right hello and then fails to produce a chunk that decrypts
gets nothing and leaves nothing behind — the file is written to a temporary name
and only moved into place once every expected byte has arrived and been
authenticated.

Both ends report `finished`, because each knows only its own half: a sender that
finished writing does not know whether the receiver kept the file, and a
receiver cannot tell a cancelled send from a dropped connection.

## 12. Errors

`ty = "err"`, with this body:

```
0  code     text   from the closed vocabulary below
1  message  text   for a human, carrying no promises
```

The vocabulary is closed. Adding a code is a deliberate act, because a client
can only say something useful about a failure it can name.

```
cap_not_negotiated   the capability was not in the negotiated set for this direction
unknown_type         well-formed, but this verb is unknown within the capability
bad_body             the body did not decode
not_allowed          refused by policy
effect_failed        the host could not carry it out
not_paired           the peer is known but this needs a pairing that is not complete
too_large            a size cap was exceeded
timeout              did not confirm in time, and may yet have succeeded
internal             a fault, always accompanied by a log line
```

`timeout` is deliberately distinct from `effect_failed`. An operation that did
not confirm may still have happened, and a caller should re-read state rather
than assume either outcome.

## 13. Golden vectors

Asserted by `crates/acrylius-proto/tests/vectors.rs`.

### Identity

```
pk = 0000000000000000000000000000000000000000000000000000000000000000
fp        = sHwTuvaZ9cfLpIyLPH6e4z2F8YLj2cXSbe-5M0xmQPQ
device_id = gWEIONLBax9DtyHhuB1DsQ

pk = 0000000000000000000000000000000000000000000000000000000000000001
fp        = NzL-Mnw4P7v4wEgbthBDmhE15enJlMjoeD_iFFt6cwU
device_id = MMq6cFxD0KKxxurgY8epBg
```

### base64url

```
"foobar"        -> Zm9vYmFy
"fo"            -> Zm8
ff efbf         -> _--_
```

### BLE fragments

Section 5.1. Fragment sizes include the header byte, so a size of 4 carries
three bytes of payload.

```
fragment("", 185)                -> 02
fragment("hi", 185)              -> 0268 69
fragment(000102030405060708, 4)  -> 03000102  01030405  00060708
fragment(00010203040506070809, 4)-> 03000102  01030405  01060708  0009

reassemble(03000102, 01030405, 00060708) -> 000102030405060708
```

A message that divides evenly across fragments emits no trailing empty fragment:
the last one carries payload and clears `MORE`.

### Pairing

```
normalize("abcd1234")  -> ABCD1234
normalize("IOIO-UUUU") -> 1010VVVV
normalize("l0l0 uuuu") -> 1010VVVV

psk("ABCD1234") = b64u dyRc5CXtth81rAlg0fgf1GXo8Nx8JDlXuuHcLNJnWv8

encode(0)             -> 00000000
encode(0xFFFFFFFFFF)  -> ZZZZZZZZ
```

### Derivations

With `handshake_hash = "acrylius test handshake hash"`:

```
sas         = 605 480
session_psk = b64u ETrFmyDTNXLF0pTa1DE49WPipihv9qMK60V2Bf6DAx0
```

### Envelope

`Envelope { v: 1, id: 7, re: nil, cap: "org.acrylius.ping/1", ty: "ping", body: "hi", flags: 0, bulk: nil }`

```
870107f6736f72672e616372796c6975732e70696e672f316470696e6742686900
```

33 bytes. The leading `87` is a CBOR array of seven, because the trailing nil
`bulk` is omitted.

### Hello

```
Hello {
  v: 1, ts_ms: 1700000000000, device_id: "AAAAAAAAAAAAAAAAAAAAAA",
  name: "pc", platform: "linux",
  caps_out: ["org.acrylius.ping/1"], caps_in: ["org.acrylius.ping/1"]
}
```

```
87011b0000018bcfe568007641414141414141414141414141414141414141414141
627063656c696e757881736f72672e616372796c6975732e70696e672f3181736f72
672e616372796c6975732e70696e672f31
```
