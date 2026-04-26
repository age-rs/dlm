use indicatif::ProgressBar;
use percent_encoding::percent_decode_str;
use reqwest::Client;
use reqwest::header::{HeaderMap, RANGE};
use std::path::Path;
use tokio::io::AsyncWriteExt;
use tokio::time::{Duration, timeout};
use tokio::{fs as tfs, select};
use tokio_util::sync::CancellationToken;

use crate::ProgressBarManager;
use crate::client::{ClientConfig, make_client};
use crate::dlm_error::DlmError;
use crate::file_link::FileLink;
use crate::headers::{
    content_disposition_value, content_length_value, content_range_total_size, location_value,
    supports_range_bytes,
};
use crate::utils::pretty_bytes_size;

pub struct DownloadContext<'a> {
    client: Client,
    client_no_redirect: Client,
    connection_timeout_secs: u32,
    output_dir: &'a Path,
    token: &'a CancellationToken,
    pb_manager: &'a ProgressBarManager,
}

impl<'a> DownloadContext<'a> {
    pub fn new(
        client_config: &ClientConfig<'_>,
        output_dir: &'a Path,
        token: &'a CancellationToken,
        pb_manager: &'a ProgressBarManager,
    ) -> Result<Self, DlmError> {
        Ok(Self {
            client: make_client(client_config, true)?,
            client_no_redirect: make_client(client_config, false)?,
            connection_timeout_secs: client_config.connection_timeout_secs,
            output_dir,
            token,
            pb_manager,
        })
    }

    /// Extract download metadata (content-length, range support, disposition filename).
    ///
    /// HEAD first. The disposition filename always comes from HEAD when HEAD
    /// succeeded; otherwise from the ranged-GET probe.
    async fn extract_metadata(
        &self,
        url: &str,
    ) -> Result<(Option<u64>, bool, Option<String>), DlmError> {
        let head = self.client.head(url).send().await?;
        let head_status = head.status();

        // HEAD outright rejected → derive the whole triple from a ranged GET.
        if head_status == reqwest::StatusCode::METHOD_NOT_ALLOWED {
            self.pb_manager.log_above_progress_bars(&format!(
                "HEAD returned 405 for {url}, falling back to GET for metadata"
            ));
            return self.metadata_from_probe(url).await;
        }

        if !head_status.is_success() {
            return Err(DlmError::ResponseStatusNotSuccess {
                status_code: head_status.as_u16(),
            });
        }

        // HEAD succeeded — the disposition filename is taken from it.
        let disposition = content_disposition_value(head.headers()).and_then(parse_filename_header);

        // For length + range support: probe with a ranged GET when HEAD claims
        // `Content-Length: 0` (a sign HEAD is faked); trust HEAD when it
        // reports a real length; give up when no header is present.
        let (length, supports_range) = match content_length_value(head.headers()) {
            Some(0) => self.length_and_range_from_probe(url).await?,
            Some(n) => (Some(n), supports_range_bytes(head.headers())),
            None => (None, false),
        };

        Ok((length, supports_range, disposition))
    }

    /// Full-triple fallback used when HEAD is outright rejected (405).
    /// A failed probe is fatal here because we have no other source of metadata.
    async fn metadata_from_probe(
        &self,
        url: &str,
    ) -> Result<(Option<u64>, bool, Option<String>), DlmError> {
        let probe = self.range_probe(url).await?;
        if !probe.status().is_success() {
            return Err(DlmError::ResponseStatusNotSuccess {
                status_code: probe.status().as_u16(),
            });
        }
        Ok(parse_metadata_from(probe.headers()))
    }

    /// Length + range-support fallback used when HEAD succeeded but reported
    /// `Content-Length: 0`. A failed probe is recoverable: log it and give up
    /// on length/range, keeping the disposition filename from HEAD.
    async fn length_and_range_from_probe(
        &self,
        url: &str,
    ) -> Result<(Option<u64>, bool), DlmError> {
        let probe = self.range_probe(url).await?;
        if !probe.status().is_success() {
            self.pb_manager.log_above_progress_bars(&format!(
                "GET fallback for metadata returned {} for {url}, proceeding without content-length",
                probe.status()
            ));
            return Ok((None, false));
        }
        let (length, supports_range, _) = parse_metadata_from(probe.headers());
        Ok((length, supports_range))
    }

