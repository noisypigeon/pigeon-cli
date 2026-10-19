//! iCalendar (`.ics`) and vCard (`.vcf`) to readable text (CLI-0160 §4).
//!
//! Both formats are vObject content lines -- `NAME;PARAM=VALUE:VALUE`,
//! folded across physical lines -- so one parser serves both, and no crate
//! is pulled in for it: the parsing is small, and the rendering, which is
//! the real work, would be written either way.
//!
//! The rendering is **lossy on purpose**, as `md`/`rst` already are
//! (CLI-0154 §8): this family's output is meant to be read. Time zones are
//! named, never converted; recurrence rules are shown as written; base64
//! payloads (contact photos) and `X-` metadata are dropped. The source is
//! untouched (CLI-0112), so nothing dropped here is lost.
//!
//! One input, one output: a contacts export holding 679 cards becomes one
//! file of 679 blocks, which keeps `placement::compute_destination_name` a
//! pure forward function (CLI-0124).

use std::path::Path;

use super::media::{NOT_A_CALENDAR_SIGNATURE, NOT_A_VCARD_SIGNATURE};

/// Largest input either renderer will read. Measured inputs top out under
/// a megabyte; this only bounds a pathological file, since the whole input
/// is held in memory.
const MAX_INPUT_BYTES: u64 = 64 * 1024 * 1024;

/// One unfolded content line.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Property {
    /// Apple's `itemN` group prefix, which ties an `X-ABLabel` to the
    /// property it labels.
    group: Option<String>,
    /// Uppercased.
    name: String,
    /// Names uppercased, values unquoted. A vCard 2.1 bare parameter
    /// (`TEL;HOME:`) is recorded as `TYPE=HOME`.
    params: Vec<(String, String)>,
    /// Raw: still escaped, and still quoted-printable if so encoded. See
    /// `text`/`components`.
    value: String,
}

