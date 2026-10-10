//! TDLib's JSON interface, loaded at runtime from `libtdjson.so`.
//!
//! The whole C surface is four functions: create a client id, send a JSON request to it,
//! receive the next JSON response or update (for every client, from one thread only), and
//! execute the few requests that are synchronous. Loading it with dlopen rather than linking
//! means `cargo build` works on a machine without TDLib; the app reports it missing instead.
use std::ffi::{CStr, CString, c_char, c_double, c_int};
use std::path::PathBuf;
use std::sync::OnceLock;

type CreateClientId = unsafe extern "C" fn() -> c_int;
type Send = unsafe extern "C" fn(c_int, *const c_char);
type Receive = unsafe extern "C" fn(c_double) -> *const c_char;
type Execute = unsafe extern "C" fn(*const c_char) -> *const c_char;

pub struct Td {
    _lib: libloading::Library,
    create_client_id: CreateClientId,
    send: Send,
    receive: Receive,
    execute: Execute,
}

/// Where to look, in order: `$AVRO_TDJSON`, the `build-tdlib.sh` install, then the system.
fn candidates() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::env::var_os("AVRO_TDJSON").map(PathBuf::from).into_iter().collect();
    if let Some(b) = directories::BaseDirs::new() { v.push(b.home_dir().join(".local/lib/libtdjson.so")); }
    v.extend(["/usr/local/lib/libtdjson.so", "/usr/lib/libtdjson.so"].map(PathBuf::from));
    v
}

fn load() -> Result<Td, String> {
    let mut tried = Vec::new();
    // A bare soname last, so LD_LIBRARY_PATH and the loader cache still get a say.
    for p in candidates().into_iter().map(|p| p.into_os_string()).chain([std::ffi::OsString::from("libtdjson.so")]) {
        // SAFETY: loading TDLib runs its static initialisers, which have no preconditions.
        match unsafe { libloading::Library::new(&p) } {
            Ok(lib) => unsafe {
                let td = Td {
                    create_client_id: *lib.get::<CreateClientId>(b"td_create_client_id\0").map_err(|e| e.to_string())?,
                    send: *lib.get::<Send>(b"td_send\0").map_err(|e| e.to_string())?,
                    receive: *lib.get::<Receive>(b"td_receive\0").map_err(|e| e.to_string())?,
                    execute: *lib.get::<Execute>(b"td_execute\0").map_err(|e| e.to_string())?,
                    _lib: lib,
                };
                tracing::info!("loaded TDLib from {}", p.to_string_lossy());
                return Ok(td);
            },
            Err(e) => tried.push(format!("{}: {e}", p.to_string_lossy())),
        }
    }
    Err(format!("libtdjson.so not found — run ./build-tdlib.sh, or point AVRO_TDJSON at it.\nTried:\n  {}", tried.join("\n  ")))
}

/// The process-wide TDLib, loaded on first use.
pub fn td() -> Result<&'static Td, &'static str> {
    static TD: OnceLock<Result<Td, String>> = OnceLock::new();
    TD.get_or_init(load).as_ref().map_err(|e| e.as_str())
}

/// Copy a TDLib-owned string out before the next call into TDLib invalidates it.
unsafe fn owned(p: *const c_char) -> Option<String> {
    if p.is_null() { None } else { Some(unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()) }
}

impl Td {
    pub fn create_client_id(&self) -> i32 { unsafe { (self.create_client_id)() } }

    pub fn send(&self, client_id: i32, request: &str) {
        let Ok(c) = CString::new(request) else { tracing::error!("request with a NUL byte dropped"); return };
        unsafe { (self.send)(client_id, c.as_ptr()) }
    }

    /// Next response or update for any client, or `None` after `timeout` seconds of quiet.
    /// Not thread-safe: only the receive thread in `client.rs` may call this.
    pub fn receive(&self, timeout: f64) -> Option<String> { unsafe { owned((self.receive)(timeout)) } }

    /// Synchronous requests only (logging setup, text parsing). Thread-safe.
    pub fn execute(&self, request: &str) -> Option<String> {
        let c = CString::new(request).ok()?;
        unsafe { owned((self.execute)(c.as_ptr())) }
    }
}
