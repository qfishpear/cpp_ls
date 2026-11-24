//! A whole-program read handed to the other Wasm instance.
//!
//! The reactor is one thread. `read_the_program` is a synchronous cook and parse, so running it inside
//! `cpp_ls_poll` is the multi-second turn a completion waits behind. The host import copies this request and
//! returns; the task stays pending until [`cpp_ls_program_done`] stores the bytes and wakes it. The next poll
//! commits them. Queries keep using this instance: nothing here changes `rt::offload`.

use std::path::{Path, PathBuf};

use cpp_code_analysis::cache::unit_codec::{decode_indexed_unit, encode_indexed_unit};
use cpp_code_analysis::session::{PreparedUnitReading, SessionRevision};
use cpp_code_analysis::{IndexedUnit, OpenDocuments};

const ASK: &[u8; 4] = b"PREA";
const ANSWER: &[u8; 4] = b"PRER";
const VERSION: u32 = 1;

/// Bytes the host copies during `host_program_read`. The root text is included even when the walk did not keep
/// a copy, because an open document is the text the read is about.
pub(crate) fn encode_request(
    root: &Path,
    root_text: Option<&str>,
    documents: &OpenDocuments,
) -> Vec<u8> {
    let mut docs: Vec<(PathBuf, String)> = documents
        .paths()
        .into_iter()
        .filter_map(|path| {
            let text = documents.text(&path)?;
            Some((path, text))
        })
        .collect();
    if !docs.iter().any(|(path, _)| path == root) {
        let text = root_text
            .map(str::to_string)
            .or_else(|| documents.text(root));
        if let Some(text) = text {
            docs.push((root.to_path_buf(), text));
        }
    }

    let mut out = Vec::new();
    out.extend_from_slice(ASK);
    put_u32(&mut out, VERSION);
    put_path(&mut out, root);
    put_u32(&mut out, docs.len() as u32);
    for (path, text) in &docs {
        put_path(&mut out, path);
        put_bytes(&mut out, text.as_bytes());
    }
    out
}

pub(crate) fn encode_result(program: u64, walked: &[PathBuf], indexed: &IndexedUnit) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(ANSWER);
    put_u32(&mut out, VERSION);
    put_u64(&mut out, program);
    put_u32(&mut out, walked.len() as u32);
    for path in walked {
        put_path(&mut out, path);
    }
    out.extend_from_slice(&encode_indexed_unit(indexed));
    out
}

#[cfg_attr(not(target_arch = "wasm32"), allow(dead_code))]
pub(crate) fn decode_reading(
    root: &Path,
    revision: SessionRevision,
    bytes: &[u8],
) -> Option<PreparedUnitReading> {
    let (program, walked, indexed) = decode_result(bytes)?;
    Some(PreparedUnitReading::from_remote(
        root.to_path_buf(),
        revision,
        program,
        indexed,
        walked,
    ))
}

fn decode_result(bytes: &[u8]) -> Option<(u64, Vec<PathBuf>, IndexedUnit)> {
    let mut reader = Reader { bytes, at: 0 };
    if reader.take(4)? != ANSWER {
        return None;
    }
    if reader.u32()? != VERSION {
        return None;
    }
    let program = reader.u64()?;
    let walked_count = reader.u32()? as usize;
    if walked_count > 1_000_000 {
        return None;
    }
    let mut walked = Vec::with_capacity(walked_count);
    for _ in 0..walked_count {
        walked.push(reader.path()?);
    }
    let indexed = decode_indexed_unit(reader.rest()?).ok()?;
    Some((program, walked, indexed))
}

fn put_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    put_u32(out, bytes.len() as u32);
    out.extend_from_slice(bytes);
}

fn put_path(out: &mut Vec<u8>, path: &Path) {
    put_bytes(out, path.to_string_lossy().as_bytes());
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, len: usize) -> Option<&'a [u8]> {
        let next = self.at.checked_add(len)?;
        if next > self.bytes.len() {
            return None;
        }
        let slice = &self.bytes[self.at..next];
        self.at = next;
        Some(slice)
    }

    fn u32(&mut self) -> Option<u32> {
        let bytes = self.take(4)?;
        Some(u32::from_le_bytes(bytes.try_into().ok()?))
    }

    fn u64(&mut self) -> Option<u64> {
        let bytes = self.take(8)?;
        Some(u64::from_le_bytes(bytes.try_into().ok()?))
    }

    fn bytes(&mut self) -> Option<&'a [u8]> {
        let len = self.u32()? as usize;
        self.take(len)
    }

    fn path(&mut self) -> Option<PathBuf> {
        let bytes = self.bytes()?;
        Some(PathBuf::from(std::str::from_utf8(bytes).ok()?))
    }

    fn rest(&self) -> Option<&'a [u8]> {
        self.bytes.get(self.at..)
    }
}