impl Property {
    fn param(&self, name: &str) -> Option<&str> {
        self.params
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    fn param_values<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a str> {
        self.params
            .iter()
            .filter(move |(key, _)| key == name)
            .flat_map(|(_, value)| value.split(','))
            .map(str::trim)
            .filter(|value| !value.is_empty())
    }

    fn is_base64(&self) -> bool {
        self.param("ENCODING").is_some_and(|encoding| {
            matches!(encoding.to_ascii_uppercase().as_str(), "B" | "BASE64")
        })
    }

    fn is_quoted_printable(&self) -> bool {
        self.param("ENCODING")
            .is_some_and(|encoding| encoding.eq_ignore_ascii_case("QUOTED-PRINTABLE"))
    }

    /// The value with quoted-printable decoded, still escaped.
    fn decoded(&self) -> String {
        if self.is_quoted_printable() {
            decode_quoted_printable(&self.value)
        } else {
            self.value.clone()
        }
    }

    /// The value as one unescaped text.
    fn text(&self) -> String {
        unescape(&self.decoded())
    }

    /// A structured value (`N`, `ADR`, `ORG`) split on its unescaped `;`,
    /// each component unescaped.
    fn components(&self) -> Vec<String> {
        split_unescaped(&self.decoded(), ';')
            .into_iter()
            .map(|component| unescape(&component))
            .collect()
    }
}

/// Renders an iCalendar file to readable text at `output`.
pub(crate) fn ics_to_text(input: &Path, output: &Path) -> Result<(), String> {
    let properties = read_properties(input)?;
    if !has_begin(&properties, "VCALENDAR") {
        return Err(format!(
            "{} {NOT_A_CALENDAR_SIGNATURE} -- it has no BEGIN:VCALENDAR line",
            input.display()
        ));
    }
    write_text(output, &render_calendar(&properties))
}

/// Renders a vCard file to readable text at `output`.
pub(crate) fn vcf_to_text(input: &Path, output: &Path) -> Result<(), String> {
    let properties = read_properties(input)?;
    if !has_begin(&properties, "VCARD") {
        return Err(format!(
            "{} {NOT_A_VCARD_SIGNATURE} -- it has no BEGIN:VCARD line",
            input.display()
        ));
    }
    write_text(output, &render_cards(&properties))
}

fn read_properties(input: &Path) -> Result<Vec<Property>, String> {
    let size = std::fs::metadata(input)
        .map(|meta| meta.len())
        .map_err(|err| format!("failed to stat {}: {err}", input.display()))?;
    if size > MAX_INPUT_BYTES {
        return Err(format!(
            "{} is {size} bytes, over this job's {MAX_INPUT_BYTES}-byte calendar/contact limit",
            input.display()
        ));
    }
    let bytes =
        std::fs::read(input).map_err(|err| format!("failed to read {}: {err}", input.display()))?;
    Ok(unfold(&bytes)
        .iter()
        .filter_map(|line| parse_line(line))
        .collect())
}

fn write_text(output: &Path, text: &str) -> Result<(), String> {
    std::fs::write(output, text.as_bytes())
        .map_err(|err| format!("failed to write {}: {err}", output.display()))
}

fn has_begin(properties: &[Property], component: &str) -> bool {
    properties
        .iter()
        .any(|prop| prop.name == "BEGIN" && prop.value.trim().eq_ignore_ascii_case(component))
}

/// Joins folded physical lines into logical ones.
///
/// Works on **bytes**, decoding each logical line only once it is whole: a
/// producer may fold in the middle of a multi-byte UTF-8 character, and
/// decoding each physical line first would turn both halves into U+FFFD.
///
/// Two kinds of continuation:
/// - a line starting with a space or tab (RFC 5545 §3.1, RFC 6350 §3.2),
///   whose first character is removed;
/// - vCard 2.1's quoted-printable soft line break: a QP-encoded line ending
///   in `=` continues on the next line, which is appended verbatim.
///
/// CRLF and bare LF are both accepted, as is a missing final newline.
fn unfold(bytes: &[u8]) -> Vec<String> {
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    let mut logical: Vec<Vec<u8>> = Vec::new();
    let mut in_soft_break = false;
    for raw in bytes.split(|&byte| byte == b'\n') {
        let line = raw.strip_suffix(b"\r").unwrap_or(raw);
        if in_soft_break && let Some(last) = logical.last_mut() {
            last.pop(); // the trailing `=`
            last.extend_from_slice(line);
        } else if let (Some(&(b' ' | b'\t')), Some(last)) = (line.first(), logical.last_mut()) {
            last.extend_from_slice(&line[1..]);
        } else if line.is_empty() {
            in_soft_break = false;
            continue;
        } else {
            logical.push(line.to_vec());
        }
        in_soft_break = logical
            .last()
            .is_some_and(|last| last.ends_with(b"=") && is_qp_line(last));
    }
    logical
        .into_iter()
        .map(|line| String::from_utf8_lossy(&line).into_owned())
        .collect()
}

/// Whether a raw logical line's parameters declare quoted-printable.
fn is_qp_line(line: &[u8]) -> bool {
    let head_end = line
        .iter()
        .position(|&byte| byte == b':')
        .unwrap_or(line.len());
    String::from_utf8_lossy(&line[..head_end])
        .to_ascii_uppercase()
        .contains("QUOTED-PRINTABLE")
}

/// Splits one logical line into a `Property`. A `:` or `;` inside a quoted
/// parameter value does not split it. Returns `None` for a line with no
/// `:` at all, which is not a content line.
fn parse_line(line: &str) -> Option<Property> {
    let mut in_quotes = false;
    let colon = line.char_indices().find_map(|(index, ch)| match ch {
        '"' => {
            in_quotes = !in_quotes;
            None
        }
        ':' if !in_quotes => Some(index),
        _ => None,
    })?;
    let (head, value) = (&line[..colon], &line[colon + 1..]);

    let mut parts = split_quoted(head, ';').into_iter();
    let full_name = parts.next()?.trim().to_string();
    let (group, name) = match full_name.rsplit_once('.') {
        Some((group, name)) => (Some(group.to_string()), name),
        None => (None, full_name.as_str()),
    };
    let params = parts
        .map(|param| match param.split_once('=') {
            Some((key, value)) => (
                key.trim().to_ascii_uppercase(),
                value.trim().trim_matches('"').to_string(),
            ),
            // vCard 2.1's bare parameter, e.g. `TEL;HOME;VOICE:`, and its
            // bare `QUOTED-PRINTABLE`/`BASE64` encodings.
            None => {
                let bare = param.trim().to_string();
                match bare.to_ascii_uppercase().as_str() {
                    "QUOTED-PRINTABLE" | "BASE64" | "B" => ("ENCODING".to_string(), bare),
                    _ => ("TYPE".to_string(), bare),
                }
            }
        })
        .collect();
    Some(Property {
        group,
        name: name.to_ascii_uppercase(),
        params,
        value: value.to_string(),
    })
}

/// Splits on `separator` outside double quotes.
fn split_quoted(text: &str, separator: char) -> Vec<String> {
    let mut parts = vec![String::new()];
    let mut in_quotes = false;
    for ch in text.chars() {
        match ch {
            '"' => {
                in_quotes = !in_quotes;
                parts.last_mut().expect("never empty").push(ch);
            }
            ch if ch == separator && !in_quotes => parts.push(String::new()),
            ch => parts.last_mut().expect("never empty").push(ch),
        }
    }
    parts
}

/// Splits on `separator` where it is not backslash-escaped, leaving every
/// escape in place for `unescape`.
fn split_unescaped(text: &str, separator: char) -> Vec<String> {
    let mut parts = vec![String::new()];
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        let current = parts.last_mut().expect("never empty");
        if ch == '\\' {
            current.push(ch);
            if let Some(next) = chars.next() {
                current.push(next);
            }
        } else if ch == separator {
            parts.push(String::new());
        } else {
            current.push(ch);
        }
    }
    parts
}

