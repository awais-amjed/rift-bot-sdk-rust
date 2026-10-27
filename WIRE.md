# WIRE.md — the formats a second implementation has to match

Everything here is a contract. Third-party bots and any SDK that is not the
Dart one read this file, and `test/wire_vectors.json` is the same contract as
numbers you can check against.

Nothing here is a *decision* — the reasoning for all of it is in
`ARCHITECTURE.md` §1 and §4, and this document deliberately does not repeat it.
This is the part somebody porting the client has to get byte-identical.

## Why the vectors exist

Two implementations of a canonical payload are two things that can disagree,
and the disagreement does not look like an error. A message signed over a
payload that differs by one character stores fine, verifies as false, and
renders as nothing — the sender sees it send, and nobody else ever sees it.

So the format is frozen as data. The vectors were generated from the Dart
implementation and then checked in, which means they **catch drift, not
original error**: they cannot tell you the format was right on the day it was
written. What they can tell you is that it has not moved since, and that a
second implementation agrees with the first — which is the failure that
actually happens.

`test/wire_test.dart` checks the app against them on every run.

---

## 1. Encodings

| Where | Encoding |
|---|---|
| Keys, signatures, nonces, ciphertext on the wire | **base64**, standard alphabet, padded (`dart:convert` `base64Encode`) |
| The SIWS address only | **base58**, Bitcoin/Solana alphabet, leading zero bytes each emitted as `1` |
| Everything hashed or signed | **UTF-8** bytes of the string given |

## 2. The key ladder

One 32-byte master seed. Everything else is `HMAC-SHA256(seed, context)`, where
the context string is domain-separating and the output is 32 bytes.

```
vaultKey     = HMAC-SHA256(seed, "vault:v1")
serverSeed   = HMAC-SHA256(seed, "<scope>:<version>")     → Ed25519 seed
stableId     = HMAC-SHA256(seed, "<scope>:identity")      → base64, as-is
chatSeed     = HMAC-SHA256(seed, "<host>:chat:<version>") → X25519 seed
cacheKey     = HMAC-SHA256(seed, "local-cache/messages:v1")
```

`cacheKey` seals the conversations a device keeps a copy of. Nothing else ever
reads it, so a port need not implement it. It is listed so that a new context
string is not chosen to collide with it.

`<scope>` is `"<host>:<serverId>"` for a self-hosted server and bare `"<host>"`
for the central host. `<version>` is `v1`.

Two things a port gets wrong:

- **`host` is the URL's host only** — no scheme, no port, no path.
  `https://chat.example.org:8443/rift` gives `chat.example.org`.
- **The chat identity is not scoped to a server.** It is per host, and pinned at
  `v1` for good: everything ever sealed to somebody is opened with it, so a new
  version would make all of it unreadable. The sign-in key's `<version>` is `v1`
  too, and nothing rotates either.

An Ed25519 or X25519 keypair is built from those 32 bytes as a **seed**, not as
a private scalar — RFC 8032 for Ed25519, RFC 7748 clamping for X25519. Most
libraries call this `fromSeed` or `newKeyPairFromSeed`.

## 3. The signed payload

Every message carries an Ed25519 signature over exactly this string:

```
chatmsg:v1:<contextId>:<keyVersion>:<nonce>:<ciphertext>
```

Joined with `:` and nothing else — no length prefixes, no canonical JSON. The
fields are the base64 strings as they appear on the row, not the bytes they
decode to.

`contextId` is:

| Kind | contextId |
|---|---|
| Channel message | the channel's UUID |
| DM | `dm:<idA>:<idB>`, the two user ids **sorted lexicographically** |

The sort is what lets both sides derive the same context without agreeing on
anything first, and what stops an envelope being replayed into a different
conversation.

A message a bot writes is **signed but not sealed**: `keyVersion` is `0`, the
nonce is the empty string, and `ciphertext` holds plaintext. The payload is
built the same way — `chatmsg:v1:<channel>:0::<text>` — with an empty field
between the two colons.

Clients drop what they cannot verify, so an unsigned or wrongly-signed message
is an invisible one.

