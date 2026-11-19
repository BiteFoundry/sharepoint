//! Shared file/media safety helpers (filename sanitization, deduplication, Bearer origin guard,
//! MIME guessing, image format sniffing).

use std::path::{Path, PathBuf};

use uuid::Uuid;

pub fn should_send_bearer(graph_base_url: &str, target_url: &str) -> bool {
    let trimmed = target_url.trim();
    if trimmed.starts_with('/') && !trimmed.starts_with("//") {
        return true;
    }
    if !trimmed.starts_with("http://") && !trimmed.starts_with("https://") {
        return !trimmed.starts_with("//") && !trimmed.contains("://") && !trimmed.is_empty();
    }
    let Ok(target) = url::Url::parse(trimmed) else {
        return false;
    };
    let Some(target_host) = target.host_str() else {
        return false;
    };
    if let Ok(base) = url::Url::parse(graph_base_url.trim()) {
        if let Some(base_host) = base.host_str() {
            if target.scheme() == base.scheme()
                && target.port_or_known_default() == base.port_or_known_default()
            {
                if target_host.eq_ignore_ascii_case(base_host) {
                    return true;
                }
                let is_loopback = |h: &str| {
                    h.eq_ignore_ascii_case("127.0.0.1") || h.eq_ignore_ascii_case("localhost")
                };
                if is_loopback(base_host) && is_loopback(target_host) {
                    return true;
                }
            }
        }
    }
    if target.scheme() == "https" {
        let lower = target_host.to_ascii_lowercase();
        if lower == "microsoft.com" || lower.ends_with(".microsoft.com") {
            return true;
        }
    }
    false
}

pub fn sanitize_filename(raw: &str) -> String {
    let leaf = Path::new(raw)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(raw);
    let mut cleaned: String = leaf
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    cleaned = cleaned.trim().trim_matches('.').to_string();
    if cleaned.is_empty() || cleaned == "." || cleaned == ".." {
        "download".to_string()
    } else {
        cleaned
    }
}

pub fn deduplicate_dest_path(dest_dir: &Path, safe_name: &str) -> PathBuf {
    let candidate = dest_dir.join(safe_name);
    if !candidate.exists() {
        return candidate;
    }

    let p = Path::new(safe_name);
    let stem = p
        .file_stem()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("file");
    let ext = p.extension().and_then(|s| s.to_str());

    for idx in 1..10_000 {
        let next_name = match ext {
            Some(e) if !e.is_empty() => format!("{stem} ({idx}).{e}"),
            _ => format!("{stem} ({idx})"),
        };
        let next_path = dest_dir.join(&next_name);
        if !next_path.exists() {
            return next_path;
        }
    }
    dest_dir.join(format!("{stem}_{}", Uuid::new_v4()))
}

pub fn guess_mime_type(path: &Path) -> &'static str {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|s| s.to_ascii_lowercase());
    match ext.as_deref() {
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("svg") => "image/svg+xml",
        Some("pdf") => "application/pdf",
        Some("txt") | Some("log") => "text/plain",
        Some("csv") => "text/csv",
        Some("html") | Some("htm") => "text/html",
        Some("md") | Some("markdown") => "text/markdown",
        Some("json") | Some("jsonl") => "application/json",
        Some("yaml") | Some("yml") => "application/yaml",
        Some("toml") => "application/toml",
        Some("xml") => "application/xml",
        Some("zip") => "application/zip",
        Some("gz") => "application/gzip",
        Some("tar") => "application/x-tar",
        Some("doc") => "application/msword",
        Some("docx") => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        Some("xls") => "application/vnd.ms-excel",
        Some("xlsx") => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        Some("ppt") => "application/vnd.ms-powerpoint",
        Some("pptx") => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        Some("mp3") => "audio/mpeg",
        Some("wav") => "audio/wav",
        Some("mp4") => "video/mp4",
        _ => "application/octet-stream",
    }
}

pub fn sniff_image_extension(bytes: &[u8]) -> Option<&'static str> {
    if bytes.len() < 3 {
        return None;
    }
    match image::guess_format(bytes).ok()? {
        image::ImageFormat::Png => Some("png"),
        image::ImageFormat::Jpeg => Some("jpg"),
        image::ImageFormat::Gif => Some("gif"),
        image::ImageFormat::WebP => Some("webp"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_sanitize_and_deduplicate_filename() {
        assert_eq!(sanitize_filename("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_filename("re:po*rt?.pdf"), "re_po_rt_.pdf");
        assert_eq!(sanitize_filename(".."), "download");

        let dir = tempdir().unwrap();
        let first = deduplicate_dest_path(dir.path(), "report.pdf");
        assert_eq!(first.file_name().unwrap(), "report.pdf");
        std::fs::write(&first, b"1").unwrap();

        let second = deduplicate_dest_path(dir.path(), "report.pdf");
        assert_eq!(second.file_name().unwrap(), "report (1).pdf");
    }

    #[test]
    fn test_should_send_bearer_origin_guard() {
        let base = "https://graph.microsoft.com/v1.0";
        assert!(should_send_bearer(base, "/me/drive/root/children"));
        assert!(should_send_bearer(
            base,
            "https://graph.microsoft.com/v1.0/sites"
        ));
        assert!(!should_send_bearer(base, "https://evil.example.com/steal"));
    }

    #[test]
    fn test_sniff_image_extension_and_guess_mime_type() {
        let png_hdr = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        assert_eq!(sniff_image_extension(&png_hdr), Some("png"));
        assert_eq!(guess_mime_type(Path::new("photo.PNG")), "image/png");
        assert_eq!(guess_mime_type(Path::new("notes.md")), "text/markdown");
    }
}
