//! Manual verification of the macOS backend, through the REAL C ABI.
//!
//! It `dlopen`s the built `libastra_hotkey.dylib` exactly the way the Astra
//! daemon does, calls `hotkey_init` + `hotkey_register`, and prints every
//! `"<combo>|down"` / `"<combo>|up"` the library reports. The main thread runs
//! NO run loop (it just blocks on a channel) — the same shape as the daemon,
//! whose main thread sits in tokio's `block_on` — so a press arriving here
//! proves the library's own Carbon thread is enough.
//!
//! ```sh
//! cargo build && cargo run --example macos_hotkey                      # Ctrl+Alt+Shift+H
//! cargo run --example macos_hotkey -- "Win+Shift+Space" "Ctrl+F5"
//! cargo run --example macos_hotkey -- --self-test   # posts the key itself, exits 0/1
//! ```
//!
//! `--self-test` synthesizes the keystroke with `CGEventPost`; POSTING needs the
//! terminal to hold Accessibility permission (the library itself needs none).
//! Pass `--lib <path>` to load a specific dylib (e.g. the universal one).

#[cfg(target_os = "macos")]
fn main() {
    use std::ffi::{c_char, CStr, CString};
    use std::sync::mpsc;
    use std::sync::OnceLock;
    use std::time::Duration;

    static TX: OnceLock<std::sync::Mutex<mpsc::Sender<String>>> = OnceLock::new();

    extern "C" fn on_hotkey(keys: *const c_char) {
        let s = unsafe { CStr::from_ptr(keys) }.to_string_lossy().into_owned();
        if let Some(tx) = TX.get() {
            let _ = tx.lock().unwrap().send(s);
        }
    }
    extern "C" fn on_log(level: u8, msg: *const c_char) {
        let s = unsafe { CStr::from_ptr(msg) }.to_string_lossy();
        eprintln!("[astra_hotkey level={level}] {s}");
    }

    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let self_test = args.iter().any(|a| a == "--self-test");
    args.retain(|a| a != "--self-test");
    let lib_path = match args.iter().position(|a| a == "--lib") {
        Some(i) => {
            let p = args.get(i + 1).cloned().expect("--lib needs a path");
            args.drain(i..=i + 1);
            std::path::PathBuf::from(p)
        }
        // target/<profile>/examples/macos_hotkey → target/<profile>/libastra_hotkey.dylib
        None => std::env::current_exe()
            .unwrap()
            .parent()
            .and_then(|d| d.parent())
            .unwrap()
            .join("libastra_hotkey.dylib"),
    };
    let combos = if args.is_empty() { vec!["Ctrl+Alt+Shift+H".to_string()] } else { args };

    let (tx, rx) = mpsc::channel();
    TX.set(std::sync::Mutex::new(tx)).unwrap();

    unsafe {
        let lib = libloading::Library::new(&lib_path)
            .unwrap_or_else(|e| panic!("load {}: {e}", lib_path.display()));
        let set_log: libloading::Symbol<extern "C" fn(extern "C" fn(u8, *const c_char))> =
            lib.get(b"hotkey_set_log_callback\0").unwrap();
        let init2: libloading::Symbol<extern "C" fn(extern "C" fn(*const c_char, bool), extern "C" fn()) -> bool> =
            lib.get(b"hotkey_init2\0").unwrap();
        let init: libloading::Symbol<extern "C" fn(extern "C" fn(*const c_char)) -> bool> =
            lib.get(b"hotkey_init\0").unwrap();
        let register: libloading::Symbol<extern "C" fn(*const c_char) -> bool> =
            lib.get(b"hotkey_register\0").unwrap();
        let count: libloading::Symbol<extern "C" fn() -> u32> = lib.get(b"hotkey_count\0").unwrap();
        let backend: libloading::Symbol<extern "C" fn() -> *const c_char> =
            lib.get(b"hotkey_backend\0").unwrap();
        let shutdown: libloading::Symbol<extern "C" fn()> = lib.get(b"hotkey_shutdown\0").unwrap();

        extern "C" fn noop_act(_: *const c_char, _: bool) {}
        extern "C" fn noop_changed() {}

        set_log(on_log);
        // The daemon tries the id ABI first; on macOS it must decline.
        assert!(!init2(noop_act, noop_changed), "hotkey_init2 must return false on macOS");
        assert!(init(on_hotkey), "hotkey_init failed");
        println!("backend: {}", CStr::from_ptr(backend()).to_string_lossy());
        for c in &combos {
            let cs = CString::new(c.as_str()).unwrap();
            println!("register {c:?} -> {}", register(cs.as_ptr()));
        }
        println!("{} hotkey(s) registered.", count());

        if self_test {
            // Two full rounds (press → down+up, shutdown, re-init) so a Carbon
            // thread that cannot be restarted, or a shutdown that hangs, fails.
            let mut pass = true;
            for round in 1..=2 {
                if round == 2 {
                    assert!(init(on_hotkey), "re-init after shutdown failed");
                    let cs = CString::new(combos[0].as_str()).unwrap();
                    assert!(register(cs.as_ptr()), "re-register failed");
                }
                let posted = synth::press(&combos[0]);
                let mut got = Vec::new();
                while let Ok(ev) = rx.recv_timeout(Duration::from_secs(2)) {
                    println!("HOTKEY {ev}");
                    got.push(ev);
                    if got.len() == 2 {
                        break;
                    }
                }
                let t = std::time::Instant::now();
                shutdown();
                println!("round {round}: posted={posted}, shutdown took {:?}", t.elapsed());
                pass &= got.iter().any(|e| e.ends_with("|down")) && got.iter().any(|e| e.ends_with("|up"));
            }
            println!("self-test: {}", if pass { "PASS" } else { "FAIL" });
            std::process::exit(if pass { 0 } else { 1 });
        }

        println!("Press the combo(s); Ctrl+C to quit.");
        for ev in rx {
            println!("HOTKEY {ev}");
        }
        shutdown();
    }
}