#[cfg(target_arch = "wasm32")]
mod host {
    use std::cell::RefCell;
    use std::path::PathBuf;
    use std::task::{Context, Poll, Waker};

    use cpp_code_analysis::{DiskFiles, OpenDocuments, Session, SessionFiles, WatchFilter};

    /// The page mounts the project here and the sysroot beside it. The compute instance opens the same
    /// roots, so `__has_include` sees the headers the session instance sees.
    const WORKSPACE: &str = "/home/web_user";
    const CONFIG: &str = "/cppls/cppls.toml";

    use super::encode_result;

    #[link(wasm_import_module = "cppls")]
    unsafe extern "C" {
        fn host_program_read(ptr: *const u8, len: i32) -> i32;
    }

    struct Slot {
        waker: Option<Waker>,
        result: Option<Vec<u8>>,
    }

    thread_local! {
        static SLOT: RefCell<Slot> = RefCell::new(Slot { waker: None, result: None });
        static COMPUTE: RefCell<Option<Session<DiskFiles>>> = const { RefCell::new(None) };
        static COMPUTE_RESULT: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
    }

    /// `None` when the host has no compute instance. The caller reads the program on this thread.
    pub(crate) fn try_submit(job: Vec<u8>) -> Option<Pending> {
        let len = i32::try_from(job.len()).ok()?;
        if len <= 0 {
            return None;
        }
        SLOT.with(|slot| {
            let mut slot = slot.borrow_mut();
            slot.result = None;
            slot.waker = None;
        });
        let accepted = unsafe { host_program_read(job.as_ptr(), len) };
        if accepted == 0 {
            return None;
        }
        Some(Pending { done: false })
    }

    pub(crate) struct Pending {
        done: bool,
    }

    impl Drop for Pending {
        fn drop(&mut self) {
            if self.done {
                return;
            }
            SLOT.with(|slot| slot.borrow_mut().waker = None);
        }
    }

    impl std::future::Future for Pending {
        type Output = Vec<u8>;

        fn poll(mut self: std::pin::Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Vec<u8>> {
            SLOT.with(|slot| {
                let mut slot = slot.borrow_mut();
                if let Some(bytes) = slot.result.take() {
                    self.done = true;
                    return Poll::Ready(bytes);
                }
                slot.waker = Some(cx.waker().clone());
                Poll::Pending
            })
        }
    }

    /// The host copied a finished reading into memory from [`cpp_ls_alloc`]. Wakes the parked read and does
    /// not drive: the host calls `cpp_ls_poll` after this returns.
    #[unsafe(no_mangle)]
    pub extern "C" fn cpp_ls_program_done(ptr: i32, len: i32) {
        let bytes = if ptr == 0 || len <= 0 {
            Vec::new()
        } else {
            unsafe { Vec::from_raw_parts(ptr as *mut u8, len as usize, len as usize) }
        };
        SLOT.with(|slot| {
            let mut slot = slot.borrow_mut();
            slot.result = Some(bytes);
            if let Some(waker) = slot.waker.take() {
                waker.wake();
            }
        });
    }

    /// Run one program read on this instance. The session instance never calls it; the compute instance does.
    /// The job buffer is the one [`cpp_ls_alloc`] returned.
    #[unsafe(no_mangle)]
    pub extern "C" fn cpp_ls_compute_program(ptr: i32, len: i32) -> i32 {
        if ptr == 0 || len <= 0 {
            return 0;
        }
        let job = unsafe { Vec::from_raw_parts(ptr as *mut u8, len as usize, len as usize) };
        let Some(bytes) = compute(&job) else {
            return 0;
        };
        let len = i32::try_from(bytes.len()).unwrap_or(0);
        COMPUTE_RESULT.with(|slot| *slot.borrow_mut() = bytes);
        len
    }

