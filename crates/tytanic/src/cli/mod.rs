use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::{env, io};

use chrono::{DateTime, Utc};
use clap::ColorChoice;
use color_eyre::eyre;
use color_eyre::eyre::WrapErr;
use termcolor::Color;
use thiserror::Error;
use tytanic_core::config::{Config, ConfigLayer};
use tytanic_core::project::Project;
use tytanic_core::test::{Id, Suite};
use tytanic_core::test_set::{self, eval, Error as TestSetError, TestSet};

use crate::ui::{self, Ui};
use crate::world::SystemWorld;
use crate::{cwrite, kit};

pub mod add;
pub mod list;
pub mod remove;
pub mod run;
pub mod status;
pub mod update;
pub mod util;

/// Whether we received a signal we can gracefully exit from.
pub static CANCELLED: AtomicBool = AtomicBool::new(false);

/// The separator used for multiple paths.
const ENV_PATH_SEP: char = if cfg!(windows) { ';' } else { ':' };

/// Tytanic exited successfully.
pub const EXIT_OK: u8 = 0;

/// At least one test failed.
pub const EXIT_TEST_FAILURE: u8 = 1;

/// The requested operation failed gracefully.
pub const EXIT_OPERATION_FAILURE: u8 = 2;

/// An unexpected error occurred.
pub const EXIT_ERROR: u8 = 3;

/// A graceful error.
#[derive(Debug, Error)]
#[error("an operation failed")]
pub struct OperationFailure;

/// A test failure.
#[derive(Debug, Error)]
#[error("one or more test failed")]
pub struct TestFailure;

pub struct Context<'a> {
    /// The parsed top-level arguments.
    pub args: &'a Args,

    /// The terminal ui.
    pub ui: &'a Ui,
}

impl<'a> Context<'a> {
    pub fn new(args: &'a Args, ui: &'a Ui) -> Self {
        Self { args, ui }
    }
}

impl Context<'_> {
    pub fn error_aborted(&self) -> io::Result<()> {
        writeln!(self.ui.error()?, "Operation aborted")
    }

    pub fn error_root_not_found(&self, root: &Path) -> io::Result<()> {
        writeln!(self.ui.error()?, "Root '{}' not found", root.display())
    }

    pub fn error_no_project(&self) -> io::Result<()> {
        writeln!(self.ui.error()?, "Must be in a typst project")?;

        let mut w = self.ui.hint()?;
        write!(w, "You can pass the project root using ")?;
        cwrite!(colored(w, Color::Cyan), "--root <path>")?;
        writeln!(w)
    }

    pub fn error_test_set_failure(&self, error: TestSetError) -> io::Result<()> {
        writeln!(
            self.ui.error()?,
            "Couldn't parse or evaluate test set expression:\n{error:?}",
        )
    }

    pub fn error_test_already_exists(&self, id: &Id) -> io::Result<()> {
        let mut w = self.ui.error()?;

        write!(w, "Test ")?;
        ui::write_test_id(&mut w, id)?;
        writeln!(w, " already exists")
    }

    pub fn error_no_tests(&self) -> io::Result<()> {
        writeln!(self.ui.error()?, "Matched no tests")
    }

    pub fn error_too_many_tests(&self, expr: &str) -> io::Result<()> {
        writeln!(self.ui.error()?, "Matched more than one test")?;

        let mut w = self.ui.hint()?;
        write!(w, "use '")?;
        cwrite!(colored(w, Color::Cyan), "all:")?;
        writeln!(w, "{expr}' to confirm using all tests")
    }

    pub fn error_nested_tests(&self) -> io::Result<()> {
        writeln!(self.ui.error()?, "Found nested tests")?;

        let mut w = self.ui.hint()?;
        writeln!(w, "This is no longer supported")?;
        write!(w, "You can run ")?;
        cwrite!(colored(w, Color::Cyan), "tt util migrate")?;
        writeln!(w, " to automatically fix the tests")
    }

    pub fn run(&mut self) -> eyre::Result<()> {
        self.args.cmd.run(self)
    }
}

