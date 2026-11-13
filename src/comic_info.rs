//! Minimal ComicInfo.xml generation.

/// Build a small but valid ComicInfo.xml document.
pub fn build(title: &str, page_count: usize) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
         <ComicInfo xmlns:xsd=\"http://www.w3.org/2001/XMLSchema\" \
         xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\">\n\
         \x20 <Title>{}</Title>\n\
         \x20 <PageCount>{}</PageCount>\n\
         </ComicInfo>\n",
        escape_xml(title),
        page_count
    )
}

fn escape_xml(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            // XML 1.0 forbids most control characters.
            c if (c as u32) < 0x20 && c != '\t' && c != '\n' && c != '\r' => {}
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapes_title_and_sets_page_count() {
        let xml = build("Tom & Jerry <#1>", 42);
        assert!(xml.contains("<Title>Tom &amp; Jerry &lt;#1&gt;</Title>"));
        assert!(xml.contains("<PageCount>42</PageCount>"));
    }
}
