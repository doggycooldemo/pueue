use std::fs::create_dir_all;
use std::path::Path;
use std::process::Command;
use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use simplelog::{Config, LevelFilter, SimpleLogger};
use structopt::StructOpt;

use pueue::message::Message;
use pueue::settings::Settings;
use pueue::state::State;

use crate::cli::Opt;
use crate::socket::accept_incoming;
use crate::task_handler::TaskHandler;

mod aliasing;
mod cli;
mod instructions;
mod platform;
mod response_helper;
mod socket;
mod streaming;
mod task_handler;

#[async_std::main]
async fn main() -> Result<()> {
    // Parse commandline options.
    let opt = Opt::from_args();

    if opt.daemonize {
        fork_daemon(&opt)?;
    }

    // Set the verbosity level of the logger.
    let level = match opt.verbose {
        0 => LevelFilter::Error,
        1 => LevelFilter::Warn,
        2 => LevelFilter::Info,
        _ => LevelFilter::Debug,
    };
    SimpleLogger::init(level, Config::default()).unwrap();

    // Try to read settings from the configuration file.
    let settings = match Settings::read(false, &opt.config) {
        Ok(settings) => settings,
        Err(_) => {
            // There's something wrong with the config file or something's missing.
            // Try to read the config and fill missing values with defaults.
            // This might be possible on version upgrade or first run.
            let settings = Settings::new(false, &opt.config)?;

            // Since we needed to add values to the configuration, we have to save it.
            // This also creates the save file in case it didn't exist yet.
            if let Err(error) = settings.save(&opt.config) {
                println!("Failed saving config file.");
                println!("{:?}", error);
            }
            settings
        }
    };

    init_directories(&settings.shared.pueue_directory);

    let state = State::new(&settings, opt.config.clone());
    let state = Arc::new(Mutex::new(state));

    let (sender, receiver) = channel();
    let mut task_handler = TaskHandler::new(state.clone(), receiver);

    // This section handles Shutdown via SigTerm/SigInt process signals
    // 1. Remove the unix socket (if it exists).
    // 2. Notify the TaskHandler, so it can shutdown gracefully.
    //
    // The actual program exit will be done via the TaskHandler.
    let unix_socket_path = settings.shared.unix_socket_path.clone();
    let sender_clone = sender.clone();
    ctrlc::set_handler(move || {
        // Clean up the unix socket if we're using it and it exists.
        if settings.shared.use_unix_socket && std::path::PathBuf::from(&unix_socket_path).exists() {
            std::fs::remove_file(&unix_socket_path)
                .expect("Failed to remove unix socket on shutdown");
        }

        // Notify the task handler
        sender_clone
            .send(Message::DaemonShutdown)
            .expect("Failed to send Message to TaskHandler on Shutdown");
    })?;

    let orig_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        // invoke the default handler and exit the process
        orig_hook(panic_info);
        std::process::exit(1);
    }));

    std::thread::spawn(move || {
        task_handler.run();
    });

    accept_incoming(sender, state.clone(), opt).await?;

    Ok(())
}

/// Initialize all directories needed for normal operation.
fn init_directories(path: &str) {
    // Pueue base path
    let pueue_dir = Path::new(path);
    if !pueue_dir.exists() {
        if let Err(error) = create_dir_all(&pueue_dir) {
            panic!(
                "Failed to create main directory at {:?} error: {:?}",
                pueue_dir, error
            );
        }
    }

    // Task log dir
    let log_dir = pueue_dir.join("log");
    if !log_dir.exists() {
        if let Err(error) = create_dir_all(&log_dir) {
            panic!(
                "Failed to create log directory at {:?} error: {:?}",
                log_dir, error
            );
        }
    }

    // Task log dir
    let logs_dir = pueue_dir.join("task_logs");
    if !logs_dir.exists() {
        if let Err(error) = create_dir_all(&logs_dir) {
            panic!(
                "Failed to create task logs directory at {:?} error: {:?}",
                logs_dir, error
            );
        }
    }
}

/// This is a simple and cheap custom fork method.
/// Simply spawn a new child with identical arguments and exit right away.
fn fork_daemon(opt: &Opt) -> Result<()> {
    let mut arguments = Vec::<String>::new();

    if let Some(port) = &opt.port {
        arguments.push("--port".to_string());
        arguments.push(port.clone());
    }

    if opt.verbose > 0 {
        arguments.push("-".to_string() + &" ".repeat(opt.verbose as usize));
    }

    Command::new("pueued").args(&arguments).spawn()?;

    println!("Pueued is now running in the background");
    std::process::exit(0);
}
