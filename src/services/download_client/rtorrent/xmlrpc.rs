//! XML-RPC wire codec for the rtorrent client. Self-contained ~360
//! LoC: encoder for the small subset rtorrent's API needs (string,
//! i4/i8, boolean, array, struct), a decoder that handles the same
//! shapes plus fault-message extraction, and a tiny pull parser.
//!
//! Split from `mod.rs` during the v1.5 refactor so the trait impl
//! and the codec stay independently navigable. The codec is driven
//! exclusively by `RtorrentClient::send` over the parent module's
//! HTTP layer; nothing else in the codebase touches XML-RPC.

// ---------------------------------------------------------------------------
// Minimal XML-RPC wire format. Ryokan uses string, i4/i8, boolean,
// array, and struct (decode only, for fault handling). Rolling this
// by hand avoids pulling in dxr/xmlrpc + their four transitive
// proc-macro crates for a narrow one-file use case.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub(super) enum XmlValue {
    String(String),
    Int(i64),
    Bool(bool),
    Array(Vec<XmlValue>),
    /// Raw binary payload, encoded as XML-RPC `<base64>` on the
    /// wire. Needed for `load.raw_start_verbose` / `load.raw_verbose`
    /// which accept an entire `.torrent` file as a base64 blob —
    /// these are the canonical ways to hand a tiny synthetic
    /// `.torrent` to rtorrent without serving it over HTTP or
    /// sharing a filesystem mount with the rtorrent container.
    /// Test-only for now (used by `live_smoke_narrowed` and friends);
    /// gated behind `#[cfg(test)]` both here and in the encoder
    /// match arm so the production binary doesn't ship a speculative
    /// feature. Promote to unconditional if a UI feature later needs
    /// to load `.torrent` bytes directly.
    #[cfg(test)]
    Base64(Vec<u8>),
}

impl XmlValue {
    pub(super) fn as_string(&self) -> Option<&str> {
        match self {
            XmlValue::String(s) => Some(s),
            _ => None,
        }
    }
    pub(super) fn as_int(&self) -> Option<i64> {
        match self {
            XmlValue::Int(i) => Some(*i),
            _ => None,
        }
    }
    pub(super) fn into_array(self) -> Option<Vec<XmlValue>> {
        match self {
            XmlValue::Array(a) => Some(a),
            _ => None,
        }
    }
}

pub(super) fn encode_request(method: &str, params: &[XmlValue]) -> String {
    let mut s = String::with_capacity(256);
    s.push_str("<?xml version=\"1.0\"?><methodCall><methodName>");
    s.push_str(&xml_text_escape(method));
    s.push_str("</methodName><params>");
    for p in params {
        s.push_str("<param>");
        encode_value(p, &mut s);
        s.push_str("</param>");
    }
    s.push_str("</params></methodCall>");
    s
}

pub(super) fn encode_value(v: &XmlValue, out: &mut String) {
    out.push_str("<value>");
    match v {
        XmlValue::String(s) => {
            out.push_str("<string>");
            out.push_str(&xml_text_escape(s));
            out.push_str("</string>");
        }
        XmlValue::Int(i) => {
            out.push_str("<i8>");
            out.push_str(&i.to_string());
            out.push_str("</i8>");
        }
        XmlValue::Bool(b) => {
            out.push_str("<boolean>");
            out.push_str(if *b { "1" } else { "0" });
            out.push_str("</boolean>");
        }
        XmlValue::Array(a) => {
            out.push_str("<array><data>");
            for inner in a {
                encode_value(inner, out);
            }
            out.push_str("</data></array>");
        }
        #[cfg(test)]
        XmlValue::Base64(bytes) => {
            use base64::{Engine, engine::general_purpose};
            out.push_str("<base64>");
            out.push_str(&general_purpose::STANDARD.encode(bytes));
            out.push_str("</base64>");
        }
    }
    out.push_str("</value>");
}

pub(super) fn xml_text_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
    }
    out
}

