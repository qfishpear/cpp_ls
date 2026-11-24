//! wasm has no filesystem watcher. The browser client sends `didChange` itself.

use std::path::PathBuf;

use cpp_code_analysis::WatchFilter;

use super::ServerContextSnapshot;

#[derive(Default)]
pub struct NativeFileWatcher;

impl NativeFileWatcher {
    pub async fn start(
        &self,
        _context: ServerContextSnapshot,
        _root: PathBuf,
        _filter: WatchFilter,
        _also: Vec<PathBuf>,
    ) -> Result<(), String> {
        Ok(())
    }

    pub async fn stop(&self) {}
}
