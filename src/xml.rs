//! A small reader for the XML the GoXLR files use (JUCE ValueTree documents: nested elements with
//! attributes, no text content). Keeps every element in document order with its parent, so values
//! can be read and changed in memory. Saving goes through `profile_file`, which edits the original
//! text, so nothing here ever writes a file.

use anyhow::{Context, Result, bail};

#[derive(Clone, Debug)]
pub struct Element {
    pub name: String,
    pub attrs: Vec<(String, String)>,
    pub parent: Option<usize>,
}

#[derive(Clone, Debug, Default)]
pub struct Doc {
    pub elements: Vec<Element>,
}

impl Doc {
    pub fn parse(text: &str) -> Result<Self> {
        let mut doc = Doc::default();
        let mut stack: Vec<usize> = Vec::new();
        let b = text.as_bytes();
        let mut i = 0;
        while let Some(off) = text[i..].find('<') {
            i += off;
            let rest = &text[i..];
            if rest.starts_with("<?") {
                i += rest.find("?>").context("unterminated <?")? + 2;
            } else if rest.starts_with("<!--") {
                i += rest.find("-->").context("unterminated comment")? + 3;
            } else if rest.starts_with("<!") {
                i += rest.find('>').context("unterminated <!")? + 1;
            } else if rest.starts_with("</") {
                let end = rest.find('>').context("unterminated end tag")?;
                let name = rest[2..end].trim();
                let open = stack.pop().with_context(|| format!("unexpected </{name}>"))?;
                if doc.elements[open].name != name {
                    bail!("</{name}> closes <{}>", doc.elements[open].name);
                }
                i += end + 1;
            } else {
                // Start tag: name, then attributes until '>' or '/>'.
                let mut j = i + 1;
                while j < b.len() && !b[j].is_ascii_whitespace() && b[j] != b'>' && b[j] != b'/' {
                    j += 1;
                }
                let name = text[i + 1..j].to_string();
                if name.is_empty() {
                    bail!("empty element name at byte {i}");
                }
                let mut attrs = Vec::new();
                let self_closing = loop {
                    while j < b.len() && b[j].is_ascii_whitespace() {
                        j += 1;
                    }
                    match b.get(j) {
                        None => bail!("unterminated <{name}>"),
                        Some(b'>') => break false,
                        Some(b'/') if b.get(j + 1) == Some(&b'>') => {
                            j += 1;
                            break true;
                        }
                        _ => {}
                    }
                    let key_start = j;
                    while j < b.len() && b[j] != b'=' && !b[j].is_ascii_whitespace() {
                        j += 1;
                    }
                    let key = text[key_start..j].to_string();
                    while j < b.len() && (b[j].is_ascii_whitespace() || b[j] == b'=') {
                        j += 1;
                    }
                    let quote = *b.get(j).with_context(|| format!("attribute {key} has no value"))?;
                    if quote != b'"' && quote != b'\'' {
                        bail!("attribute {key} value is not quoted");
                    }
                    let value_end = j + 1 + text[j + 1..].find(quote as char).context("unterminated attribute")?;
                    attrs.push((key, unescape(&text[j + 1..value_end])));
                    j = value_end + 1;
                };
                doc.elements.push(Element { name, attrs, parent: stack.last().copied() });
                if !self_closing {
                    stack.push(doc.elements.len() - 1);
                }
                i = j + 1;
            }
        }
        if let Some(&open) = stack.last() {
            bail!("<{}> is never closed", doc.elements[open].name);
        }
        Ok(doc)
    }

    /// The first element called `name`.
    pub fn find(&self, name: &str) -> Option<usize> {
        self.elements.iter().position(|e| e.name == name)
    }

    /// The first child of `parent` called `name`.
    pub fn find_child(&self, parent: usize, name: &str) -> Option<usize> {
        self.elements.iter().position(|e| e.parent == Some(parent) && e.name == name)
    }

    pub fn attr(&self, element: usize, key: &str) -> Option<&str> {
        self.elements[element].attrs.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
    }

    /// Sets an attribute, adding it if it's missing.
    pub fn set_attr(&mut self, element: usize, key: &str, value: String) {
        let attrs = &mut self.elements[element].attrs;
        match attrs.iter_mut().find(|(k, _)| k == key) {
            Some((_, v)) => *v = value,
            None => attrs.push((key.to_string(), value)),
        }
    }

    /// Adds an empty element under `parent` (or at the top level) and returns it.
    pub fn add(&mut self, name: &str, parent: Option<usize>) -> usize {
        self.elements.push(Element { name: name.to_string(), attrs: Vec::new(), parent });
        self.elements.len() - 1
    }
}

fn unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(p) = rest.find('&') {
        out.push_str(&rest[..p]);
        rest = &rest[p..];
        let Some(end) = rest.find(';') else { break };
        let entity = &rest[1..end];
        let ch = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ if entity.starts_with("#x") || entity.starts_with("#X") => {
                u32::from_str_radix(&entity[2..], 16).ok().and_then(char::from_u32)
            }
            _ if entity.starts_with('#') => entity[1..].parse().ok().and_then(char::from_u32),
            _ => None,
        };
        match ch {
            Some(c) => {
                out.push(c);
                rest = &rest[end + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::Doc;

    #[test]
    fn reads_nested_elements_attributes_and_entities() {
        let doc = Doc::parse(
            r#"<?xml version="1.0" encoding="UTF-8"?>
            <!-- a comment -->
            <Root a="1">
              <child x = "a &amp; b &#65;&#x42;" y='&quot;q&quot;'/>
              <group><child x="inner"/></group>
            </Root>"#,
        )
        .unwrap();
        let root = doc.find("Root").unwrap();
        let child = doc.find("child").unwrap();
        assert_eq!(doc.attr(root, "a"), Some("1"));
        assert_eq!(doc.attr(child, "x"), Some("a & b AB"));
        assert_eq!(doc.attr(child, "y"), Some("\"q\""));
        assert_eq!(doc.elements[child].parent, Some(root));
        let group = doc.find("group").unwrap();
        let inner = doc.find_child(group, "child").unwrap();
        assert_eq!(doc.attr(inner, "x"), Some("inner"));
        assert!(Doc::parse("<a><b></a>").is_err());
        assert!(Doc::parse("<a>").is_err());
    }
}