// TODO(tinger): cache these values
impl Context<'_> {
    /// Resolve the current root.
    pub fn root(&self) -> eyre::Result<PathBuf> {
        Ok(match &self.args.global.root {
            Some(root) => {
                if !root.try_exists()? {
                    self.error_root_not_found(root)?;
                    eyre::bail!(OperationFailure);
                }

                root.canonicalize()?
            }
            None => env::current_dir().wrap_err("reading PWD")?,
        })
    }

    /// Resolve the user and override config layers.
    pub fn config(&self) -> eyre::Result<Config> {
        // TODO(tinger): cli/envar overrides go here

        let mut config = Config::new(None);
        config.user = ConfigLayer::collect_user()?;

        Ok(config)
    }

    /// Discover the current and ensure it is initialized.
    pub fn project(&self) -> eyre::Result<Project> {
        let root = self.root()?;

        let Some(project) = Project::discover(root, self.args.global.root.is_some())? else {
            self.error_no_project()?;
            eyre::bail!(OperationFailure);
        };

        Ok(project)
    }

    /// Create a new test set from the arguments with the given context.
    pub fn test_set(&self, filter: &FilterArgs) -> eyre::Result<TestSet> {
        if !filter.tests.is_empty() {
            let mut tests = filter
                .tests
                .iter()
                .map(|test| eval::Set::built_in_pattern(test_set::Pat::Exact(test.into())));

            let a = tests.next().expect("`tests` is not empty");

            let set = match tests.next() {
                Some(b) => eval::Set::built_in_union(a, b, tests),
                None => a,
            };

            Ok(TestSet::new(eval::Context::empty(), set))
        } else {
            let ctx = eval::Context::with_built_ins();
            let mut set = match TestSet::parse_and_evaluate(ctx, &filter.expression) {
                Ok(set) => set,
                Err(err) => {
                    self.error_test_set_failure(err)?;
                    eyre::bail!(OperationFailure);
                }
            };

            if !filter.no_implicit_skip {
                set.add_implicit_skip();
            }

            Ok(set)
        }
    }

    /// Collect and filter tests for the given project.
    pub fn collect_tests(&self, project: &Project, set: &TestSet) -> eyre::Result<Suite> {
        if !util::migrate::collect_old_structure(project.paths(), "self")?.is_empty() {
            self.error_nested_tests()?;
            eyre::bail!(OperationFailure);
        }

        let suite = Suite::collect(project.paths(), set)?;

        Ok(suite)
    }

    /// Collect all tests for the given project.
    pub fn collect_all_tests(&self, project: &Project) -> eyre::Result<Suite> {
        let suite = Suite::collect(
            project.paths(),
            &TestSet::new(eval::Context::empty(), eval::Set::built_in_all()),
        )?;
        Ok(suite)
    }

    /// Create a SystemWorld from the given args.
    pub fn world(&self, compile: &CompileArgs) -> eyre::Result<SystemWorld> {
        kit::world(
            self.root()?,
            &self.args.global.fonts,
            &self.args.global.package,
            compile,
        )
    }
}

macro_rules! ansi {
    ($s:expr; b) => {
        concat!("\x1B[1m", $s, "\x1B[0m")
    };
    ($s:expr; u) => {
        concat!("\x1B[4m", $s, "\x1B[0m")
    };
    ($s:expr;) => {
        $s
    };
    ($s:expr; $first:ident $( + $rest:tt)*) => {
        ansi!(ansi!($s; $($rest)*); $first)
    };
}

// NOTE(tinger): we use clap style formatting here and keep it simple to avoid a
// proc macro dependency for a single use of static ansi formatting
#[rustfmt::skip]
static AFTER_LONG_ABOUT: &str = concat!(
    ansi!("Exit Codes:\n"; u + b),
    "  ", ansi!("0"; b), "  Success\n",
    "  ", ansi!("1"; b), "  At least one test failed\n",
    "  ", ansi!("2"; b), "  The requested operation failed\n",
    "  ", ansi!("3"; b), "  An unexpected error occurred",
);

