//! CHECK: deterministic checks of the recorded deliverables, with no model call.
//!
//! - Each file is read back through the protocol and checked for the format its name promises; a
//!   directory must hold files.
//! - No stray files: every file added to the task's folders since PROBE must be a deliverable
//!   (scratch work belongs in /tmp/volund).
//! - Around VERIFY, [`hashes`] tells whether it changed the deliverables.

use std::collections::BTreeMap;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64;
use serde_json::Value;
use volund_proto::Outgoing;
use volund_world::{CheckResult, DeliverableStatus, Format, World};

use super::probe::{LIST_FILES, START_FILES};
use crate::link::Link;

pub const STRAY_CHECK: &str = "no stray files in the task's folders";
pub const VERIFY_CHECK: &str = "deliverables unchanged by VERIFY";
/// New files listed by the stray-file check at most; the rest are not seen.
const NEW_FILES_LISTED: usize = 1000;
/// Stray files named in a check's detail; the rest are counted.
const STRAY_SHOWN: usize = 20;

/// Each deliverable's absolute path and the hash of its contents (`missing` when absent).
pub type Hashes = BTreeMap<String, String>;

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
    if let Some(stray) = stray_files(link, world).await {
        record(world, stray);
    }
}

/// Puts `result` in the world's checks, in place of an earlier result of the same name.
pub fn record(world: &mut World, result: CheckResult) {
    match world.checks.iter_mut().find(|c| c.name == result.name) {
        Some(earlier) => *earlier = result,
        None => world.checks.push(result),
    }
}

fn absolute(workdir: &str, path: &str) -> String {
    let joined = if path.starts_with('/') {
        path.to_owned()
    } else {
        format!("{}/{path}", workdir.trim_end_matches('/'))
    };
    normalize(&joined)
}

/// Removes empty and `.` segments and resolves `..`; a trailing `/` (a directory) is kept.
fn normalize(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            p => parts.push(p),
        }
    }
    let mut out = format!("/{}", parts.join("/"));
    if path.ends_with('/') && out.len() > 1 {
        out.push('/');
    }
    out
}

fn quote(path: &str) -> String {
    format!("'{}'", path.replace('\'', r"'\''"))
}

