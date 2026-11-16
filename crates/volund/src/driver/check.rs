//! CHECK: deterministic checks of the recorded deliverables, with no model call. Each file is read
//! back through the protocol and checked for the format its name promises; a directory must hold files.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use serde_json::Value;
use volund_proto::Outgoing;
use volund_world::{CheckResult, DeliverableStatus, Format, World};

use crate::link::Link;

/// Runs the checks, records them in the world (replacing the previous ones) and moves deliverable
/// statuses along: a file that exists is at least written, one that passes is checked.
pub async fn run(link: &Link, world: &mut World) {
    let mut results = Vec::new();
    if world.deliverables.is_empty() {
        results.push(CheckResult {
            name: "deliverables recorded".into(),
            passed: false,
            detail: "no deliverable is recorded; record each with world_deliverable".into(),
        });
    }
    let workdir = link.task.workdir.clone();
    for d in &mut world.deliverables {
        let path = absolute(&workdir, &d.path);
        let (exists, outcome) = if d.format == Format::Directory {
            directory(link, &path).await
        } else {
            file(link, &path, d.format).await
        };
        d.status = match (&outcome, exists) {
            (Ok(_), _) => DeliverableStatus::Checked,
            (Err(_), true) => DeliverableStatus::Written,
            (Err(_), false) => DeliverableStatus::Missing,
        };
        results.push(CheckResult {
            name: d.path.clone(),
            passed: outcome.is_ok(),
            detail: outcome.unwrap_or_else(|e| e),
        });
    }
    world.checks = results;
}

fn absolute(workdir: &str, path: &str) -> String {
    if path.starts_with('/') {
        path.to_owned()
    } else {
        format!(
            "{}/{}",
            workdir.trim_end_matches('/'),
            path.trim_start_matches("./")
        )
    }
}

/// Whether the path exists, and the check's verdict (`Ok(detail)` or `Err(problem)`).
async fn file(link: &Link, path: &str, format: Format) -> (bool, Result<String, String>) {
    let reply = match link
        .request(|id| Outgoing::ReadFile {
            id,
            path: path.to_owned(),
        })
        .await
    {
        Ok(r) => r,
        Err(e) => return (false, Err(format!("not read: {e}"))),
    };
    if !reply.ok() {
        return (
            false,
            Err(format!(
                "missing: {}",
                reply.error().unwrap_or("cannot be read")
            )),
        );
    }
    let bytes = B64
        .decode(
            reply
                .fields
                .get("content_b64")
                .and_then(Value::as_str)
                .unwrap_or(""),
        )
        .unwrap_or_default();
    let whole = reply.fields.get("truncated").and_then(Value::as_bool) != Some(true);
    (true, verdict(&bytes, whole, format))
}

/// The format check on a file's bytes (`whole`: all of the file was read).
pub fn verdict(bytes: &[u8], whole: bool, format: Format) -> Result<String, String> {
    if bytes.is_empty() {
        return Err("the file is empty".into());
    }
    let size = format!("{} bytes", bytes.len());
    let zip = bytes.starts_with(b"PK\x03\x04");
    let has = |needle: &[u8]| bytes.windows(needle.len()).any(|w| w == needle);
    match format {
        Format::Docx | Format::Xlsx | Format::Pptx => {
            let part: &[u8] = match format {
                Format::Docx => b"word/",
                Format::Xlsx => b"xl/",
                _ => b"ppt/",
            };
            if !zip {
                return Err("not an Office file: it is not a zip package".into());
            }
            if !has(b"[Content_Types].xml") || !has(part) {
                return Err(format!(
                    "not a {format:?} package: [Content_Types].xml or {} is missing",
                    String::from_utf8_lossy(part)
                ));
            }
            Ok(size)
        }
        Format::Zip => {
            if !zip {
                return Err("not a zip archive".into());
            }
            let entries = bytes.windows(4).filter(|w| *w == b"PK\x01\x02").count();
            if whole && entries == 0 {
                return Err("the zip has no central directory: it is cut short".into());
            }
            Ok(format!("{size}, {entries} entries"))
        }
        Format::Pdf => {
            if !bytes.starts_with(b"%PDF-") {
                return Err("not a PDF: no %PDF- header".into());
            }
            let tail = &bytes[bytes.len().saturating_sub(2048)..];
            if whole && !tail.windows(5).any(|w| w == b"%%EOF") {
                return Err("the PDF has no %%EOF: it is cut short".into());
            }
            Ok(size)
        }
        Format::Ifc => {
            let head = &bytes[..bytes.len().min(512)];
            if !head.windows(12).any(|w| w == b"ISO-10303-21") {
                return Err("not an IFC file: no ISO-10303-21 header".into());
            }
            if whole && !has(b"END-ISO-10303-21") {
                return Err("the IFC file has no END-ISO-10303-21: it is cut short".into());
            }
            Ok(size)
        }
        Format::Json if whole => serde_json::from_slice::<Value>(bytes)
            .map(|_| size)
            .map_err(|e| format!("not valid JSON: {e}")),
        Format::Markdown
        | Format::Text
        | Format::Csv
        | Format::Yaml
        | Format::Html
        | Format::Xml
        | Format::Python
        | Format::Json => match std::str::from_utf8(bytes) {
            Ok(text) if text.trim().is_empty() => Err("the file holds only whitespace".into()),
            Ok(_) => Ok(size),
            Err(_) if !whole => Ok(size),
            Err(_) => Err("not UTF-8 text".into()),
        },
        Format::Directory | Format::Other => Ok(size),
    }
}