    #[unsafe(no_mangle)]
    pub extern "C" fn cpp_ls_compute_result() -> i32 {
        COMPUTE_RESULT.with(|slot| {
            let bytes = slot.borrow();
            if bytes.is_empty() {
                0
            } else {
                bytes.as_ptr() as i32
            }
        })
    }

    fn compute(job: &[u8]) -> Option<Vec<u8>> {
        let (root, docs) = decode_request(job)?;
        let mut session = COMPUTE.with(|slot| {
            let mut slot = slot.borrow_mut();
            if slot.is_none() {
                let documents = OpenDocuments::new();
                let files = SessionFiles::new(documents, DiskFiles);
                let opened = Session::open_with_config_file(
                    PathBuf::from(WORKSPACE),
                    files,
                    WatchFilter::new(WORKSPACE),
                    Some(std::path::Path::new(CONFIG)),
                );
                *slot = Some(opened);
            }
            // The session stays in the slot. A second job reuses the summaries the first one built.
            // Taking it out and putting it back keeps the borrow of the `RefCell` off the pump.
            slot.take()
        })?;
        for (path, text) in &docs {
            session.did_change(path, text);
        }
        for _ in 0..2_000 {
            if session.pending() == 0 {
                break;
            }
            if session.advance(64).is_empty() {
                break;
            }
        }
        let prepared = session.prepare_a_unit_reading(&root);
        let encoded = prepared.map(|prepared| {
            let program = prepared_program(&prepared);
            let walked = prepared_walked(&prepared);
            let indexed = prepared_indexed(prepared);
            encode_result(program, &walked, &indexed)
        });
        COMPUTE.with(|slot| *slot.borrow_mut() = Some(session));
        encoded
    }

    fn prepared_program(prepared: &cpp_code_analysis::session::PreparedUnitReading) -> u64 {
        prepared.program_hash()
    }

    fn prepared_walked(prepared: &cpp_code_analysis::session::PreparedUnitReading) -> Vec<PathBuf> {
        prepared.walked_paths().to_vec()
    }

    fn prepared_indexed(
        prepared: cpp_code_analysis::session::PreparedUnitReading,
    ) -> cpp_code_analysis::IndexedUnit {
        prepared.into_indexed()
    }

    fn decode_request(bytes: &[u8]) -> Option<(PathBuf, Vec<(PathBuf, String)>)> {
        let mut reader = super::Reader { bytes, at: 0 };
        if reader.take(4)? != super::ASK {
            return None;
        }
        if reader.u32()? != super::VERSION {
            return None;
        }
        let root = reader.path()?;
        let count = reader.u32()? as usize;
        if count > 100_000 {
            return None;
        }
        let mut docs = Vec::with_capacity(count);
        for _ in 0..count {
            let path = reader.path()?;
            let text = std::str::from_utf8(reader.bytes()?).ok()?.to_string();
            docs.push((path, text));
        }
        Some((root, docs))
    }
}

#[cfg(target_arch = "wasm32")]
pub(crate) use host::try_submit;

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use cpp_code_analysis::{IndexedUnit, OpenDocuments};

    use super::{decode_result, encode_request, encode_result};

    #[test]
    fn a_request_lists_the_open_document_and_the_root() {
        let documents = OpenDocuments::new();
        documents.open("/home/web_user/main.cpp", "int main(){return 0;}\n");
        let bytes = encode_request(
            PathBuf::from("/home/web_user/main.cpp").as_path(),
            None,
            &documents,
        );
        assert!(bytes.starts_with(b"PREA"));
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("/home/web_user/main.cpp"));
        assert!(text.contains("int main(){return 0;}"));
    }

    #[test]
    fn a_result_round_trips_the_hash_the_walk_and_the_facts() {
        let indexed = IndexedUnit {
            files: Vec::new(),
            unplaced: 0,
            tokens: 4,
            files_with_tokens: 1,
            missing: 0,
            unbalanced: vec![PathBuf::from("/home/web_user/main.cpp")],
            braces: 0,
            errors: 1,
        };
        let walked = vec![PathBuf::from("/home/web_user/main.cpp")];
        let bytes = encode_result(0xabc, &walked, &indexed);
        let (program, decoded_walked, decoded) = decode_result(&bytes).expect("decodes");
        assert_eq!(program, 0xabc);
        assert_eq!(decoded_walked, walked);
        assert_eq!(decoded.tokens, 4);
        assert_eq!(decoded.errors, 1);
        assert_eq!(decoded.unbalanced, indexed.unbalanced);
    }
}