    /// Single-byte ranged GET used to coax metadata out of servers that don't
    /// answer HEAD properly. Returns the raw response for header inspection.
    async fn range_probe(&self, url: &str) -> Result<reqwest::Response, DlmError> {
        Ok(self
            .client
            .get(url)
            .header(RANGE, "bytes=0-0")
            .send()
            .await?)
    }

    pub async fn download_link(
        &self,
        raw_link: &str,
        pb_dl: &ProgressBar,
    ) -> Result<String, DlmError> {
        let file_link = FileLink::new(raw_link)?;

        // When the filename is fully known from the URL, skip the HEAD request if the file exists
        if file_link.extension.is_some() {
            let filename = file_link.filename();
            let final_file_path = self.output_dir.join(&filename);
            if final_file_path.exists() {
                let final_file_size = tfs::metadata(&final_file_path).await?.len();
                let msg = format!(
                    "Skipping {} because the file is already completed [{}]",
                    filename,
                    pretty_bytes_size(final_file_size)
                );
                return Ok(msg);
            }
        }

        // select between stop signal and download
        select! {
            () = self.token.cancelled() => Err(DlmError::ProgramInterrupted),
            dl = self.download(file_link, pb_dl) => dl,
        }
    }

    async fn download(
        &self,
        mut file_link: FileLink,
        pb_dl: &ProgressBar,
    ) -> Result<String, DlmError> {
        // extract metadata with a HEAD request, falling back to GET if needed
        let (content_length, supports_range, disposition_filename) =
            self.extract_metadata(&file_link.url).await?;

        // resolve filename and extension if not already known from the URL
        if file_link.extension.is_none() {
            self.resolve_filename(&mut file_link, disposition_filename)
                .await?;
        }

        let filename = file_link.filename();
        let output_dir = self.output_dir;
        let final_file_path = output_dir.join(&filename);

        // skip completed download (needed for the case where filename was resolved via headers)
        if final_file_path.exists() {
            let final_file_size = tfs::metadata(&final_file_path).await?.len();
            let msg = format!(
                "Skipping {} because the file is already completed [{}]",
                filename,
                pretty_bytes_size(final_file_size)
            );
            return Ok(msg);
        }

        // setup progress bar for the file
        pb_dl.set_message(ProgressBarManager::message_progress_bar(&filename));
        if let Some(total_size) = content_length {
            pb_dl.set_length(total_size);
        }

        let tmp_name = output_dir.join(format!("{filename}.part"));
        let query_range = compute_query_range(
            pb_dl,
            self.pb_manager,
            content_length,
            supports_range,
            &tmp_name,
        )
        .await?;

        // create/open file.part
        // no need for a BufWriter because the HTTP chunks are rather large
        let mut file = match &query_range {
            Some(_range) => {
                tfs::OpenOptions::new()
                    .append(true)
                    .create(false)
                    .open(&tmp_name)
                    .await?
            }
            None => tfs::File::create(&tmp_name).await?,
        };

        // build and send the download request
        let mut request = self.client.get(&file_link.url);
        if let Some(range) = query_range {
            request = request.header(RANGE, range);
        }
        let mut dl_response = request.send().await?;
        if !dl_response.status().is_success() {
            let status_code = dl_response.status().as_u16();
            return Err(DlmError::ResponseStatusNotSuccess { status_code });
        }

        // incremental save chunk by chunk into part file
        let chunk_timeout = Duration::from_secs(u64::from(self.connection_timeout_secs));
        while let Some(chunk) = timeout(chunk_timeout, dl_response.chunk()).await?? {
            file.write_all(&chunk).await?;
            pb_dl.inc(chunk.len() as u64);
        }
        file.flush().await?; // flush buffer → OS
        file.sync_all().await?; // sync OS → disk
        let final_file_size = file.metadata().await?.len();

        // check download complete
        match content_length {
            Some(expected) if final_file_size != expected => {
                return Err(DlmError::IncompleteDownload {
                    expected,
                    actual: final_file_size,
                });
            }
            None => {
                self.pb_manager.log_above_progress_bars(&format!(
                    "No Content-Length available for {}, cannot verify download completeness",
                    filename
                ));
            }
            _ => {}
        }

        // check if the destination already has a finished file
        if tfs::metadata(&final_file_path).await.is_ok() {
            let message = format!(
                "Can't finalize download because the file {} already exists",
                final_file_path.display()
            );
            return Err(DlmError::other(message));
        }

        // rename part file to final
        tfs::rename(&tmp_name, final_file_path).await?;
        let msg = format!(
            "Completed {} [{}]",
            filename,
            pretty_bytes_size(final_file_size)
        );
        Ok(msg)
    }

