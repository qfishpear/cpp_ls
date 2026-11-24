//! The browser host's session. The page delivers one message at a time; this drives the server until every task
//! is waiting, then tells the page when to come back.
//!
//! `send` releases timers only, so a pump parked on a yield stays parked. `poll` releases timers and yields. The
//! return is `-2` when the session has ended, `-1` when nothing is waiting, `0` when a task yielded, and a positive
//! number of milliseconds (rounded up) until the next timer.

#![cfg_attr(not(test), allow(dead_code))]

use std::sync::Arc;
use std::time::Duration;

use lsp_server::{Message, Response};
use tokio::sync::mpsc;

use crate::cmd_args::CmdArgs;
use crate::context::Outbound;
use crate::rt::{Clock, Executor, Next, Wake};

use super::connection::AsyncConnection;

pub(crate) struct BrowserSession {
    executor: Option<Executor>,
    clock: Clock,
    outbound: Outbound,
    inbound: Option<mpsc::UnboundedSender<Message>>,
}

impl BrowserSession {
    pub(crate) fn new(emit: impl Fn(Message) + Send + Sync + 'static) -> Self {
        Self::with_clock(emit, Clock::Real)
    }

    fn with_clock(emit: impl Fn(Message) + Send + Sync + 'static, clock: Clock) -> Self {
        #[cfg(test)]
        crate::context::clear_live_analysis();
        Self {
            executor: None,
            clock,
            outbound: Arc::new(emit),
            inbound: None,
        }
    }

    /// Parse `bytes` and drive timers. Yields parked by an earlier `poll` stay parked.
    pub(crate) fn send(&mut self, bytes: &[u8]) -> i32 {
        let message = parse_message(bytes);
        self.deliver(message)
    }

    /// [`BrowserSession::send`] for a buffer the caller is done with. The bytes are dropped before the turn, so a
    /// multi-megabyte `didOpen` is not alive twice while the server runs.
    pub(crate) fn send_owned(&mut self, bytes: Vec<u8>) -> i32 {
        let message = parse_message(&bytes);
        drop(bytes);
        self.deliver(message)
    }

    /// Drive timers and every yield the previous turn parked.
    pub(crate) fn poll(&mut self) -> i32 {
        self.start();
        self.turn(Wake::TimersAndTurns)
    }

    fn deliver(&mut self, message: Message) -> i32 {
        self.start();
        let Some(inbound) = &self.inbound else {
            return -2;
        };
        if inbound.send(message).is_err() {
            return -2;
        }
        self.turn(Wake::Timers)
    }

    fn start(&mut self) {
        if self.executor.is_some() {
            return;
        }
        let executor = Executor::new(self.clock.clone());
        let (inbound, outbound_messages) = mpsc::unbounded_channel();
        let emit = Arc::clone(&self.outbound);
        executor.spawn_root(session_root(outbound_messages, emit));
        self.inbound = Some(inbound);
        self.executor = Some(executor);
    }

    fn turn(&mut self, wake: Wake) -> i32 {
        let Some(executor) = &self.executor else {
            return -2;
        };
        map_next(executor.drive(wake))
    }
}

fn parse_message(bytes: &[u8]) -> Message {
    serde_json::from_slice(bytes).expect("host sent bytes that are not a Message")
}

fn map_next(next: Next) -> i32 {
    match next {
        Next::Ended => -2,
        Next::Idle => -1,
        Next::Again => 0,
        Next::After(duration) => ceil_millis(duration),
    }
}

fn ceil_millis(duration: Duration) -> i32 {
    let millis = duration.as_nanos().div_ceil(1_000_000);
    i32::try_from(millis).unwrap_or(i32::MAX)
}