/// Backslash + quote escape for values embedded inside an rtorrent
/// inline command string like `d.custom1.set="value"`. We aren't
/// encoding an XML attribute — we're embedding inside a param string
/// that rtorrent's own parser then re-parses. The quoting convention
/// rtorrent's cmd parser accepts is double-quoted + backslash-escape.
pub(super) fn xml_attr_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            _ => out.push(c),
        }
    }
    out
}

/// Parse an XML-RPC response body. Returns the single `<param>` value
/// on success, or a decoded fault string on `<fault>`.
pub(super) fn decode_response(xml: &str) -> Result<XmlValue, String> {
    let mut p = Parser::new(xml);
    p.expect_open("methodResponse")?;
    let tag = p.peek_open().ok_or("malformed XML-RPC response")?;
    match tag {
        "params" => {
            p.expect_open("params")?;
            p.expect_open("param")?;
            let v = decode_value(&mut p)?;
            p.expect_close("param")?;
            // Only single-param responses from rtorrent; skip any
            // trailing params defensively.
            while p.peek_open() == Some("param") {
                p.expect_open("param")?;
                let _ = decode_value(&mut p)?;
                p.expect_close("param")?;
            }
            p.expect_close("params")?;
            p.expect_close("methodResponse")?;
            Ok(v)
        }
        "fault" => {
            p.expect_open("fault")?;
            let v = decode_value(&mut p)?;
            p.expect_close("fault")?;
            p.expect_close("methodResponse")?;
            // Fault values are structs of {faultCode, faultString}.
            // We decode the string out of the raw XML, since the
            // struct decoder isn't needed elsewhere — cheap and
            // sufficient.
            let msg = fault_message(xml).unwrap_or_else(|| "(no fault message)".to_string());
            let _ = v;
            Err(format!("rtorrent fault: {msg}"))
        }
        other => Err(format!("unexpected XML-RPC response tag: {other}")),
    }
}

pub(super) fn fault_message(xml: &str) -> Option<String> {
    // Find `<name>faultString</name>` then the next <string>...</string>.
    let needle = "<name>faultString</name>";
    let idx = xml.find(needle)?;
    let rest = &xml[idx + needle.len()..];
    let s_start = rest.find("<string>")?;
    let from = &rest[s_start + "<string>".len()..];
    let s_end = from.find("</string>")?;
    Some(xml_text_unescape(&from[..s_end]))
}

pub(super) fn xml_text_unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// Maximum nesting depth `decode_value` will recurse into before
/// erroring out. rtorrent's real responses nest at most 2-3 levels
/// (`d.multicall2` returns an array of arrays of primitives); 256
/// is hundreds of times more than any legitimate response shape and
/// well below the default cargo-test thread stack budget. Without
/// this gate, a malicious proxy or buggy plugin sitting between
/// Ryokan and rtorrent could send a deeply nested `<array>` tower
/// and panic-abort the post-processing task on stack overflow.
pub(super) const MAX_NESTING_DEPTH: usize = 256;

pub(super) fn decode_value(p: &mut Parser) -> Result<XmlValue, String> {
    decode_value_with_depth(p, 0)
}