    /// Resolve filename when the URL does not contain the extension (e.g. redirect).
    /// Mutates the FileLink in place with the resolved extension and filename.
    async fn resolve_filename(
        &self,
        file_link: &mut FileLink,
        disposition_filename: Option<String>,
    ) -> Result<(), DlmError> {
        // try to get the file name from the Content-Disposition header
        if let Some(fh) = disposition_filename {
            let (ext, filename) = FileLink::extract_extension_from_filename(&fh);
            if ext.is_some() {
                file_link.extension = ext;
                file_link.filename_without_extension = filename;
                return Ok(());
            }
            let msg = format!(
                "Could not determine file extension based on header {filename} for {}",
                file_link.url
            );
            self.pb_manager.log_above_progress_bars(&msg);
            return Ok(());
        }

        // check if it is maybe a redirect
        match self
            .compute_filename_from_location_header(&file_link.url)
            .await?
        {
            None => {
                let msg = format!("No extension found for {}", file_link.url);
                self.pb_manager.log_above_progress_bars(&msg);
            }
            Some(fl) => {
                file_link.extension = fl.extension;
                file_link.filename_without_extension = fl.filename_without_extension;
            }
        }
        Ok(())
    }

    async fn compute_filename_from_location_header(
        &self,
        url: &str,
    ) -> Result<Option<FileLink>, DlmError> {
        let head_result = self.client_no_redirect.head(url).send().await?;
        if head_result.status().is_redirection() {
            // https://developer.mozilla.org/en-US/docs/Web/HTTP/Headers/Location
            match location_value(head_result.headers()) {
                None => Ok(None),
                Some(location) => {
                    let fl = FileLink::new(location)?;
                    Ok(Some(fl))
                }
            }
        } else {
            Ok(None)
        }
    }
}

/// Pull `(content-length, supports range, disposition filename)` out of a
/// response's headers. Prefers `Content-Range` total over `Content-Length`
/// because a `Range: bytes=0-0` probe makes `Content-Length` equal to 1.
fn parse_metadata_from(headers: &HeaderMap) -> (Option<u64>, bool, Option<String>) {
    let cl = content_range_total_size(headers).or_else(|| content_length_value(headers));
    let sr = supports_range_bytes(headers);
    let df = content_disposition_value(headers).and_then(parse_filename_header);
    (cl, sr, df)
}

async fn compute_query_range(
    pb_dl: &ProgressBar,
    pb_manager: &ProgressBarManager,
    content_length: Option<u64>,
    supports_range: bool,
    tmp_name: &Path,
) -> Result<Option<String>, DlmError> {
    if tmp_name.exists() {
        // get existing file size
        let tmp_size = tfs::metadata(tmp_name).await?.len();
        match (supports_range, content_length) {
            (true, Some(cl)) => {
                // set the progress bar to the current size
                pb_dl.set_position(tmp_size);
                // reset the elapsed time to avoid showing a really large speed
                pb_dl.reset_elapsed();
                let range_msg = format!("bytes={tmp_size}-{cl}");
                Ok(Some(range_msg))
            }
            _ => {
                let log = format!(
                    "Found part file {} with size {tmp_size} but it will be overridden because the server does not support resuming the download (range bytes)",
                    tmp_name.display()
                );
                pb_manager.log_above_progress_bars(&log);
                pb_dl.set_position(0);
                Ok(None)
            }
        }
    } else if !supports_range {
        let log = format!(
            "The download of file {} should not be interrupted because the server does not support resuming the download (range bytes)",
            tmp_name.display()
        );
        pb_manager.log_above_progress_bars(&log);
        Ok(None)
    } else {
        Ok(None)
    }
}

fn parse_filename_header(content_disposition: &str) -> Option<String> {
    // Try RFC 6266 filename*= (UTF-8 encoded) first, then fall back to filename=
    // e.g. filename*=UTF-8''my%20file.txt
    if let Some(star_value) = find_param(content_disposition, "filename*=") {
        // strip encoding prefix like "UTF-8''" or "utf-8''"
        if let Some((_, name)) = star_value.split_once("''") {
            let decoded = percent_decode_filename(name);
            if !decoded.is_empty() {
                return sanitize_filename(&decoded);
            }
        }
    }
    // Standard filename= parameter (quoted or unquoted)
    if let Some(value) = find_param(content_disposition, "filename=") {
        let unquoted = value
            .strip_prefix('"')
            .and_then(|s| s.strip_suffix('"'))
            .unwrap_or(value);
        if !unquoted.is_empty() {
            return sanitize_filename(unquoted);
        }
    }
    None
}

