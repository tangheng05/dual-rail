use clap::{Parser, Subcommand};
use time::Date;
use time::macros::format_description;
use uuid::Uuid;

#[derive(Debug, Parser)]
#[command(
    name = "dual-rail-api",
    version,
    about = "Stripe and Bakong KHQR payments behind one ledger"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, PartialEq, Eq, Subcommand)]
pub enum Command {
    /// Run the API, the KHQR poller and the reconciliation scheduler (the default)
    Serve,
    /// Reconcile one business day and print the report as JSON
    Reconcile {
        /// The local business day, YYYY-MM-DD
        #[arg(value_parser = parse_date)]
        date: Date,
    },
    /// Work the review queue; needs only DATABASE_URL
    #[command(subcommand)]
    Flags(FlagsCommand),
}

#[derive(Debug, PartialEq, Eq, Subcommand)]
pub enum FlagsCommand {
    /// Print open flags as JSON, oldest first
    List {
        /// Include resolved flags
        #[arg(long)]
        all: bool,
    },
    /// Record how a flag was settled; a resolution can't be changed afterwards
    Resolve {
        id: Uuid,
        /// What was decided and why
        #[arg(long, value_parser = non_empty)]
        note: String,
    },
}

fn parse_date(value: &str) -> Result<Date, String> {
    Date::parse(value, format_description!("[year]-[month]-[day]"))
        .map_err(|err| format!("expected YYYY-MM-DD: {err}"))
}

fn non_empty(value: &str) -> Result<String, String> {
    let value = value.trim();
    if value.is_empty() {
        return Err("the note can't be empty".to_owned());
    }
    Ok(value.to_owned())
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;
    use time::macros::date;

    use super::*;

    fn parse(args: &[&str]) -> Result<Option<Command>, clap::Error> {
        Cli::try_parse_from(args).map(|cli| cli.command)
    }

    #[test]
    fn the_command_line_is_well_formed() {
        Cli::command().debug_assert();
    }

    #[test]
    fn serving_is_the_default() {
        assert_eq!(parse(&["dual-rail-api"]).unwrap(), None);
    }

    #[test]
    fn parses_each_command() {
        assert_eq!(
            parse(&["dual-rail-api", "reconcile", "2026-09-26"]).unwrap(),
            Some(Command::Reconcile {
                date: date!(2026 - 09 - 26)
            })
        );
        assert_eq!(
            parse(&["dual-rail-api", "flags", "list", "--all"]).unwrap(),
            Some(Command::Flags(FlagsCommand::List { all: true }))
        );
        assert_eq!(
            parse(&[
                "dual-rail-api",
                "flags",
                "resolve",
                "00000000-0000-0000-0000-000000000001",
                "--note",
                " refunded by hand ",
            ])
            .unwrap(),
            Some(Command::Flags(FlagsCommand::Resolve {
                id: Uuid::from_u128(1),
                note: "refunded by hand".to_owned(),
            }))
        );
    }

    #[test]
    fn rejects_bad_input() {
        for args in [
            &["dual-rail-api", "reconcile", "26-09-2026"][..],
            &[
                "dual-rail-api",
                "flags",
                "resolve",
                "not-a-uuid",
                "--note",
                "x",
            ],
            &[
                "dual-rail-api",
                "flags",
                "resolve",
                "00000000-0000-0000-0000-000000000001",
                "--note",
                "  ",
            ],
            &[
                "dual-rail-api",
                "flags",
                "resolve",
                "00000000-0000-0000-0000-000000000001",
            ],
        ] {
            assert!(parse(args).is_err(), "{args:?}");
        }
    }
}