/// RFC 5545 §3.3.11 / RFC 6350 §3.4 text unescaping. An unknown escape is
/// kept as written rather than guessed at.
fn unescape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars();
    while let Some(ch) = chars.next() {
        if ch != '\\' {
            out.push(ch);
            continue;
        }
        match chars.next() {
            Some('n' | 'N') => out.push('\n'),
            Some(escaped @ (',' | ';' | ':' | '\\')) => out.push(escaped),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// Decodes `=XX` octets. The bytes are read as UTF-8 where they are valid
/// UTF-8, and otherwise as Latin-1, the usual charset of vCard 2.1
/// exports that predate UTF-8 defaults.
fn decode_quoted_printable(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'='
            && let Some(hex) = bytes.get(index + 1..index + 3)
            && let Ok(octet) = u8::from_str_radix(&String::from_utf8_lossy(hex), 16)
        {
            out.push(octet);
            index += 3;
        } else {
            out.push(bytes[index]);
            index += 1;
        }
    }
    match String::from_utf8(out) {
        Ok(text) => text,
        Err(err) => err.into_bytes().iter().map(|&byte| byte as char).collect(),
    }
}

/// The properties directly inside each `component`, in file order. Lines
/// inside a nested component (a `VALARM` in a `VEVENT`) are not included.
fn components<'a>(properties: &'a [Property], wanted: &[&str]) -> Vec<(String, Vec<&'a Property>)> {
    let mut found: Vec<(String, Vec<&Property>)> = Vec::new();
    let mut stack: Vec<String> = Vec::new();
    let mut open: Option<usize> = None;
    for prop in properties {
        match prop.name.as_str() {
            "BEGIN" => {
                let name = prop.value.trim().to_ascii_uppercase();
                if open.is_none() && wanted.contains(&name.as_str()) {
                    found.push((name.clone(), Vec::new()));
                    open = Some(stack.len());
                }
                stack.push(name);
            }
            "END" => {
                stack.pop();
                if open == Some(stack.len()) {
                    open = None;
                }
            }
            _ => {
                if let Some(depth) = open
                    && stack.len() == depth + 1
                    && let Some((_, props)) = found.last_mut()
                {
                    props.push(prop);
                }
            }
        }
    }
    found
}

fn first<'a>(props: &[&'a Property], name: &str) -> Option<&'a Property> {
    props.iter().copied().find(|prop| prop.name == name)
}

/// Appends `label: value`, or -- for a value spanning lines -- the label on
/// its own line and every line indented, so a description's own blank
/// lines cannot be mistaken for the gap between two blocks.
fn push_field(out: &mut String, label: &str, value: &str) {
    let value = value.trim_end();
    if value.trim().is_empty() {
        return;
    }
    if value.contains('\n') {
        out.push_str(label);
        out.push_str(":\n");
        for line in value.lines() {
            if !line.trim().is_empty() {
                out.push_str("  ");
                out.push_str(line.trim_end());
            }
            out.push('\n');
        }
    } else {
        out.push_str(label);
        out.push_str(": ");
        out.push_str(value.trim());
        out.push('\n');
    }
}