## 4. Sign-in

`login` takes a SIWS message and its base64 signature. The message is built
from a fixed template:

```
localhost wants you to sign in with your Solana account:
<base58 public key>

Sign in to Rift.

URI: http://localhost
Version: 1
Chain ID: solana:mainnet
Nonce: <alphanumeric>
Issued At: <ISO-8601 UTC>
```

`localhost` and `http://localhost` are **constants, not the server's address**,
for the reason in `crypto_repository_identity.dart`: GoTrue's web3 grant
refuses most real addresses, and these two fields exist so a wallet can tell
you which site is asking — Rift has no wallet. The key is derived per
`(host, serverId)` and posted only to the host it was derived for.

`Nonce` is any alphanumeric string; `Issued At` is the current UTC time. Both
vary per login, which is why this one has no vector — what is frozen is the
template, and `wire_vectors.json` pins the address encoding it embeds.

## 5. Panels

`messages.blocks` holds `{"v":1,"blocks":[…]}`. Each block is an object with a
`type`, and **anything not in this list is not drawn** — that is the safety
property, not a limitation of the first version. A bot never controls a pixel,
only a structure.

| `type` | Fields | Draws |
|---|---|---|
| `heading` | `text` | a title line |
| `text` | `text` | a paragraph |
| `fields` | `items: [{label, value}]` | key/value rows |
| `progress` | `value` (0–1, clamped), `text` | a bar with a caption |
| `divider` | — | a rule |
| `actions` | `items: [{label, action, style?}]` | a row of buttons |
| `select` | `action`, `text?`, `options: [{label, value?}]` | a menu |
| `image` | `path`, `sha256`, `width?`, `height?`, `text?` | a picture |

`style` is `primary`, `danger`, or absent. It is a **weight, not a colour** —
a bot cannot paint a button into looking like part of Rift's own chrome.

A button with no `label` or no `action` is dropped rather than half-drawn, and
a block with nothing in it is dropped too: an empty row reads as a bug.

Pressing one writes a row addressed to the bot with `is_interaction` true,
`reply_to` naming the panel, `action_id` the button's own id and `action_value`
a menu option's value. It is signed like a command, because it is attributed —
the bot is told who pressed. It is **not a message**: no view renders it, no
phone rings for it, and it does not count as unread.

A panel may carry `"live": true` beside `v` and `blocks`
(`{"v":1,"live":true,"blocks":[…]}`): the bot says this panel is its current
one, a player while it plays. Clients then draw it in the bar above the
channel's message box instead of in the history, **and only while its bot is
connected** — a bot that crashed cannot take the mark back. Only the JSON
boolean counts; `"true"`, a string, is not live. The bot drops the key (or
writes `false`) on its last redraw, and the panel is drawn back in the history
where it was posted, as that redraw says. A server finds them with
`channel_live_messages`, so a live panel is in the bar however far up the chat
it was posted. The blocks are the same eight; nothing else changes.

Clients show **one live panel per bot**, its newest. A bot that was killed
never cleared its last one, so on start it should redraw any panel it left
live without the mark — it can read its own rows, `sender_id` itself and
`blocks->live` true — before posting a new one; until it does, its old player
would be back in the bar the moment it reconnects.

The bot redraws by writing `blocks` again on the same row and ringing
`message_changed` with its id. Ringing `new_message` makes clients fetch what is
*newer* than they have, and a redraw is not newer than anything.

An `image` is never a URL. `path` names an object in the server's own
attachment bucket (`chat-<server id>`), uploaded unencrypted by the bot:
`<channel id>/<name>.<png|jpg|jpeg|webp|gif>`, where the channel is the one the
panel is in and the name is letters, digits, `_` and `-`. A block naming any
other channel, a folder below it, or any other kind of file is dropped.
`sha256` is the base64 SHA-256 of the bytes (44 characters); a download that
does not match is not drawn. `width` and `height` are the picture's size in
pixels, 1 to 16384, kept so the panel holds room for it before it arrives;
anything else is ignored. `text` describes it for a screen reader. Why it is
built this way is in BOTS.md §5.

---

