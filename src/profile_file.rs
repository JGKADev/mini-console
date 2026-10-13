//! Saving changes back into a `.goxlr` profile without re-serialising it.
//!
//! Rewriting the whole file risks losing or reformatting values the official app relies on, so this
//! module edits only the attributes the app manages inside the original profile.xml text, and
//! copies every other zip entry (the scribble images) unchanged.

use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use zip::write::SimpleFileOptions;

/// One attribute to set: (element name, attribute name, value).
pub type Edit = (String, String, String);

/// Sets `attr="value"` on the first `<element …>` start tag. Adds the attribute if it is missing.
pub fn set_attribute(xml: &str, element: &str, attr: &str, value: &str) -> Result<String> {
    let mut out = xml.to_string();
    set_attribute_in(&mut out, element, attr, value)?;
    Ok(out)
}

/// `set_attribute` in place. On error the text is unchanged.
fn set_attribute_in(xml: &mut String, element: &str, attr: &str, value: &str) -> Result<()> {
    let start = find_element(xml, element).with_context(|| format!("element <{element}> not found"))?;
    let end = tag_end(xml, start).context("unterminated start tag")?;
    let b = xml.as_bytes();
    // Walk the attributes after the name: key, '=', then a value in double or single quotes.
    let mut j = start + 1 + element.len();
    let mut found = None;
    loop {
        while j < end && b[j].is_ascii_whitespace() {
            j += 1;
        }
        if j >= end || b[j] == b'/' {
            break;
        }
        let key_start = j;
        while j < end && b[j] != b'=' && !b[j].is_ascii_whitespace() {
            j += 1;
        }
        let key = &xml[key_start..j];
        while j < end && (b[j].is_ascii_whitespace() || b[j] == b'=') {
            j += 1;
        }
        if j >= end || (b[j] != b'"' && b[j] != b'\'') {
            bail!("<{element}> attribute {key} is not quoted");
        }
        let quote = b[j];
        let value_end = j + 1 + xml[j + 1..end].find(quote as char).context("unterminated attribute")?;
        if key == attr {
            found = Some((j + 1, value_end, quote));
            break;
        }
        j = value_end + 1;
    }
    match found {
        Some((value_start, value_end, quote)) => xml.replace_range(value_start..value_end, &escape(value, quote)),
        None => {
            let insert_at = if xml[..end].ends_with('/') { end - 1 } else { end };
            xml.insert_str(insert_at, &format!(" {attr}=\"{}\"", escape(value, b'"')));
        }
    }
    Ok(())
}

/// Escapes an attribute value written between `quote` characters.
fn escape(value: &str, quote: u8) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' if quote == b'\'' => out.push_str("&apos;"),
            c => out.push(c),
        }
    }
    out
}

/// Where the start tag at `start` ends: its closing '>', skipping any inside quoted values.
fn tag_end(xml: &str, start: usize) -> Option<usize> {
    let mut quote = None;
    for (i, &c) in xml.as_bytes()[start..].iter().enumerate() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => {}
            None if c == b'"' || c == b'\'' => quote = Some(c),
            None if c == b'>' => return Some(start + i),
            None => {}
        }
    }
    None
}

fn find_element(xml: &str, element: &str) -> Option<usize> {
    let open = format!("<{element}");
    xml.match_indices(&open)
        .map(|(at, _)| at)
        // The tag name must end here (so <mute1 does not match <mute10), outside any comment.
        .find(|&at| {
            xml[at + open.len()..].starts_with(|c: char| c.is_whitespace() || c == '>' || c == '/')
                && xml[..at].rfind("<!--").is_none_or(|c| xml[c..at].contains("-->"))
        })
}

/// Adds a missing element, empty. Older profiles have no submix elements: those go where the
/// official app keeps them (mixRoutingTree and submixerTree first under the root, linkingTree
/// inside submixerTree); any other element goes last under the root. Missing attributes read as
/// the loader's defaults, so an empty element changes nothing.
fn ensure_element(xml: &str, element: &str) -> Result<String> {
    if find_element(xml, element).is_some() {
        return Ok(xml.to_string());
    }
    if element.is_empty() || !element.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.')) {
        bail!("<{element}> is not an element name this app creates");
    }
    let root = crate::xml::Doc::parse(xml.trim_start_matches('\u{feff}'))?
        .elements
        .first()
        .map(|e| e.name.clone())
        .context("no root element")?;
    match element {
        "mixRoutingTree" | "submixerTree" => insert_child(xml, &root, &format!("<{element}/>")),
        "linkingTree" => insert_child(&ensure_element(xml, "submixerTree")?, "submixerTree", "<linkingTree/>"),
        _ => insert_last(xml, &root, &format!("<{element}/>")),
    }
}