fn decode_value_with_depth(p: &mut Parser, depth: usize) -> Result<XmlValue, String> {
    if depth > MAX_NESTING_DEPTH {
        return Err(format!(
            "XML-RPC value nesting exceeds limit of {MAX_NESTING_DEPTH}"
        ));
    }
    p.expect_open("value")?;
    // Implicit string: `<value>bare text</value>` is legal per XML-RPC
    // spec. rtorrent sometimes emits this for empty strings.
    let inner_tag = p.peek_open();
    let v = match inner_tag {
        Some("string") => {
            if p.open_tag_or_empty("string")? {
                XmlValue::String(String::new())
            } else {
                let s = p.read_text_until("</string>")?;
                XmlValue::String(xml_text_unescape(s))
            }
        }
        Some("i4") | Some("int") => {
            let tag = inner_tag.unwrap();
            p.consume_open_tag(tag)?;
            let s = p.read_text_until(&format!("</{tag}>"))?;
            let i = s
                .trim()
                .parse::<i64>()
                .map_err(|e| format!("XML-RPC int parse: {e}"))?;
            XmlValue::Int(i)
        }
        Some("i8") => {
            p.expect_open("i8")?;
            let s = p.read_text_until("</i8>")?;
            let i = s
                .trim()
                .parse::<i64>()
                .map_err(|e| format!("XML-RPC i8 parse: {e}"))?;
            XmlValue::Int(i)
        }
        Some("boolean") => {
            p.expect_open("boolean")?;
            let s = p.read_text_until("</boolean>")?;
            XmlValue::Bool(s.trim() != "0")
        }
        Some("array") => {
            p.expect_open("array")?;
            let mut items = Vec::new();
            // rtorrent 0.16 answers an empty list (`d.multicall2` with no
            // torrents, a fresh install) with `<data/>`; requiring
            // `<data>` made the Downloads page call the client
            // unreachable until it held a torrent.
            if !p.open_tag_or_empty("data")? {
                while p.peek_open() == Some("value") {
                    items.push(decode_value_with_depth(p, depth + 1)?);
                }
                p.expect_close("data")?;
            }
            p.expect_close("array")?;
            XmlValue::Array(items)
        }
        Some("struct") => {
            // We don't use struct values anywhere except fault, and
            // fault decoding goes through fault_message() not here.
            // Skip through to the closing </struct>.
            p.expect_open("struct")?;
            p.skip_to_close("struct")?;
            XmlValue::String(String::new())
        }
        _ => {
            // Implicit-string case: raw text until </value>.
            // `read_text_until` consumes the marker, so the closing
            // `</value>` has already been swallowed — no additional
            // expect_close call below.
            let s = p.read_text_until("</value>")?;
            return Ok(XmlValue::String(xml_text_unescape(s.trim())));
        }
    };
    p.expect_close("value")?;
    Ok(v)
}

/// Cursor-based pull parser. Just enough shape-awareness to walk
/// well-formed XML-RPC responses. Not a general XML parser — assumes
/// ASCII tag names, no CDATA sections, no namespaces, no processing
/// instructions beyond the `<?xml?>` prolog. All of those hold for
/// rtorrent 0.9.x's XML-RPC responses.
pub(super) struct Parser<'a> {
    buf: &'a str,
    pos: usize,
}

impl<'a> Parser<'a> {
    fn new(s: &'a str) -> Self {
        // Skip `<?xml?>` prolog if present.
        let mut pos = 0;
        let trimmed = s.trim_start();
        pos += s.len() - trimmed.len();
        if trimmed.starts_with("<?xml")
            && let Some(end) = trimmed.find("?>")
        {
            pos += end + 2;
        }
        Self { buf: s, pos }
    }

