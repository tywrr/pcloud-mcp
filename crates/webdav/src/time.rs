//! `getlastmodified` arrives as an HTTP date; the storage contract wants RFC 3339.

/// `Fri, 30 Oct 2026 00:32:52 GMT` becomes `2026-10-30T00:32:52Z`. Accepts the three HTTP
/// date formats (IMF-fixdate, RFC 850, asctime); `None` for anything else.
pub(crate) fn http_date_to_rfc3339(value: &str) -> Option<String> {
    // Parsing to SystemTime and printing it back gives IMF-fixdate whatever the input format,
    // so the fields below sit at fixed offsets. Neither step reads the clock, which matters
    // because SystemTime::now() panics on wasm32-unknown-unknown.
    let time = httpdate::parse_http_date(value.trim()).ok()?;
    let fixed = httpdate::fmt_http_date(time);
    // "Fri, 30 Oct 2026 00:32:52 GMT"
    //  0123456789012345678901234567
    let day = fixed.get(5..7)?;
    let month = match fixed.get(8..11)? {
        "Jan" => "01",
        "Feb" => "02",
        "Mar" => "03",
        "Apr" => "04",
        "May" => "05",
        "Jun" => "06",
        "Jul" => "07",
        "Aug" => "08",
        "Sep" => "09",
        "Oct" => "10",
        "Nov" => "11",
        "Dec" => "12",
        _ => return None,
    };
    let year = fixed.get(12..16)?;
    let clock = fixed.get(17..25)?;
    Some(format!("{year}-{month}-{day}T{clock}Z"))
}

#[cfg(test)]
mod tests {
    use super::http_date_to_rfc3339 as convert;

    #[test]
    fn converts_all_http_date_formats() {
        assert_eq!(
            convert("Fri, 30 Oct 2026 00:32:52 GMT").as_deref(),
            Some("2026-10-30T00:32:52Z")
        );
        assert_eq!(
            convert(" Sun, 06 Nov 1994 08:49:37 GMT ").as_deref(),
            Some("1994-11-06T08:49:37Z")
        );
        assert_eq!(
            convert("Sunday, 06-Nov-94 08:49:37 GMT").as_deref(),
            Some("1994-11-06T08:49:37Z")
        );
        assert_eq!(
            convert("Sun Nov  6 08:49:37 1994").as_deref(),
            Some("1994-11-06T08:49:37Z")
        );
        assert_eq!(
            convert("Thu, 01 Jan 1970 00:00:00 GMT").as_deref(),
            Some("1970-01-01T00:00:00Z")
        );
    }

    #[test]
    fn rejects_other_formats() {
        assert_eq!(convert(""), None);
        assert_eq!(convert("2026-10-30T00:32:52Z"), None);
        assert_eq!(convert("1793320372"), None);
        assert_eq!(convert("Fri, 30 Foo 2026 00:32:52 GMT"), None);
    }
}