#[cfg(target_os = "macos")]
mod synth {
    use std::ffi::c_void;

    #[link(name = "ApplicationServices", kind = "framework")]
    extern "C" {
        fn CGEventCreateKeyboardEvent(src: *const c_void, key: u16, down: bool) -> *mut c_void;
        fn CGEventSetFlags(ev: *mut c_void, flags: u64);
        fn CGEventPost(tap: u32, ev: *mut c_void);
        fn CFRelease(cf: *const c_void);
        fn AXIsProcessTrusted() -> bool;
    }

    /// Post `combo`'s key down + up with its modifier flags. Supports the
    /// default `Ctrl+Alt+Shift+H` shape (modifiers + one letter key).
    pub fn press(combo: &str) -> bool {
        let mut flags = 0u64;
        let mut key = None;
        for p in combo.split('+') {
            match p.to_ascii_lowercase().as_str() {
                "ctrl" => flags |= 1 << 18,
                "alt" => flags |= 1 << 19,
                "shift" => flags |= 1 << 17,
                "win" | "cmd" | "meta" | "super" => flags |= 1 << 20,
                "h" => key = Some(0x04u16),
                "t" => key = Some(0x11u16),
                "space" => key = Some(0x31u16),
                _ => {}
            }
        }
        let Some(key) = key else {
            eprintln!("--self-test only knows H / T / Space");
            return false;
        };
        unsafe {
            if !AXIsProcessTrusted() {
                eprintln!("note: this terminal lacks Accessibility; CGEventPost may be dropped");
            }
            for down in [true, false] {
                let ev = CGEventCreateKeyboardEvent(std::ptr::null(), key, down);
                CGEventSetFlags(ev, flags);
                CGEventPost(0, ev);
                CFRelease(ev);
                std::thread::sleep(std::time::Duration::from_millis(80));
            }
        }
        true
    }
}

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("macos_hotkey is a macOS-only example");
}
