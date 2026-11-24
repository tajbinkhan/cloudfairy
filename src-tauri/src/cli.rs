//! Reads Cloudfairy's command line:
//!
//! ```text
//! cloudfairy start 3000 [--name Storefront]
//! cloudfairy stop 3000
//! cloudfairy stop --all
//! cloudfairy open
//! ```
//!
//! On Windows and Linux a `cloudfairy://` link also arrives as an argument. Any web page can
//! open such a link, so it is returned as `Command::Link` and never read as a command.

use crate::{transfer, tunnel};

#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    /// Start the tunnel for this normalized target, saving a new one if needed.
    Start { target: String, name: Option<String> },
    Stop { target: String },
    StopAll,
    Open,
    Link(String),
}

/// Arguments Cloudfairy passes to itself, such as the login item's `--minimized`.
const OWN_FLAGS: [&str; 1] = [crate::MINIMIZED_ARG];

pub fn is_link(arg: &str) -> bool {
    arg.get(..11)
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("cloudfairy:"))
}

/// Parses the arguments after the program name. `Ok(None)` means there is nothing to do,
/// as on a normal launch.
pub fn parse<S: AsRef<str>>(args: &[S]) -> Result<Option<Command>, String> {
    let args: Vec<&str> = args
        .iter()
        .map(AsRef::as_ref)
        .filter(|a| !OWN_FLAGS.contains(a))
        .collect();
    // Checked first, so a link can never be read as a terminal command.
    if let Some(link) = args.iter().find(|a| is_link(a)) {
        return Ok(Some(Command::Link(link.to_string())));
    }
    let Some((&command, rest)) = args.split_first() else {
        return Ok(None);
    };
    match command.to_ascii_lowercase().as_str() {
        "start" => parse_start(rest).map(Some),
        "stop" => parse_stop(rest).map(Some),
        "open" => match rest.first() {
            None => Ok(Some(Command::Open)),
            Some(extra) => Err(unexpected(extra)),
        },
        // Launchers and the OS sometimes add flags of their own. They aren't commands.
        _ if command.starts_with('-') => Ok(None),
        _ => Err(format!(
            "Cloudfairy doesn't know the command \"{command}\". Use start, stop or open."
        )),
    }
}

fn unexpected(arg: &str) -> String {
    if arg.starts_with('-') {
        format!("Cloudfairy doesn't know the option \"{arg}\".")
    } else {
        format!("Cloudfairy didn't expect \"{arg}\" in the command.")
    }
}

fn parse_start(args: &[&str]) -> Result<Command, String> {
    let mut target = None;
    let mut name = None;
    let mut it = args.iter();
    while let Some(&arg) = it.next() {
        let value = if arg == "--name" || arg == "-n" {
            Some(*it.next().ok_or("Add a name after --name.")?)
        } else {
            arg.strip_prefix("--name=")
        };
        match value {
            Some(_) if name.is_some() => return Err("Give --name only once.".into()),
            Some(v) => name = Some(v),
            None if arg.starts_with('-') || target.is_some() => return Err(unexpected(arg)),
            None => target = Some(arg),
        }
    }
    let target = target.ok_or("Say which port to share, for example \"cloudfairy start 3000\".")?;
    Ok(Command::Start {
        target: tunnel::normalize_target(target)?,
        name: name.map(transfer::clean_name).transpose()?.flatten(),
    })
}

fn parse_stop(args: &[&str]) -> Result<Command, String> {
    match args {
        [] => Err("Say which tunnel to stop, for example \"cloudfairy stop 3000\" or \"cloudfairy stop --all\".".into()),
        ["--all"] => Ok(Command::StopAll),
        [option] if option.starts_with('-') => Err(unexpected(option)),
        [target] => Ok(Command::Stop {
            target: tunnel::normalize_target(target)?,
        }),
        [_, extra, ..] => Err(format!("Cloudfairy didn't expect \"{extra}\" in the command.")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_ok(args: &[&str]) -> Option<Command> {
        parse(args).unwrap()
    }

    fn start(target: &str, name: Option<&str>) -> Option<Command> {
        Some(Command::Start {
            target: target.into(),
            name: name.map(Into::into),
        })
    }

    #[test]
    fn plain_launches_do_nothing() {
        assert_eq!(parse_ok(&[]), None);
        assert_eq!(parse_ok(&["--minimized"]), None);
        // macOS used to pass a process serial number to apps opened from Finder.
        assert_eq!(parse_ok(&["-psn_0_12345"]), None);
    }

    #[test]
    fn start_commands() {
        assert_eq!(parse_ok(&["start", "3000"]), start("http://localhost:3000", None));
        assert_eq!(parse_ok(&["START", ":8080"]), start("http://localhost:8080", None));
        assert_eq!(
            parse_ok(&["start", "3000", "--name", "Storefront"]),
            start("http://localhost:3000", Some("Storefront"))
        );
        assert_eq!(
            parse_ok(&["start", "--name=My shop", "localhost:5173"]),
            start("http://localhost:5173", Some("My shop"))
        );
        assert_eq!(parse_ok(&["start", "-n", "  ", "3000"]), start("http://localhost:3000", None));
        assert_eq!(
            parse_ok(&["--minimized", "start", "https://localhost:8443"]),
            start("https://localhost:8443", None)
        );
    }

    #[test]
    fn stop_and_open_commands() {
        assert_eq!(
            parse_ok(&["stop", "3000"]),
            Some(Command::Stop { target: "http://localhost:3000".into() })
        );
        assert_eq!(parse_ok(&["stop", "--all"]), Some(Command::StopAll));
        assert_eq!(parse_ok(&["open"]), Some(Command::Open));
    }

    #[test]
    fn bad_commands_explain_themselves() {
        let err = |args: &[&str]| parse(args).unwrap_err();
        assert!(err(&["share", "3000"]).contains("start, stop or open"));
        assert!(err(&["start"]).contains("which port"));
        assert!(err(&["start", "0"]).contains("1 to 65535"));
        assert!(err(&["start", "3000", "4000"]).contains("\"4000\""));
        assert!(err(&["start", "3000", "--name"]).contains("after --name"));
        assert!(err(&["start", "3000", "--name", "a", "--name", "b"]).contains("once"));
        assert!(err(&["start", "3000", "--verbose"]).contains("option"));
        assert!(err(&["start", "3000", "--name", "a\tb"]).contains("control"));
        assert!(err(&["stop"]).contains("which tunnel"));
        assert!(err(&["stop", "--force"]).contains("option"));
        assert!(err(&["stop", "3000", "--all"]).contains("--all"));
        assert!(err(&["stop", "--all", "3000"]).contains("3000"));
        assert!(err(&["stop", "nope://x"]).contains("http://"));
        assert!(err(&["open", "now"]).contains("now"));
    }

    #[test]
    fn links_always_take_the_link_path() {
        let link = "cloudfairy://start?port=3000";
        assert_eq!(parse_ok(&[link]), Some(Command::Link(link.into())));
        assert_eq!(parse_ok(&["CloudFairy:open"]), Some(Command::Link("CloudFairy:open".into())));
        // Even next to words that look like a command, a link is never run as one.
        assert_eq!(parse_ok(&[link, "start", "4000"]), Some(Command::Link(link.into())));
        assert_eq!(parse_ok(&["start", "4000", link]), Some(Command::Link(link.into())));
        assert!(!is_link("cloudfairy"));
        assert!(!is_link("start"));
    }
}