fn render_calendar(properties: &[Property]) -> String {
    let mut out = String::new();
    let calendar_props: Vec<&Property> = components(properties, &["VCALENDAR"])
        .into_iter()
        .flat_map(|(_, props)| props)
        .collect();
    match first(&calendar_props, "X-WR-CALNAME") {
        Some(name) if !name.text().trim().is_empty() => {
            push_field(&mut out, "Calendar", &name.text())
        }
        _ => out.push_str("Calendar\n"),
    }
    if let Some(method) = first(&calendar_props, "METHOD") {
        push_field(&mut out, "Method", &method.text());
    }

    let items = components(properties, &["VEVENT", "VTODO"]);
    if items.is_empty() {
        out.push_str("\nNo events.\n");
        return out;
    }
    let total = items.len();
    for (index, (kind, props)) in items.iter().enumerate() {
        out.push('\n');
        let noun = if kind == "VTODO" { "Task" } else { "Event" };
        out.push_str(&format!("{noun} {} of {total}\n", index + 1));
        let summary = first(props, "SUMMARY")
            .map(Property::text)
            .filter(|summary| !summary.trim().is_empty())
            .unwrap_or_else(|| "(untitled)".to_string());
        push_field(&mut out, "Summary", &summary);
        if let Some(when) = render_when(props) {
            push_field(&mut out, "When", &when);
        }
        if let Some(location) = first(props, "LOCATION") {
            push_field(&mut out, "Where", &location.text());
        }
        if let Some(organizer) = first(props, "ORGANIZER") {
            push_field(&mut out, "Organizer", &render_person(organizer));
        }
        for attendee in props.iter().filter(|prop| prop.name == "ATTENDEE") {
            push_field(&mut out, "Attendee", &render_person(attendee));
        }
        if let Some(status) = first(props, "STATUS") {
            push_field(&mut out, "Status", &status.text());
        }
        for rule in props.iter().filter(|prop| prop.name == "RRULE") {
            push_field(&mut out, "Repeats", &rule.value);
        }
        if let Some(url) = first(props, "URL") {
            push_field(&mut out, "URL", &url.text());
        }
        if let Some(description) = first(props, "DESCRIPTION") {
            push_field(&mut out, "Description", &description.text());
        }
    }
    out
}

/// `DTSTART`–`DTEND` (or `DUE`, or a `DURATION`), as written, with each
/// end's zone named. No conversion: that needs the `VTIMEZONE` rules, and a
/// wrongly converted time is worse than a local one with its zone named.
fn render_when(props: &[&Property]) -> Option<String> {
    let start = first(props, "DTSTART");
    let end = first(props, "DTEND").or_else(|| first(props, "DUE"));
    let mut when = match (start, end) {
        (Some(start), Some(end)) => {
            let (start_time, start_zone) = render_time(start);
            let (end_time, end_zone) = render_time(end);
            if start_zone == end_zone {
                with_zone(format!("{start_time} – {end_time}"), start_zone)
            } else {
                format!(
                    "{} – {}",
                    with_zone(start_time, start_zone),
                    with_zone(end_time, end_zone)
                )
            }
        }
        (Some(only), None) | (None, Some(only)) => {
            let (time, zone) = render_time(only);
            with_zone(time, zone)
        }
        (None, None) => return None,
    };
    if end.is_none()
        && let Some(duration) = first(props, "DURATION")
    {
        when.push_str(&format!(", lasting {}", duration.value.trim()));
    }
    Some(when)
}

fn with_zone(time: String, zone: Option<String>) -> String {
    match zone {
        Some(zone) => format!("{time} ({zone})"),
        None => time,
    }
}

/// A date-time as `YYYY-MM-DD HH:MM[:SS]`, a date as `YYYY-MM-DD`, and its
/// zone: the `TZID` if given, `UTC` for a trailing `Z`, none for a floating
/// time. Anything not in RFC 5545's basic format is shown as written.
fn render_time(prop: &Property) -> (String, Option<String>) {
    let raw = prop.value.trim();
    let tzid = prop.param("TZID").map(str::to_string);
    let (body, utc) = match raw.strip_suffix(['Z', 'z']) {
        Some(body) => (body, true),
        None => (raw, false),
    };
    let digits = |range: std::ops::Range<usize>| -> Option<&str> {
        body.get(range)
            .filter(|part| part.bytes().all(|byte| byte.is_ascii_digit()))
    };
    let date = match (digits(0..4), digits(4..6), digits(6..8)) {
        (Some(year), Some(month), Some(day)) => format!("{year}-{month}-{day}"),
        _ => return (raw.to_string(), tzid),
    };
    if body.len() == 8 {
        return (date, tzid);
    }
    let time = match (
        body.get(8..9),
        digits(9..11),
        digits(11..13),
        digits(13..15),
    ) {
        (Some("T" | "t"), Some(hour), Some(minute), Some(second)) if body.len() == 15 => {
            if second == "00" {
                format!("{date} {hour}:{minute}")
            } else {
                format!("{date} {hour}:{minute}:{second}")
            }
        }
        _ => return (raw.to_string(), tzid),
    };
    let zone = tzid.or_else(|| utc.then(|| "UTC".to_string()));
    (time, zone)
}