#[derive(clap::Args, Debug, Clone)]
pub struct GlobalArgs {
    /// The project root directory
    #[arg(long, short, env = "TYPST_ROOT", global = true)]
    pub root: Option<PathBuf>,

    /// The amount of threads to use.
    #[arg(long, short, global = true)]
    pub jobs: Option<usize>,

    #[command(flatten, next_help_heading = "Font Options")]
    pub fonts: FontArgs,

    #[command(flatten, next_help_heading = "Package Options")]
    pub package: PackageArgs,

    #[command(flatten, next_help_heading = "Output Options")]
    pub output: OutputArgs,
}

#[derive(clap::Args, Debug, Clone)]
pub struct FilterArgs {
    // reason: as above, clap does not ignore the extra formatting
    #[allow(rustdoc::bare_urls)]
    /// A test set expression which selects which tests to operate on
    ///
    /// Note that some commands will wrap the expression in `(...) ~ skip()`
    /// unless `--no-implicit-skip` or explcit tests are, these include `list`,
    /// `run`, `remove` and `update`.
    ///
    /// See the language reference and guide a
    /// https://tingerrr.github.io/tytanic/index.html
    /// for more info.
    #[arg(short, long, default_value = "all()")]
    pub expression: String,

    /// Don't automatically remove tests marked as skip
    ///
    /// If this option is not passed, then this is equivalent to wrapping the
    /// test set expression in `(...) ~ skip()`.
    #[arg(short = 'S', long)]
    pub no_implicit_skip: bool,

    /// The exact tests to operate on
    ///
    /// Equivalent to passing `--expression 'exact:a | exact:b | ...'` and
    /// implies `--no-implicit-skip`.
    #[arg(required = false, conflicts_with = "expression")]
    pub tests: Vec<String>,
}

fn parse_source_date_epoch(raw: &str) -> Result<DateTime<Utc>, String> {
    let timestamp: i64 = raw
        .parse()
        .map_err(|err| format!("timestamp must be decimal integer ({err})"))?;
    DateTime::from_timestamp(timestamp, 0).ok_or_else(|| "timestamp out of range".to_string())
}

#[derive(clap::Args, Debug, Clone)]
pub struct CompileArgs {
    /// The timestamp used for compilation.
    ///
    /// For more information, see
    /// <https://reproducible-builds.org/specs/source-date-epoch/>.
    #[arg(
        long,
        env = "SOURCE_DATE_EPOCH",
        value_name = "UNIX_TIMESTAMP",
        value_parser = parse_source_date_epoch,
        global = true,
    )]
    pub creation_timestamp: Option<DateTime<Utc>>,

    /// Promote warnings to errors
    #[arg(long, global = true)]
    pub promote_warnings: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, clap::ValueEnum)]
pub enum Direction {
    /// The document is read left-to-right.
    Ltr,

    /// The document is read right-to-left.
    Rtl,
}

#[derive(clap::Args, Debug, Clone)]
pub struct RenderArgs {
    /// The document direction
    ///
    /// This is used to correctly align images with different dimensions when
    /// generating diff images.
    #[arg(long, visible_alias = "dir", global = true)]
    pub direction: Option<Direction>,

    /// The pixel per inch to use for raster export
    #[arg(long, visible_alias = "ppi", default_value_t = 144.0, global = true)]
    pub pixel_per_inch: f32,
}

#[derive(clap::Args, Debug, Clone)]
pub struct ExportArgs {
    #[command(flatten)]
    pub render: RenderArgs,

    /// Whether to skip saving temporary output, such as ephemeral references
    #[arg(long, global = true)]
    pub no_save_temporary: bool,

    /// Whether to skip optimizing reference images
    #[arg(long, global = true)]
    pub no_optimize_references: bool,
}

#[derive(clap::Args, Debug, Clone)]
pub struct CompareArgs {
    /// The maximum delta in each channel of a pixel
    ///
    /// If a single channel (red/green/blue/alpha component) of a pixel differs
    /// by this much between reference and output the pixel is counted as a
    /// deviation.
    #[arg(long, default_value_t = 0, global = true)]
    pub max_delta: u8,

