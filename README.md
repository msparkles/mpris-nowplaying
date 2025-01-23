# mpris-nowplaying

[crates.io](https://crates.io/crates/mpris-nowplaying)

A websocket based [MPRIS2](https://specifications.freedesktop.org/mpris-spec/latest/) "now-playing" / status client.

Main application is embedding a now-playing dock in your streams, but it isn't limited to that.

Written in Rust.

**Default bound IP is 127.0.0.1:32100**

## Why?

Iunno, the Media Session doesn't seem to work for us, maybe it uses MPRIS1, we have no idea the difference.

We wanted to have a Now Playing thing in our stream (via OBS) aligned to what's playing in our media player.

## Example HTML

Look in `/examples`.

## API

The returned message from the bound WebSocket address is similar to that of a [Media Session](https://developer.mozilla.org/en-US/docs/Web/API/MediaSession) JSON.

The JSON looks roughly like:

```
mediaSession: {
    metadata: {
        title: string,
        artist: string,
        album: string,
        artwork: {
            src: string, // whatever the music app returns, can be a local path
        }[],
        length: u64, // unit: microseconds, the media's length in time
    }
    playbackState: "playing" | "paused" | "none",
    position: u64 // unit: microseconds, the current playback position
}
```

You can get it from the websocket stream by default without sending anything special.

If you send in `artwork/<index>`, the server will respond with the artwork image blob at the index if it is a local file, or the remote link itself, or `null` if it's been requested already and there isn't a new one.