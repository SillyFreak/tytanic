use std::io::Write;
use std::ops::Not;

use color_eyre::eyre;
use termcolor::Color;
use typst::diag::Warned;
use typst_syntax::{FileId, Source, VirtualPath};
use tytanic_core::doc::render::ppi_to_ppp;
use tytanic_core::doc::Document;
use tytanic_core::test::{Id, Reference, Test};

use super::{CompileArgs, Context, ExportArgs};
use crate::cli::OperationFailure;
use crate::ui;
use crate::{cwriteln, DEFAULT_OPTIMIZE_OPTIONS};

#[derive(clap::Args, Debug, Clone)]
#[group(id = "add-args")]
pub struct Args {
    /// Whether to create an ephemeral test
    #[arg(long, short)]
    pub ephemeral: bool,

    /// Whether to create a compile only test
    #[arg(long, short, conflicts_with = "ephemeral")]
    pub compile_only: bool,

    /// Ignore the test template for this test
    #[arg(long)]
    pub no_template: bool,

    #[command(flatten)]
    pub compile: CompileArgs,

    #[command(flatten)]
    pub export: ExportArgs,

    /// The name of the test to add
    pub test: Id,
}

pub fn run(ctx: &mut Context, args: &Args) -> eyre::Result<()> {
    let project = ctx.project()?;
    let suite = ctx.collect_all_tests(&project)?;

    if suite.matched().contains_key(&args.test) {
        ctx.error_test_already_exists(&args.test)?;
        eyre::bail!(OperationFailure);
    }

    let paths = project.paths();
    let vcs = project.vcs();
    let id = args.test.clone();

    if let Some(template) = suite.template().filter(|_| !args.no_template) {
        if args.ephemeral {
            Test::create(
                paths,
                vcs,
                id,
                template,
                Some(Reference::Ephemeral(template.into())),
            )?;
        } else if args.compile_only {
            Test::create(paths, vcs, id, template, None)?;
        } else {
            let world = ctx.world(&args.compile)?;
            let path = project.paths().template();

            let path = path
                .strip_prefix(project.paths().project_root())
                .expect("template is in project root");

            let Warned { output, warnings } = Document::compile(
                Source::new(
                    FileId::new(None, VirtualPath::new(path)),
                    template.to_owned(),
                ),
                &world,
                ppi_to_ppp(args.export.render.pixel_per_inch),
                args.compile.promote_warnings,
            );

            let doc = match output {
                Ok(doc) => {
                    if !warnings.is_empty() {
                        ui::write_diagnostics(
                            &mut ctx.ui.stderr(),
                            ctx.ui.diagnostic_config(),
                            &world,
                            &warnings,
                            &[],
                        )?;
                    }
                    doc
                }
                Err(err) => {
                    ui::write_diagnostics(
                        &mut ctx.ui.stderr(),
                        ctx.ui.diagnostic_config(),
                        &world,
                        &warnings,
                        &err.0,
                    )?;
                    eyre::bail!(OperationFailure);
                }
            };

            Test::create(
                paths,
                vcs,
                id,
                template,
                Some(Reference::Persistent(
                    doc,
                    args.export
                        .no_optimize_references
                        .not()
                        .then(|| Box::new(DEFAULT_OPTIMIZE_OPTIONS.clone())),
                )),
            )?;
        };
    } else {
        Test::create_default(paths, vcs, id)?;
    }

    let mut w = ctx.ui.stderr();

    write!(w, "Added ")?;
    cwriteln!(colored(w, Color::Cyan), "{}", args.test)?;

    Ok(())
}
