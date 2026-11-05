//! Bounded stdout/stderr logging for the device service (2 MiB + one 2 MiB archive).
use std::{
    fs::{self, File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
};

#[cfg(target_os = "linux")]
const LOG_LIMIT: u64 = 2 * 1024 * 1024;

#[cfg(target_os = "linux")]
pub fn init() -> io::Result<()> {
    use std::os::fd::{AsRawFd, FromRawFd};
    let path = std::env::current_exe()?.with_file_name("server.log");
    let mut log = BoundedLog::open(path, LOG_LIMIT)?;
    let mut fds = [0; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } == -1 {
        return Err(io::Error::last_os_error());
    }
    let mut reader = unsafe { File::from_raw_fd(fds[0]) };
    let writer = unsafe { File::from_raw_fd(fds[1]) };
    std::thread::Builder::new()
        .name("bounded-log".into())
        .spawn(move || {
            let mut buffer = [0; 8192];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(count) => {
                        // Keep draining if storage fails: logging must not block audio.
                        let _ = log.append(&buffer[..count]);
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
        })?;
    for fd in [libc::STDOUT_FILENO, libc::STDERR_FILENO] {
        if unsafe { libc::dup2(writer.as_raw_fd(), fd) } == -1 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

struct BoundedLog {
    path: PathBuf,
    file: File,
    size: u64,
    limit: u64,
}

impl BoundedLog {
    fn open(path: PathBuf, limit: u64) -> io::Result<Self> {
        assert!(limit > 0);
        trim_existing(&path, limit)?;
        trim_existing(&archive_path(&path), limit)?;
        let file = open_log(&path)?;
        let size = file.metadata()?.len();
        Ok(Self {
            path,
            file,
            size,
            limit,
        })
    }

    fn append(&mut self, mut bytes: &[u8]) -> io::Result<()> {
        while !bytes.is_empty() {
            if self.size >= self.limit {
                self.file.flush()?;
                fs::rename(&self.path, archive_path(&self.path))?;
                self.file = open_log(&self.path)?;
                self.size = 0;
            }
            let count = bytes.len().min((self.limit - self.size) as usize);
            self.file.write_all(&bytes[..count])?;
            self.size += count as u64;
            bytes = &bytes[count..];
        }
        Ok(())
    }
}

fn open_log(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
}

fn archive_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(".1");
    PathBuf::from(name)
}

fn trim_existing(path: &Path, limit: u64) -> io::Result<()> {
    let mut file = match OpenOptions::new().read(true).write(true).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if file.metadata()?.len() > limit {
        file.seek(SeekFrom::End(-(limit as i64)))?;
        let mut tail = Vec::with_capacity(limit as usize);
        file.read_to_end(&mut tail)?;
        file.seek(SeekFrom::Start(0))?;
        file.write_all(&tail)?;
        file.set_len(tail.len() as u64)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounds_existing_logs_and_retains_newest_output_across_rotations() {
        let dir = std::env::temp_dir().join(format!("gpt-alice-log-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&dir).unwrap();
        let path = dir.join("server.log");
        fs::write(&path, vec![b'x'; 1000]).unwrap();
        fs::write(archive_path(&path), vec![b'y'; 1000]).unwrap();
        let mut log = BoundedLog::open(path.clone(), 64).unwrap();
        assert_eq!(fs::metadata(&path).unwrap().len(), 64);
        assert_eq!(fs::metadata(archive_path(&path)).unwrap().len(), 64);
        let data: Vec<u8> = (0..200).collect();
        log.append(&data).unwrap();
        let mut retained = fs::read(archive_path(&path)).unwrap();
        retained.extend(fs::read(&path).unwrap());
        assert_eq!(retained, data[128..]);
        assert!(fs::metadata(&path).unwrap().len() <= 64);
        drop(log);
        fs::remove_dir_all(dir).unwrap();
    }
}
