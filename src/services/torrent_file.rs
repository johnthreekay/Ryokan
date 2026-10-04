//! Reading a torrent's v1 info-hash from a magnet link or a `.torrent`
//! file, for grabs that arrive without one (an autobrr push whose
//! template had no `{{.InfoHash}}` to fill). Without a hash the grab
//! can only be found in the client by name, which a single-file
//! torrent named after its file never matches, so post-processing
//! marked it removed while it was still downloading.

use std::sync::LazyLock;
use std::time::Duration;

/// A real `.torrent` is well under this; a season pack with small
/// pieces reaches a few MB.
const TORRENT_BODY_CAP: usize = 10 << 20;

static HTTP_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .user_agent("Ryokan/0.1")
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()
        .expect("torrent-file HTTP client")
});

/// The info-hash a magnet link names (`xt=urn:btih:`, 40 hex or 32
/// base32 characters), as lowercase hex.
pub fn magnet_info_hash(uri: &str) -> Option<String> {
    let query = uri.strip_prefix("magnet:?")?;
    query.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        if !key.eq_ignore_ascii_case("xt") {
            return None;
        }
        let hash = value
            .get(..9)
            .filter(|p| p.eq_ignore_ascii_case("urn:btih:"))
            .map(|_| &value[9..])?;
        match hash.len() {
            40 => super::download_client::normalize_info_hash(hash),
            32 => base32_to_hex(hash),
            _ => None,
        }
    })
}

fn base32_to_hex(s: &str) -> Option<String> {
    let mut bits: u64 = 0;
    let mut count = 0;
    let mut out = Vec::with_capacity(20);
    for c in s.bytes() {
        let v = match c.to_ascii_uppercase() {
            c @ b'A'..=b'Z' => c - b'A',
            c @ b'2'..=b'7' => c - b'2' + 26,
            _ => return None,
        };
        bits = (bits << 5) | u64::from(v);
        count += 5;
        if count >= 8 {
            count -= 8;
            out.push((bits >> count) as u8);
        }
    }
    (out.len() == 20).then(|| hex::encode(out))
}

/// The v1 info-hash of a `.torrent`: the SHA-1 of the top-level `info`
/// value's bencoded bytes. `None` for anything that isn't a dictionary
/// with an `info` dictionary (an NZB, an HTML error page).
pub fn torrent_info_hash(bytes: &[u8]) -> Option<String> {
    if bytes.first() != Some(&b'd') {
        return None;
    }
    let mut i = 1;
    while *bytes.get(i)? != b'e' {
        let (key, after_key) = bencode_string(bytes, i)?;
        let end = bencode_end(bytes, after_key, 0)?;
        if key == b"info" {
            if bytes[after_key] != b'd' {
                return None;
            }
            let mut hasher = sha1_smol::Sha1::new();
            hasher.update(&bytes[after_key..end]);
            return Some(hasher.digest().to_string());
        }
        i = end;
    }
    None
}

/// A byte string at `start`: its contents and the index past it.
fn bencode_string(bytes: &[u8], start: usize) -> Option<(&[u8], usize)> {
    let colon = start + bytes.get(start..)?.iter().position(|&b| b == b':')?;
    let len: usize = std::str::from_utf8(&bytes[start..colon])
        .ok()?
        .parse()
        .ok()?;
    let end = colon.checked_add(1)?.checked_add(len)?;
    Some((bytes.get(colon + 1..end)?, end))
}

/// The index just past the bencoded value at `start`. Depth-limited so
/// a hostile file can't exhaust the stack.
fn bencode_end(bytes: &[u8], start: usize, depth: u32) -> Option<usize> {
    if depth > 64 {
        return None;
    }
    match *bytes.get(start)? {
        b'd' | b'l' => {
            let mut i = start + 1;
            while *bytes.get(i)? != b'e' {
                i = bencode_end(bytes, i, depth + 1)?;
            }
            Some(i + 1)
        }
        b'i' => Some(start + bytes.get(start..)?.iter().position(|&b| b == b'e')? + 1),
        b'0'..=b'9' => bencode_string(bytes, start).map(|(_, end)| end),
        _ => None,
    }
}

/// Fetch a `.torrent` and read its info-hash. `None` on any failure;
/// the caller goes on without a hash, as before.
pub async fn fetch_torrent_info_hash(url: &str) -> Option<String> {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return None;
    }
    let resp = HTTP_CLIENT.get(url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let body = super::http_body::read_capped(resp, TORRENT_BODY_CAP)
        .await
        .ok()?;
    torrent_info_hash(&body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn magnet_hashes_read_in_hex_and_base32() {
        let hex = "c12fe1c06bba254a9dc9f519b335aa7c1367a88a";
        assert_eq!(
            magnet_info_hash(&format!(
                "magnet:?dn=x&xt=urn:btih:{}&tr=y",
                hex.to_uppercase()
            )),
            Some(hex.to_string())
        );
        // The same hash in base32.
        assert_eq!(
            magnet_info_hash("magnet:?xt=urn:btih:YEX6DQDLXISUVHOJ6UM3GNNKPQJWPKEK"),
            Some(hex.to_string())
        );
        assert_eq!(magnet_info_hash("magnet:?xt=urn:btih:abc"), None);
        assert_eq!(magnet_info_hash("https://x/y.torrent"), None);
    }

    #[test]
    fn the_info_hash_is_the_top_level_info_dict_only() {
        let info = b"d6:lengthi5e4:name5:a.mkv12:piece lengthi16384e6:pieces0:e";
        // An `info` key inside another value (here, the announce URL's
        // text and a nested dict) must not be taken for the real one.
        let mut file = b"d8:announce15:http://x/4:info6:nested".to_vec();
        file.extend_from_slice(b"d4:info");
        file.extend_from_slice(b"d1:ai1eee");
        file.extend_from_slice(b"4:info");
        file.extend_from_slice(info);
        file.push(b'e');
        let mut hasher = sha1_smol::Sha1::new();
        hasher.update(info);
        assert_eq!(torrent_info_hash(&file), Some(hasher.digest().to_string()));

        assert_eq!(torrent_info_hash(b"<?xml version=\"1.0\"?><nzb/>"), None);
        assert_eq!(
            torrent_info_hash(b"d4:infoi1ee"),
            None,
            "info must be a dict"
        );
        assert_eq!(torrent_info_hash(b"d4:info"), None, "truncated");
        let deep = format!("d4:info{}{}e", "l".repeat(100), "e".repeat(100));
        assert_eq!(
            torrent_info_hash(deep.as_bytes()),
            None,
            "nesting is bounded"
        );
        assert_eq!(torrent_info_hash(b"d99999999999999999999:x"), None);
    }
}