async fn directory(link: &Link, path: &str) -> (bool, Result<String, String>) {
    let quoted = format!("'{}'", path.replace('\'', r"'\''"));
    let command = format!(
        "if [ -d {quoted} ]; then find {quoted} -type f 2>/dev/null | head -n 10000 | wc -l; else echo missing; fi"
    );
    let reply = match link
        .request(|id| Outgoing::Exec {
            id,
            command,
            timeout_sec: Some(60),
        })
        .await
    {
        Ok(r) if r.ok() => r,
        Ok(r) => {
            return (
                false,
                Err(r.error().unwrap_or("the check could not run").to_owned()),
            );
        }
        Err(e) => return (false, Err(format!("not checked: {e}"))),
    };
    let out = reply
        .fields
        .get("output")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_owned();
    match out.parse::<u64>() {
        Ok(0) => (true, Err("the directory is empty".into())),
        Ok(n) => (true, Ok(format!("{n} file(s)"))),
        Err(_) => (false, Err("the directory does not exist".into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_are_checked_by_their_bytes() {
        assert!(verdict(b"", true, Format::Text).is_err());
        assert!(verdict(b"# Memo\n", true, Format::Markdown).is_ok());
        assert!(verdict(b"   \n", true, Format::Markdown).is_err());
        assert!(verdict(b"%PDF-1.7 ... %%EOF\n", true, Format::Pdf).is_ok());
        assert!(verdict(b"%PDF-1.7 ... cut", true, Format::Pdf).is_err());
        assert!(
            verdict(b"%PDF-1.7 ... cut", false, Format::Pdf).is_ok(),
            "only the start was read"
        );
        assert!(verdict(b"<html>", true, Format::Pdf).is_err());
        assert!(verdict(b"{\"a\": 1}", true, Format::Json).is_ok());
        assert!(verdict(b"{\"a\": ", true, Format::Json).is_err());
        assert!(
            verdict(
                b"ISO-10303-21;\nHEADER;\nEND-ISO-10303-21;",
                true,
                Format::Ifc
            )
            .is_ok()
        );
        assert!(
            verdict(
                b"PK\x03\x04....[Content_Types].xml....word/document.xml",
                true,
                Format::Docx
            )
            .is_ok()
        );
        assert!(
            verdict(
                b"PK\x03\x04....[Content_Types].xml....xl/workbook.xml",
                true,
                Format::Docx
            )
            .is_err()
        );
        assert!(verdict(b"plain text", true, Format::Xlsx).is_err());
        assert_eq!(
            verdict(b"PK\x03\x04..PK\x01\x02..PK\x01\x02", true, Format::Zip).unwrap(),
            "16 bytes, 2 entries"
        );
        assert!(verdict(b"PK\x03\x04 no directory", true, Format::Zip).is_err());
    }

    #[test]
    fn relative_paths_resolve_against_the_workdir() {
        assert_eq!(absolute("/app", "out/a.md"), "/app/out/a.md");
        assert_eq!(absolute("/app/", "./a.md"), "/app/a.md");
        assert_eq!(absolute("/app", "/filesystem/x"), "/filesystem/x");
    }
}
