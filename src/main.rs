use std::io::{self, Write};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use crossterm::event::{self, DisableMouseCapture, EnableMouseCapture, Event};
use crossterm::execute;

use diffier::app::App;
use diffier::install;
use diffier::paths::Paths;
use diffier::render::{EditCard, Renderer, delta_ansi};
use diffier::session::Pipeline;
use diffier::spool;
use diffier::ui;

const TICK: Duration = Duration::from_millis(100);

#[derive(Parser)]
#[command(
    name = "diffier",
    version,
    about = "Live diff feed for Claude Code edits"
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    #[command(flatten)]
    run: RunArgs,
}

#[derive(clap::Args, Clone)]
struct RunArgs {
    /// Directory whose Claude Code session to follow (default: current dir).
    #[arg(long)]
    cwd: Option<PathBuf>,
    /// Spool file to read (default: the hook's spool).
    #[arg(long)]
    spool: Option<PathBuf>,
    /// Snapshot root (default: the hook's cache dir).
    #[arg(long)]
    snapshots: Option<PathBuf>,
    /// Render diffs without delta even when it is installed.
    #[arg(long)]
    no_delta: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Tail the spool and show diffs in a TUI (default).
    Run(RunArgs),
    /// Write the hook script and register it in ~/.claude/settings.json.
    Install,
    /// Remove the hook script and its settings entries.
    Uninstall {
        /// Also delete the spool and snapshot directories.
        #[arg(long)]
        purge: bool,
    },
    /// Print the current session's cards to stdout and exit.
    Dump {
        #[command(flatten)]
        run: RunArgs,
        /// Print delta's ANSI output instead of plain text.
        #[arg(long)]
        ansi: bool,
    },
}

fn resolve(args: &RunArgs) -> Result<(Paths, PathBuf)> {
    let mut paths = Paths::discover();
    if let Some(s) = &args.spool {
        paths.spool = s.clone();
    }
    if let Some(s) = &args.snapshots {
        paths.snapshot_root = s.clone();
    }
    let cwd = match &args.cwd {
        Some(c) => c.clone(),
        None => std::env::current_dir().context("current directory")?,
    };
    Ok((paths, cwd))
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        None => run_tui(&cli.run),
        Some(Command::Run(args)) => run_tui(&args),
        Some(Command::Install) => install::install(&Paths::discover()),
        Some(Command::Uninstall { purge }) => install::uninstall(&Paths::discover(), purge),
        Some(Command::Dump { run, ansi }) => dump(&run, ansi),
    }
}

fn dump(args: &RunArgs, ansi: bool) -> Result<()> {
    let (paths, cwd) = resolve(args)?;
    if !paths.spool.exists() {
        anyhow::bail!(
            "spool not found: {} (run `diffier install` and make an edit)",
            paths.spool.display()
        );
    }
    let detected = Renderer::detect(args.no_delta);
    // With --ansi we run delta ourselves below, so building the card through
    // delta as well would render every diff twice.
    let build_with = if ansi {
        Renderer::Plain
    } else {
        detected.clone()
    };
    let replay = spool::scan_replay(&paths.spool, &cwd);
    let mut pipeline = Pipeline::new(cwd.clone(), paths.snapshot_root.clone());
    let inputs = pipeline.replay(&replay.events);
    let width = crossterm::terminal::size().map(|(w, _)| w).unwrap_or(100);
    let mut out = io::stdout().lock();
    for input in inputs {
        let card = EditCard::new(input, &build_with, width, &cwd);
        writeln!(out, "== {}", card.header_text())?;
        let rendered = match (&detected, ansi) {
            (Renderer::Delta(bin), true) if !card.input.diff.unified.is_empty() => {
                delta_ansi(bin, &card.input.diff.unified, width, &cwd)
            }
            _ => None,
        };
        match rendered {
            Some(bytes) => out.write_all(&bytes)?,
            None => {
                for l in card.body_plain() {
                    writeln!(out, "{l}")?;
                }
            }
        }
        writeln!(out)?;
    }
    Ok(())
}

fn run_tui(args: &RunArgs) -> Result<()> {
    let (paths, cwd) = resolve(args)?;
    let renderer = Renderer::detect(args.no_delta);
    let width = crossterm::terminal::size().map(|(w, _)| w).unwrap_or(100);
    let mut app = App::new(cwd, &paths, renderer, width);

    let mut terminal = ratatui::init();
    let _ = execute!(io::stdout(), EnableMouseCapture);
    // ratatui::init installs a hook that restores the terminal on panic, but it
    // knows nothing about mouse capture; without this the shell keeps emitting
    // escape sequences on every mouse move after a crash.
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(io::stdout(), DisableMouseCapture);
        previous(info);
    }));
    let result = event_loop(&mut terminal, &mut app);
    let _ = execute!(io::stdout(), DisableMouseCapture);
    ratatui::restore();
    let _ = std::panic::take_hook();
    result
}

fn dispatch(app: &mut App, ev: Event) {
    match ev {
        Event::Key(k) if k.kind != event::KeyEventKind::Release => app.on_key(k),
        Event::Mouse(m) => app.on_mouse(m),
        Event::Resize(w, h) => app.on_resize(w, h),
        _ => {}
    }
}

fn event_loop(terminal: &mut ratatui::DefaultTerminal, app: &mut App) -> Result<()> {
    loop {
        terminal.draw(|f| ui::draw(f, app))?;
        if event::poll(TICK)? {
            dispatch(app, event::read()?);
            // Drain any burst of input before redrawing.
            while event::poll(Duration::ZERO)? {
                dispatch(app, event::read()?);
            }
        }
        app.tick();
        if app.quit {
            return Ok(());
        }
    }
}
