#[cfg(not(target_arch = "wasm32"))]
mod best_log_path;
#[cfg(target_arch = "wasm32")]
mod wasm_stderr;

#[cfg(not(target_arch = "wasm32"))]
use chrono::Local;
use log::LevelFilter;
#[cfg(not(target_arch = "wasm32"))]
use log::info;

use crate::cmd_args::{CmdArgs, LogLevel};

#[cfg(not(target_arch = "wasm32"))]
const CRATE_NAME: &str = env!("CARGO_PKG_NAME");
#[cfg(not(target_arch = "wasm32"))]
const CRATE_VERSION: &str = env!("CARGO_PKG_VERSION");

pub fn init_logger(root: Option<&str>, cmd_args: &CmdArgs) -> Option<std::path::PathBuf> {
    let level = match cmd_args.log_level {
        LogLevel::Error => LevelFilter::Error,
        LogLevel::Warn => LevelFilter::Warn,
        LogLevel::Info => LevelFilter::Info,
        LogLevel::Debug => LevelFilter::Debug,
        LogLevel::Trace => LevelFilter::Trace,
    };

    // Fern's channel output is an `std` mpsc sender, and that sender yields the thread. The reactor has no such
    // import, so it logs on stderr without fern. A browser session calls this from the executor.
    #[cfg(target_arch = "wasm32")]
    {
        let _ = (root, cmd_args);
        wasm_stderr::init(level);
        return None;
    }

    #[cfg(not(target_arch = "wasm32"))]
    {
        if crate::rt::on_executor() {
            init_stderr_logger(level);
            return None;
        }
        return init_file_logger(root, cmd_args, level);
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn init_file_logger(
    root: Option<&str>,
    cmd_args: &CmdArgs,
    level: LevelFilter,
) -> Option<std::path::PathBuf> {
    use std::{fs, path::PathBuf};

    use fern::Dispatch;

    use crate::util::path_to_uri;
    use best_log_path::get_best_log_dir;

    let cmd_log_path = cmd_args.log_path.clone();
    if root.is_none() && cmd_log_path.0.is_none() {
        init_stderr_logger(level);
        return None;
    }
    let root = root.unwrap_or("");
    let cmd_log_path = cmd_log_path.0.as_ref().cloned().unwrap_or("".to_string());

    let filename = if root.is_empty() || root == "/" {
        "root".to_string()
    } else {
        root.trim_start_matches('/')
            .split(['/', '\\', ':'])
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("_")
    };

    let log_dir = if cmd_log_path.is_empty() {
        get_best_log_dir()
    } else {
        PathBuf::from(cmd_log_path.as_str())
    };
    if !log_dir.exists() {
        match fs::create_dir_all(&log_dir) {
            Ok(_) => {}
            Err(e) => {
                eprintln!("Failed to create log directory: {:?}", e);
                init_stderr_logger(level);
                return None;
            }
        }
    }

    let log_file_path = log_dir.join(format!("{}.log", filename));

    let log_file = match fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&log_file_path)
    {
        Ok(f) => f,
        Err(e) => {
            eprintln!("Failed to open log file: {:?}", e);
            init_stderr_logger(level);
            return None;
        }
    };

    let logger = Dispatch::new()
        .format(|out, message, record| {
            out.finish(format_args!(
                "[{} {} {}] {}",
                Local::now().format("%Y-%m-%d %H:%M:%S %:z"),
                record.level(),
                record.target(),
                message
            ))
        })
        // set level
        .level(level)
        // set output
        .chain(log_file);

    if let Err(e) = logger.apply() {
        eprintln!("Failed to apply logger: {:?}", e);
        return None;
    }

    let uri = path_to_uri(&log_file_path).unwrap();
    eprintln!("init logger success with file: {}", uri.as_str());
    info!("{} v{}", CRATE_NAME, CRATE_VERSION);
    Some(log_file_path)
}

#[cfg(not(target_arch = "wasm32"))]
fn init_stderr_logger(level: LevelFilter) {
    use fern::Dispatch;

    let logger = Dispatch::new()
        .format(|out, message, record| {
            out.finish(format_args!(
                "[{} {} {}] {}",
                Local::now().format("%Y-%m-%d %H:%M:%S %:z"),
                record.level(),
                record.target(),
                message
            ))
        })
        .level(level)
        .chain(std::io::stderr());

    if let Err(error) = logger.apply() {
        if !crate::rt::on_executor() {
            eprintln!("Failed to apply logger: {:?}", error);
        }
        return;
    }

    info!("{} v{}", CRATE_NAME, CRATE_VERSION);
}
