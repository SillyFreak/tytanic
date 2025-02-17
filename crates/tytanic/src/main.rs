//! A test runner for t4gl set suites.

use std::io::{self, Write};
use std::process::ExitCode;
use std::sync::atomic::Ordering;

use clap::Parser;
use cli::Context;
use codespan_reporting::term;
use color_eyre::eyre;
use once_cell::sync::Lazy;
use termcolor::{StandardStream, WriteColor};
use tracing::level_filters::LevelFilter;
use tracing_subscriber::filter::Targets;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_tree::HierarchicalLayer;
use tytanic_core::config::{Config, ConfigLayer};

use crate::cli::{Args, OperationFailure, TestFailure};
use crate::ui::Ui;

mod cli;
mod json;
mod kit;
mod report;
mod runner;
mod ui;
mod world;

/// The default optimization options to use.
pub static DEFAULT_OPTIMIZE_OPTIONS: Lazy<oxipng::Options> =
    Lazy::new(oxipng::Options::max_compression);

fn main() -> ExitCode {
    match main_impl() {
        Ok(code) => code,
        Err(err) => {
            eprintln!("{err:?}");
            ExitCode::from(cli::EXIT_ERROR)
        }
    }
}

fn main_impl() -> eyre::Result<ExitCode> {
    let args = Args::parse();

    color_eyre::install()?;

    let cc = match args.global.output.color {
        clap::ColorChoice::Auto => termcolor::ColorChoice::Auto,
        clap::ColorChoice::Always => termcolor::ColorChoice::Always,
        clap::ColorChoice::Never => termcolor::ColorChoice::Never,
    };

    let ui = Ui::new(
        cc,
        cc,
        term::Config {
            display_style: term::DisplayStyle::Rich,
            tab_width: 2,
            ..Default::default()
        },
    );

    // this is a hack, termcolor does not expose any way for us to easily reuse
    // their internal mechanism of checking whether the given stream is color
    // capable without constructing a stream and asking for it
    let tracing_ansi = StandardStream::stderr(cc).supports_color();

    tracing_subscriber::registry()
        .with(
            // we set with_ansi to true, because ui handles the usage of color
            // through termcolor::StandardStream
            HierarchicalLayer::new(4)
                .with_targets(true)
                .with_ansi(tracing_ansi),
        )
        .with(Targets::new().with_target(
            tytanic_core::TOOL_NAME,
            match args.global.output.verbose {
                0 => LevelFilter::OFF,
                1 => LevelFilter::ERROR,
                2 => LevelFilter::WARN,
                3 => LevelFilter::INFO,
                4 => LevelFilter::DEBUG,
                5.. => LevelFilter::TRACE,
            },
        ))
        .init();

    if let Err(err) = ctrlc::set_handler(|| {
        cli::CANCELLED.store(true, Ordering::SeqCst);
    }) {
        writeln!(ui.error()?, "couldn't register ctrl-c handler:\n{err}")?;
        writeln!(
            ui.hint()?,
            "pressing ctrl-c will discard output of failed tests"
        )?;
    }

    if let Some(jobs) = args.global.jobs {
        let jobs = if jobs < 2 {
            writeln!(ui.warn()?, "at least 2 threads are needed, using 2")?;
            2
        } else {
            jobs
        };

        rayon::ThreadPoolBuilder::new()
            .num_threads(jobs)
            .build_global()
            .ok();
    }

    let mut config = Config::new(None);
    config.user = ConfigLayer::collect_user()?;

    let mut ctx = Context::new(&args, &ui);

    let exit_code = match ctx.run() {
        Ok(()) => cli::EXIT_OK,
        Err(err) => 'err: {
            let root = err.root_cause();

            for cause in err.chain() {
                if let Some(TestFailure) = cause.downcast_ref() {
                    break 'err cli::EXIT_TEST_FAILURE;
                }

                if let Some(OperationFailure) = cause.downcast_ref() {
                    break 'err cli::EXIT_OPERATION_FAILURE;
                }
            }

            // FIXME: https://github.com/serde-rs/json/issues/1169
            if root
                .downcast_ref()
                .and_then(serde_json::Error::io_error_kind)
                .or_else(|| root.downcast_ref().map(io::Error::kind))
                .is_some_and(|kind| kind == io::ErrorKind::BrokenPipe)
            {
                break 'err cli::EXIT_OK;
            }

            writeln!(
                ctx.ui.error()?,
                "tytanic ran into an unexpected error, this is most likely a bug"
            )?;
            writeln!(
                ctx.ui.hint()?,
                "Please consider reporting this at {}/issues/new",
                std::env!("CARGO_PKG_REPOSITORY")
            )?;

            if !std::env::var("RUST_BACKTRACE").is_ok_and(|var| var == "full") {
                writeln!(
                    ctx.ui.hint()?,
                    "consider running with the environment variable RUST_BACKTRACE set to 'full' when reporting issues\n",
                )?;
            }
            writeln!(ctx.ui.error()?, "{err:?}")?;

            cli::EXIT_OPERATION_FAILURE
        }
    };

    ctx.ui.flush()?;

    Ok(ExitCode::from(exit_code))
}
