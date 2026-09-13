use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use crossterm::event::{self, DisableMouseCapture, EnableMouseCapture, Event};
use crossterm::execute;

use diffier::app::App;
use diffier::paths::Paths;
use diffier::render::{EditCard, Renderer, ViewMode, delta_ansi};
use diffier::session::{Pipeline, session_matches, session_order};
use diffier::spool::{self, MatchMode};
use diffier::ui;
use diffier::{hook, install};

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
    run: TuiArgs,
}

/// Options shared by the TUI and `dump`.
#[derive(clap::Args, Clone)]
struct RunArgs {
    /// Directory whose Claude Code sessions to follow (default: current dir).
    #[arg(long)]
    cwd: Option<PathBuf>,
    /// Follow only sessions in this exact directory, not every worktree and
    /// subdirectory of its git repository.
    #[arg(long)]
    cwd_only: bool,
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

impl RunArgs {
    fn match_mode(&self) -> MatchMode {
        if self.cwd_only {
            MatchMode::CwdOnly
        } else {
            MatchMode::Repo
        }
    }
}

#[derive(clap::Args, Clone)]
struct TuiArgs {
    #[command(flatten)]
    run: RunArgs,
    /// Show unified diffs instead of side by side.
    #[arg(long)]
    unified: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Tail the spool and show diffs in a TUI (default).
    Run(TuiArgs),
    /// Register `diffier hook` in ~/.claude/settings.json.
    Install,
    /// Remove the hook entries from ~/.claude/settings.json.
    Uninstall {
        /// Also delete the spool and snapshot directories.
        #[arg(long)]
        purge: bool,
    },
    /// Print the current sessions' cards to stdout and exit.
    Dump {
        #[command(flatten)]
        run: RunArgs,
        /// Print delta's ANSI output instead of plain text.
        #[arg(long)]
        ansi: bool,
        /// Only cards whose session id starts or ends with this; a card's
        /// session tag works.
        #[arg(long)]
        session: Option<String>,
        /// Lay diffs out side by side instead of unified.
        #[arg(long)]
        side_by_side: bool,
        /// Render width in columns (default: the terminal width, or 100).
        #[arg(long)]
        width: Option<u16>,
    },
    /// Consume one Claude Code hook payload from stdin. Registered by
    /// `diffier install`; not meant to be run by hand.
    Hook,
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
        Some(Command::Install) => {
            let exe = std::env::current_exe().context("locating the diffier binary")?;
            install::install(&Paths::discover(), &exe)
        }
        Some(Command::Uninstall { purge }) => install::uninstall(&Paths::discover(), purge),
        Some(Command::Dump {
            run,
            ansi,
            session,
            side_by_side,
            width,
        }) => dump(&run, ansi, session.as_deref(), side_by_side, width),
        Some(Command::Hook) => run_hook(),
    }
}

/// Exit 0 no matter what: a non-zero exit from a PreToolUse hook blocks the
/// tool call, and a panic would otherwise exit 101.
fn run_hook() -> Result<()> {
    let _ = std::panic::catch_unwind(|| {
        let mut payload = Vec::new();
        if io::stdin().lock().read_to_end(&mut payload).is_err() {
            return;
        }
        if let Some(paths) = hook::paths_from_env() {
            hook::run(&paths, &payload);
        }
    });
    std::process::exit(0)
}

fn dump(
    args: &RunArgs,
    ansi: bool,
    session: Option<&str>,
    side_by_side: bool,
    width: Option<u16>,
) -> Result<()> {
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
    let mut pipeline = Pipeline::new(cwd, paths.snapshot_root.clone(), args.match_mode());
    let replay = spool::scan_replay(&paths.spool, pipeline.matcher_mut());
    let inputs = pipeline.replay(&replay.events);
    // Tags follow the same rule as the TUI: shown once the feed has more than
    // one session, before any --session filter narrows it.
    let tags = session_order(&inputs).len() > 1;
    let mode = if side_by_side {
        ViewMode::SideBySide
    } else {
        ViewMode::Unified
    };
    let width = width.unwrap_or_else(terminal_width);
    let mut out = io::stdout().lock();
    for input in inputs {
        if let Some(needle) = session
            && !input
                .session_id
                .as_deref()
                .is_some_and(|id| session_matches(id, needle))
        {
            continue;
        }
        let card = EditCard::new(input, &build_with, mode, width);
        writeln!(out, "== {}", card.header_text(tags))?;
        let rendered = match (&detected, ansi) {
            (Renderer::Delta(bin), true) if !card.input.diff.unified.is_empty() => delta_ansi(
                bin,
                &card.input.diff.unified,
                mode.effective(width) == ViewMode::SideBySide,
                width,
                &card.input.root,
            ),
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

fn terminal_width() -> u16 {
    crossterm::terminal::size().map(|(w, _)| w).unwrap_or(100)
}

fn run_tui(args: &TuiArgs) -> Result<()> {
    let (paths, cwd) = resolve(&args.run)?;
    let renderer = Renderer::detect(args.run.no_delta);
    let mode = if args.unified {
        ViewMode::Unified
    } else {
        ViewMode::SideBySide
    };
    let mut app = App::new(
        cwd,
        &paths,
        renderer,
        mode,
        terminal_width(),
        args.run.match_mode(),
    );

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
