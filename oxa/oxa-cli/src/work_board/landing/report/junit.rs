//! Decode failed test identities from a verified single-report archive.
use quick_xml::{
    events::{BytesStart, Event},
    Reader, XmlVersion,
};
use std::{
    collections::BTreeSet,
    io::{Cursor, Read},
};
use zip::ZipArchive;

const MAX_XML_BYTES: u64 = 16 * 1024 * 1024;
const MAX_ELEMENTS_DEEP: usize = 64;
const MAX_NAME_BYTES: usize = 512;

pub(super) fn failed_tests(bytes: &[u8]) -> Result<Vec<String>, String> {
    let mut archive = ZipArchive::new(Cursor::new(bytes)).map_err(|_| "invalid CI report archive")?;
    if archive.len() != 1 {
        return Err("CI archive must contain one JUnit report".into());
    }
    let entry = archive.by_index(0).map_err(|_| "CI report entry unavailable")?;
    if !entry.is_file()
        || entry.is_symlink()
        || entry.encrypted()
        || entry.enclosed_name().is_none()
        || !entry.name().is_ok_and(|name| name.ends_with(".xml"))
        || entry.size() == 0
        || entry.size() > MAX_XML_BYTES
    {
        return Err("invalid CI report entry".into());
    }
    let expected = entry.size();
    let mut xml = Vec::new();
    entry
        .take(MAX_XML_BYTES + 1)
        .read_to_end(&mut xml)
        .map_err(|_| "cannot read CI report")?;
    if xml.len() as u64 != expected {
        return Err("CI report size mismatch".into());
    }
    parse(&xml)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Element {
    Suites,
    Suite,
    Test,
    Other,
}

struct TestCase {
    qualified_name: String,
    failed: bool,
}

fn test_case(element: &BytesStart<'_>) -> Result<TestCase, String> {
    let mut class = None;
    let mut name = None;
    for attr in element.attributes() {
        let attr = attr.map_err(|_| "invalid JUnit test attributes")?;
        let slot = match attr.key.as_ref() {
            "classname" => &mut class,
            "name" => &mut name,
            _ => continue,
        };
        let text = attr
            .normalized_value(XmlVersion::Implicit1_0)
            .map_err(|_| "invalid JUnit test name")?;
        if text.is_empty() || text.len() > MAX_NAME_BYTES || text.chars().any(char::is_control) {
            return Err("invalid JUnit test name".into());
        }
        *slot = Some(text.into_owned());
    }
    let class = class.ok_or("JUnit test is missing a class name")?;
    let name = name.ok_or("JUnit test is missing a name")?;
    Ok(TestCase {
        qualified_name: format!("{class}::{name}"),
        failed: false,
    })
}

fn parse(xml: &[u8]) -> Result<Vec<String>, String> {
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().expand_empty_elements = true;
    let mut stack = Vec::new();
    let mut root_seen = false;
    let mut test: Option<TestCase> = None;
    let mut failed = BTreeSet::new();
    loop {
        match reader.read_event().map_err(|_| "invalid JUnit XML")? {
            Event::Start(tag) => {
                let parent = stack.last().copied();
                let element = match (parent, tag.name().as_ref()) {
                    (None, "testsuites") => Element::Suites,
                    (None | Some(Element::Suites | Element::Suite), "testsuite") => Element::Suite,
                    (Some(Element::Suite), "testcase") => {
                        test = Some(test_case(&tag)?);
                        Element::Test
                    }
                    (Some(Element::Test), "failure" | "error") => {
                        test.as_mut().ok_or("missing JUnit test case")?.failed = true;
                        Element::Other
                    }
                    (None, _) | (_, "testcase" | "testsuite" | "testsuites") => {
                        return Err("unexpected JUnit element".into());
                    }
                    _ => Element::Other,
                };
                if parent.is_none() && std::mem::replace(&mut root_seen, true) {
                    return Err("multiple JUnit roots".into());
                }
                if stack.len() == MAX_ELEMENTS_DEEP {
                    return Err("JUnit report exceeds nesting limit".into());
                }
                stack.push(element);
            }
            Event::End(_) if stack.pop().ok_or("unexpected JUnit closing tag")? == Element::Test => {
                let test = test.take().ok_or("missing JUnit test case")?;
                if test.failed {
                    failed.insert(test.qualified_name);
                }
            }
            Event::DocType(_) => return Err("JUnit document type declarations are unsupported".into()),
            Event::Text(text)
                if stack.is_empty() && !text.as_ref().bytes().all(|byte| byte.is_ascii_whitespace()) =>
            {
                return Err("unexpected text outside JUnit root".into());
            }
            Event::CData(_) | Event::GeneralRef(_) if stack.is_empty() => {
                return Err("unexpected text outside JUnit root".into());
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if !root_seen || !stack.is_empty() {
        return Err("incomplete JUnit report".into());
    }
    Ok(failed.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use zip::{write::SimpleFileOptions, CompressionMethod, ZipWriter};

    fn archive(documents: &[(&str, &str)]) -> Vec<u8> {
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        for (path, xml) in documents {
            writer.start_file(*path, options).unwrap();
            writer.write_all(xml.as_bytes()).unwrap();
        }
        writer.finish().unwrap().into_inner()
    }

    #[test]
    fn finds_failed_and_errored_identities_without_reading_test_output_as_metadata() {
        let xml = r#"<?xml version="1.0"?>
<testsuites><testsuite name="native">
    <testcase classname="crate" name="suite::failed"><failure>arbitrary output</failure></testcase>
    <testcase classname="crate" name="suite::errored"><error/></testcase>
    <testcase classname="crate" name="passed"><system-out>&lt;failure/&gt;</system-out></testcase>
    <testcase classname="crate" name="skipped"><skipped/></testcase>
</testsuite></testsuites>"#;
        let bytes = archive(&[("junit.xml", xml)]);
        assert_eq!(
            failed_tests(&bytes).unwrap(),
            ["crate::suite::errored", "crate::suite::failed"]
        );
    }

    #[test]
    fn refuses_untrusted_report_shapes() {
        let good = "<testsuites><testsuite/></testsuites>";
        for xml in [
            "<!DOCTYPE testsuites><testsuites/>",
            "<![CDATA[unexpected]]><testsuites/>",
            "<testsuites/>&amp;",
            "<testsuite><testcase name=\"missing-class\"><failure/></testcase></testsuite>",
            "<testsuite><system-out><testcase name=\"forged\" classname=\"crate\"/></system-out></testsuite>",
            "<testsuites><testsuite/>",
        ] {
            assert!(failed_tests(&archive(&[("junit.xml", xml)])).is_err());
        }
        assert!(failed_tests(&archive(&[("one.xml", good), ("two.xml", good)])).is_err());
        assert!(failed_tests(&archive(&[("../junit.xml", good)])).is_err());
        assert!(failed_tests(b"not an archive").is_err());
    }
}