/// Inserts `child` as the first child of the first `<parent …>`, opening a self-closing tag if needed.
fn insert_child(xml: &str, parent: &str, child: &str) -> Result<String> {
    let start = find_element(xml, parent).with_context(|| format!("element <{parent}> not found"))?;
    let end = tag_end(xml, start).context("unterminated start tag")?;
    Ok(if xml[..end].ends_with('/') {
        format!("{}>{child}</{parent}>{}", &xml[..end - 1], &xml[end + 1..])
    } else {
        format!("{}{child}{}", &xml[..=end], &xml[end + 1..])
    })
}

/// Inserts `child` just before the root's closing tag: on its own line, indented like the line
/// above, when the closing tag starts a line.
fn insert_last(xml: &str, root: &str, child: &str) -> Result<String> {
    let Some(close) = xml.rfind(&format!("</{root}")) else { return insert_child(xml, root, child) };
    let line_start = xml[..close].rfind('\n').map_or(0, |i| i + 1);
    if line_start == 0 || !xml[line_start..close].trim().is_empty() {
        return Ok(format!("{}{child}{}", &xml[..close], &xml[close..]));
    }
    let above_end = xml[..line_start - 1].trim_end_matches('\r').len();
    let above = xml[..above_end].rfind('\n').map_or(0, |i| i + 1);
    let indent: String = xml[above..above_end].chars().take_while(|c| *c == ' ' || *c == '\t').collect();
    let newline = if xml[..line_start].ends_with("\r\n") { "\r\n" } else { "\n" };
    Ok(format!("{}{indent}{child}{newline}{}", &xml[..line_start], &xml[line_start..]))
}

/// Applies `edits` to the text, adding missing elements. An edit that can't be applied safely is
/// skipped and reported (on stderr) rather than failing the whole save. The result is then checked
/// by parsing it again (`verify`); if anything looks wrong this fails and nothing gets written.
fn apply(xml: &str, edits: &[Edit]) -> Result<String> {
    let mut out = xml.to_string();
    let mut applied = Vec::new();
    let mut created: Vec<String> = Vec::new();
    for edit in edits {
        let (element, attr, value) = edit;
        if find_element(&out, element).is_none() {
            match ensure_element(&out, element) {
                Ok(new) => {
                    // linkingTree may have brought its submixerTree with it.
                    for e in ["submixerTree", element.as_str()] {
                        if find_element(xml, e).is_none() && find_element(&new, e).is_some() && !created.iter().any(|c| c == e) {
                            created.push(e.to_string());
                        }
                    }
                    out = new;
                }
                Err(e) => {
                    eprintln!("not saved: {element} {attr}: {e:#}");
                    continue;
                }
            }
        }
        match set_attribute_in(&mut out, element, attr, value) {
            Ok(()) => applied.push(edit),
            Err(e) => eprintln!("not saved: {element} {attr}: {e:#}"),
        }
    }
    verify(xml, &out, &applied, &created).context("the edited file didn't check out, so nothing was written")?;
    Ok(out)
}

/// The safety net: `new` must parse, hold the same elements as `old` plus the `created` ones, read
/// back every applied edit's value, and keep every other attribute as it was.
fn verify(old: &str, new: &str, applied: &[&Edit], created: &[String]) -> Result<()> {
    use crate::xml::Doc;
    let a = Doc::parse(old.trim_start_matches('\u{feff}'))?;
    let b = Doc::parse(new.trim_start_matches('\u{feff}'))?;
    if b.elements.len() != a.elements.len() + created.len() {
        bail!("the element count changed from {} to {}", a.elements.len(), b.elements.len());
    }
    for (i, (element, attr, value)) in applied.iter().enumerate() {
        // A later edit of the same attribute wins.
        if applied[i + 1..].iter().any(|(e, k, _)| e == element && k == attr) {
            continue;
        }
        let e = b.find(element).with_context(|| format!("<{element}> is missing after the edit"))?;
        if b.attr(e, attr) != Some(value.as_str()) {
            bail!("{element} {attr} reads back as {:?}, not {value:?}", b.attr(e, attr));
        }
    }
    let kept = b.elements.iter().filter(|e| !created.contains(&e.name));
    for (i, (was, now)) in a.elements.iter().zip(kept).enumerate() {
        if was.name != now.name {
            bail!("<{}> became <{}>", was.name, now.name);
        }
        let edited = |k: &str| a.find(&was.name) == Some(i) && applied.iter().any(|(e, at, _)| *e == was.name && at == k);
        let before: Vec<_> = was.attrs.iter().filter(|(k, _)| !edited(k)).collect();
        let after: Vec<_> = now.attrs.iter().filter(|(k, _)| !edited(k)).collect();
        if before != after {
            bail!("other attributes of <{}> changed", was.name);
        }
    }
    Ok(())
}

