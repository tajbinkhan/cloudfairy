//! Reads `cloudfairy://` links:
//!
//! ```text
//! cloudfairy://start?port=3000&name=Storefront
//! cloudfairy://stop?port=3000
//! cloudfairy://open
//! ```
//!
//! Any web page can open these links, so parsing is strict: only a port on localhost, an
//! optional name, and nothing else. `remote.rs` asks the user before a link starts anything.

use url::Url;

use crate::transfer;

pub const SCHEME: &str = "cloudfairy";
/// Real links are short. A long one is junk or an attempt to flood the window.
const MAX_LINK_LEN: usize = 512;

#[derive(Debug, PartialEq, Eq)]
pub enum LinkAction {
    Start { port: u16, name: Option<String> },
    Stop { port: u16 },
    Open,
}

pub fn parse(link: &str) -> Result<LinkAction, String> {
    if link.len() > MAX_LINK_LEN {
        return Err("The link is too long.".into());
    }
    let url = Url::parse(link).map_err(|_| "The link isn't a valid address.".to_string())?;
    if !url.scheme().eq_ignore_ascii_case(SCHEME) {
        return Err("The link isn't a cloudfairy:// link.".into());
    }
    if !url.username().is_empty() || url.password().is_some() || url.port().is_some() || url.fragment().is_some() {
        return Err("The link has parts Cloudfairy doesn't use.".into());
    }
    // `cloudfairy://start?…` puts the action in the host, `cloudfairy:start?…` in the path.
    let action = match url.host_str() {
        Some(host) if matches!(url.path(), "" | "/") => host.to_string(),
        Some(_) => return Err("The link has parts Cloudfairy doesn't use.".into()),
        None => url.path().trim_end_matches('/').to_string(),
    };

    let mut port = None;
    let mut name = None;
    for (key, value) in url.query_pairs() {
        let slot = match key.as_ref() {
            "port" => &mut port,
            "name" => &mut name,
            other => return Err(format!("The link has an unknown setting \"{}\".", short(other))),
        };
        if slot.replace(value.into_owned()).is_some() {
            return Err(format!("The link gives \"{key}\" more than once."));
        }
    }

    let action = action.to_ascii_lowercase();
    let port = port.map(|p| parse_port(&p)).transpose()?;
    let need_port = || port.ok_or_else(|| "The link doesn't say which port.".to_string());
    let unknown = |key: &str| Err(format!("The link has an unknown setting \"{key}\"."));
    match action.as_str() {
        "start" => Ok(LinkAction::Start {
            port: need_port()?,
            name: name.map(|n| transfer::clean_name(&n)).transpose()?.flatten(),
        }),
        "stop" | "open" if name.is_some() => unknown("name"),
        "stop" => Ok(LinkAction::Stop { port: need_port()? }),
        "open" if port.is_some() => unknown("port"),
        "open" => Ok(LinkAction::Open),
        _ => Err(format!("The link asks for \"{}\", which Cloudfairy doesn't do.", short(&action))),
    }
}

fn parse_port(value: &str) -> Result<u16, String> {
    let bad = || "The port in the link must be a number from 1 to 65535.".to_string();
    if value.is_empty() || value.len() > 5 || !value.bytes().all(|b| b.is_ascii_digit()) {
        return Err(bad());
    }
    value.parse::<u16>().ok().filter(|p| *p > 0).ok_or_else(bad)
}

/// Keeps text from a link short enough to show in a message.
fn short(s: &str) -> String {
    let mut out: String = s.chars().filter(|c| !c.is_control()).take(40).collect();
    if s.chars().count() > 40 {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn start(port: u16, name: Option<&str>) -> LinkAction {
        LinkAction::Start { port, name: name.map(Into::into) }
    }

    #[test]
    fn reads_valid_links() {
        assert_eq!(parse("cloudfairy://start?port=3000&name=Storefront"), Ok(start(3000, Some("Storefront"))));
        assert_eq!(parse("cloudfairy://start?port=3000"), Ok(start(3000, None)));
        assert_eq!(parse("cloudfairy://start/?port=8080&name="), Ok(start(8080, None)));
        assert_eq!(parse("cloudfairy://start?name=My%20shop&port=5173"), Ok(start(5173, Some("My shop"))));
        assert_eq!(parse("cloudfairy://start?port=5173&name=Caf%C3%A9+app"), Ok(start(5173, Some("Café app"))));
        assert_eq!(parse("CLOUDFAIRY://START?port=1"), Ok(start(1, None)));
        assert_eq!(parse("cloudfairy:start?port=65535"), Ok(start(65535, None)));
        assert_eq!(parse("cloudfairy://stop?port=3000"), Ok(LinkAction::Stop { port: 3000 }));
        assert_eq!(parse("cloudfairy://open"), Ok(LinkAction::Open));
        assert_eq!(parse("cloudfairy://open/"), Ok(LinkAction::Open));
    }

    #[test]
    fn rejects_anything_else() {
        let err = |link: &str| parse(link).unwrap_err();
        assert!(err("cloudfairy://start").contains("which port"));
        assert!(err("cloudfairy://stop").contains("which port"));
        for port in ["0", "65536", "-1", "3000a", "+80", " 80", "0x50", "", "000003000"] {
            let link = format!("cloudfairy://start?port={}", port.replace(' ', "%20"));
            assert!(err(&link).contains("1 to 65535"), "{port}");
        }
        assert!(err("cloudfairy://start?port=3000&port=4000").contains("more than once"));
        assert!(err("cloudfairy://start?port=3000&host=192.168.1.5").contains("unknown setting"));
        assert!(err("cloudfairy://start?port=3000&target=http://example.com").contains("unknown setting"));
        assert!(err("cloudfairy://start?port=3000&name=a%0Ab").contains("control"));
        assert!(err(&format!("cloudfairy://start?port=3000&name={}", "x".repeat(101))).contains("longer"));
        assert!(err("cloudfairy://stop?port=3000&name=x").contains("name"));
        assert!(err("cloudfairy://open?port=3000").contains("port"));
        assert!(err("cloudfairy://delete?port=3000").contains("delete"));
        assert!(err("cloudfairy://start/evil?port=3000").contains("parts"));
        assert!(err("cloudfairy://user:pw@start?port=3000").contains("parts"));
        assert!(err("cloudfairy://start:99?port=3000").contains("parts"));
        assert!(err("cloudfairy://start?port=3000#x").contains("parts"));
        assert!(err("https://start?port=3000").contains("cloudfairy://"));
        assert!(err(&format!("cloudfairy://start?port=3000&name={}", "x".repeat(600))).contains("too long"));
    }
}
