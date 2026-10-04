//! Write-only secret fields. A settings form never sends a stored
//! password, API key or token back to the browser: the field renders
//! empty with [`KEEP_PLACEHOLDER`], and what comes back decides the
//! stored value ([`resolve`]). The page used to carry every credential
//! Ryokan holds for other systems, in `value=` attributes and hidden
//! inputs, for anything that could read it (a stolen session, an
//! extension, a script injected anywhere on the page).
//!
//! A stored secret is kept only while the address it is sent to keeps
//! its scheme, host and port. Otherwise a blank password beside an
//! edited URL would send the stored password to whatever host the form
//! names, on Save or on Test, and reading it back out would be one
//! click away again.

/// What the Clear button posts (blank means keep). The notification
/// providers' HMAC secret uses the same value.
pub(crate) const CLEAR: &str = "__CLEAR__";

/// Placeholder for a write-only field whose secret is set.
pub(crate) const KEEP_PLACEHOLDER: &str = "[set; leave blank to keep]";

/// The secret a write-only field stands for. `submitted` is what the
/// form posted, `stored` the saved secret and the URL it was saved
/// with, `url` the URL the form names now. Blank keeps the stored
/// secret while `url` has the same destination (a blank `url` clears
/// it); [`CLEAR`] empties it; anything else replaces it.
pub(crate) fn resolve(
    submitted: &str,
    stored: Option<(&str, &str)>,
    url: &str,
) -> Result<String, String> {
    match submitted {
        CLEAR => Ok(String::new()),
        "" => match stored {
            None => Ok(String::new()),
            Some(("", _)) => Ok(String::new()),
            // No address (Jellyfin turned off): nothing to send it to.
            Some(_) if url.trim().is_empty() => Ok(String::new()),
            Some((secret, stored_url)) if same_destination(stored_url, url) => {
                Ok(secret.to_string())
            }
            Some(_) => Err(
                "The address changed, so the saved secret isn't sent to it. \
                 Enter it again."
                    .to_string(),
            ),
        },
        value => Ok(value.to_string()),
    }
}

/// Scheme, host and port of a URL, read the way the clients read it (a
/// scheme-less `host:port` is `http://host:port`).
fn destination(url: &str) -> Option<(String, String, Option<u16>)> {
    let url = url.trim();
    let parsed = reqwest::Url::parse(url)
        .ok()
        .filter(|u| u.has_host())
        .or_else(|| reqwest::Url::parse(&format!("http://{url}")).ok())?;
    Some((
        parsed.scheme().to_string(),
        parsed.host_str()?.to_ascii_lowercase(),
        parsed.port_or_known_default(),
    ))
}

pub(crate) fn same_destination(a: &str, b: &str) -> bool {
    matches!((destination(a), destination(b)), (Some(x), Some(y)) if x == y)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blank_keeps_only_while_the_destination_stays() {
        let stored = Some(("hunter2", "http://qbit:8080/"));
        assert_eq!(resolve("", stored, "http://qbit:8080").unwrap(), "hunter2");
        assert_eq!(
            resolve("", stored, "http://QBIT:8080/api").unwrap(),
            "hunter2"
        );
        assert_eq!(
            resolve("", stored, "qbit:8080").unwrap(),
            "hunter2",
            "scheme-less is http"
        );
        for moved in [
            "http://evil.example:8080",
            "http://qbit:9090",
            "https://qbit:8080",
        ] {
            assert!(resolve("", stored, moved).is_err(), "{moved}");
        }
        assert_eq!(
            resolve("new", stored, "http://evil.example").unwrap(),
            "new"
        );
        assert_eq!(resolve(CLEAR, stored, "http://qbit:8080").unwrap(), "");
        assert_eq!(
            resolve("", None, "http://qbit:8080").unwrap(),
            "",
            "a new row"
        );
        assert_eq!(
            resolve("", Some(("", "http://qbit:8080")), "http://evil.example").unwrap(),
            "",
            "nothing stored, nothing to send"
        );
        assert_eq!(
            resolve("", stored, "  ").unwrap(),
            "",
            "no address clears it"
        );
    }
}