/// Writes a temporary file next to `path` with `write`, flushes it to disk, then swaps it in. The
/// temporary file is removed if anything fails.
fn replace_file(path: &Path, write: impl FnOnce(File) -> Result<File>) -> Result<()> {
    let tmp = path.with_extension("mini-console.tmp");
    let result = File::create(&tmp)
        .map_err(anyhow::Error::from)
        .and_then(write)
        .and_then(|file| Ok(file.sync_all()?))
        .and_then(|()| std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display())));
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Applies `edits` to the profile at `path`. A timestamped copy of the original goes to
/// `backup_dir` first. Returns the backup's path, or None when nothing would change.
pub fn save_edits(path: &Path, edits: &[Edit], backup_dir: &Path) -> Result<Option<PathBuf>> {
    let original = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(&original))?;

    let mut xml = String::new();
    archive.by_name("profile.xml").context("profile has no profile.xml")?.read_to_string(&mut xml)?;
    let new_xml = apply(&xml, edits)?;
    if new_xml == xml {
        return Ok(None);
    }

    let backup = backup_path(path, backup_dir)?;
    std::fs::write(&backup, &original).with_context(|| format!("writing backup {}", backup.display()))?;

    // Write the new zip next to the profile, then swap it in.
    replace_file(path, |file| {
        let mut out = zip::ZipWriter::new(file);
        for i in 0..archive.len() {
            let entry = archive.by_index_raw(i)?;
            if entry.name() == "profile.xml" {
                let options = SimpleFileOptions::default().compression_method(entry.compression());
                drop(entry);
                out.start_file("profile.xml", options)?;
                out.write_all(new_xml.as_bytes())?;
            } else {
                out.raw_copy_file(entry)?;
            }
        }
        Ok(out.finish()?)
    })?;
    Ok(Some(backup))
}

/// Applies `edits` to a plain XML file (a `.goxlrMicProfile`), with a backup first.
pub fn save_xml_edits(path: &Path, edits: &[Edit], backup_dir: &Path) -> Result<Option<PathBuf>> {
    if edits.is_empty() {
        return Ok(None);
    }
    let original = std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let xml = apply(&original, edits)?;
    if xml == original {
        return Ok(None);
    }
    let backup = backup_path(path, backup_dir)?;
    std::fs::write(&backup, &original)?;
    replace_file(path, |mut file| {
        file.write_all(xml.as_bytes())?;
        Ok(file)
    })?;
    Ok(Some(backup))
}

fn backup_path(path: &Path, backup_dir: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(backup_dir)?;
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_millis();
    let name = path.file_name().context("path has no file name")?.to_string_lossy();
    Ok(backup_dir.join(format!("{stamp}-{name}")))
}

#[cfg(test)]
mod tests {
    use super::{Edit, apply, ensure_element, set_attribute, verify};

    fn edit(element: &str, attr: &str, value: &str) -> Edit {
        (element.into(), attr.into(), value.into())
    }

