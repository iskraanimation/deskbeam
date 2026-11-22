//! `deskbeam-cli`: the engine without a window (plan §3.1, principle 1).
//!
//! It dispatches the same commands as the app. Only one process may use a data directory at
//! a time, so point `--data-dir` somewhere else while the app is running.

use std::{path::PathBuf, sync::Arc, time::Duration};

use anyhow::Context;
use clap::{Parser, Subcommand};
use deskbeam_engine::{Command, DataDirLock, Engine, EngineConfig, Event, default_data_dir};
use deskbeam_store::SqliteStore;

#[derive(Parser)]
#[command(
    name = "deskbeam-cli",
    version,
    about = "DeskBeam's engine from the command line"
)]
struct Cli {
    /// Where this device's identity, pairings and database live.
    #[arg(long, global = true)]
    data_dir: Option<PathBuf>,

    #[command(subcommand)]
    action: Action,
}

#[derive(Subcommand)]
enum Action {
    /// Lists every command id.
    Commands,
    /// Prints the snapshot as JSON once the node is up.
    Status {
        /// Seconds to wait first, so presence can settle.
        #[arg(long, default_value_t = 3)]
        settle: u64,
    },
    /// Runs one command and prints its result as JSON.
    Run {
        /// The command id, e.g. `pairing.open`.
        id: String,
        /// The parameters as JSON, e.g. '{"code":"…"}'.
        params: Option<String>,
        /// Keep running afterwards and print events, until Ctrl-C (e.g. to keep a pairing
        /// code open).
        #[arg(long)]
        stay: bool,
        /// Seconds to wait before running it, so nearby devices and presence can be found
        /// first (e.g. for `nearby.requestPair`).
        #[arg(long, default_value_t = 0)]
        settle: u64,
    },
    /// Runs the engine and prints its events as JSON lines, until Ctrl-C.
    Daemon,
}

fn main() -> anyhow::Result<()> {
    // Started by LAN mode to make the network adapter with admin rights: only that.
    if let Some(code) = deskbeam_engine::run_lan_helper_if_asked() {
        std::process::exit(code);
    }
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(run())
}

async fn run() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();

    if let Action::Commands = cli.action {
        for id in Command::IDS {
            println!("{id}");
        }
        return Ok(());
    }

    let data_dir = cli.data_dir.unwrap_or_else(default_data_dir);
    std::fs::create_dir_all(&data_dir).with_context(|| format!("create {}", data_dir.display()))?;
    let _lock = DataDirLock::acquire(&data_dir).context(
        "the app or another deskbeam-cli has this data directory; pass --data-dir to use another",
    )?;
    let store = Arc::new(SqliteStore::open_in(&data_dir)?);
    let engine = Engine::start_and_wait(EngineConfig::new(&data_dir), store).await?;

    let outcome = match cli.action {
        Action::Commands => unreachable!(),
        Action::Status { settle } => {
            tokio::time::sleep(Duration::from_secs(settle)).await;
            println!("{}", serde_json::to_string_pretty(&*engine.snapshot())?);
            Ok(())
        }
        Action::Run {
            id,
            params,
            stay,
            settle,
        } => {
            let params = match params {
                Some(p) => serde_json::from_str(&p).context("the parameters are not valid JSON")?,
                None => serde_json::Value::Null,
            };
            let command = Command::parse(&id, params)?;
            tokio::time::sleep(Duration::from_secs(settle)).await;
            let mut events = engine.events();
            match engine.execute(command).await {
                Ok(result) => {
                    println!("{}", serde_json::to_string_pretty(&result)?);
                    if stay {
                        print_events(&mut events).await;
                    }
                    Ok(())
                }
                Err(e) => Err(anyhow::anyhow!(e)),
            }
        }
        Action::Daemon => {
            let mut events = engine.events();
            print_events(&mut events).await;
            Ok(())
        }
    };

    engine.shutdown().await;
    outcome
}

async fn print_events(events: &mut tokio::sync::broadcast::Receiver<Event>) {
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => return,
            ev = events.recv() => match ev {
                Ok(ev) => match serde_json::to_string(&ev) {
                    Ok(line) => println!("{line}"),
                    Err(e) => eprintln!("event not printable: {e}"),
                },
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => eprintln!("missed {n} events"),
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            },
        }
    }
}
