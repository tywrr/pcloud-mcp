//! A coarse kind for each file, from its extension, so audits can say "videos" or "documents"
//! without a list of extensions.

/// Every kind, in the order type breakdowns list them when sizes tie.
pub const KINDS: &[&str] = &[
    "folder",
    "image",
    "video",
    "audio",
    "pdf",
    "document",
    "spreadsheet",
    "presentation",
    "text",
    "code",
    "email",
    "archive",
    "ebook",
    "other",
];

/// The lower-case extension of a file name, without the dot: `photo.JPG` → `jpg`. Names that
/// start with their only dot (`.bashrc`) and implausibly long tails have none.
pub fn extension(name: &str) -> Option<String> {
    let (stem, ext) = name.rsplit_once('.')?;
    if stem.is_empty() || ext.is_empty() || ext.len() > 12 || ext.contains(' ') {
        return None;
    }
    Some(ext.to_lowercase())
}

/// The kind of an entry.
pub fn kind_of(is_dir: bool, ext: Option<&str>) -> &'static str {
    if is_dir {
        return "folder";
    }
    match ext.unwrap_or("") {
        "jpg" | "jpeg" | "jpe" | "jfif" | "png" | "gif" | "webp" | "heic" | "heif" | "avif"
        | "bmp" | "tif" | "tiff" | "svg" | "ico" | "psd" | "raw" | "dng" | "cr2" | "cr3"
        | "nef" | "arw" | "orf" | "rw2" | "raf" => "image",
        "mp4" | "mov" | "m4v" | "mkv" | "avi" | "wmv" | "webm" | "mts" | "m2ts" | "3gp" | "mpg"
        | "mpeg" | "flv" | "vob" => "video",
        "mp3" | "m4a" | "aac" | "flac" | "wav" | "ogg" | "opus" | "wma" | "aiff" | "aif"
        | "alac" | "mid" | "midi" => "audio",
        "pdf" => "pdf",
        "doc" | "docx" | "odt" | "rtf" | "pages" | "wpd" => "document",
        "xls" | "xlsx" | "xlsm" | "ods" | "numbers" | "csv" | "tsv" => "spreadsheet",
        "ppt" | "pptx" | "odp" | "key" => "presentation",
        "txt" | "md" | "markdown" | "rst" | "org" | "log" | "tex" | "nfo" => "text",
        "rs" | "py" | "js" | "mjs" | "ts" | "tsx" | "jsx" | "go" | "c" | "h" | "cc" | "cpp"
        | "hpp" | "java" | "kt" | "swift" | "rb" | "php" | "sh" | "bash" | "zsh" | "ps1"
        | "json" | "yaml" | "yml" | "toml" | "ini" | "xml" | "html" | "htm" | "css" | "scss"
        | "sql" | "ipynb" | "lua" | "pl" | "r" | "cs" => "code",
        "eml" | "mbox" | "msg" | "pst" | "emlx" => "email",
        "zip" | "tar" | "gz" | "tgz" | "bz2" | "xz" | "zst" | "7z" | "rar" | "iso" | "dmg"
        | "img" | "cab" | "lz" | "lzma" => "archive",
        "epub" | "mobi" | "azw" | "azw3" | "djvu" | "fb2" => "ebook",
        _ => "other",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extensions_and_kinds() {
        assert_eq!(extension("IMG_0001.JPG").as_deref(), Some("jpg"));
        assert_eq!(extension("archive.tar.gz").as_deref(), Some("gz"));
        assert_eq!(extension(".bashrc"), None);
        assert_eq!(extension("README"), None);
        assert_eq!(extension("notes."), None);
        assert_eq!(kind_of(false, Some("mov")), "video");
        assert_eq!(kind_of(false, Some("heic")), "image");
        assert_eq!(kind_of(false, None), "other");
        assert_eq!(kind_of(true, Some("jpg")), "folder");
        for kind in KINDS {
            assert!(!kind.is_empty());
        }
    }
}
