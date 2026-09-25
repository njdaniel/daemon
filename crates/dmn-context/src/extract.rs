//! Text extraction from PDF, Markdown and plain text.
//!
//! PDF goes through `pdf-extract` (pure Rust, `lopdf` underneath): text-layer only, no OCR, so a
//! scanned PDF comes back as [`ContextError::NoText`]. Markdown is stored as written; the markup
//! is harmless to retrieval and keeps headings readable in Resume Context. Plain text and
//! Markdown must be UTF-8.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;

use crate::error::ContextError;

/// The file formats `dmn sources add` reads, by extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Pdf,
    Markdown,
    Text,
}

impl Format {
    /// The format of `path` from its extension (case-insensitive), or `None` when unsupported.
    pub fn of(path: &Path) -> Option<Self> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        match ext.as_str() {
            "pdf" => Some(Format::Pdf),
            "md" | "markdown" => Some(Format::Markdown),
            "txt" | "text" => Some(Format::Text),
            _ => None,
        }
    }
}

/// The text of the file at `path` and its size in bytes. Refuses unsupported types, unreadable
/// or empty files, PDFs without a text layer and non-UTF-8 text, each with its own
/// [`ContextError`].
pub fn extract(path: &Path) -> Result<(String, u64), ContextError> {
    let format = Format::of(path).ok_or_else(|| ContextError::Unsupported {
        path: path.to_path_buf(),
    })?;
    let bytes = std::fs::read(path).map_err(|source| ContextError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let size = bytes.len() as u64;
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Err(ContextError::Empty {
            path: path.to_path_buf(),
        });
    }
    let text = match format {
        Format::Pdf => pdf_text(path, &bytes)?,
        Format::Markdown | Format::Text => {
            let s = String::from_utf8(bytes).map_err(|_| ContextError::NotUtf8 {
                path: path.to_path_buf(),
            })?;
            s.trim_start_matches('\u{feff}').to_owned()
        }
    };
    if !text.chars().any(char::is_alphanumeric) {
        return Err(match format {
            Format::Pdf => ContextError::NoText {
                path: path.to_path_buf(),
            },
            Format::Markdown | Format::Text => ContextError::Empty {
                path: path.to_path_buf(),
            },
        });
    }
    Ok((text, size))
}

/// `pdf-extract` panics on some malformed files instead of returning an error; a bad PDF must
/// be a refusal with a reason, not a crash of `dmn`.
fn pdf_text(path: &Path, bytes: &[u8]) -> Result<String, ContextError> {
    let result = catch_unwind(AssertUnwindSafe(|| {
        pdf_extract::extract_text_from_mem(bytes)
    }));
    match result {
        Ok(Ok(text)) => Ok(text.replace('\u{0}', "")),
        Ok(Err(e)) => Err(ContextError::Pdf {
            path: path.to_path_buf(),
            reason: e.to_string(),
        }),
        Err(_) => Err(ContextError::Pdf {
            path: path.to_path_buf(),
            reason: "the PDF parser failed on this file".into(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(rel: &str) -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../testdata/context")
            .join(rel)
    }

    #[test]
    fn formats_come_from_the_extension() {
        assert_eq!(Format::of(Path::new("a/b.PDF")), Some(Format::Pdf));
        assert_eq!(Format::of(Path::new("r.md")), Some(Format::Markdown));
        assert_eq!(Format::of(Path::new("r.markdown")), Some(Format::Markdown));
        assert_eq!(Format::of(Path::new("jd.txt")), Some(Format::Text));
        assert_eq!(Format::of(Path::new("r.docx")), None);
        assert_eq!(Format::of(Path::new("README")), None);
    }

    #[test]
    fn the_pdf_markdown_and_text_fixtures_extract() {
        let (pdf, pdf_bytes) = extract(&fixture("resume.pdf")).unwrap();
        assert!(pdf.contains("Northwind Payments"), "{pdf}");
        assert!(pdf.contains("rate limiter"), "{pdf}");
        assert!(pdf_bytes > 1000);
        let (md, _) = extract(&fixture("resume.md")).unwrap();
        assert!(md.contains("Northwind Payments"));
        let (txt, _) = extract(&fixture("job-description.txt")).unwrap();
        assert!(txt.contains("Senior Backend Engineer"));
    }

    #[test]
    fn unsupported_empty_and_broken_files_are_refused_with_a_reason() {
        let dir = tempfile::tempdir().unwrap();
        let write = |name: &str, bytes: &[u8]| {
            let p = dir.path().join(name);
            std::fs::write(&p, bytes).unwrap();
            p
        };
        let cases = [
            (write("photo.png", b"\x89PNG"), "unsupported file type"),
            (write("empty.txt", b""), "empty"),
            (write("blank.md", b"  \n\t\n"), "empty"),
            (write("punctuation.md", "--- *** ---\n".as_bytes()), "empty"),
            (write("latin1.txt", b"caf\xe9 au lait"), "not UTF-8"),
            (
                write("broken.pdf", b"%PDF-1.4\nthis is not a pdf"),
                "cannot read the PDF",
            ),
        ];
        for (path, want) in cases {
            let err = extract(&path).unwrap_err().to_string();
            assert!(err.contains(want), "{}: {err}", path.display());
            assert!(!err.contains('\n'), "one line: {err}");
        }
        let missing = extract(&dir.path().join("gone.md")).unwrap_err();
        assert!(matches!(missing, ContextError::Io { .. }));
    }

    #[test]
    fn a_pdf_without_a_text_layer_is_reported_as_such() {
        // A valid one-page PDF whose page has no text operators, like a scan's image-only page.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("scan.pdf");
        std::fs::write(&path, blank_pdf()).unwrap();
        let err = extract(&path).unwrap_err();
        assert!(matches!(err, ContextError::NoText { .. }), "{err}");
    }

    /// A minimal valid PDF with one empty page, xref offsets computed.
    fn blank_pdf() -> Vec<u8> {
        let objs = [
            "<< /Type /Catalog /Pages 2 0 R >>",
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>",
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R >>",
            "<< /Length 0 >>\nstream\n\nendstream",
        ];
        let mut out = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        for (i, o) in objs.iter().enumerate() {
            offsets.push(out.len());
            out.extend(format!("{} 0 obj\n{o}\nendobj\n", i + 1).bytes());
        }
        let xref = out.len();
        out.extend(format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1).bytes());
        for off in offsets {
            out.extend(format!("{off:010} 00000 n \n").bytes());
        }
        out.extend(
            format!(
                "trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n",
                objs.len() + 1
            )
            .bytes(),
        );
        out
    }
}