    fn skip_ws(&mut self) {
        while self.pos < self.buf.len() {
            let c = self.buf.as_bytes()[self.pos];
            if matches!(c, b' ' | b'\t' | b'\n' | b'\r') {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    fn peek_open(&mut self) -> Option<&'a str> {
        self.skip_ws();
        let rest = self.buf.get(self.pos..)?;
        if !rest.starts_with('<') || rest.starts_with("</") {
            return None;
        }
        let end = rest.find('>')?;
        let tag_inner = &rest[1..end];
        // Attributes not expected; take the tag name up to the first
        // whitespace or the `/` of a self-closing `<string/>`.
        let name_end = tag_inner
            .find(|c: char| c.is_whitespace() || c == '/')
            .unwrap_or(tag_inner.len());
        Some(&tag_inner[..name_end])
    }

    fn consume_open_tag(&mut self, tag: &str) -> Result<(), String> {
        self.skip_ws();
        let expected = format!("<{tag}>");
        if self.buf[self.pos..].starts_with(&expected) {
            self.pos += expected.len();
            Ok(())
        } else {
            Err(format!(
                "XML parse: expected <{tag}> at position {}",
                self.pos
            ))
        }
    }

    fn expect_open(&mut self, tag: &str) -> Result<(), String> {
        self.consume_open_tag(tag)
    }

    /// `<tag>`, or the empty element `<tag/>` / `<tag />`. True for
    /// the self-closing form, which has no closing tag to expect.
    fn open_tag_or_empty(&mut self, tag: &str) -> Result<bool, String> {
        self.skip_ws();
        let rest = &self.buf[self.pos..];
        for (form, empty) in [
            (format!("<{tag}>"), false),
            (format!("<{tag}/>"), true),
            (format!("<{tag} />"), true),
        ] {
            if rest.starts_with(&form) {
                self.pos += form.len();
                return Ok(empty);
            }
        }
        Err(format!(
            "XML parse: expected <{tag}> at position {}",
            self.pos
        ))
    }

    fn expect_close(&mut self, tag: &str) -> Result<(), String> {
        self.skip_ws();
        let expected = format!("</{tag}>");
        if self.buf[self.pos..].starts_with(&expected) {
            self.pos += expected.len();
            Ok(())
        } else {
            Err(format!(
                "XML parse: expected </{tag}> at position {}",
                self.pos
            ))
        }
    }

    fn read_text_until(&mut self, end_marker: &str) -> Result<&'a str, String> {
        let rest = &self.buf[self.pos..];
        let idx = rest
            .find(end_marker)
            .ok_or_else(|| format!("XML parse: no {end_marker} found"))?;
        let text = &rest[..idx];
        self.pos += idx + end_marker.len();
        Ok(text)
    }

    fn skip_to_close(&mut self, tag: &str) -> Result<(), String> {
        let close = format!("</{tag}>");
        let rest = &self.buf[self.pos..];
        let idx = rest
            .find(&close)
            .ok_or_else(|| format!("XML parse: no {close} found"))?;
        self.pos += idx + close.len();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    //! XML-RPC codec coverage. The decoder runs against rtorrent's
    //! real responses (with the wiremock harness in
    //! `wiremock_tests/`), but those are fixture-based and don't pin
    //! the codec's individual parse branches. These tests do.
    //!
    //! The malformed-input battery at the bottom is fuzz-lite: a
    //! curated corpus of inputs the parser must reject as `Err`
    //! rather than panic. A future cargo-fuzz target for
    //! `decode_response` would seed from this corpus.
    use super::*;
    use rstest::rstest;

    // ── Empty elements ────────────────────────────────────────────────

    #[test]
    fn an_empty_list_is_the_self_closing_data_element() {
        // rtorrent 0.16's answer to d.multicall2 with no torrents,
        // captured from a live instance.
        let xml = r#"<?xml version="1.0"?><methodResponse><params><param><value><array><data/></array></value></param></params></methodResponse>"#;
        assert_eq!(
            decode_response(xml).unwrap().into_array().map(|a| a.len()),
            Some(0)
        );
        let spaced = r#"<methodResponse><params><param><value><array><data /></array></value></param></params></methodResponse>"#;
        assert_eq!(
            decode_response(spaced)
                .unwrap()
                .into_array()
                .map(|a| a.len()),
            Some(0)
        );
    }

    #[test]
    fn an_empty_string_may_be_self_closing() {
        let xml = r#"<methodResponse><params><param><value><array><data><value><string/></value><value><string>x</string></value></data></array></value></param></params></methodResponse>"#;
        let items = decode_response(xml).unwrap().into_array().unwrap();
        assert_eq!(items[0].as_string(), Some(""));
        assert_eq!(items[1].as_string(), Some("x"));
    }
    // ── XML escape / unescape ─────────────────────────────────────────
    //
    // The escape and unescape paths must round-trip every legal XML
    // entity in both text and attribute contexts. A regression here
    // silently corrupts torrent paths containing `&` (every magnet
    // URI carries one in `dn=…&tr=…`).

    #[test]
    fn xml_text_escape_handles_three_xml_text_entities() {
        // `xml_text_escape` only escapes the three characters that
        // are unsafe in an XML text node (`&`, `<`, `>`); single and
        // double quotes are LEGAL in text nodes and pass through.
        // The unescape path still strips `&apos;`/`&quot;` for
        // round-trip safety with sources that emit them.
        assert_eq!(xml_text_escape("a&b<c>d"), "a&amp;b&lt;c&gt;d");
        // Quotes pass through.
        assert_eq!(xml_text_escape(r#"'"#), "'");
        assert_eq!(xml_text_escape(r#"""#), "\"");
    }

    #[test]
    fn xml_text_escape_leaves_safe_chars_alone() {
        // ASCII printables, whitespace, and a high-codepoint character
        // — none of these need escaping for an XML text node.
        let s = "Show: 12 — résumé 漢字";
        assert_eq!(xml_text_escape(s), s);
    }

    #[test]
    fn xml_attr_escape_is_backslash_quote_escape() {
        // Despite the name, this isn't an XML attribute escape —
        // rtorrent's cmd parser re-parses the value out of a
        // double-quoted command-string param, so we backslash-escape
        // `"` and `\` per rtorrent's own convention. Pin the actual
        // contract so a future "let's make this an XML escape"
        // refactor has to update the test (and the rtorrent-side
        // command parser, which would then break).
        assert_eq!(xml_attr_escape(r#"a"b\c"#), r#"a\"b\\c"#);
        // Other characters pass through, including `<` / `>` / `&`
        // (the param string isn't an XML attr — those are fine here).
        assert_eq!(xml_attr_escape("a<b>c&d"), "a<b>c&d");
    }

    #[test]
    fn unescape_inverts_escape_on_text() {
        // Round-trip the three xml_text_escape entities. The unescape
        // also handles `&apos;` and `&quot;` (incoming wire form), but
        // the escape path doesn't emit them — so the round-trip target
        // is whatever survives `text_escape → text_unescape`.
        let original = "a&b<c>d";
        let escaped = xml_text_escape(original);
        assert_eq!(xml_text_unescape(&escaped), original);
    }

    // ── Decode primitives ─────────────────────────────────────────────

    fn wrap_methodresponse(value_xml: &str) -> String {
        format!(
            "<?xml version=\"1.0\"?><methodResponse><params><param>{value_xml}</param></params></methodResponse>"
        )
    }

    #[test]
    fn decode_string_value() {
        let xml = wrap_methodresponse("<value><string>hello</string></value>");
        match decode_response(&xml).unwrap() {
            XmlValue::String(s) => assert_eq!(s, "hello"),
            other => panic!("expected String, got {other:?}"),
        }
    }

    #[test]
    fn decode_string_unescapes_entities() {
        // The on-the-wire form of `Show & Tell <2024>` carries entities;
        // the decoded value must be the original.
        let xml =
            wrap_methodresponse("<value><string>Show &amp; Tell &lt;2024&gt;</string></value>");
        match decode_response(&xml).unwrap() {
            XmlValue::String(s) => assert_eq!(s, "Show & Tell <2024>"),
            other => panic!("expected String, got {other:?}"),
        }
    }

    #[test]
    fn decode_implicit_string_without_explicit_type_tag() {
        // `<value>bare text</value>` is legal per the XML-RPC spec and
        // rtorrent emits it for empty / short strings. The decoder
        // promotes the raw text into an `XmlValue::String`.
        let xml = wrap_methodresponse("<value>bare</value>");
        match decode_response(&xml).unwrap() {
            XmlValue::String(s) => assert_eq!(s, "bare"),
            other => panic!("expected String, got {other:?}"),
        }
    }

    #[rstest]
    #[case("<i4>42</i4>", 42)]
    #[case("<int>42</int>", 42)]
    #[case("<i8>9223372036854775807</i8>", i64::MAX)]
    #[case("<i4>-1</i4>", -1)]
    #[case("<i4>  17  </i4>", 17)] // trim whitespace per spec
    fn decode_integer_variants(#[case] inner: &str, #[case] expected: i64) {
        // i4 / int / i8 should all decode to XmlValue::Int. rtorrent's
        // responses mix all three (i8 for sizes/rates, i4 for state
        // codes) so dropping any of these would break list_scoped.
        let xml = wrap_methodresponse(&format!("<value>{inner}</value>"));
        match decode_response(&xml).unwrap() {
            XmlValue::Int(i) => assert_eq!(i, expected),
            other => panic!("expected Int, got {other:?}"),
        }
    }

    #[rstest]
    #[case("0", false)]
    #[case("1", true)]
    fn decode_boolean_canonical(#[case] inner: &str, #[case] expected: bool) {
        let xml = wrap_methodresponse(&format!("<value><boolean>{inner}</boolean></value>"));
        match decode_response(&xml).unwrap() {
            XmlValue::Bool(b) => assert_eq!(b, expected),
            other => panic!("expected Bool, got {other:?}"),
        }
    }

    #[test]
    fn decode_boolean_treats_anything_nonzero_as_true() {
        // Defensive parser: any non-`0` character flips the bool. This
        // is more lenient than the spec but matches rtorrent's actual
        // emissions which sometimes leak literal `true`/`false`.
        let xml = wrap_methodresponse("<value><boolean>true</boolean></value>");
        match decode_response(&xml).unwrap() {
            XmlValue::Bool(true) => {}
            other => panic!("expected Bool(true), got {other:?}"),
        }
    }

    #[test]
    fn decode_empty_array() {
        let xml = wrap_methodresponse("<value><array><data></data></array></value>");
        match decode_response(&xml).unwrap() {
            XmlValue::Array(items) => assert!(items.is_empty()),
            other => panic!("expected empty Array, got {other:?}"),
        }
    }

    #[test]
    fn decode_array_of_mixed_values() {
        // rtorrent's d.multicall2 returns arrays-of-arrays-of-mixed.
        // Mixed primitives is the cheapest pinnable shape.
        let xml = wrap_methodresponse(
            "<value><array><data>\
             <value><string>abc</string></value>\
             <value><i8>42</i8></value>\
             <value><boolean>1</boolean></value>\
             </data></array></value>",
        );
        let v = decode_response(&xml).unwrap();
        let items = v.into_array().expect("Array");
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].as_string(), Some("abc"));
        assert_eq!(items[1].as_int(), Some(42));
        // Bool's accessor isn't exposed; pattern-match.
        match &items[2] {
            XmlValue::Bool(true) => {}
            other => panic!("expected Bool(true), got {other:?}"),
        }
    }

    #[test]
    fn decode_nested_arrays() {
        // d.multicall2 nests one level deep. Keep the test shallow on
        // purpose — the recursive parser handles unbounded depth via
        // the regular Rust call stack, so deep nesting is a separate
        // concern (covered by the malformed-input battery below).
        let xml = wrap_methodresponse(
            "<value><array><data>\
             <value><array><data>\
             <value><i4>1</i4></value>\
             <value><i4>2</i4></value>\
             </data></array></value>\
             </data></array></value>",
        );
        let outer = decode_response(&xml).unwrap().into_array().unwrap();
        assert_eq!(outer.len(), 1);
        let inner = outer.into_iter().next().unwrap().into_array().unwrap();
        assert_eq!(inner.len(), 2);
        assert_eq!(inner[0].as_int(), Some(1));
        assert_eq!(inner[1].as_int(), Some(2));
    }

    #[test]
    fn decode_struct_collapses_to_empty_string() {
        // The decoder doesn't care about struct shapes anywhere except
        // inside fault responses, which take the fault_message
        // shortcut. Plain struct values flatten to empty string —
        // documented contract; pin it so a future "let's actually
        // parse structs" change has to update this test.
        let xml = wrap_methodresponse(
            "<value><struct>\
             <member><name>a</name><value><string>x</string></value></member>\
             </struct></value>",
        );
        match decode_response(&xml).unwrap() {
            XmlValue::String(s) => assert_eq!(s, ""),
            other => panic!("expected empty String for struct, got {other:?}"),
        }
    }

    // ── Fault handling ────────────────────────────────────────────────

    #[test]
    fn decode_fault_returns_err_with_message() {
        // rtorrent's fault responses are the canonical path for "this
        // method doesn't exist" / "session not connected" / etc. The
        // caller matches on the Err string to discriminate — a regression
        // that flipped Err → Ok would be silent until something downstream
        // tried to use the bogus value.
        let xml = r#"<?xml version="1.0"?><methodResponse><fault><value><struct>
            <member><name>faultCode</name><value><i4>-503</i4></value></member>
            <member><name>faultString</name><value><string>method not found</string></value></member>
        </struct></value></fault></methodResponse>"#;
        let err = decode_response(xml).unwrap_err();
        assert!(err.contains("rtorrent fault"), "got {err}");
        assert!(err.contains("method not found"), "got {err}");
    }

    #[test]
    fn fault_message_unescapes_entities_in_message() {
        // Real rtorrent fault strings sometimes carry the offending
        // method name with embedded `<` / `>` (debug rendering of
        // un-resolved typed targets). The unescape must run.
        let xml = r#"<?xml version="1.0"?><methodResponse><fault><value><struct>
            <member><name>faultCode</name><value><i4>-503</i4></value></member>
            <member><name>faultString</name><value><string>bad &lt;target&gt;</string></value></member>
        </struct></value></fault></methodResponse>"#;
        assert_eq!(fault_message(xml), Some("bad <target>".to_string()));
    }

    #[test]
    fn fault_message_returns_none_when_absent() {
        // Defensive — if the fault struct doesn't carry a faultString
        // member at all, the function returns None and the caller
        // falls back to "(no fault message)".
        let xml = "<?xml version=\"1.0\"?><methodResponse><params><param><value><i4>0</i4></value></param></params></methodResponse>";
        assert!(fault_message(xml).is_none());
    }

    // ── Encode round-trips ────────────────────────────────────────────
    //
    // The encoder is one-way from Ryokan's side, but the round-trip
    // through (encode → wrap as response → decode) catches "encoder
    // emits something the decoder rejects" regressions in one shot.

    #[test]
    fn encode_request_shape_is_well_formed() {
        let req = encode_request("d.multicall2", &[XmlValue::String("main".into())]);
        assert!(req.starts_with("<?xml version=\"1.0\"?><methodCall>"));
        assert!(req.contains("<methodName>d.multicall2</methodName>"));
        assert!(req.contains("<value><string>main</string></value>"));
        assert!(req.ends_with("</methodCall>"));
    }

    #[test]
    fn encode_request_escapes_unsafe_chars_in_method_name() {
        // Method name shouldn't ever contain unsafe chars (rtorrent's
        // are dotted ASCII), but the escape is the right defensive
        // shape — pin it.
        let req = encode_request("a&b", &[]);
        assert!(req.contains("<methodName>a&amp;b</methodName>"));
    }

    #[test]
    fn round_trip_string_via_methodresponse() {
        // Encoder emits an inner <value>; wrap as a methodResponse
        // and decode it back. End-to-end: a torrent name with every
        // entity must survive the round-trip exactly.
        let original = "Show & Tell <Vol. \"1\">";
        let mut inner = String::new();
        encode_value(&XmlValue::String(original.into()), &mut inner);
        let xml = format!(
            "<?xml version=\"1.0\"?><methodResponse><params><param>{inner}</param></params></methodResponse>"
        );
        match decode_response(&xml).unwrap() {
            XmlValue::String(s) => assert_eq!(s, original),
            other => panic!("expected String, got {other:?}"),
        }
    }

    #[test]
    fn round_trip_int_array_via_methodresponse() {
        let mut inner = String::new();
        encode_value(
            &XmlValue::Array(vec![XmlValue::Int(1), XmlValue::Int(2), XmlValue::Int(-3)]),
            &mut inner,
        );
        let xml = format!(
            "<?xml version=\"1.0\"?><methodResponse><params><param>{inner}</param></params></methodResponse>"
        );
        let items = decode_response(&xml).unwrap().into_array().unwrap();
        assert_eq!(items.len(), 3);
        assert_eq!(items[0].as_int(), Some(1));
        assert_eq!(items[1].as_int(), Some(2));
        assert_eq!(items[2].as_int(), Some(-3));
    }

    // ── Malformed input battery (fuzz-lite) ───────────────────────────
    //
    // Every entry in this list is a string the decoder must reject as
    // `Err`, never panic. A future `cargo-fuzz` target for
    // `decode_response` would seed its corpus from these — they
    // cover the known-rough edges (truncations, wrong-tag swaps,
    // empty inputs) without random mutation. Add new lines here when
    // a fuzzing run finds a panic; promoting the panicking input to a
    // pinning test is the cheapest way to lock in the fix.

    #[rstest]
    #[case("")] // empty
    #[case("<?xml version=\"1.0\"?>")] // only prolog
    #[case("<methodResponse>")] // truncated open
    #[case("<methodResponse></methodResponse>")] // empty body
    #[case("<methodResponse><params></params></methodResponse>")] // no <param>
    #[case("<methodResponse><params><param></param></params></methodResponse>")] // empty <param>
    #[case("<methodResponse><params><param><value>")] // truncated mid-value
    #[case(
        "<methodResponse><params><param><value><i4>not-a-number</i4></value></param></params></methodResponse>"
    )]
    #[case(
        "<methodResponse><params><param><value><i8>9999999999999999999999</i8></value></param></params></methodResponse>"
    )] // i64 overflow
    #[case(
        "<methodResponse><params><param><value><array></array></value></param></params></methodResponse>"
    )] // array missing <data>
    #[case(
        "<methodResponse><params><param><value><array><data><value><i4>1</i4></value></data></value></param></params></methodResponse>"
    )] // unclosed array
    #[case("<methodResponse><randomtag/></methodResponse>")] // unexpected top-level
    fn malformed_input_returns_err_not_panic(#[case] xml: &str) {
        // We don't care about the specific error message — the
        // contract is "no panic, returns Err." A regression that
        // collapses any of these into Ok / panic is what this test
        // catches.
        let result = decode_response(xml);
        assert!(result.is_err(), "expected Err for {xml:?}, got {result:?}");
    }

    #[test]
    fn deeply_nested_array_returns_err_without_stack_overflow() {
        // Pin the depth-limit defense. Without `MAX_NESTING_DEPTH`,
        // recursive `decode_value` calls grow the stack ~one frame
        // per `<array>` level; the default cargo-test thread stack
        // (2 MiB on Linux) overflows around the 4-5k mark and
        // panic-aborts the test process. The depth limit makes the
        // parser refuse pathological inputs gracefully — feeds well
        // above the cap to confirm we hit `Err`, not abort.
        let mut xml = String::from("<?xml version=\"1.0\"?><methodResponse><params><param>");
        let depth = MAX_NESTING_DEPTH * 4;
        for _ in 0..depth {
            xml.push_str("<value><array><data>");
        }
        xml.push_str("<value><i4>1</i4></value>");
        for _ in 0..depth {
            xml.push_str("</data></array></value>");
        }
        xml.push_str("</param></params></methodResponse>");

        let result = decode_response(&xml);
        assert!(result.is_err(), "expected depth-limit Err, got {result:?}");
        let err = result.unwrap_err();
        assert!(
            err.contains("nesting exceeds"),
            "expected depth-limit message, got {err}"
        );
    }
}