/// Whether `path` is the deliverable at `deliverable` or inside it.
fn covers(deliverable: &str, path: &str) -> bool {
    let d = deliverable.trim_end_matches('/');
    path == d
        || path
            .strip_prefix(d)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// The stray-file check: files added to the task's folders since PROBE that no deliverable covers.
/// `None` when it cannot run (PROBE kept no list, or the container did not answer).
pub async fn stray_files(link: &Link, world: &World) -> Option<CheckResult> {
    let command = format!(
        "echo '### new files'\nif [ -f {START_FILES} ]; then {{ {LIST_FILES}; }} | LC_ALL=C comm -13 {START_FILES} - | head -n {NEW_FILES_LISTED}; else echo '### no list from PROBE'; fi"
    );
    let output = exec_output(link, command, 120).await?;
    let new = new_files(&output)?;
    let deliverables: Vec<String> = world
        .deliverables
        .iter()
        .map(|d| absolute(&world.workdir, &d.path))
        .collect();
    Some(stray_verdict(&new, &deliverables))
}

/// The paths after the `### new files` line; `None` when the check did not run.
fn new_files(output: &str) -> Option<Vec<String>> {
    let mut lines = output.lines().map(str::trim_end);
    lines.find(|l| *l == "### new files")?;
    let mut out = Vec::new();
    for line in lines {
        if line == "### no list from PROBE" {
            return None;
        }
        if line.starts_with('/') {
            out.push(normalize(line));
        }
    }
    Some(out)
}

fn stray_verdict(new: &[String], deliverables: &[String]) -> CheckResult {
    let stray: Vec<&String> = new
        .iter()
        .filter(|p| !deliverables.iter().any(|d| covers(d, p)))
        .collect();
    let at_least = if new.len() >= NEW_FILES_LISTED {
        "at least "
    } else {
        ""
    };
    if stray.is_empty() {
        return CheckResult {
            name: STRAY_CHECK.into(),
            passed: true,
            detail: format!("{at_least}{} new file(s), all deliverables", new.len()),
        };
    }
    let shown: Vec<&str> = stray.iter().take(STRAY_SHOWN).map(|p| p.as_str()).collect();
    let more = stray.len() - shown.len();
    let mut detail = format!(
        "{at_least}{} file(s) added that are not recorded deliverables: {}",
        stray.len(),
        shown.join(", ")
    );
    if more > 0 {
        detail.push_str(&format!(" (and {more} more)"));
    }
    detail.push_str(
        ". Move scratch work to /tmp/volund; if a file belongs to the result, record it with \
         world_deliverable instead.",
    );
    CheckResult {
        name: STRAY_CHECK.into(),
        passed: false,
        detail,
    }
}

/// Hashes every deliverable in the task container (a directory as a whole). `None` when there is
/// nothing to hash or the container cannot hash.
pub async fn hashes(link: &Link, world: &World) -> Option<Hashes> {
    let paths: Vec<String> = world
        .deliverables
        .iter()
        .map(|d| absolute(&world.workdir, &d.path))
        .collect();
    if paths.is_empty() {
        return None;
    }
    let quoted: Vec<String> = paths.iter().map(|p| quote(p)).collect();
    let command = format!(
        "echo '### hashes'\ncommand -v sha256sum >/dev/null 2>&1 || {{ echo '### no sha256sum'; exit 0; }}\n\
         for p in {}; do\n\
         if [ -d \"$p\" ]; then h=$(cd \"$p\" && find . -type f -print0 | LC_ALL=C sort -z | xargs -0 -r sha256sum | sha256sum | cut -c1-64)\n\
         elif [ -f \"$p\" ]; then h=$(sha256sum < \"$p\" | cut -c1-64)\n\
         else h=missing; fi\n\
         printf '%s\\t%s\\n' \"$h\" \"$p\"\ndone",
        quoted.join(" ")
    );
    let output = exec_output(link, command, 300).await?;
    parse_hashes(&output, &paths)
}

/// Each path's hash from the `### hashes` output; a path the output does not name is `missing`.
fn parse_hashes(output: &str, paths: &[String]) -> Option<Hashes> {
    let mut lines = output.lines().map(str::trim_end);
    lines.find(|l| *l == "### hashes")?;
    let mut found = BTreeMap::new();
    for line in lines {
        if line == "### no sha256sum" {
            return None;
        }
        if let Some((hash, path)) = line.split_once('\t')
            && (hash == "missing"
                || (hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit())))
        {
            found.insert(normalize(path), hash.to_owned());
        }
    }
    Some(
        paths
            .iter()
            .map(|p| {
                let hash = found
                    .get(&normalize(p))
                    .cloned()
                    .unwrap_or_else(|| "missing".into());
                (p.clone(), hash)
            })
            .collect(),
    )
}

/// The deliverables whose hash differs between `before` and `after`.
pub fn changed(before: &Hashes, after: &Hashes) -> Vec<String> {
    let mut out: Vec<String> = before
        .iter()
        .filter(|(p, h)| after.get(*p) != Some(*h))
        .map(|(p, _)| p.clone())
        .collect();
    out.extend(after.keys().filter(|p| !before.contains_key(*p)).cloned());
    out
}

/// The record of VERIFY having changed deliverables; it passes only when they were restored.
pub fn unchanged_by_verify(changed: &[String], restored: bool) -> CheckResult {
    let list = changed.join(", ");
    CheckResult {
        name: VERIFY_CHECK.into(),
        passed: restored,
        detail: if restored {
            format!("VERIFY changed {list}; restored from the copy made before it")
        } else {
            format!(
                "VERIFY changed {list}, and the copy made before it could not be restored; check \
                 that each is complete and correct"
            )
        },
    }
}

