//! Mail: an RFC 822 message's headers, its text (from the HTML part when
//! it has no text part), its HTML, and its attachments.

use serde_json::{Value, json};

fn parse(raw: &[u8]) -> anyhow::Result<mail_parser::Message<'_>> {
    let msg = mail_parser::MessageParser::default().parse(raw).ok_or_else(|| anyhow::anyhow!("not an RFC 822 message"))?;
    anyhow::ensure!(
        msg.from().is_some() || msg.message_id().is_some() || msg.subject().is_some() || msg.date().is_some(),
        "not an RFC 822 message: no headers"
    );
    Ok(msg)
}

fn addrs(a: Option<&mail_parser::Address>) -> Vec<(String, String)> {
    a.map(|a| a.iter().filter_map(|x| Some((x.address()?.to_lowercase(), x.name().unwrap_or("").to_owned()))).collect()).unwrap_or_default()
}

fn mime_of(a: &mail_parser::MessagePart) -> String {
    use mail_parser::MimeHeaders;
    a.content_type().map(|c| format!("{}/{}", c.ctype(), c.subtype().unwrap_or(""))).unwrap_or_default()
}

/// What the knowledge store keeps of a message, and its HTML (if any).
pub fn message(raw: &[u8]) -> anyhow::Result<(Value, Option<String>)> {
    use mail_parser::MimeHeaders;
    let msg = parse(raw)?;
    let list = |h: &mail_parser::HeaderValue| -> Vec<String> {
        h.as_text_list().map(|l| l.iter().map(|r| r.to_string()).collect()).unwrap_or_default()
    };
    let plain = msg.body_text(0).map(|t| t.into_owned());
    // Its own HTML, not what the parser makes of a text part.
    let html = msg
        .html_part(0)
        .filter(|p| matches!(p.body, mail_parser::PartType::Html(_)))
        .and_then(|_| msg.body_html(0))
        .map(|h| h.into_owned());
    let text = match &plain {
        Some(t) if !t.trim().is_empty() => t.clone(),
        _ => html.as_deref().map(|h| htmd::convert(h).unwrap_or_default()).unwrap_or_default(),
    };
    let from = msg.from().and_then(|a| a.first()).and_then(|a| a.address()).map(str::to_owned);
    let attachments: Vec<Value> = msg
        .attachments()
        .enumerate()
        .map(|(i, a)| json!({"index": i, "name": a.attachment_name().unwrap_or("attachment"), "size": a.contents().len(), "mime": mime_of(a)}))
        .collect();
    let v = json!({
        "message_id": msg.message_id().map(|m| m.trim()).filter(|m| !m.is_empty()),
        // As the first From header gives it (identity without a Message-ID).
        "from_raw": from,
        "from": addrs(msg.from()).into_iter().next(),
        "to": addrs(msg.to()),
        "cc": addrs(msg.cc()),
        "subject": msg.subject().unwrap_or(""),
        "date": msg.date().map(|d| d.to_timestamp()),
        "references": list(msg.references()),
        "in_reply_to": list(msg.in_reply_to()),
        "list": msg.header_raw("List-Unsubscribe").is_some() || msg.header_raw("List-Id").is_some(),
        "plain": plain.unwrap_or_default(),
        "text": text,
        "html": html.is_some(),
        "attachments": attachments,
    });
    Ok((v, html))
}

/// A message's nth attachment: (name, mime, bytes).
pub fn attachment(raw: &[u8], index: usize) -> anyhow::Result<(String, String, Vec<u8>)> {
    use mail_parser::MimeHeaders;
    let msg = parse(raw)?;
    let a = msg.attachments().nth(index).ok_or_else(|| anyhow::anyhow!("no attachment {index}"))?;
    let mime = mime_of(a);
    Ok((
        a.attachment_name().unwrap_or("attachment").to_owned(),
        if mime.is_empty() { "application/octet-stream".into() } else { mime },
        a.contents().to_vec(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_message() {
        let raw = b"From: Ada <Ada@example.com>\r\nTo: bob@example.com\r\nSubject: Engines\r\nMessage-ID: <m1@example.com>\r\n\
In-Reply-To: <m0@example.com>\r\nDate: Mon, 1 Jan 2024 10:00:00 +0000\r\nList-Id: x\r\n\r\nShall we meet?\r\n";
        let (v, html) = message(raw).unwrap();
        assert_eq!(v["message_id"], "m1@example.com");
        assert_eq!(v["from"], json!(["ada@example.com", "Ada"]));
        assert_eq!(v["in_reply_to"], json!(["m0@example.com"]));
        assert_eq!((v["date"].as_i64(), v["list"].as_bool()), (Some(1704103200), Some(true)));
        assert!(v["text"].as_str().unwrap().starts_with("Shall we meet?") && html.is_none());
        assert!(message(b"just text").is_err());
    }
}