async fn session_root(mut inbound: mpsc::UnboundedReceiver<Message>, out: Outbound) {
    let Some(message) = inbound.recv().await else {
        return;
    };
    let Message::Request(request) = message else {
        return;
    };
    if request.method != "initialize" {
        (out)(super::reject_initialize(
            request.id,
            "the first message must be initialize".to_string(),
        ));
        return;
    }
    let params = match super::read_initialize(request.params) {
        Ok(params) => params,
        Err(message) => {
            (out)(super::reject_initialize(request.id, message));
            return;
        }
    };
    let body = super::accept_initialize(&params).expect("the initialize result is JSON");
    (out)(Message::Response(Response::new_ok(request.id, body)));
    let connection = AsyncConnection::from_parts(out, inbound);
    if let Err(error) = super::main_loop::main_loop(connection, params, CmdArgs::browser()).await {
        log::error!("browser session ended: {error}");
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use lsp_server::Message;
    use serde_json::{Value, json};

    use super::BrowserSession;
    use crate::context::live_analysis;
    use crate::rt::{Clock, VirtualClock};
    use crate::util::path_to_uri;

    struct Host {
        session: BrowserSession,
        clock: VirtualClock,
        seen: Arc<Mutex<Vec<Message>>>,
    }

    impl Host {
        fn new() -> Self {
            let clock = VirtualClock::new();
            let seen = Arc::new(Mutex::new(Vec::new()));
            let collect = Arc::clone(&seen);
            let session = BrowserSession::with_clock(
                move |message| collect.lock().unwrap().push(message),
                Clock::Virtual(clock.clone()),
            );
            Self {
                session,
                clock,
                seen,
            }
        }

        fn send(&mut self, value: Value) -> i32 {
            self.session
                .send(&serde_json::to_vec(&value).expect("the test message is JSON"))
        }

        fn poll(&mut self) -> i32 {
            self.session.poll()
        }

        fn advance(&self, by: Duration) {
            self.clock.advance(by);
        }

        fn initialize(&mut self, root: &Path) -> i32 {
            let uri = path_to_uri(root).expect("the workspace path is a file URI");
            self.send(json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "processId": null,
                    "rootUri": uri.as_str(),
                    "capabilities": {
                        "workspace": { "configuration": true }
                    }
                }
            }))
        }

        fn did_open(&mut self, path: &Path, text: &str, version: i32) -> i32 {
            let uri = path_to_uri(path).expect("the document path is a file URI");
            self.send(json!({
                "jsonrpc": "2.0",
                "method": "textDocument/didOpen",
                "params": {
                    "textDocument": {
                        "uri": uri.as_str(),
                        "languageId": "cpp",
                        "version": version,
                        "text": text
                    }
                }
            }))
        }

        fn did_change(&mut self, path: &Path, text: &str, version: i32) -> i32 {
            let uri = path_to_uri(path).expect("the document path is a file URI");
            self.send(json!({
                "jsonrpc": "2.0",
                "method": "textDocument/didChange",
                "params": {
                    "textDocument": { "uri": uri.as_str(), "version": version },
                    "contentChanges": [{ "text": text }]
                }
            }))
        }

        fn hover(&mut self, path: &Path, line: u32, character: u32) -> i32 {
            let uri = path_to_uri(path).expect("the document path is a file URI");
            self.send(json!({
                "jsonrpc": "2.0",
                "id": 7,
                "method": "textDocument/hover",
                "params": {
                    "textDocument": { "uri": uri.as_str() },
                    "position": { "line": line, "character": character }
                }
            }))
        }

        fn reply_configuration(&mut self) -> i32 {
            let id = self
                .seen
                .lock()
                .unwrap()
                .iter()
                .find_map(|message| match message {
                    Message::Request(request) if request.method == "workspace/configuration" => {
                        Some(serde_json::to_value(&request.id).unwrap())
                    }
                    _ => None,
                })
                .expect("the server asks workspace/configuration");
            self.send(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": [Value::Null]
            }))
        }

        fn has_response(&self, id: i64) -> bool {
            self.seen
                .lock()
                .unwrap()
                .iter()
                .any(|message| match message {
                    Message::Response(response) => {
                        serde_json::to_value(&response.id).ok() == Some(json!(id))
                    }
                    _ => false,
                })
        }

        fn initialize_error(&self) -> Option<(i32, String)> {
            self.seen
                .lock()
                .unwrap()
                .iter()
                .find_map(|message| match message {
                    Message::Response(response) => match &response.response_result {
                        Err(error) => Some((error.code, error.message.clone())),
                        Ok(_) => None,
                    },
                    _ => None,
                })
        }

        fn advertised_type_hierarchy(&self) -> bool {
            self.seen.lock().unwrap().iter().any(|message| {
                let Message::Response(response) = message else {
                    return false;
                };
                let Ok(value) = &response.response_result else {
                    return false;
                };
                value
                    .get("capabilities")
                    .and_then(|caps| caps.get("typeHierarchyProvider"))
                    .and_then(Value::as_bool)
                    == Some(true)
            })
        }

        fn configuration_requested(&self) -> bool {
            self.seen
                .lock()
                .unwrap()
                .iter()
                .any(|message| method_of(message) == Some("workspace/configuration"))
        }

        fn published_nonempty(&self) -> bool {
            self.seen.lock().unwrap().iter().any(|message| {
                let Message::Notification(notification) = message else {
                    return false;
                };
                if notification.method != "textDocument/publishDiagnostics" {
                    return false;
                }
                notification
                    .params
                    .get("diagnostics")
                    .and_then(Value::as_array)
                    .is_some_and(|items| !items.is_empty())
            })
        }

        /// Poll until a whole-program read is owed, the index queue is empty, and the session is parked on a timer
        /// longer than the diagnostic debounce. That timer is the quiet period.
        fn park_on_owed_program(&mut self) -> i32 {
            for _ in 0..60 {
                let code = self.poll();
                if code == 0 {
                    continue;
                }
                assert!(
                    code > 0,
                    "session ended while a program was still owed: {}",
                    self.outline()
                );
                if program_owed() && pending() == 0 && code > 400 {
                    return code;
                }
                self.advance(Duration::from_millis(code as u64));
            }
            panic!("never parked on an owed program: {}", self.outline());
        }

        fn outline(&self) -> String {
            let methods: Vec<_> = self
                .seen
                .lock()
                .unwrap()
                .iter()
                .filter_map(method_of)
                .map(str::to_string)
                .collect();
            format!(
                "pending={} owed={} wait={:?} messages={methods:?}",
                pending(),
                program_owed(),
                program_wait(),
            )
        }
    }

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(label: &str) -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let n = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("cppls-browser-{label}-{}-{n}", std::process::id()));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }

        fn join(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn method_of(message: &Message) -> Option<&str> {
        match message {
            Message::Request(request) => Some(request.method.as_str()),
            Message::Notification(notification) => Some(notification.method.as_str()),
            Message::Response(_) => None,
        }
    }

    fn buffer(path: &Path) -> Option<String> {
        live_analysis()?.files().overlay.text(path)
    }

    fn pending() -> usize {
        live_analysis()
            .and_then(|analysis| analysis.with_snapshot(|session| session.pending_work()))
            .unwrap_or(usize::MAX)
    }

    fn program_owed() -> bool {
        program_wait().is_some()
    }

    fn program_wait() -> Option<Duration> {
        live_analysis()
            .and_then(|analysis| {
                analysis.with_snapshot(|session| session.the_wait_before_a_program_may_be_read())
            })
            .flatten()
    }

    fn next_open() -> Option<PathBuf> {
        live_analysis()
            .and_then(|analysis| analysis.with_snapshot(|session| session.next_open_file_to_read()))
            .flatten()
    }

    #[test]
    fn initialize_emits_configuration_then_the_pump_yields() {
        let scratch = Scratch::new("handshake");
        let file = scratch.join("main.cpp");
        let text = "int ready;\n";
        std::fs::write(&file, text).unwrap();
        let mut host = Host::new();

        // The configuration cancel is five seconds, and the request is already in the callback. The update queue's
        // idle wait is one second and is already armed, so that is the sooner timer the host is told about.
        assert_eq!(host.initialize(scratch.path()), 1000);
        assert!(
            host.configuration_requested(),
            "the configuration request is emitted before the reply"
        );
        assert!(
            host.advertised_type_hierarchy(),
            "initialize advertises typeHierarchyProvider"
        );

        assert_eq!(host.did_open(&file, text, 1), 1000);
        assert!(
            buffer(&file).is_none(),
            "didOpen is queued until the configuration reply"
        );

        assert_eq!(host.reply_configuration(), 0);
        assert_eq!(buffer(&file).as_deref(), Some(text));
    }

    #[test]
    fn a_malformed_initialize_is_rejected_and_the_session_ends() {
        let mut host = Host::new();
        let bytes = serde_json::to_vec(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": { "capabilities": 1 }
        }))
        .unwrap();
        assert_eq!(host.session.send_owned(bytes), -2);
        let (code, message) = host
            .initialize_error()
            .expect("the malformed initialize is answered");
        assert_eq!(code, -32602);
        assert!(
            message.contains("cannot read initialize params"),
            "{message}"
        );
    }

    #[test]
    #[should_panic(expected = "not a Message")]
    fn send_of_bytes_that_are_not_a_message_panics() {
        let mut session = BrowserSession::new(|_| {});
        session.send(b"not json");
    }

    #[test]
    fn diagnostics_are_published_after_the_debounce_and_before_idle() {
        let scratch = Scratch::new("diag");
        let file = scratch.join("main.cpp");
        let text = "struct Unclosed { int x;\n";
        std::fs::write(&file, text).unwrap();
        let mut host = Host::new();
        assert_eq!(host.initialize(scratch.path()), 1000);
        host.did_open(&file, text, 1);
        assert_eq!(host.reply_configuration(), 0);

        let mut advanced_ms = 0u64;
        let mut published = false;
        let mut idle_before = false;
        for _ in 0..40 {
            let code = host.poll();
            if host.published_nonempty() {
                published = true;
                assert_ne!(
                    code, -1,
                    "diagnostics arrived with nothing left to wait for"
                );
                break;
            }
            if (900..=1100).contains(&code) {
                idle_before = true;
            }
            if code > 0 {
                advanced_ms += code as u64;
                host.advance(Duration::from_millis(code as u64));
            }
        }
        assert!(
            published,
            "no diagnostics after {advanced_ms} ms; {}",
            host.outline()
        );
        assert!(
            advanced_ms >= 300,
            "published before the 300 ms debounce ({advanced_ms} ms)"
        );
        assert!(
            !idle_before,
            "the session was already idle when diagnostics were published"
        );
    }

    #[test]
    fn a_hover_waits_one_slice_then_answers_with_what_it_has() {
        let scratch = Scratch::new("hover");
        let mut host = Host::new();
        assert_eq!(host.initialize(scratch.path()), 1000);
        // No open file, so the pump has nothing to yield for and is parked on its idle wait.
        let parked = host.reply_configuration();
        assert!(
            parked > 25,
            "an empty project should be waiting, not yielding; got {parked}; {}",
            host.outline()
        );

        // Longer than `catch_up`'s nine files, so the closure is still open when the hover first waits.
        // Written after the scan: the pump has not queued any of it.
        std::fs::write(scratch.join("h12.h"), "int value;\n").unwrap();
        for i in (0..12).rev() {
            std::fs::write(
                scratch.path().join(format!("h{i}.h")),
                format!("#include \"h{}.h\"\n", i + 1),
            )
            .unwrap();
        }
        let file = scratch.join("main.cpp");
        std::fs::write(&file, "#include \"h0.h\"\n").unwrap();

        let code = host.hover(&file, 0, 0);
        assert_eq!(
            code,
            25,
            "the first hover stop is one 25 ms slice; {}",
            host.outline()
        );
        host.advance(Duration::from_secs(2));
        let _ = host.poll();
        assert!(
            host.has_response(7),
            "hover did not answer; {}",
            host.outline()
        );
    }

    #[test]
    fn a_woken_hover_finishes_before_the_entry_returns() {
        let scratch = Scratch::new("hover-yield");
        let file = scratch.join("main.cpp");
        let text = "int ready;\n";
        std::fs::write(&file, text).unwrap();
        let mut host = Host::new();
        assert_eq!(host.initialize(scratch.path()), 1000);
        host.did_open(&file, text, 1);
        assert_eq!(host.reply_configuration(), 0);

        // The reply left the pump on a yield. A hover sent now is served in that same entry, and its answer is in
        // the callback before `send` returns — the yield stays parked, and the hover does not wait out its budget.
        let code = host.hover(&file, 0, 4);
        assert_eq!(
            code,
            0,
            "the pump's yield is still what brings the host back; {}",
            host.outline()
        );
        assert!(
            host.has_response(7),
            "the hover finished before the entry returned; {}",
            host.outline()
        );
    }

    #[test]
    fn an_edit_wakes_a_waiting_pump_and_the_entry_stops_at_the_next_yield() {
        let scratch = Scratch::new("wake");
        let file = scratch.join("main.cpp");
        let text = "int ready;\n";
        std::fs::write(&file, text).unwrap();
        let mut host = Host::new();
        assert_eq!(host.initialize(scratch.path()), 1000);
        host.did_open(&file, text, 1);
        assert_eq!(host.reply_configuration(), 0);

        let _ = host.park_on_owed_program();
        let code = host.did_change(&file, "int edited;\n", 2);
        assert_eq!(
            code,
            0,
            "the edit wakes the pump and this entry runs to the next yield; {}",
            host.outline()
        );
        assert_eq!(buffer(&file).as_deref(), Some("int edited;\n"));
    }

    #[test]
    fn a_yield_advances_one_open_file_and_a_send_does_not_wake_it() {
        let scratch = Scratch::new("yield");
        let first = scratch.join("a.cpp");
        let second = scratch.join("b.cpp");
        std::fs::write(&first, "int a;\n").unwrap();
        std::fs::write(&second, "int b;\n").unwrap();
        let mut host = Host::new();
        assert_eq!(host.initialize(scratch.path()), 1000);
        assert_eq!(host.did_open(&first, "int a;\n", 1), 1000);
        assert_eq!(host.did_open(&second, "int b;\n", 1), 1000);
        assert_eq!(host.reply_configuration(), 0);

        let mut parked = next_open();
        for _ in 0..30 {
            if parked.is_some() {
                break;
            }
            let code = host.poll();
            assert_eq!(
                code,
                0,
                "expected a yield while files remain; {}",
                host.outline()
            );
            parked = next_open();
        }
        let parked = parked.expect("an open file is still queued");
        host.send(json!({
            "jsonrpc": "2.0",
            "method": "$/cancelRequest",
            "params": { "id": 123456 }
        }));
        assert_eq!(
            next_open(),
            Some(parked.clone()),
            "a send does not release the pump's yield"
        );
        assert_eq!(host.poll(), 0);
        assert_ne!(
            next_open(),
            Some(parked),
            "the next poll takes the next file"
        );
    }

    #[test]
    fn a_program_read_yields_before_it_starts_and_an_edit_lands_first() {
        let scratch = Scratch::new("program");
        let file = scratch.join("main.cpp");
        let text = "int ready;\n";
        std::fs::write(&file, text).unwrap();
        let mut host = Host::new();
        assert_eq!(host.initialize(scratch.path()), 1000);
        host.did_open(&file, text, 1);
        assert_eq!(host.reply_configuration(), 0);

        let parked = host.park_on_owed_program();
        host.advance(Duration::from_millis(100));
        let remainder = host.poll();
        assert_eq!(
            remainder,
            parked - 100,
            "advancing the clock, without a pretend pause, returns the time still left; {}",
            host.outline()
        );
        assert!(
            program_owed(),
            "the read must not start just because the clock moved; {}",
            host.outline()
        );

        assert!(
            live_analysis()
                .expect("the server is up")
                .pretend_a_pause(Duration::from_secs(2)),
            "the session exists to move the editing clock"
        );
        host.advance(Duration::from_millis(remainder as u64));
        assert_eq!(
            host.poll(),
            0,
            "this entry yields before read_a_program; {}",
            host.outline()
        );
        assert!(
            program_owed(),
            "the read has not started; {}",
            host.outline()
        );

        let edited = "int changed;\n";
        host.did_change(&file, edited, 2);
        assert_eq!(buffer(&file).as_deref(), Some(edited));
        assert!(
            program_owed(),
            "the edit is in the session and the read has not started; {}",
            host.outline()
        );

        let _ = host.poll();
        assert!(
            !program_owed(),
            "the read happens on the poll after the yield; {}",
            host.outline()
        );
    }

    #[test]
    fn an_idle_session_waits_about_a_second() {
        let scratch = Scratch::new("idle");
        let file = scratch.join("main.cpp");
        let text = "int ready;\n";
        std::fs::write(&file, text).unwrap();
        let mut host = Host::new();
        assert_eq!(host.initialize(scratch.path()), 1000);
        host.did_open(&file, text, 1);
        assert_eq!(host.reply_configuration(), 0);

        let parked = host.park_on_owed_program();
        assert!(
            live_analysis()
                .expect("the server is up")
                .pretend_a_pause(Duration::from_secs(2))
        );
        host.advance(Duration::from_millis(parked as u64));
        assert_eq!(host.poll(), 0, "{}", host.outline());
        // The program read leaves the pump on its one-second idle wait. A timer that was already almost due — the
        // update queue's own wait, armed earlier — is nearer than that, so step over those and the idle wait is
        // what the host is told.
        let mut idle = None;
        for _ in 0..8 {
            let code = host.poll();
            if !program_owed() && (900..=1000).contains(&code) {
                idle = Some(code);
                break;
            }
            if code > 0 && code < 900 {
                host.advance(Duration::from_millis(code as u64));
            }
        }
        let idle = idle
            .unwrap_or_else(|| panic!("an idle session waits about a second; {}", host.outline()));
        assert_ne!(idle, -1);

        host.advance(Duration::from_secs(1));
        let again = host.poll();
        assert!(
            (900..=1000).contains(&again),
            "an empty slice comes back to the same wait, got {again}; {}",
            host.outline()
        );
    }

    #[test]
    fn shutdown_then_exit_ends_the_session() {
        let scratch = Scratch::new("exit");
        let file = scratch.join("main.cpp");
        std::fs::write(&file, "int ready;\n").unwrap();
        let mut host = Host::new();
        assert_eq!(host.initialize(scratch.path()), 1000);
        host.did_open(&file, "int ready;\n", 1);
        let code = host.reply_configuration();
        assert!(code >= 0, "{code}");
        let _ = host.send(json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "shutdown"
        }));
        assert!(host.has_response(4), "{}", host.outline());
        let ended = host.send(json!({
            "jsonrpc": "2.0",
            "method": "exit"
        }));
        assert_eq!(ended, -2);
    }
}
