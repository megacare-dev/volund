//! PROBE: one scripted command lists the input files and what the container offers, so no model
//! turn is spent finding out. Hidden paths (the apps' own data among them) are left out.

use serde_json::Value;
use volund_proto::Outgoing;
use volund_world::{FileEntry, World};

use crate::link::Link;

/// Most files recorded; the rest are left for the model to find.
const MAX_FILES: usize = 500;
/// Input files listed in a phase's opening; the rest are counted.
const FILES_SHOWN: usize = 150;

/// Every visible file under the task's folders, one path per line in byte order. PROBE keeps this
/// list in [`START_FILES`]; CHECK lists the folders the same way to find files added since.
pub const LIST_FILES: &str = r#"for d in "$PWD" /app /filesystem; do [ -d "$d" ] && find "$d" -maxdepth 8 -type f -not -path '*/.*' -not -path '*/node_modules/*' 2>/dev/null; done | LC_ALL=C sort -u"#;
/// Where PROBE keeps [`LIST_FILES`]'s output, in the task container.
pub const START_FILES: &str = "/tmp/volund/start-files.txt";

const SCRIPT: &str = r#"tab="$(printf '\t')"
echo '### files'
for d in "$PWD" /app /filesystem; do
  [ -d "$d" ] && find "$d" -maxdepth 6 -type f -not -path '*/.*' -not -path '*/node_modules/*' -printf '%s\t%p\n' 2>/dev/null
done | sort -t "$tab" -k2,2 -u | head -n 500
echo '### tools'
for t in python3 pdftoppm pdftotext libreoffice soffice unzip zip pandoc; do
  command -v "$t" >/dev/null 2>&1 && echo "$t"
done
echo '### python'
python3 -c 'import importlib.util as u; print(" ".join(m for m in ["numpy","scipy","pandas","sympy","openpyxl","docx","pptx","reportlab","matplotlib","ifcopenshell","ezdxf","shapely","pypdf","PyPDF2","pdfplumber","yaml"] if u.find_spec(m)))' 2>/dev/null
echo '### tmp'
if mkdir -p /tmp/volund 2>/dev/null && touch /tmp/volund/.probe 2>/dev/null; then echo writable; else echo read-only; fi
"#;

/// The probe, ending with the file list kept for CHECK (silent: a failed write leaves no list, and
/// CHECK then skips the stray-file check).
fn script() -> String {
    format!("{SCRIPT}{{ {LIST_FILES}; }} 2>/dev/null > {START_FILES} || rm -f {START_FILES}\n")
}

/// Runs the probe and records what it found. Returns a one-line summary for the log.
pub async fn run(link: &Link, world: &mut World) -> String {
    let reply = link
        .request(|id| Outgoing::Exec {
            id,
            command: script(),
            timeout_sec: Some(90),
        })
        .await;
    let output = match reply {
        Ok(r) if r.ok() => r
            .fields
            .get("output")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
        Ok(r) => return format!("probe failed: {}", r.error().unwrap_or("no reason given")),
        Err(e) => return format!("probe not run: {e}"),
    };
    let (files, environment) = parse(&output);
    let summary = format!("{} file(s); {}", files.len(), environment.join("; "));
    world.files = files;
    world.environment = environment;
    summary
}

/// The input files as a phase's opening lists them.
pub fn input_files(world: &World) -> String {
    let mut out = String::from("Input files:\n");
    for f in world.files.iter().take(FILES_SHOWN) {
        out.push_str(&format!("- {} ({} bytes)\n", f.path, f.bytes));
    }
    if world.files.len() > FILES_SHOWN {
        out.push_str(&format!(
            "- ... and {} more\n",
            world.files.len() - FILES_SHOWN
        ));
    }
    out
}

/// Splits the probe's output into input files and facts about the container.
pub fn parse(output: &str) -> (Vec<FileEntry>, Vec<String>) {
    let mut section = "";
    let mut files = Vec::new();
    let (mut tools, mut modules, mut tmp) = (Vec::new(), Vec::new(), None);
    for line in output.lines() {
        if let Some(name) = line.strip_prefix("### ") {
            section = match name.trim() {
                "files" => "files",
                "tools" => "tools",
                "python" => "python",
                "tmp" => "tmp",
                _ => "",
            };
            continue;
        }
        let line = line.trim_end();
        if line.is_empty() {
            continue;
        }
        match section {
            "files" if files.len() < MAX_FILES => {
                if let Some((size, path)) = line.split_once('\t')
                    && let Ok(bytes) = size.trim().parse()
                {
                    files.push(FileEntry {
                        path: path.to_owned(),
                        bytes,
                    });
                }
            }
            "tools" => tools.push(line.trim().to_owned()),
            "python" => modules.extend(line.split_whitespace().map(str::to_owned)),
            "tmp" => tmp = Some(line.trim() == "writable"),
            _ => {}
        }
    }
    let mut environment = Vec::new();
    environment.push(if tools.is_empty() {
        "no python3, pdf or office tools found".to_owned()
    } else {
        format!("tools: {}", tools.join(", "))
    });
    if !modules.is_empty() {
        environment.push(format!("python modules: {}", modules.join(", ")));
    }
    match tmp {
        Some(true) => {
            environment.push("/tmp/volund is writable (scratch work goes there)".to_owned())
        }
        Some(false) => environment.push("/tmp is not writable".to_owned()),
        None => {}
    }
    (files, environment)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_probe_output_becomes_files_and_facts() {
        let out = "### files\n2048\t/app/files/spec.pdf\n17\t/app/task.md\nnot-a-size\t/x\n### tools\npython3\npdftotext\n### python\nnumpy openpyxl docx\n### tmp\nwritable\n";
        let (files, env) = parse(out);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].path, "/app/files/spec.pdf");
        assert_eq!(files[0].bytes, 2048);
        assert_eq!(
            env,
            [
                "tools: python3, pdftotext",
                "python modules: numpy, openpyxl, docx",
                "/tmp/volund is writable (scratch work goes there)"
            ]
        );
        let (files, env) = parse("");
        assert!(files.is_empty());
        assert_eq!(env, ["no python3, pdf or office tools found"]);
    }
}
