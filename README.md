# rift-bot

Write a bot for [Rift](https://joinrift.app) in Rust: commands, replies, live
panels, DMs, and voice in stereo, against any self-hosted Rift server.

A bot is an ordinary member whose identity lives in a file instead of a phone.
It signs in the same way, sits under the same permissions, and can never read
more than it has been granted. Channels stay end-to-end encrypted; a bot only
reads a channel if an admin hands it that channel's key.

## Quick start

```toml
[dependencies]
rift-bot = "0.1"
tokio = { version = "1", features = ["rt-multi-thread", "macros", "signal"] }
```

```rust
use rift_bot::{Bot, BotSession, SessionConfig, load_seed};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // `join` (examples/join.rs) writes these two files the first time.
    let config: SessionConfig = serde_json::from_str(&std::fs::read_to_string("bot.json")?)?;
    let session = BotSession::from_config(&config, &load_seed("bot.seed")?)?;

    let bot = Bot::new(session);
    let replier = bot.clone();
    bot.listen(move |message| {
        let bot = replier.clone();
        async move {
            if message.command() == "echo" {
                bot.reply(&message, message.args()).await?;
            }
            Ok::<(), rift_bot::BotError>(())
        }
    })
    .await?;

    tokio::signal::ctrl_c().await?;
    Ok(())
}
```

The seed is 32 random bytes and **is the bot**: whoever holds it can act as it,
so keep it out of version control. It is the same file the TypeScript SDK
writes, so a bot can move between the two and stay itself. An invite is spent
once used, so `join` saves the session's config beside the seed.

| Example | Shows |
|---|---|
| [`examples/join.rs`](examples/join.rs) | joining a server with an invite, once |
| [`examples/echo_bot.rs`](examples/echo_bot.rs) | a command bot |
| [`examples/panel_bot.rs`](examples/panel_bot.rs) | a poll on a panel that redraws itself |
| [`examples/dm_bot.rs`](examples/dm_bot.rs) | answering direct messages |
| [`examples/music_bot.rs`](examples/music_bot.rs) | joining a call and playing into it |
| [`examples/watch_bot.rs`](examples/watch_bot.rs) | reading a channel it has been granted |

```bash
cargo run --example join -- '<invite link>' mybot bot.seed bot.json
cargo run --example echo_bot -- bot.json bot.seed
cargo run --features voice --example music_bot -- bot.json bot.seed
```

## What a bot can do

- **Commands**: `/name args`, offered to members as they type, with suggestions
- **Replies**: public, or visible only to whoever asked
- **Panels**: a message the bot keeps redrawing, with buttons, menus and pictures
- **DMs**: conversations with members, private from the server too
- **Reading a channel**: only one an admin has granted it
- **Voice**: play into a call, in stereo; hear one only if an admin allows it

The [TypeScript SDK's guide](https://github.com/awais-amjed/rift-bot-sdk-node/blob/dev/docs/GUIDE.md)
explains each of these; the calls here have the same names in snake case.

## Voice

Behind the `voice` feature, so a text bot never builds a WebRTC stack:

```toml
rift-bot = { version = "0.1", features = ["voice"] }
```

The first build downloads LiveKit's prebuilt WebRTC library. On Linux the
glue around it needs **clang 21 or newer** (set `CC` and `CXX` to it) and
GLib's headers (`libglib2.0-dev` on Debian); Debian's own clang is too old,
so a Docker build takes clang from [apt.llvm.org](https://apt.llvm.org). A bot plays
16-bit PCM at 48 kHz, two channels interleaved, from anything readable; an
`ffmpeg` child's stdout is the usual source. `Quality::Music` sends 128 kbps
stereo.

Calls are stereo because this crate builds on our fork of LiveKit's Rust SDK
with one change: the published SDK never tells the server a two-channel track
is stereo, so every bot built on it (LiveKit's Node and Python SDKs included)
is mono. The fork is on crates.io as
[`joinrift-livekit`](https://crates.io/crates/joinrift-livekit), and
[`RIFT.md`](https://github.com/awais-amjed/livekit-rust-sdks/blob/rift/RIFT.md)
on its `rift` branch has the details. This crate moves back to LiveKit's own
once LiveKit ships the fix. The TypeScript SDK gets the same fix through
`@joinrift/rtc-node`, LiveKit's Node SDK on bindings built from the fork.

Listeners hear the stereo on Rift 1.5.0-beta.1 or newer; an older Rift plays it in
mono.

Members see the bot come and go. `join_voice` tells the server's members which
channel the bot is in, and the connection tells them again when it closes, for
any reason, the way a member's app does when it moves. Before 0.1.1 a bot said
nothing, and a member's sidebar could keep drawing it in a call it had left
until that member restarted Rift. `leave()` returns once they have been told
(five seconds at most).

## Documentation

| Document | For |
|---|---|
| [`WIRE.md`](WIRE.md) | the exact formats this crate implements, copied from the Rift app |
| `cargo doc --open --features voice` | every type and call |

## Tests

```bash
cargo test --features voice
```

`tests/wire.rs` checks this crate byte for byte against test vectors generated
from the Rift app's own crypto. The implementations never read each other's
code, so agreeing on the vectors is what "compatible" means. The app's
`tool/gen_wire_vectors.dart` writes this repository's copy of the vectors and
of `WIRE.md`.

One test plays into a real LiveKit server and checks a listener hears each ear
apart. It needs the server's credentials:

```bash
LIVEKIT_URL=ws://localhost:7880 LIVEKIT_API_KEY=… LIVEKIT_API_SECRET=… \
  cargo test --features voice live_ -- --ignored --nocapture
```

## Related repositories

| Repository | What it is |
|---|---|
| [`rift`](https://github.com/awais-amjed/rift) | the app: Flutter client for desktop, mobile and web |
| [`rift-self-host`](https://github.com/awais-amjed/rift-self-host) | a server anyone can run, where a bot lives |
| [`rift-bot-sdk-node`](https://github.com/awais-amjed/rift-bot-sdk-node) | the TypeScript SDK for building bots |
| **`rift-bot-sdk-rust`** | this: the Rust SDK for building bots |

## License

[Apache-2.0](LICENSE).
