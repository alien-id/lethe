use std::ffi::c_char;
use std::sync::Once;

use rusqlite::ffi::{sqlite3, sqlite3_api_routines, sqlite3_auto_extension};
use sqlite_vec::sqlite3_vec_init;

static REGISTER: Once = Once::new();

// `c_char` resolves to `i8` on x86_64-linux and `u8` on aarch64-linux —
// matching whatever the platform's sqlite3 headers expose. Hard-coding
// either side would break the other arch.
type SqliteExtensionInit =
    unsafe extern "C" fn(*mut sqlite3, *mut *mut c_char, *const sqlite3_api_routines) -> i32;

pub fn register() {
    REGISTER.call_once(|| unsafe {
        sqlite3_auto_extension(Some(std::mem::transmute::<*const (), SqliteExtensionInit>(
            sqlite3_vec_init as *const (),
        )));
    });
}
