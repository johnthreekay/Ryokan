//! Response bodies read with a size cap, for fetches whose URL comes
//! from a third party (metadata providers, feeds, indexers): a server
//! there must not be able to make Ryokan buffer an unbounded body.

/// Read `resp`'s body, refusing more than `cap` bytes. A declared
/// `Content-Length` over the cap is refused before anything is read.
pub(crate) async fn read_capped(
    mut resp: reqwest::Response,
    cap: usize,
) -> Result<Vec<u8>, String> {
    let too_large = || format!("response body is over the {} KB cap", cap >> 10);
    if resp.content_length().is_some_and(|len| len > cap as u64) {
        return Err(too_large());
    }
    let mut body = Vec::new();
    while let Some(chunk) = resp
        .chunk()
        .await
        // Without the URL: an indexer's carries its API key.
        .map_err(|e| format!("response body read failed: {}", e.without_url()))?
    {
        if body.len() + chunk.len() > cap {
            return Err(too_large());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Bodies from metadata providers and token endpoints (AniList, Jikan,
/// Kitsu, MAL, SeaDex, OAuth). Real ones are well under 1 MB; the cap
/// only stops a hostile or broken server from making Ryokan buffer an
/// unbounded body inside the request timeout.
pub(crate) const PROVIDER_BODY_CAP: usize = 16 << 20;

/// `.text()` / `.json()` with [`PROVIDER_BODY_CAP`], for third-party
/// responses. Same call shape, so a request chain only swaps the name.
pub(crate) trait CappedBody {
    async fn text_capped(self) -> Result<String, String>;
    async fn json_capped<T: serde::de::DeserializeOwned>(self) -> Result<T, String>;
}

impl CappedBody for reqwest::Response {
    async fn text_capped(self) -> Result<String, String> {
        let body = read_capped(self, PROVIDER_BODY_CAP).await?;
        Ok(String::from_utf8_lossy(&body).into_owned())
    }

    async fn json_capped<T: serde::de::DeserializeOwned>(self) -> Result<T, String> {
        let body = read_capped(self, PROVIDER_BODY_CAP).await?;
        serde_json::from_slice(&body)
            .map_err(|e| format!("response body is not the expected JSON: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn serve(body: Vec<u8>) -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(body))
            .mount(&server)
            .await;
        server
    }

    #[tokio::test]
    async fn a_body_within_the_cap_is_returned_whole() {
        let server = serve(vec![b'x'; 4096]).await;
        let resp = reqwest::get(server.uri()).await.unwrap();
        assert_eq!(read_capped(resp, 4096).await.unwrap().len(), 4096);
    }

    #[tokio::test]
    async fn a_body_over_the_cap_is_refused() {
        let server = serve(vec![b'x'; 4097]).await;
        let resp = reqwest::get(server.uri()).await.unwrap();
        let err = read_capped(resp, 4096).await.unwrap_err();
        assert!(err.contains("over the 4 KB cap"), "{err}");
    }

    #[tokio::test]
    async fn capped_json_and_text_refuse_an_oversized_provider_body() {
        let server = serve(vec![b' '; PROVIDER_BODY_CAP + 1]).await;
        let err = reqwest::get(server.uri())
            .await
            .unwrap()
            .text_capped()
            .await
            .unwrap_err();
        assert!(err.contains("cap"), "{err}");
        let server = serve(br#"{"data":[1,2,3]}"#.to_vec()).await;
        let value: serde_json::Value = reqwest::get(server.uri())
            .await
            .unwrap()
            .json_capped()
            .await
            .unwrap();
        assert_eq!(value["data"][2], 3);
    }
}
