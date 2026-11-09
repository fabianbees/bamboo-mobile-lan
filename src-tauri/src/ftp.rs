//! The printer's file store: implicit FTPS on port 990, user `bblp` with the access code.
//!
//! The printer's vsFTPd refuses a data connection that does not resume the control
//! connection's TLS session. suppaftp opens both through the one rustls config passed in
//! here, whose session cache provides the resumption.

use std::path::Path;
use std::time::{Duration, UNIX_EPOCH};

use serde::Serialize;
use suppaftp::list::{File, ListParser};
use suppaftp::tokio::{AsyncRustlsConnector, AsyncRustlsFtpStream};
use suppaftp::types::FileType;
use suppaftp::{FtpError, Status};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::time::timeout;

pub(crate) type Ftp = AsyncRustlsFtpStream;

const PORT: u16 = 990;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const NOOP_TIMEOUT: Duration = Duration::from_secs(5);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const READ_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Serialize)]
pub struct FileEntry {
    pub name: String,
    pub size: u64,
    pub is_dir: bool,
    /// Unix seconds.
    pub modified: i64,
}

impl From<File> for FileEntry {
    fn from(f: File) -> Self {
        let modified = f
            .modified()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64);
        FileEntry {
            name: f.name().to_owned(),
            size: f.size() as u64,
            is_dir: f.is_directory(),
            modified,
        }
    }
}

/// Keeps the open session if it still answers, otherwise connects a new one.
pub(crate) async fn ensure_session(
    slot: &mut Option<Ftp>,
    ip: &str,
    access_code: &str,
) -> Result<(), String> {
    if let Some(ftp) = slot.as_mut() {
        if matches!(timeout(NOOP_TIMEOUT, ftp.noop()).await, Ok(Ok(()))) {
            return Ok(());
        }
    }
    *slot = Some(connect(ip, access_code).await?);
    Ok(())
}

async fn connect(ip: &str, access_code: &str) -> Result<Ftp, String> {
    let connector =
        AsyncRustlsConnector::from(tokio_rustls::TlsConnector::from(crate::lan_tls_config()));
    let mut ftp = timeout(
        CONNECT_TIMEOUT,
        Ftp::connect_secure_implicit((ip, PORT), connector, ip),
    )
    .await
    .map_err(|_| "FTP connect timed out".to_string())?
    .map_err(|e| format!("FTP connect: {e}"))?;
    // Data connections go to the address we connected to, not the one in the PASV reply,
    // which can name another interface of the printer.
    ftp.set_passive_nat_workaround(true);
    ftp.login("bblp", access_code)
        .await
        .map_err(|e| format!("FTP login: {e}"))?;
    ftp.custom_command("PBSZ 0", &[Status::CommandOk])
        .await
        .map_err(|e| format!("FTP PBSZ: {e}"))?;
    ftp.custom_command("PROT P", &[Status::CommandOk])
        .await
        .map_err(|e| format!("FTP PROT: {e}"))?;
    ftp.transfer_type(FileType::Binary)
        .await
        .map_err(|e| format!("FTP TYPE: {e}"))?;
    Ok(ftp)
}

pub(crate) async fn quit(ftp: &mut Ftp) {
    let _ = timeout(NOOP_TIMEOUT, ftp.quit()).await;
}

/// Directories first, then newest first.
pub(crate) async fn list(ftp: &mut Ftp, path: &str) -> Result<Vec<FileEntry>, String> {
    let lines = timeout(COMMAND_TIMEOUT, ftp.list(Some(path)))
        .await
        .map_err(|_| format!("LIST {path}: timed out"))?
        .map_err(|e| format!("LIST {path}: {e}"))?;
    let mut entries: Vec<FileEntry> = lines
        .iter()
        .filter_map(|line| ListParser::parse_posix(line).ok())
        .filter(|f| f.name() != "." && f.name() != "..")
        .map(FileEntry::from)
        .collect();
    entries.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then(b.modified.cmp(&a.modified))
            .then(a.name.cmp(&b.name))
    });
    Ok(entries)
}

/// Up to `limit` bytes of a file, or `None` when the server reports it missing.
pub(crate) async fn read(
    ftp: &mut Ftp,
    path: &str,
    limit: usize,
) -> Result<Option<Vec<u8>>, String> {
    let mut stream = match timeout(COMMAND_TIMEOUT, ftp.retr_as_stream(path)).await {
        Err(_) => return Err(format!("RETR {path}: timed out")),
        Ok(Err(FtpError::UnexpectedResponse(r))) if r.status == Status::FileUnavailable => {
            return Ok(None)
        }
        Ok(Err(e)) => return Err(format!("RETR {path}: {e}")),
        Ok(Ok(stream)) => stream,
    };
    let mut buf = Vec::new();
    if copy(&mut stream, &mut buf, limit).await? {
        stream
            .finish()
            .await
            .map_err(|e| format!("RETR {path}: {e}"))?;
    }
    // A cut-off transfer ends by dropping the stream; suppaftp reads the server's reply before
    // the next command. abort() is no option: it rejects vsFTPd's 225 reply to ABOR.
    Ok(Some(buf))
}

/// Streams a file to `dest`; a partial file is removed on failure.
pub(crate) async fn download(ftp: &mut Ftp, path: &str, dest: &Path) -> Result<(), String> {
    let result = async {
        let mut stream = timeout(COMMAND_TIMEOUT, ftp.retr_as_stream(path))
            .await
            .map_err(|_| format!("RETR {path}: timed out"))?
            .map_err(|e| format!("RETR {path}: {e}"))?;
        let mut file = tokio::fs::File::create(dest)
            .await
            .map_err(|e| format!("{}: {e}", dest.display()))?;
        copy(&mut stream, &mut file, usize::MAX).await?;
        file.flush().await.map_err(|e| e.to_string())?;
        stream
            .finish()
            .await
            .map_err(|e| format!("RETR {path}: {e}"))
    }
    .await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(dest).await;
    }
    result
}

/// Deletes a file, or a directory with everything below it.
pub(crate) async fn remove(ftp: &mut Ftp, path: &str) -> Result<(), String> {
    if ftp.rm(path).await.is_ok() {
        return Ok(());
    }
    let base = path.trim_end_matches('/');
    for child in list(ftp, path).await? {
        Box::pin(remove(ftp, &format!("{base}/{}", child.name))).await?;
    }
    ftp.rmdir(path)
        .await
        .map_err(|e| format!("RMD {path}: {e}"))
}

/// Copies until EOF or `limit` bytes; returns whether EOF was reached.
async fn copy<R, W>(from: &mut R, to: &mut W, limit: usize) -> Result<bool, String>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut chunk = vec![0u8; 64 * 1024];
    let mut copied = 0;
    while copied < limit {
        let n = timeout(READ_TIMEOUT, from.read(&mut chunk))
            .await
            .map_err(|_| "FTP read timed out".to_string())?
            .map_err(|e| format!("FTP read: {e}"))?;
        if n == 0 {
            return Ok(true);
        }
        let n = n.min(limit - copied);
        to.write_all(&chunk[..n]).await.map_err(|e| e.to_string())?;
        copied += n;
    }
    Ok(false)
}