/// Strip path components to prevent directory traversal attacks.
/// A malicious server could send `Content-Disposition: attachment; filename="../../etc/evil"`.
fn sanitize_filename(name: &str) -> Option<String> {
    // Use Path to extract just the file name, stripping any directory components
    let file_name = Path::new(name).file_name()?.to_str()?;
    if file_name.is_empty() {
        None
    } else {
        Some(file_name.to_string())
    }
}

/// Extract the value of a named parameter from a header value.
/// Handles both `; param=value` and `; param="value"` forms.
fn find_param<'a>(header: &'a str, param: &str) -> Option<&'a str> {
    // Case-insensitive search for the parameter name
    let lower = header.to_ascii_lowercase();
    let param_lower = param.to_ascii_lowercase();
    let idx = lower.find(&param_lower)?;
    let value_start = idx + param.len();
    let rest = &header[value_start..];
    // Value ends at next `;` (or end of string), trimmed
    let value = rest.split(';').next()?.trim();
    if value.is_empty() { None } else { Some(value) }
}

/// Percent-decode a `filename*=` parameter value (lossy on invalid UTF-8).
fn percent_decode_filename(input: &str) -> String {
    percent_decode_str(input).decode_utf8_lossy().into_owned()
}

#[cfg(test)]
mod downloader_tests {
    use crate::downloader::*;

    #[test]
    fn parse_filename_header_quoted() {
        let header = "attachment; filename=\"code-stable-x64-1639562789.tar.gz\"";
        assert_eq!(
            parse_filename_header(header),
            Some("code-stable-x64-1639562789.tar.gz".to_owned())
        );
    }

    #[test]
    fn parse_filename_header_unquoted() {
        let header = "attachment; filename=report.pdf";
        assert_eq!(parse_filename_header(header), Some("report.pdf".to_owned()));
    }

    #[test]
    fn parse_filename_header_inline() {
        let header = "inline; filename=\"preview.png\"";
        assert_eq!(
            parse_filename_header(header),
            Some("preview.png".to_owned())
        );
    }

    #[test]
    fn parse_filename_header_star_utf8() {
        let header = "attachment; filename*=UTF-8''my%20file.txt";
        assert_eq!(
            parse_filename_header(header),
            Some("my file.txt".to_owned())
        );
    }

    #[test]
    fn parse_filename_header_star_takes_precedence() {
        let header = "attachment; filename=\"fallback.txt\"; filename*=UTF-8''preferred.txt";
        assert_eq!(
            parse_filename_header(header),
            Some("preferred.txt".to_owned())
        );
    }

    #[test]
    fn parse_filename_header_no_filename() {
        let header = "attachment";
        assert_eq!(parse_filename_header(header), None);
    }

    #[test]
    fn parse_filename_header_path_traversal() {
        let header = "attachment; filename=\"../../../etc/passwd\"";
        assert_eq!(parse_filename_header(header), Some("passwd".to_owned()));
    }

    #[test]
    fn parse_filename_header_path_traversal_star() {
        let header = "attachment; filename*=UTF-8''..%2F..%2Fevil.txt";
        assert_eq!(parse_filename_header(header), Some("evil.txt".to_owned()));
    }

    #[test]
    fn parse_filename_header_absolute_path() {
        let header = "attachment; filename=\"/tmp/evil.sh\"";
        assert_eq!(parse_filename_header(header), Some("evil.sh".to_owned()));
    }

    #[test]
    fn percent_decode_valid() {
        assert_eq!(percent_decode_filename("my%20file.txt"), "my file.txt");
    }

    #[test]
    fn percent_decode_trailing_percent() {
        assert_eq!(percent_decode_filename("file%"), "file%");
    }

    #[test]
    fn percent_decode_incomplete_sequence() {
        assert_eq!(percent_decode_filename("file%2"), "file%2");
    }

    #[test]
    fn percent_decode_invalid_hex() {
        assert_eq!(percent_decode_filename("file%GG"), "file%GG");
    }
}
