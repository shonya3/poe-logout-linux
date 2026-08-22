# PoE logout for Linux

A Linux alternative to [Lutbot's](https://github.com/5k-mirrors/poe-lutbot-ahk) Windows AutoHotkey logout macro.

Pressing `~` instantly kills the game's live TCP connection.

## Install

Download from the [latest release](https://github.com/shonya3/poe-logout-linux/releases/latest):

```bash
chmod +x poe-logout-linux
./poe-logout-linux
```

## Requirements

- Linux with `ss` from iproute2 (preinstalled on virtually every distro)
- Rust toolchain (`cargo`) — only for building
- `sudo` rights at runtime (destroying sockets needs root)


## Testing without pressing keys

```bash
cargo run --release -- --test    # performs one logout pass immediately, then exits
```

## Changing the hotkey
  
One line in `src/main.rs`:

```rust
const HOTKEY: Key = Key::KEY_GRAVE;   // ~ ; e.g. Key::KEY_SCROLLLOCK, Key::KEY_F13
```

Rebuild afterwards. The daemon listens on raw evdev events, so it works regardless of keyboard layout, and does *not* swallow the key — the game also sees the press.

## How it decides what to kill

Every hotkey press runs this check:

1. find processes whose `argv[0]` ends with `.exe` and whose command line mentions `pathofexile` (zombies skipped) → their PIDs
2. collect the kernel **socket inode numbers** those processes own (`/proc/<pid>/fd`)
3. keep established connections from `/proc/net/tcp{,6}` matching those inodes, excluding loopback peers and well-known ports (<1024)
4. destroy each surviving remote address with `ss -K dst :<port>`, then re-count connections to verify the kill landed

Fallback if step 1–3 find nothing: sweep all connections to ports 6113/6112.

Known limitation: Steam's wine-side helper (`steam.exe`) matches step 1, so its high-port Valve connections may be killed too — harmless, they reconnect.

## Notes

- Works with both the standalone client and Steam/Proton.
