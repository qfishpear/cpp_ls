pub(crate) mod browser_session;
mod connection;
mod error;
mod lsp_server;
mod main_loop;
mod message_processor;

pub use connection::AsyncConnection;
pub use error::ExitError;

use ::lsp_server::{Message, RequestId, Response};
use lsp_types::InitializeParams;
use serde_json::Value;
use std::error::Error;

use crate::cmd_args::{self, CmdArgs};
use crate::handlers::server_capabilities;

const CRATE_NAME: &str = env!("CARGO_PKG_NAME");
const CRATE_VERSION: &str = env!("CARGO_PKG_VERSION");

/// **A malformed `initialize` must be answered, not fatal.** It used to be `unwrap()`: a capability object that
/// this build of `lsp_types` cannot deserialize — a required field a newer client omits, a field renamed — took
/// the whole server down before it had answered anything, which an editor shows as "the language server crashed
/// on startup" with no way to know why. Measured with a real client shape: `textDocument.semanticTokens` without
/// `requests` panicked with `missing field 'requests'`. The protocol's answer to a request the server cannot
/// understand is an **error response with `InvalidParams`**, and the client stays alive to report it.
fn read_initialize(params: Value) -> Result<InitializeParams, String> {
    serde_json::from_value(params)
        .map_err(|error| format!("cannot read initialize params: {error}"))
}

/// `InvalidParams`, as the protocol defines it (JSON-RPC **-32602**), and **not** a successful initialize result.
fn reject_initialize(id: RequestId, message: String) -> Message {
    Message::Response(Response::new_err(id, -32602, message))
}

/// **The type hierarchy capability is written into the JSON by hand.** The protocol's field is
/// `typeHierarchyProvider` (LSP 3.17, `textDocument/prepareTypeHierarchy`), and the `lsp_types` fork this server
/// links against predates it: its `ServerCapabilities` has no such field, so the typed value cannot carry it. A
/// client reads the raw JSON, so this is the one place the capability can be stated — and the handlers behind it
/// are real (`handlers::type_hierarchy`). `tests/handshake.rs` asserts it crosses the wire, because a client that
/// does not see it never asks and the handlers would be dead code however correct they are.
fn accept_initialize(params: &InitializeParams) -> Result<Value, serde_json::Error> {
    let mut capabilities = serde_json::to_value(server_capabilities(&params.capabilities))?;
    if let Some(object) = capabilities.as_object_mut() {
        object.insert("typeHierarchyProvider".to_string(), Value::Bool(true));
        // **clangd's inactive-region notification is an extension, so its flag is written by hand too.** A client
        // that asked for it (see `context::wants_inactive_regions`) is told the server sends it; a client that did not
        // sees nothing, because a client that never heard of the method would only log an unhandled notification.
        if crate::context::wants_inactive_regions(
            &params.capabilities,
            params.initialization_options.as_ref(),
        ) {
            object.insert("inactiveRegionsProvider".to_string(), Value::Bool(true));
        }
    }
    Ok(serde_json::json!({
        "capabilities": capabilities,
        "serverInfo": {
            "name": CRATE_NAME,
            "version": CRATE_VERSION
        }
    }))
}

#[allow(unused)]
pub async fn run_ls(cmd_args: CmdArgs) -> Result<(), Box<dyn Error + Sync + Send>> {
    let (connection, threads) = match cmd_args.communication {
        cmd_args::Communication::Stdio => ::lsp_server::Connection::stdio(),
        cmd_args::Communication::Tcp => {
            let port = cmd_args.port;
            let ip = cmd_args.ip.clone();
            let addr = (ip.as_str(), port);
            ::lsp_server::Connection::listen(addr).unwrap()
        }
    };

    let (id, params) = connection.initialize_start()?;
    let initialization_params = match read_initialize(params) {
        Ok(params) => params,
        Err(message) => {
            connection.sender.send(reject_initialize(id, message))?;
            return Ok(());
        }
    };
    let initialize_data = accept_initialize(&initialization_params)?;
    connection.initialize_finish(id, initialize_data)?;

    // Create async connection wrapper
    let async_connection = AsyncConnection::from_sync(connection);
    main_loop::main_loop(async_connection, initialization_params, cmd_args).await?;

    // The I/O threads are deliberately **not** joined. They read and write the client's pipes, and the reader
    // blocks until the client closes stdin — so joining here would make a clean shutdown (`shutdown`, then `exit`)
    // hang until the client went away as well, which is the opposite of what `exit` asks for. Returning from `main`
    // is what closes the pipes; nothing after this point can be waiting for a message.
    drop(threads);

    eprintln!("Server shutting down.");
    Ok(())
}