    #[test]
    fn adds_missing_submix_elements_in_place() {
        let xml = r#"<ValueTreeRoot v="1"><mixerTree micLevel="1"/></ValueTreeRoot>"#;
        let out = ensure_element(xml, "linkingTree").unwrap();
        assert_eq!(
            out,
            r#"<ValueTreeRoot v="1"><submixerTree><linkingTree/></submixerTree><mixerTree micLevel="1"/></ValueTreeRoot>"#
        );
        assert_eq!(ensure_element(&out, "submixerTree").unwrap(), out, "present: unchanged");
        let out = set_attribute(&ensure_element(xml, "mixRoutingTree").unwrap(), "mixRoutingTree", "stream", "2").unwrap();
        assert!(out.starts_with(r#"<ValueTreeRoot v="1"><mixRoutingTree stream="2"/>"#));
        let out = ensure_element(xml, "mute1").unwrap();
        assert_eq!(out, r#"<ValueTreeRoot v="1"><mixerTree micLevel="1"/><mute1/></ValueTreeRoot>"#, "others go last");
        assert!(ensure_element(xml, "bad name").is_err());
    }

    #[test]
    fn a_missing_element_goes_last_under_the_root_on_its_own_line() {
        let xml = "<Root>
  <mixerTree micLevel=\"1\"/>
</Root>
";
        let out = apply(xml, &[edit("animationTree", "animationMode", "2")]).unwrap();
        assert_eq!(
            out,
            "<Root>
  <mixerTree micLevel=\"1\"/>
  <animationTree animationMode=\"2\"/>
</Root>
"
        );
        let mic = "<MicProfileTree>
  <dspTreeMicProfile A=\"1\"/>
</MicProfileTree>";
        let out = apply(mic, &[edit("setupTreeMicProfile", "MIC_TYPE", "1"), edit("dspTreeMicProfile", "A", "2")]).unwrap();
        assert_eq!(
            out,
            "<MicProfileTree>
  <dspTreeMicProfile A=\"2\"/>
  <setupTreeMicProfile MIC_TYPE=\"1\"/>
</MicProfileTree>"
        );
    }

    #[test]
    fn an_edit_that_cannot_be_made_is_skipped_not_fatal() {
        let xml = r#"<Root><mixerTree micLevel="1"/></Root>"#;
        let out = apply(xml, &[edit("bad name", "x", "1"), edit("mixerTree", "micLevel", "2")]).unwrap();
        assert_eq!(out, r#"<Root><mixerTree micLevel="2"/></Root>"#);
    }

    #[test]
    fn greater_than_inside_a_value_is_not_the_end_of_the_tag() {
        let xml = r#"<a><b x="1>2" y="0"/></a>"#;
        assert_eq!(set_attribute(xml, "b", "y", "1").unwrap(), r#"<a><b x="1>2" y="1"/></a>"#);
        assert_eq!(set_attribute(xml, "b", "z", "1").unwrap(), r#"<a><b x="1>2" y="0" z="1"/></a>"#);
        assert_eq!(set_attribute(r#"<b x="y=&quot;2&quot;" y="0">"#, "b", "y", "3").unwrap(), r#"<b x="y=&quot;2&quot;" y="3">"#);
    }

    #[test]
    fn single_quoted_attributes_are_matched_and_values_escaped() {
        let xml = "<a><b x='0' y=\"0\"/></a>";
        assert_eq!(set_attribute(xml, "b", "x", "it's").unwrap(), "<a><b x='it&apos;s' y=\"0\"/></a>");
        assert_eq!(set_attribute(xml, "b", "y", "a&b\"<>'").unwrap(), "<a><b x='0' y=\"a&amp;b&quot;&lt;&gt;'\"/></a>");
        let out = apply(xml, &[edit("b", "x", "<&>"), edit("b", "z", "\"'")]).unwrap();
        let doc = crate::xml::Doc::parse(&out).unwrap();
        let b = doc.find("b").unwrap();
        assert_eq!((doc.attr(b, "x"), doc.attr(b, "z")), (Some("<&>"), Some("\"'")));
        assert!(set_attribute("<b x=0/>", "b", "y", "1").is_err(), "unquoted values are refused");
    }

    #[test]
    fn elements_named_in_comments_are_skipped() {
        let xml = r#"<!-- <mixerTree micLevel="1"/> --><Root><mixerTree micLevel="1"/></Root>"#;
        let out = apply(xml, &[edit("mixerTree", "micLevel", "2")]).unwrap();
        assert_eq!(out, r#"<!-- <mixerTree micLevel="1"/> --><Root><mixerTree micLevel="2"/></Root>"#);
    }

    #[test]
    fn the_safety_net_refuses_a_bad_result() {
        let xml = r#"<Root><a x="1" y="1"/><b/></Root>"#;
        let e = edit("a", "x", "2");
        assert!(verify(xml, r#"<Root><a x="2" y="1"/><b/></Root>"#, &[&e], &[]).is_ok());
        assert!(verify(xml, r#"<Root><a x="1" y="1"/><b/></Root>"#, &[&e], &[]).is_err(), "value not written");
        assert!(verify(xml, r#"<Root><a x="2" y="2"/><b/></Root>"#, &[&e], &[]).is_err(), "another attribute changed");
        assert!(verify(xml, r#"<Root><a x="2" y="1"/></Root>"#, &[&e], &[]).is_err(), "an element went missing");
        assert!(verify(xml, r#"<Root><a x="2" y="1"/><b/>"#, &[&e], &[]).is_err(), "no longer parses");
    }

    #[test]
    fn replaces_existing_attribute_only_in_the_named_element() {
        let xml = r#"<root><mute10 mute10state="0"/><mute1 a="x"
            mute1state="0" mute1blink="0"/></root>"#;
        let out = set_attribute(xml, "mute1", "mute1state", "1").unwrap();
        assert_eq!(out, xml.replace(r#"mute1state="0""#, r#"mute1state="1""#));
    }

    #[test]
    fn adds_missing_attribute() {
        let out = set_attribute(r#"<a><muteChat blink="0"/></a>"#, "muteChat", "coughButtonIsOn", "1").unwrap();
        assert_eq!(out, r#"<a><muteChat blink="0" coughButtonIsOn="1"/></a>"#);
    }

    #[test]
    fn does_not_match_attribute_suffixes() {
        let xml = r#"<mixerTree micLevel="1" mic2headphoneSubLevel="2"/>"#;
        let out = set_attribute(xml, "mixerTree", "micLevel", "9").unwrap();
        assert_eq!(out, r#"<mixerTree micLevel="9" mic2headphoneSubLevel="2"/>"#);
        let out = set_attribute(xml, "mixerTree", "SubLevel", "9").unwrap();
        assert!(out.contains(r#"mic2headphoneSubLevel="2""#), "must not edit a longer attribute name");
    }
}

#[cfg(test)]
mod file_tests {
    use super::*;
    use crate::state::State;

    /// Set GOXLR_PROFILE / GOXLR_MIC to run this against a copy of a real profile.
    #[test]
    fn save_changes_only_the_edited_attribute() {
        let (test_profile, test_mic) = crate::profile::test_files();
        let src = std::env::var("GOXLR_PROFILE").map(PathBuf::from).unwrap_or_else(|_| test_profile.to_path_buf());
        let mic = std::env::var("GOXLR_MIC").map(PathBuf::from).unwrap_or_else(|_| test_mic.to_path_buf());
        let work = std::env::temp_dir().join(format!("mini-console-test-{}", std::process::id()));
        std::fs::create_dir_all(&work).unwrap();
        let copy = work.join("profile.goxlr");
        std::fs::copy(&src, &copy).unwrap();
        let before = std::fs::read(&copy).unwrap();

        let mut state = State::load(&copy, &mic).unwrap();
        assert!(save_edits(&copy, &state.changed_edits(), &work.join("bak")).unwrap().is_none(), "no change, no write");
        assert_eq!(std::fs::read(&copy).unwrap(), before);

        let (on, _, _) = state.mute_state(crate::profile::Fader::B);
        let leds = state.profile().mute_leds(crate::profile::Fader::B);
        state.profile_mut().set_lit(&leds, !on);
        let backup = save_edits(&copy, &state.changed_edits(), &work.join("bak")).unwrap().expect("a change was saved");
        assert_eq!(std::fs::read(&backup).unwrap(), before, "backup holds the original");

        let read_xml = |p: &Path| {
            let mut a = zip::ZipArchive::new(File::open(p).unwrap()).unwrap();
            let mut s = String::new();
            a.by_name("profile.xml").unwrap().read_to_string(&mut s).unwrap();
            (s, a.len())
        };
        let ((old, n_old), (new, n_new)) = (read_xml(&backup), read_xml(&copy));
        assert_eq!(n_old, n_new, "every zip entry kept");
        let expected = set_attribute(&old, "mute2", "mute2state", if on { "0" } else { "1" }).unwrap();
        assert_eq!(new, expected, "only mute2state differs");
        assert_ne!(new, old);
        assert!(State::load(&copy, &mic).is_ok(), "saved profile still loads");
        assert!(!copy.with_extension("mini-console.tmp").exists(), "no temporary file left");
        std::fs::remove_dir_all(&work).ok();
    }

    #[test]
    fn mic_save_adds_a_missing_element() {
        let (_, test_mic) = crate::profile::test_files();
        let mic = test_mic.with_file_name("Old.goxlrMicProfile");
        std::fs::write(
            &mic,
            "<MicProfileTree>
  <dspTreeMicProfile MIC_DEESS_AMOUNT=\"20\"/>
</MicProfileTree>
",
        )
        .unwrap();
        let edits = [
            ("setupTreeMicProfile".into(), "MIC_TYPE".into(), "1".into()),
            (crate::profile::DSP.into(), "MIC_DEESS_AMOUNT".into(), "30".into()),
        ];
        let bak = test_mic.with_file_name("bak");
        assert!(save_xml_edits(&mic, &edits, &bak).unwrap().is_some());
        let m = crate::profile::MicProfile::load(&mic).unwrap();
        assert_eq!(m.mic_type(), 1);
        assert_eq!(m.dsp("MIC_DEESS_AMOUNT", 0), 30);
        assert!(!mic.with_extension("mini-console.tmp").exists(), "no temporary file left");
    }
}
