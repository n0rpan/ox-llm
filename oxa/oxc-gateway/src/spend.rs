//! Incremental daily spend. An unfinished final line is not a committed row.
//! Complete malformed rows fail closed. Replacement/truncation resets the cache.
use serde_json::Value;
use std::{
    fs::OpenOptions,
    io::{self, BufRead, BufReader, Read, Seek, SeekFrom},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

#[derive(Default, Debug)]
pub struct SpendReader {
    path: PathBuf,
    identity: Option<(u64, u64)>,
    offset: u64,
    total: f64,
    tail: Vec<u8>,
    bytes_read: u64,
}
impl SpendReader {
    pub fn bytes_read(&self) -> u64 {
        self.bytes_read
    }
    pub fn read(&mut self, path: &Path) -> io::Result<f64> {
        let mut file = match OpenOptions::new()
            .read(true)
            .custom_flags(nix::libc::O_CLOEXEC | nix::libc::O_NONBLOCK)
            .open(path)
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                self.identity = None;
                self.offset = 0;
                self.total = 0.0;
                self.tail.clear();
                self.path = path.into();
                return Ok(0.0);
            }
            Err(error) => return Err(error),
        };
        let info = file.metadata()?;
        if !info.is_file() {
            return Err(io::ErrorKind::InvalidData.into());
        }
        let identity = (info.dev(), info.ino());
        if self.path != path || self.identity != Some(identity) || info.len() < self.offset {
            self.path = path.into();
            self.identity = Some(identity);
            self.offset = 0;
            self.total = 0.0;
            self.tail.clear();
        } else if self.offset > 0 {
            // A same-inode rewrite can be at least as long as the cached file.
            // Verify a bounded tail, like the reference, before reusing totals.
            file.seek(SeekFrom::Start(self.offset - self.tail.len() as u64))?;
            let mut tail = vec![0; self.tail.len()];
            file.read_exact(&mut tail)?;
            self.bytes_read += tail.len() as u64;
            if tail != self.tail {
                self.offset = 0;
                self.total = 0.0;
                self.tail.clear();
            }
        }
        let mut reader = BufReader::new(file);
        reader.seek(SeekFrom::Start(self.offset))?;
        let mut offset = self.offset;
        let mut total = self.total;
        let mut tail = self.tail.clone();
        let mut line = Vec::new();
        loop {
            line.clear();
            let count = reader.read_until(b'\n', &mut line)?;
            self.bytes_read += count as u64;
            if count == 0 || !line.ends_with(b"\n") {
                break;
            }
            offset += count as u64;
            tail.extend_from_slice(&line);
            if tail.len() > 64 {
                tail.drain(..tail.len() - 64);
            }
            if line.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            let row: Value =
                serde_json::from_slice(&line).map_err(|_| io::Error::from(io::ErrorKind::InvalidData))?;
            if !row.is_object() {
                return Err(io::ErrorKind::InvalidData.into());
            }
            if row["remote"] != true {
                continue;
            }
            let cost = row["cost_usd"]
                .as_f64()
                .filter(|cost| cost.is_finite() && *cost >= 0.0)
                .ok_or(io::ErrorKind::InvalidData)?;
            total += cost;
            if !total.is_finite() {
                return Err(io::ErrorKind::InvalidData.into());
            }
        }
        // Commit the cursor/total together only after every complete row passed.
        self.offset = offset;
        self.total = total;
        self.tail = tail;
        Ok(if total < f64::MAX / 1e8 {
            (total * 1e8).round_ties_even() / 1e8
        } else {
            total
        })
    }
}

/// Serialize complete private rows across processes. Durable append must precede
/// releasing a persisted reservation. Readers tolerate an unfinished last write.
pub fn append_private(path: &Path, row: &Value) -> io::Result<()> {
    use std::{
        fs::{self, OpenOptions},
        io::Write,
        os::unix::fs::PermissionsExt,
    };
    let parent = path.parent().ok_or(io::ErrorKind::InvalidInput)?;
    fs::create_dir_all(parent)?;
    fs::set_permissions(parent, fs::Permissions::from_mode(0o700))?;
    let file = OpenOptions::new()
        .read(true)
        .append(true)
        .create(true)
        .mode(0o600)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_CLOEXEC | nix::libc::O_NONBLOCK)
        .open(path)?;
    let info = file.metadata()?;
    if !info.is_file() || info.uid() != nix::unistd::Uid::current().as_raw() {
        return Err(io::ErrorKind::PermissionDenied.into());
    }
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    let mut file = nix::fcntl::Flock::lock(file, nix::fcntl::FlockArg::LockExclusive)
        .map_err(|(_, error)| io::Error::from(error))?;
    // A failed/dead writer may have left an unfinished row. Preserve those
    // bytes, but keep this healthy settlement on its own parseable line.
    if file.metadata()?.len() > 0 {
        file.seek(SeekFrom::End(-1))?;
        let mut last = [0];
        file.read_exact(&mut last)?;
        if last != [b'\n'] {
            file.write_all(b"\n")?;
        }
    }
    let mut bytes = serde_json::to_vec(row)?;
    bytes.push(b'\n');
    file.write_all(&bytes)?;
    file.sync_data()?;
    // The first row's name must survive as well as its contents before the
    // accounting transaction can durably remove the corresponding hold.
    fs::File::open(parent)?.sync_all()
}
