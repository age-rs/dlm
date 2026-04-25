use crate::dlm_error::DlmError;
use percent_encoding::percent_decode_str;

#[derive(Debug)]
pub struct FileLink {
    pub url: String,
    pub filename_without_extension: String,
    pub extension: Option<String>,
}

impl FileLink {
    pub fn new(url: &str) -> Result<Self, DlmError> {
        let trimmed = url.trim();
        if trimmed.is_empty() {
            Err(DlmError::other(
                "FileLink cannot be built from an empty URL".to_string(),
            ))
        } else if trimmed.ends_with('/') {
            let message = format!("FileLink cannot be built with an invalid extension '{trimmed}'");
            Err(DlmError::other(message))
        } else {
            // Split BEFORE decoding so an encoded '/' (%2F) inside a path
            // segment is not mistaken for a separator.
            let last_segment = url.rsplit_once('/').map_or(url, |(_, s)| s);
            let decoded = percent_decode_str(last_segment).decode_utf8_lossy();
            // A decoded segment may still contain '/' or '\\' (from %2F / %5C);
            // those are path separators on disk, replace them.
            let safe_name = decoded.replace(['/', '\\'], "_");
            let (extension, filename_without_extension) =
                Self::extract_extension_from_filename(&safe_name);

            let file_link = Self {
                url: url.to_string(),
                filename_without_extension,
                extension,
            };
            Ok(file_link)
        }
    }

    pub fn filename(&self) -> String {
        match &self.extension {
            Some(ext) => format!("{}.{ext}", self.filename_without_extension),
            None => self.filename_without_extension.clone(),
        }
    }

    pub fn extract_extension_from_filename(filename: &str) -> (Option<String>, String) {
        if let Some((before, after)) = filename.rsplit_once('.') {
            // remove potential query params from extension
            let ext = after.split('?').next().unwrap_or(after);
            (Some(ext.to_string()), before.to_string())
        } else {
            // no extension found, the file name will be used
            // sanitize as it contains query params
            // which are not allowed in filenames on some OS
            let sanitized = filename.replace(['?', '&'], "-");
            (None, sanitized)
        }
    }
}

#[cfg(test)]
mod file_link_tests {
    use crate::file_link::*;

    #[test]
    fn no_empty_string() {
        match FileLink::new("") {
            Err(DlmError::Other { message }) => assert_eq!(
                message,
                "FileLink cannot be built from an empty URL".to_string()
            ),
            other => panic!("expected error, got {other:?}"),
        }
    }

    #[test]
    fn happy_case() {
        let url = "https://www.google.com/area51.txt";
        match FileLink::new(url) {
            Ok(fl) => {
                assert_eq!(fl.url, url);
                assert_eq!(fl.filename_without_extension, "area51".to_string());
                assert_eq!(fl.extension, Some("txt".to_string()));
            }
            other => panic!("unexpected error, got {other:?}"),
        }
    }

    #[test]
    fn trailing_slash() {
        let url = "https://www.google.com/area51/";
        match FileLink::new(url) {
            Err(DlmError::Other { message }) => assert_eq!(
                message,
                "FileLink cannot be built with an invalid extension 'https://www.google.com/area51/'".to_string()
            ),
            other => panic!("expected error, got {other:?}"),
        }
    }

    #[test]
    fn no_extension() {
        let url = "https://www.google.com/area51";
        let fl = FileLink::new(url).unwrap();
        assert_eq!(fl.extension, None);
        assert_eq!(fl.filename_without_extension, "area51");
        assert_eq!(fl.url, url);
    }

    #[test]
    fn no_extension_use_query_params() {
        let url = "https://oeis.org/search?q=id:A000001&fmt=json";
        let fl = FileLink::new(url).unwrap();
        assert_eq!(fl.extension, None);
        assert_eq!(
            fl.filename_without_extension,
            "search-q=id:A000001-fmt=json"
        );
        assert_eq!(fl.url, url);
    }

    #[test]
    fn filename_with_extension() {
        let fl = FileLink::new("https://example.com/area51.txt").unwrap();
        assert_eq!(fl.filename(), "area51.txt");
    }

    #[test]
    fn filename_without_extension() {
        let fl = FileLink::new("https://example.com/area51").unwrap();
        assert_eq!(fl.filename(), "area51");
    }

    #[test]
    fn extract_extension_ok() {
        let (ext, filename) = FileLink::extract_extension_from_filename("area51.txt");
        assert_eq!(filename, "area51");
        assert_eq!(ext, Some("txt".to_string()));
    }

    #[test]
    fn extract_extension_with_query_param() {
        let url = "https://releases.ubuntu.com/21.10/ubuntu-21.10-desktop-amd64.iso?id=123";
        let fl = FileLink::new(url).unwrap();
        assert_eq!(fl.extension, Some("iso".to_string()));
        assert_eq!(fl.filename_without_extension, "ubuntu-21.10-desktop-amd64");
        assert_eq!(fl.url, url);
    }

    #[test]
    fn extract_extension_with_query_param_bis() {
        let url = "https://atom-installer.github.com/v1.58.0/atom-amd64.deb?s=1627025597&ext=.deb";
        let fl = FileLink::new(url).unwrap();
        assert_eq!(fl.extension, Some("deb".to_string()));
        assert_eq!(fl.url, url);
        // FIXME
        //assert_eq!(fl.filename_without_extension, "atom-amd64");
    }

    #[test]
    fn extract_extension_with_parts() {
        let url = "https://www.google.com/area51/alien-archive.tar.00";
        let fl = FileLink::new(url).unwrap();
        assert_eq!(fl.extension, Some("00".to_string()));
        // TODO fix this - should be alien-archive.tar.00 or parts will collide on tmp file
        assert_eq!(fl.filename_without_extension, "alien-archive.tar");
        assert_eq!(fl.url, url);
    }

    #[test]
    fn percent_encoded_space_in_filename() {
        let url = "https://example.com/path/My%20Report.pdf";
        let fl = FileLink::new(url).unwrap();
        assert_eq!(fl.extension, Some("pdf".to_string()));
        assert_eq!(fl.filename_without_extension, "My Report");
    }

    #[test]
    fn encoded_slash_in_segment_is_not_a_separator() {
        // %2F is a literal '/' inside a single path segment — must not split on it.
        let url = "https://example.com/files/My%2FReport.pdf";
        let fl = FileLink::new(url).unwrap();
        assert_eq!(fl.extension, Some("pdf".to_string()));
        // '/' is sanitized to '_' so the name remains usable on disk.
        assert_eq!(fl.filename_without_extension, "My_Report");
    }
}