    /// The maximum deviations per reference
    ///
    /// If a reference and output image have more than the given deviations it's
    /// counted as a failure.
    #[arg(long, default_value_t = 0, global = true)]
    pub max_deviations: usize,
}

#[derive(clap::Args, Debug, Clone)]
pub struct RunArgs {
    /// Whether to abort after the first failure
    ///
    /// Keep in mind that because tests are run in parallel, this may not stop
    /// immediately. But it will not schedule any new tests to run after one
    /// failure has been detected.
    #[arg(long, global = true)]
    pub no_fail_fast: bool,
}

#[derive(clap::Args, Debug, Clone)]
pub struct FontArgs {
    /// Do not read system fonts
    #[arg(long, global = true)]
    pub ignore_system_fonts: bool,

    /// Add a directory to read fonts from (can be repeated)
    #[arg(
        long = "font-path",
        env = "TYPST_FONT_PATHS",
        value_name = "DIR",
        value_delimiter = ENV_PATH_SEP,
        global = true,
    )]
    pub font_paths: Vec<PathBuf>,
}

#[derive(clap::Args, Debug, Clone)]
pub struct PackageArgs {
    /// Custom path to local packages, defaults to system-dependent location
    #[clap(long, env = "TYPST_PACKAGE_PATH", value_name = "DIR")]
    pub package_path: Option<PathBuf>,

    /// Custom path to package cache, defaults to system-dependent location
    #[clap(long, env = "TYPST_PACKAGE_CACHE_PATH", value_name = "DIR")]
    pub package_cache_path: Option<PathBuf>,

    /// Path to a custom CA certificate to use when making network requests
    #[clap(long, visible_alias = "cert", env = "TYPST_CERT")]
    pub certificate: Option<PathBuf>,
}

#[derive(clap::Args, Debug, Clone)]
pub struct OutputArgs {
    /// When to use colorful output
    ///
    /// If set to auto, color will only be enabled if a capable terminal is
    /// detected.
    #[clap(
        long,
        value_name = "WHEN",
        require_equals = true,
        num_args = 0..=1,
        default_value = "auto",
        default_missing_value = "always",
        global = true,
    )]
    pub color: ColorChoice,

    /// Produce more logging output [-v ... -vvvvv]
    ///
    /// Logs are written to stderr, the increasing number of verbose flags
    /// corresponds to the log levels ERROR, WARN, INFO, DEBUG, TRACE.
    #[arg(long, short, action = clap::ArgAction::Count, global = true)]
    pub verbose: u8,
}

/// Run and manage tests for typst projects
#[derive(clap::Parser, Debug, Clone)]
#[command(version, after_long_help = AFTER_LONG_ABOUT)]
pub struct Args {
    #[command(flatten)]
    pub global: GlobalArgs,

    /// The command to run
    #[command(subcommand)]
    pub cmd: Command,
}

#[derive(clap::Subcommand, Debug, Clone)]
pub enum Command {
    /// Show information about the current project
    #[command(visible_alias = "st")]
    Status(status::Args),

    /// List the tests in the current project
    #[command(visible_alias = "ls")]
    List(list::Args),

    /// Compile and compare tests
    #[command(visible_alias = "r")]
    Run(run::Args),

    /// Compile and update tests
    #[command()]
    Update(update::Args),

    /// Add a new test
    ///
    /// The default test simply contains `Hello World`, if a
    /// test template file is given, it is used instead.
    #[command()]
    Add(add::Args),

    /// Remove tests
    #[command(visible_alias = "rm")]
    Remove(remove::Args),

    /// Utility commands
    #[command()]
    Util(util::Args),
}

impl Command {
    pub fn run(&self, ctx: &mut Context) -> eyre::Result<()> {
        match self {
            Command::Add(args) => add::run(ctx, args),
            Command::Remove(args) => remove::run(ctx, args),
            Command::Status(args) => status::run(ctx, args),
            Command::List(args) => list::run(ctx, args),
            Command::Update(args) => update::run(ctx, args),
            Command::Run(args) => run::run(ctx, args),
            Command::Util(args) => args.cmd.run(ctx),
        }
    }
}