/// `Name <address> (status)` from an `ORGANIZER`/`ATTENDEE`.
fn render_person(prop: &Property) -> String {
    let value = prop.text();
    let address = value
        .strip_prefix("mailto:")
        .or_else(|| value.strip_prefix("MAILTO:"))
        .or_else(|| {
            value
                .get(..7)
                .filter(|scheme| scheme.eq_ignore_ascii_case("mailto:"))
                .map(|_| &value[7..])
        })
        .unwrap_or(&value)
        .trim();
    let name = prop
        .param("CN")
        .map(str::trim)
        .filter(|name| !name.is_empty());
    let mut person = match name {
        Some(name) if !address.is_empty() && name != address => format!("{name} <{address}>"),
        Some(name) => name.to_string(),
        None => address.to_string(),
    };
    if let Some(status) = prop.param("PARTSTAT") {
        person.push_str(&format!(
            " ({})",
            status.to_ascii_lowercase().replace('-', " ")
        ));
    }
    person
}

/// Properties never rendered from a card: binary payloads, and metadata
/// with no meaning to a reader.
const DROPPED_CARD_PROPERTIES: &[&str] = &["PHOTO", "LOGO", "SOUND", "KEY", "PRODID", "REV", "UID"];

fn render_cards(properties: &[Property]) -> String {
    let cards = components(properties, &["VCARD"]);
    let total = cards.len();
    let mut out = String::new();
    for (index, (_, props)) in cards.iter().enumerate() {
        if index > 0 {
            out.push('\n');
        }
        out.push_str(&format!("Contact {} of {total}\n", index + 1));
        let mut block = String::new();
        render_card(&mut block, props);
        if block.is_empty() {
            block.push_str("(no details)\n");
        }
        out.push_str(&block);
    }
    out
}

fn render_card(out: &mut String, all_props: &[&Property]) {
    // Read before the filter below drops every `X-` property.
    let labels: Vec<(&str, String)> = all_props
        .iter()
        .filter(|prop| prop.name == "X-ABLABEL")
        .filter_map(|prop| Some((prop.group.as_deref()?, apple_label(&prop.text()))))
        .collect();
    let props: Vec<&Property> = all_props
        .iter()
        .copied()
        .filter(|prop| {
            !prop.is_base64()
                && !prop.name.starts_with("X-")
                && !DROPPED_CARD_PROPERTIES.contains(&prop.name.as_str())
        })
        .collect();

    let name = first(&props, "FN")
        .map(Property::text)
        .filter(|name| !name.trim().is_empty())
        .or_else(|| first(&props, "N").map(render_structured_name));
    if let Some(name) = name {
        push_field(out, "Name", &name);
    }
    if let Some(org) = first(&props, "ORG") {
        push_field(out, "Organization", &join_nonempty(&org.components(), ", "));
    }
    if let Some(title) = first(&props, "TITLE") {
        push_field(out, "Title", &title.text());
    }
    for (property, noun) in [
        ("TEL", "Phone"),
        ("EMAIL", "Email"),
        ("ADR", "Address"),
        ("URL", "URL"),
    ] {
        for prop in props.iter().filter(|prop| prop.name == property) {
            let value = if property == "ADR" {
                join_nonempty(&prop.components(), ", ")
            } else {
                prop.text()
            };
            let label = match card_label(prop, &labels) {
                Some(label) => format!("{noun} ({label})"),
                None => noun.to_string(),
            };
            push_field(out, &label, &value.replace('\n', ", "));
        }
    }
    if let Some(birthday) = first(&props, "BDAY") {
        push_field(out, "Birthday", &birthday.text());
    }
    if let Some(categories) = first(&props, "CATEGORIES") {
        push_field(out, "Categories", &categories.text());
    }
    if let Some(note) = first(&props, "NOTE") {
        push_field(out, "Note", &note.text());
    }
}

/// Apple writes its built-in labels as `_$!<Home>!$_` and custom ones
/// plainly; either way the reader wants `Home`.
fn apple_label(raw: &str) -> String {
    raw.trim()
        .strip_prefix("_$!<")
        .and_then(|rest| rest.strip_suffix(">!$_"))
        .unwrap_or(raw.trim())
        .to_string()
}

/// A property's label: Apple's `itemN.X-ABLabel` for its group when there
/// is one, else its `TYPE` values minus the uninformative ones.
fn card_label(prop: &Property, labels: &[(&str, String)]) -> Option<String> {
    if let Some(group) = prop.group.as_deref()
        && let Some((_, label)) = labels.iter().find(|(owner, _)| *owner == group)
        && !label.is_empty()
    {
        return Some(label.clone());
    }
    let types: Vec<String> = prop
        .param_values("TYPE")
        .filter(|kind| {
            !matches!(
                kind.to_ascii_uppercase().as_str(),
                "INTERNET" | "PREF" | "X400"
            )
        })
        .map(str::to_ascii_lowercase)
        .collect();
    (!types.is_empty()).then(|| types.join(", "))
}

