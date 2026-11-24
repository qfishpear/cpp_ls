//! Exports the browser reactor calls. The page copies bytes into [`cpp_ls_alloc`]'s buffer after that call returns
//! (the allocation may grow and detach the host's view), then [`cpp_ls_send`] takes the buffer. [`host_emit`]
//! borrows the outbound bytes only until it returns, so the host copies them before coming back.

use std::cell::RefCell;

use lsp_server::Message;

use crate::server::browser_session::BrowserSession;

#[link(wasm_import_module = "cppls")]
unsafe extern "C" {
    fn host_emit(ptr: *const u8, len: i32);
}

thread_local! {
    static SESSION: RefCell<Option<BrowserSession>> = const { RefCell::new(None) };
}

fn emit(message: Message) {
    let bytes = serde_json::to_vec(&message).expect("an outbound message is JSON");
    let len = i32::try_from(bytes.len()).expect("an outbound message fits in an i32");
    unsafe { host_emit(bytes.as_ptr(), len) };
}

fn with_session(body: impl FnOnce(&mut BrowserSession) -> i32) -> i32 {
    SESSION.with(|slot| {
        let mut session = slot.borrow_mut();
        if session.is_none() {
            *session = Some(BrowserSession::new(emit));
        }
        body(session.as_mut().expect("the session was just installed"))
    })
}

/// Bytes the host will fill. `0` means the host must stop: a non-positive length, a layout the allocator refuses,
/// or an allocation that did not return a pointer.
#[unsafe(no_mangle)]
pub extern "C" fn cpp_ls_alloc(len: i32) -> i32 {
    if len <= 0 {
        return 0;
    }
    let Ok(layout) = std::alloc::Layout::from_size_align(len as usize, 1) else {
        return 0;
    };
    let ptr = unsafe { std::alloc::alloc(layout) };
    if ptr.is_null() { 0 } else { ptr as i32 }
}

/// Take the buffer from [`cpp_ls_alloc`] and deliver it. A null pointer or an empty length traps: `Vec` cannot be
/// rebuilt from either.
#[unsafe(no_mangle)]
pub extern "C" fn cpp_ls_send(ptr: i32, len: i32) -> i32 {
    assert!(
        ptr != 0 && len > 0,
        "cpp_ls_send needs the buffer cpp_ls_alloc returned"
    );
    let bytes = unsafe { Vec::from_raw_parts(ptr as *mut u8, len as usize, len as usize) };
    with_session(|session| session.send_owned(bytes))
}

/// Drive timers and every yield the previous turn parked.
#[unsafe(no_mangle)]
pub extern "C" fn cpp_ls_poll() -> i32 {
    with_session(|session| session.poll())
}