## 6. Voice

A channel's calls are end-to-end encrypted with the channel's own key. Three
things a second implementation has to match, and each of them fails silently if
it does not.

### The sealed key — `wrap:v1`

A member seals a key to somebody's X25519 chat identity:

```
shared      = X25519(ephemeralPrivate, recipientPublic)
wrappingKey = HMAC-SHA256(shared, "wrap:v1")
sealed      = AES-256-GCM(base64(key), wrappingKey, nonce)   // ciphertext ‖ tag
```

On the wire: `ephemeral_public_key`, `ciphertext` (with the 16-byte GCM tag
appended) and `nonce`, all base64.

**The plaintext inside the envelope is the key's base64, not its bytes.** The
Dart side seals a string, so a port that decodes once gets 44 bytes of ASCII
where it wanted 32 and fails at the frame cryptor, a long way from here.

`wrapped_key` in the vectors is a frozen blob rather than a reproducible one —
the ephemeral half is random, so there are no bytes to compare. A port proves
itself by *opening* it, which is the half a bot actually does anyway.

### The key a bot speaks with

```
botKey = HMAC-SHA256(channelKey, "voicebot:v1:<botId>")
```

`<botId>` is the bot's `users.id`. Members compute this and hear the bot; the
bot is handed only the result, and cannot invert it to reach `channelKey`. That
asymmetry is the whole of "publishes but does not listen" — see BOTS.md §6b.

### A DM's key

```
dmKey = HMAC-SHA256(X25519(myChatPrivate, theirChatPublic), "dm:v1")
```

Both ends derive the same bytes from opposite halves, so there is nothing to
distribute — and no round trip in which two implementations could notice they
disagree. The message itself is sealed and signed like any other, with the
`contextId` from §3's DM row.

Not voice, strictly, but it is the other thing a non-Dart client derives rather
than is given, and `dm_key` in the vectors pins it.

### Which slot a key goes in

LiveKit addresses keys by a slot in a fixed-size ring, not by Rift's version
number, so the two are mapped:

```
keyIndex = keyVersion % 16
```

Every client has to agree. A sender encrypting into a slot its listeners do not
read is a call where everybody connects, every track publishes, and nobody hears
anyone — with no error reported anywhere. It is the most quietly broken thing in
this document, which is why it is in it.

**A bot is the exception, and always encrypts in slot 0** — whatever version its
key is:

```
keyIndex = 0        for a bot's own media
keyIndex = keyVersion % 16   for a member's
```

That is not a preference, it is what an SDK can do. A frame cryptor is created
when its track is published and keeps the index it was born with; moving it
needs `FrameCryptor.setKeyIndex`, which throws in `@livekit/rtc-node` because
the request it builds omits a `track_sid` the native side requires. So the rule
is the slot, and members read a bot's key from 0 for exactly this reason
(`livekit_e2ee.dart` in the app, `src/voice.ts` in the SDK).

A port that applied the arithmetic above to a bot's key would put it in slot 1
on a channel at version 1 and the bot would be inaudible — which is the failure
this section exists to prevent, arrived at by following this section.

---

## 7. Suggestions

A command the manifest marks `"suggest": true` asks its bot what to offer while
somebody types it — `/play thats so tr` lists songs above the composer. Both
halves are Realtime broadcasts sent with `POST /realtime/v1/api/broadcast` and
`private: true`; nothing is written to a table.

The member's client sends `bot_suggest` to `user:<bot id>`, which any co-member
may send to and only the bot hears:

```
{ "v": 1, "id": "<8–64 of A–Z a–z 0–9 _ ->", "from": "<asker's user id>",
  "channel": "<channel id>", "command": "play", "text": "thats so tr" }
```

`command` is the lower-case name without its slash; `text` is everything after
`/play `, up to 200 characters. The bot answers `bot_suggestions` on
`user:<from>`:

```
{ "v": 1, "id": "<the request's id>", "bot": "<bot id>",
  "items": [ { "label": "That's So True — Gracie Abrams · 2:47",
               "value": "https://example.com/songs/thats-so-true" } ] }
```

