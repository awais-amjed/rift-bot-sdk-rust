# livekit, kept here and patched

LiveKit's `livekit` 0.9.1, as published on crates.io (from
[livekit/rust-sdks](https://github.com/livekit/rust-sdks), commit `2ff180e4`,
Apache-2.0 — `LICENSE` is the repository's, which the published crate leaves
out). The whole crate is kept, less cargo's `.cargo-ok` marker. The SDK's
`Cargo.toml` depends on it by path, under the `voice` feature. A path, and not
`[patch.crates-io]`, because a patch in a library's manifest does not reach the
bots that depend on it.

The one change is marked `RIFT PATCH`; `git diff` against the commit that
brought the copy in shows it. It is the same change the Rift app carries in its
own copy.

## Why

A bot playing music publishes two channels, but every listener heard it in
mono. The LiveKit server writes `stereo=1` into its answer for an audio track
only when the track's `AddTrackRequest` lists `TF_STEREO`; without it, Opus
encodes one channel. The browser SDK lists `TF_STEREO` for a stereo track. The
Rust SDK never does, so no track published from Rust can be stereo
(livekit/rust-sdks issue #1018). LiveKit's Node and Python SDKs run on the same
Rust code, which is why the TypeScript SDK's bots are mono too.

An unencrypted connection in single peer connection mode gets stereo anyway,
because the SDK writes `stereo=1` into its own offer; an encrypted one, which
every Rift call is, does not.

Measured Oct 13 2026 against the local stack's LiveKit with
`live_a_bot_plays_into_a_call_in_stereo` (`src/voice_live_test.rs`): with the
change the bot sent `stereo=1` and a tone in one ear arrived in that ear alone;
without it the bot sent no `stereo=1` and the tone came out of both.

## The patch

1. **`publish_track` lists `TF_STEREO` for an audio track whose source has two
   or more channels** (`src/room/participant/local_participant.rs`). The same
   change as LiveKit rust-sdks PR #1023.

A stereo track also needs a stereo bitrate, which is the caller's to choose:
`src/voice.rs` publishes music at 128 kbps.

## When to remove it

When a `livekit` release lists `TF_STEREO` for a stereo source (#1023 or an
equivalent): depend on that release from crates.io and delete this folder.
That is also what lets the SDK be published to crates.io, which refuses a
dependency it cannot fetch itself.