/// `N`'s `Family;Given;Additional;Prefix;Suffix` in reading order.
fn render_structured_name(prop: &Property) -> String {
    let parts = prop.components();
    let get = |index: usize| parts.get(index).map(String::as_str).unwrap_or("");
    join_nonempty(
        &[get(3), get(1), get(2), get(0), get(4)].map(str::to_string),
        " ",
    )
}

fn join_nonempty(parts: &[String], separator: &str) -> String {
    parts
        .iter()
        .map(|part| part.trim())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(separator)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::job::transform::media;

    fn render(
        input: &str,
        convert: fn(&Path, &Path) -> Result<(), String>,
    ) -> Result<String, String> {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("input");
        std::fs::write(&path, input).unwrap();
        let output = dir.path().join("output.txt.scratch");
        convert(&path, &output)?;
        Ok(std::fs::read_to_string(output).unwrap())
    }

    fn ics(input: &str) -> String {
        render(input, ics_to_text).expect("renders")
    }

    fn vcf(input: &str) -> String {
        render(input, vcf_to_text).expect("renders")
    }

    /// Shaped on a real invite from the bucket: CRLF, a folded and escaped
    /// description, a `TZID` start, an organizer with a `CN`, a
    /// `VTIMEZONE` and a `VALARM` that must not leak into the output.
    const MEETUP: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nMETHOD:PUBLISH\r\n\
X-WR-CALNAME:Events - Cocoaheads Berlin\r\n\
BEGIN:VTIMEZONE\r\nTZID:Europe/Berlin\r\nBEGIN:DAYLIGHT\r\nTZNAME:CEST\r\n\
DTSTART:19700329T020000\r\nEND:DAYLIGHT\r\nEND:VTIMEZONE\r\n\
BEGIN:VEVENT\r\nDTSTAMP:20170602T084602Z\r\n\
DTSTART;TZID=Europe/Berlin:20170621T184500\r\n\
DTEND;TZID=Europe/Berlin:20170621T220000\r\nSTATUS:CONFIRMED\r\n\
SUMMARY:CocoaHeads June 2017\r\n\
DESCRIPTION:Cocoaheads Berlin\\nWednesday\\, June 21 at 6:45 PM\\n\\nJoin us at Teu\r\n fel\\; our host.\r\n\
ORGANIZER;CN=Meetup Reminder:MAILTO:info@meetup.com\r\n\
LOCATION:Lautsprecher Teufel (Europa Center\\, Tauentzienstraße 9-12)\r\n\
UID:event_240264939@meetup.com\r\n\
BEGIN:VALARM\r\nTRIGGER:-PT15M\r\nDESCRIPTION:Reminder\r\nEND:VALARM\r\n\
END:VEVENT\r\nEND:VCALENDAR\r\n";

    #[test]
    fn an_invite_renders_its_fields_unfolded_and_unescaped() {
        let text = ics(MEETUP);
        assert!(
            text.starts_with("Calendar: Events - Cocoaheads Berlin\nMethod: PUBLISH\n"),
            "{text}"
        );
        assert!(
            text.contains("Event 1 of 1\nSummary: CocoaHeads June 2017\n"),
            "{text}"
        );
        assert!(
            text.contains("When: 2017-06-21 18:45 – 2017-06-21 22:00 (Europe/Berlin)\n"),
            "{text}"
        );
        assert!(
            text.contains("Where: Lautsprecher Teufel (Europa Center, Tauentzienstraße 9-12)\n"),
            "{text}"
        );
        assert!(
            text.contains("Organizer: Meetup Reminder <info@meetup.com>\n"),
            "{text}"
        );
        assert!(text.contains("Status: CONFIRMED\n"), "{text}");
        assert!(text.contains("Description:\n  Cocoaheads Berlin\n  Wednesday, June 21 at 6:45 PM\n\n  Join us at Teufel; our host.\n"), "{text}");
        // Dropped: the alarm, the time-zone block, the identifiers.
        assert!(
            !text.contains("Reminder\n  ") && !text.contains("Description: Reminder"),
            "{text}"
        );
        assert!(
            !text.contains("CEST") && !text.contains("UID") && !text.contains("DTSTAMP"),
            "{text}"
        );
    }

    /// The bucket's eight airline stubs: a calendar with no component.
    #[test]
    fn an_empty_calendar_still_produces_a_file() {
        let text = ics(
            "BEGIN:VCALENDAR\r\nPRODID:-//AFKLM//CRMPUSH//TKT//EN\r\nVERSION:2.0\r\n\
CALSCALE:GREGORIAN\r\nUID:b821dddc\r\nEND:VCALENDAR\r\n",
        );
        assert_eq!(text, "Calendar\n\nNo events.\n");
    }

    #[test]
    fn times_render_as_written_with_their_zone_named() {
        let text = ics("BEGIN:VCALENDAR\nBEGIN:VEVENT\nSUMMARY:All day\n\
DTSTART;VALUE=DATE:20230501\nDTEND;VALUE=DATE:20230502\nEND:VEVENT\n\
BEGIN:VEVENT\nSUMMARY:Utc\nDTSTART:20230501T120030Z\nDURATION:PT1H\nEND:VEVENT\n\
BEGIN:VEVENT\nSUMMARY:Floating\nDTSTART:20230501T090000\nEND:VEVENT\n\
BEGIN:VEVENT\nSUMMARY:Flight\nDTSTART;TZID=America/Toronto:20230501T090000\n\
DTEND;TZID=Europe/Berlin:20230501T230000\nEND:VEVENT\n\
BEGIN:VEVENT\nSUMMARY:Odd\nDTSTART:sometime\nEND:VEVENT\nEND:VCALENDAR");
        assert!(text.contains("When: 2023-05-01 – 2023-05-02\n"), "{text}");
        assert!(
            text.contains("When: 2023-05-01 12:00:30 (UTC), lasting PT1H\n"),
            "{text}"
        );
        assert!(text.contains("When: 2023-05-01 09:00\n"), "{text}");
        assert!(
            text.contains(
                "When: 2023-05-01 09:00 (America/Toronto) – 2023-05-01 23:00 (Europe/Berlin)\n"
            ),
            "{text}"
        );
        assert!(text.contains("When: sometime\n"), "{text}");
        assert!(text.contains("Event 5 of 5\n"), "{text}");
    }

    #[test]
    fn attendees_rules_and_tasks_are_rendered() {
        let text = ics("BEGIN:VCALENDAR\nBEGIN:VEVENT\nSUMMARY:Standup\n\
RRULE:FREQ=WEEKLY;BYDAY=MO,WE\n\
ATTENDEE;CN=\"Doe, Jane\";PARTSTAT=NEEDS-ACTION:mailto:jane@example.com\n\
ATTENDEE:mailto:bob@example.com\nEND:VEVENT\n\
BEGIN:VTODO\nSUMMARY:File taxes\nDUE;VALUE=DATE:20240430\nEND:VTODO\nEND:VCALENDAR\n");
        assert!(
            text.contains("Repeats: FREQ=WEEKLY;BYDAY=MO,WE\n"),
            "{text}"
        );
        assert!(
            text.contains("Attendee: Doe, Jane <jane@example.com> (needs action)\n"),
            "{text}"
        );
        assert!(text.contains("Attendee: bob@example.com\n"), "{text}");
        assert!(
            text.contains("Task 2 of 2\nSummary: File taxes\nWhen: 2024-04-30\n"),
            "{text}"
        );
    }

    /// A producer may fold in the middle of a multi-byte character; the
    /// halves must rejoin as bytes, not as two U+FFFDs.
    #[test]
    fn a_fold_inside_a_multibyte_character_rejoins() {
        let mut bytes = b"BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nSUMMARY:Stra\xc3".to_vec();
        bytes.extend_from_slice(b"\r\n \x9fe\r\nEND:VEVENT\r\nEND:VCALENDAR");
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("in.ics");
        std::fs::write(&input, bytes).unwrap();
        let output = dir.path().join("out");
        ics_to_text(&input, &output).unwrap();
        assert!(
            std::fs::read_to_string(output)
                .unwrap()
                .contains("Summary: Straße\n")
        );
    }

    /// Shaped on the bucket's Apple cards: `itemN.` groups with their
    /// `X-ABLabel`, a base64 blob and a photo that must be dropped.
    #[test]
    fn an_apple_card_renders_labels_and_drops_binary_data() {
        let text = vcf(
            "BEGIN:VCARD\r\nVERSION:3.0\r\nPRODID:-//Apple Inc.//iOS 18.3.2//EN\r\n\
N:Doe;Jane;;;\r\nFN:Jane Doe\r\nORG:Acme\\, Inc.;Research\r\nTITLE:Chemist\r\n\
item1.X-ADDRESSING-GRAMMAR:a7CYIvtjcNQ81/ukB6RfCfo9R/a0IWYAGJGwkNs2KO1ZBUYu\r\n r0aolD7K3\r\n\
item2.TEL;type=pref:+1 (604) 849-1226\r\nitem2.X-ABLabel:_$!<Mobile>!$_\r\n\
EMAIL;type=INTERNET;type=HOME;type=pref:jane@example.com\r\n\
item3.ADR;type=HOME:;;1 Main St\\nApt 2;Vancouver;BC;V5K 0A1;Canada\r\n\
item3.X-ABLabel:cottage\r\n\
PHOTO;ENCODING=b;TYPE=JPEG:/9j/4AAQSkZJRgABAQAAAQABAAD\r\n\
BDAY:1990-04-01\r\nNOTE:Met at\\nthe conference\r\nREV:2025-05-09T19:19:33Z\r\n\
UID:4B227341\r\nEND:VCARD\r\n",
        );
        assert_eq!(
            text,
            "Contact 1 of 1\nName: Jane Doe\nOrganization: Acme, Inc., Research\nTitle: Chemist\n\
Phone (Mobile): +1 (604) 849-1226\nEmail (home): jane@example.com\n\
Address (cottage): 1 Main St, Apt 2, Vancouver, BC, V5K 0A1, Canada\n\
Birthday: 1990-04-01\nNote:\n  Met at\n  the conference\n"
        );
    }

    /// `All Contacts.vcf` holds 679 cards: one output, one block per card,
    /// including a card with nothing left to show and one with no `FN`.
    #[test]
    fn a_multi_card_export_renders_one_block_per_card() {
        let text = vcf("BEGIN:VCARD\nVERSION:3.0\nFN:Bear\nEND:VCARD\n\
BEGIN:VCARD\nVERSION:3.0\nN:Smith;John;Q;Dr.;Jr.\nEND:VCARD\n\
BEGIN:VCARD\nVERSION:3.0\nPHOTO;ENCODING=b:AAAA\nEND:VCARD\n");
        assert_eq!(
            text,
            "Contact 1 of 3\nName: Bear\n\nContact 2 of 3\nName: Dr. John Q Smith Jr.\n\n\
Contact 3 of 3\n(no details)\n"
        );
    }

    /// The bucket's one vCard 2.1 card: bare parameters, and a
    /// quoted-printable value continued by a soft line break.
    #[test]
    fn a_vcard_21_quoted_printable_value_is_decoded() {
        let text = vcf("BEGIN:VCARD\r\nVERSION:2.1\r\n\
FN;CHARSET=UTF-8;ENCODING=QUOTED-PRINTABLE:Ren=C3=A9e Fran=\r\n=C3=A7ois\r\n\
TEL;CELL;PREF:555-0100\r\nNOTE;QUOTED-PRINTABLE:caf=E9\r\nEND:VCARD\r\n");
        assert!(text.contains("Name: Renée François\n"), "{text}");
        assert!(text.contains("Phone (cell): 555-0100\n"), "{text}");
        // Not valid UTF-8, so read as Latin-1.
        assert!(text.contains("Note: café\n"), "{text}");
    }

    #[test]
    fn a_quoted_parameter_may_contain_separators() {
        let prop = parse_line("ATTENDEE;CN=\"Doe; Jane: PhD\":mailto:j@example.com").unwrap();
        assert_eq!(prop.param("CN"), Some("Doe; Jane: PhD"));
        assert_eq!(prop.value, "mailto:j@example.com");
    }

    #[test]
    fn unescape_keeps_an_unknown_escape_as_written() {
        assert_eq!(unescape(r"a\nb\,c\;d\\e\qf\"), "a\nb,c;d\\e\\qf\\");
    }

    /// CLI-0154 §2's precedent: check the input positively. A file that is
    /// not the claimed kind is wrong-input-kind, so CLI-0143 drops it.
    #[test]
    fn each_kind_refuses_a_file_that_is_not_one() {
        for (input, convert) in [
            (
                "BEGIN:VCARD\nFN:x\nEND:VCARD\n",
                ics_to_text as fn(&Path, &Path) -> Result<(), String>,
            ),
            ("BEGIN:VCALENDAR\nEND:VCALENDAR\n", vcf_to_text),
            ("not a vobject at all", ics_to_text),
            ("", vcf_to_text),
        ] {
            let err = render(input, convert).expect_err("refused");
            assert!(media::is_wrong_input_kind_error(&err), "{err}");
            assert!(media::is_non_retryable_transcode_error(&err), "{err}");
        }
    }
}