At most ten items; a `label` of at most 100 characters and a `value` of at most
500 with no line break. Picking one sends `/<command> <value>` as an ordinary
command, signed and addressed like anything typed.

**Neither side is vouched for by the server**, and the rules follow from that.
`from` is the asking client's word, so a bot only looks things up for a
suggestion and acts on nothing until the command itself arrives. The answer
is accepted only when its `id` is one this client sent and has not had answered:
nobody else hears `user:<bot id>`, so nobody else knows the id, and an answer
naming any other id is dropped. A request sent while an earlier one from the
same asker is still being worked out replaces any waiting one, so only the
newest is answered next.

What somebody types after the command goes to the bot before they press Enter,
unencrypted, like the command it becomes. The client asks only once a
`suggest` command and a space have been typed, so nothing reaches the bot
before the person has chosen to talk to it.

---

## 8. Attachment files

A file sent with a message is an object in the server's attachment storage,
and everything needed to read it travels in the message's body, under `att`
(the body itself is not frozen here yet). The server holds the bytes and
nothing that opens them. Each entry:

| Key | Holds |
|---|---|
| `id`, `kind`, `name`, `mime` | the file's id in the message, `image` / `audio` / `file`, its name and type |
| `size` | the **plaintext** length in bytes |
| `path` | the object in the server's attachment bucket; which folder a member may write is the server's rule (`API.md`), not the format's |
| `key`, `nonce` | base64: a 32-byte key, random per file, and a 12-byte nonce — or a 7-byte prefix when `chunk` is set |
| `chunk` | present only when the file is sealed a chunk at a time: the plaintext bytes per chunk |
| `plain`, `sha256` | present only when the file was sent unencrypted: `true`, and the base64 SHA-256 of its bytes |
| `w`, `h`, `dur` | optional: a picture's size, a recording's length in ms |

**In one piece** (no `chunk`): the object is `AES-256-GCM(file, key, nonce)`
with no associated data and the 16-byte tag after the ciphertext.
`attachment` in the vectors pins it.

**A chunk at a time** (`chunk` set), STREAM from the `aead-stream` crate. The
file is cut into `chunk`-byte pieces, the last shorter — an empty file is one
empty chunk, so there is always a last one — and piece *n* is sealed on its own:

```
nonce(n) = prefix ‖ BE32(n) ‖ (0x01 if n is the last chunk, else 0x00)   // 12 bytes
chunk(n) = AES-256-GCM(piece n, key, nonce(n))                           // tag appended, no AAD
object   = chunk(0) ‖ chunk(1) ‖ … ‖ chunk(last)
```

Nothing about the layout is written into the object. Everything follows from
`size` and `chunk`: there are `max(1, ceil(size / chunk))` chunks, chunk *n*
starts at `(chunk + 16) × n`, and the object is `size + 16 × count` bytes. A
reader refuses a `chunk` outside 4 KiB to 16 MiB, since it holds a whole chunk
before it can open one, and a file whose chunks run out before the last.
Because the position and the last flag are in the nonce, a server that reorders,
drops or cuts off chunks makes a tag fail. Rift seals 1 MiB chunks.
`chunked_attachment` pins three 4 KiB chunks by the SHA-256 of the whole object,
with each chunk's nonce and the short last chunk in full.

**Unencrypted** (`plain: true`): the object is the file as it is, and a reader
shows it only if its SHA-256 is `sha256`. `key` and `nonce` are still written,
as empty strings, so a client from before this fails to open the file rather
than losing the message.

---

## Out of scope

The channel keyring itself, the links that chain its versions
(`ARCHITECTURE.md` §4, *The key chain*) and the DM key derivation are between
clients that hold keys; a bot holds one only for voice, and only its own. They are in
`ARCHITECTURE.md` §4 and are not frozen here yet — the moment a non-Dart client
needs to *read* a channel rather than be spoken to, they belong here too.

The same goes for a one-to-one call's media key, `HMAC-SHA256(dmKey,
"dmcall:v1:<callId>")` in slot 1: only the two members derive it, and a bot is
never on a DM call (`ARCHITECTURE.md` §5).