/// The output of a scripted command; `None` when it did not run.
async fn exec_output(link: &Link, command: String, timeout_sec: u64) -> Option<String> {
    let reply = link
        .request(|id| Outgoing::Exec {
            id,
            command,
            timeout_sec: Some(timeout_sec),
        })
        .await
        .ok()?;
    if !reply.ok() {
        return None;
    }
    reply
        .fields
        .get("output")
        .and_then(Value::as_str)
        .map(str::to_owned)
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
    let quoted = quote(path);
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
        assert_eq!(absolute("/app", "out//./sub/../b/"), "/app/out/b/");
        assert_eq!(absolute("/app", "/"), "/");
    }

    #[test]
    fn stray_files_are_new_files_no_deliverable_covers() {
        let out =
            "### new files\n/app/out.md\n/app/output/fig/a.png\n/app/notes.txt\n/app/outer.md\n";
        let new = new_files(out).unwrap();
        assert_eq!(new.len(), 4);
        let deliverables = ["/app/out.md".to_owned(), "/app/output/".to_owned()];
        let r = stray_verdict(&new, &deliverables);
        assert!(!r.passed);
        assert!(
            r.detail.starts_with(
                "2 file(s) added that are not recorded deliverables: /app/notes.txt, /app/outer.md."
            ),
            "{}",
            r.detail
        );
        let r = stray_verdict(&new[..2], &deliverables);
        assert!(r.passed, "{}", r.detail);
        assert_eq!(r.detail, "2 new file(s), all deliverables");
        assert!(stray_verdict(&[], &[]).passed);
    }

    #[test]
    fn the_stray_check_is_skipped_without_the_probe_list() {
        assert_eq!(new_files("### new files\n### no list from PROBE\n"), None);
        assert_eq!(new_files(""), None, "the command did not run");
        assert_eq!(new_files("### new files\n"), Some(vec![]));
    }

    #[test]
    fn hashes_name_every_deliverable_and_changes_are_found() {
        let a = "a".repeat(64);
        let b = "b".repeat(64);
        let paths = [
            "/app/x.md".to_owned(),
            "/app/out/".to_owned(),
            "/app/gone.pdf".to_owned(),
        ];
        let before = parse_hashes(
            &format!("### hashes\n{a}\t/app/x.md\n{b}\t/app/out/\nmissing\t/app/gone.pdf\nnot a hash\t/app/y\n"),
            &paths,
        )
        .unwrap();
        assert_eq!(before.len(), 3);
        assert_eq!(before["/app/gone.pdf"], "missing");
        let after = parse_hashes(
            &format!("### hashes\n{b}\t/app/x.md\n{b}\t/app/out/\n"),
            &paths,
        )
        .unwrap();
        assert_eq!(changed(&before, &after), ["/app/x.md"]);
        assert!(changed(&before, &before).is_empty());
        assert_eq!(parse_hashes("### hashes\n### no sha256sum\n", &paths), None);
        assert_eq!(parse_hashes("", &paths), None);
    }

    #[test]
    fn a_change_by_verify_passes_only_once_restored() {
        let changed = ["/app/x.md".to_owned()];
        assert!(unchanged_by_verify(&changed, true).passed);
        let r = unchanged_by_verify(&changed, false);
        assert!(!r.passed && r.detail.starts_with("VERIFY changed /app/x.md, and"));
    }

    #[test]
    fn a_new_result_replaces_one_of_the_same_name() {
        let mut world = World::default();
        let result = |passed| CheckResult {
            name: STRAY_CHECK.into(),
            passed,
            detail: String::new(),
        };
        record(&mut world, result(false));
        record(&mut world, result(true));
        assert_eq!(world.checks.len(), 1);
        assert!(world.checks[0].passed);
    }
}
