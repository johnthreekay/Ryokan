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

/// Scheme, host and port of a URL, read the way the clients read it: a
/// scheme-less `host:port` is `http://` for a local address and
/// `https://` for anything else (each client's `normalize_base_url`).
/// Reading every scheme-less address as `http://` kept a secret saved
/// for `seedbox.example.com:8080` (sent over https) when the form named
/// `http://seedbox.example.com:8080`, and it then went out in the clear.
fn destination(url: &str) -> Option<(String, String, Option<u16>)> {
    let url = url.trim();
    let parsed = reqwest::Url::parse(url)
        .ok()
        .filter(|u| u.has_host())
        .or_else(|| {
            let local = crate::services::jellyfin::is_local_address(&url.to_ascii_lowercase());
            let scheme = if local { "http" } else { "https" };
            reqwest::Url::parse(&format!("{scheme}://{url}")).ok()
        })?;
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
        assert!(
            resolve("", stored, "qbit:8080").is_err(),
            "a scheme-less name that isn't local is https, as the clients send it"
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

    #[test]
    fn a_scheme_less_address_has_the_scheme_the_clients_give_it() {
        // The clients send `seedbox.example.com:8080` over https; reading
        // it as http kept its secret for `http://seedbox.example.com:8080`,
        // which sent it in the clear.
        let remote = Some(("hunter2", "seedbox.example.com:8080"));
        assert!(resolve("", remote, "http://seedbox.example.com:8080").is_err());
        assert_eq!(
            resolve("", remote, "https://seedbox.example.com:8080/").unwrap(),
            "hunter2"
        );
        // A local address is http, scheme or not.
        for (saved, now) in [
            ("192.168.1.5:8080", "http://192.168.1.5:8080"),
            ("localhost:8112", "http://localhost:8112"),
            ("http://10.0.0.2:9091", "10.0.0.2:9091"),
        ] {
            assert_eq!(
                resolve("", Some(("hunter2", saved)), now).unwrap(),
                "hunter2",
                "{saved} -> {now}"
            );
        }
        assert!(
            resolve(
                "",
                Some(("hunter2", "192.168.1.5:8080")),
                "https://192.168.1.5:8080"
            )
            .is_err()
        );
    }
}
