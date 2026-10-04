# AstraHotkey

Native global hotkey service for [Astra AI assistant](https://github.com/misha/Astra).

## Privacy Guarantee

**This library does NOT capture, log, store, or transmit any keyboard input.**

The source code is publicly available so users can verify:
- Only **registered** hotkey combinations trigger callbacks
- No keystrokes are sent to Astra or any external service
- No data collection occurs

## How It Works

1. Astra registers specific hotkey combinations (e.g., `Ctrl+Shift+T`)
2. This library installs a low-level keyboard hook
3. When keys are pressed, they are compared against registered hotkeys using O(1) HashSet lookup
4. **Only matching hotkeys** invoke the callback to Astra
5. Non-matching keypresses are immediately passed through with no processing

```
┌─────────────────┐                    ┌──────────────────┐
│   Astra C#      │     P/Invoke       │  astra_hotkey    │
│                 │ ←────(~50ns)────→  │     .dll/.so     │
│                 │                    │                  │
│ hotkey_register │ ──────────────────→│ HashSet<Keys>    │
│ "Ctrl+Shift+T"  │                    │   ↓              │
│                 │                    │ Keyboard hook    │
│ OnHotkey()      │ ←── callback ───── │ if match: fire   │
└─────────────────┘                    └──────────────────┘
```

## API

```c
// Initialize with callback function
bool hotkey_init(void (*callback)(const char* keys));

// Register a hotkey combination
bool hotkey_register(const char* keys);  // "Ctrl+Shift+T"

// Unregister a hotkey
bool hotkey_unregister(const char* keys);

// Unregister all hotkeys
void hotkey_unregister_all();

// Get count of registered hotkeys
uint32_t hotkey_count();

// Shutdown and cleanup
void hotkey_shutdown();
```

## Hotkey Format

Modifiers: `Ctrl`, `Alt`, `Shift`, `Win` (or `Meta`, `Super`, `Cmd`)

Keys: `A-Z`, `0-9`, `F1-F12`, `Enter`, `Escape`, `Space`, `Tab`, etc.

Examples:
- `Ctrl+T`
- `Ctrl+Shift+T`
- `Alt+F4`
- `Win+E`
- `Ctrl+Alt+Delete`

## Building

### Prerequisites

- Rust 1.70+ (install via [rustup](https://rustup.rs))
- Windows: Visual Studio Build Tools
- Linux: `libx11-dev` package
- macOS: Xcode Command Line Tools; for the universal build also
  `rustup target add aarch64-apple-darwin x86_64-apple-darwin`

### Build Commands

```bash
# Windows (produces astra_hotkey.dll)
cargo build --release

# Linux (produces libastra_hotkey.so)
cargo build --release

# macOS (produces libastra_hotkey.dylib for the host arch)
cargo build --release

# macOS universal (Apple Silicon + Intel, one file)
scripts/build-macos-universal.sh

# Cross-compile for specific targets
cargo build --release --target x86_64-pc-windows-msvc
cargo build --release --target x86_64-unknown-linux-gnu
```

### Output

- Windows: `target/release/astra_hotkey.dll`
- Linux: `target/release/libastra_hotkey.so`
- macOS: `target/release/libastra_hotkey.dylib`, universal:
  `target/universal-apple-darwin/release/libastra_hotkey.dylib`
  (install name `@rpath/libastra_hotkey.dylib`)

## macOS backend

Carbon `RegisterEventHotKey` (`src/hook_macos.rs`). **It needs no permission** —
no Accessibility, no Input Monitoring: the WindowServer matches the registered
combo and only that combo is reported, so the privacy guarantee above holds
literally. Both edges are reported (`"<combo>|down"` / `"<combo>|up"`), so
push-to-talk works.

- **ABI:** the legacy combo ABI (`hotkey_init` + `hotkey_register`), like
  Windows. `hotkey_init2` returns `false`; `hotkey_backend()` returns `"macos"`.
- **Threading:** the library runs its own `astra-hotkey-carbon` thread with the
  Carbon application event loop; the host needs no main-thread run loop and does
  not have to be an app (works in a daemon whose main thread is blocked). The
  callback fires on that thread.
- **Modifiers:** `Ctrl` → Control ⌃, `Alt` → Option ⌥, `Shift` → ⇧,
  `Win`/`Meta`/`Super`/`Cmd` → Command ⌘ (literal; no Ctrl→Cmd rewriting).
- **Not supported (register returns `false`):** mouse buttons (`MOUSE2..5`),
  modifier-only combos, and keys Macs lack (`Insert`, `PrintScreen`, `Pause`,
  `ScrollLock`, `NumLock`, `CapsLock`). These would need a CGEventTap, which
  needs Input Monitoring — deliberately not used.
- **Manual check:** `cargo build && cargo run --example macos_hotkey` (press
  ⌃⌥⇧H), or `-- --self-test` to post the keystroke itself (posting needs the
  terminal to hold Accessibility; the library does not).

The `astra-hotkey-consent` helper is Linux/Hyprland-only and is not built on macOS.

## Integration with Astra

Place the compiled library in Astra's runtime directory:
- Windows: `runtimes/win-x64/native/astra_hotkey.dll`
- Linux: `runtimes/linux-x64/native/libastra_hotkey.so`
- macOS: `libastra_hotkey.dylib` beside the daemon executable

## License

Source Available License - see [LICENSE](LICENSE) for details.

This software can be viewed and audited but may only be used with Astra.

## Security

If you discover a security vulnerability, please report it by creating an issue or contacting the maintainers directly.
