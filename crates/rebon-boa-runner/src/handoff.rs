use std::fs::File;
use std::io;
use std::time::{Duration, Instant};

pub(crate) const MAGIC: [u8; 8] = *b"RBBOA001";
pub(crate) const LAYOUT_VERSION: u16 = 1;
pub(crate) const HEADER_LEN: usize = 64;
pub(crate) const MAX_REQUEST: usize = 64 * 1024;
pub(crate) const MAX_RESPONSE: usize = 8 * 1024;
pub(crate) const GATE_OFFSET: u64 = 12;
pub(crate) const STATE_COMPLETE: u32 = 1;
const GATE_GO: u32 = 1;
const STARTUP_LIMIT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy)]
pub(crate) struct Layout {
    pub(crate) total_capacity: u64,
    pub(crate) request_offset: u64,
    pub(crate) request_len: u32,
    pub(crate) request_capacity: u32,
    pub(crate) response_offset: u64,
    pub(crate) response_capacity: u32,
}

impl Layout {
    pub(crate) fn new(
        request_len: usize,
        request_capacity: usize,
        response_capacity: usize,
    ) -> io::Result<Self> {
        if request_len > request_capacity
            || request_capacity > MAX_REQUEST
            || response_capacity > MAX_RESPONSE
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "handoff capacity exceeds fixed limits",
            ));
        }
        let request_offset = u64::try_from(HEADER_LEN).map_err(invalid)?;
        let request_capacity_u64 = u64::try_from(request_capacity).map_err(invalid)?;
        let response_offset = request_offset
            .checked_add(request_capacity_u64)
            .ok_or_else(overflow)?;
        let total_capacity = response_offset
            .checked_add(u64::try_from(response_capacity).map_err(invalid)?)
            .ok_or_else(overflow)?;
        Ok(Self {
            total_capacity,
            request_offset,
            request_len: u32::try_from(request_len).map_err(invalid)?,
            request_capacity: u32::try_from(request_capacity).map_err(invalid)?,
            response_offset,
            response_capacity: u32::try_from(response_capacity).map_err(invalid)?,
        })
    }

    fn encode(self) -> [u8; HEADER_LEN] {
        let mut bytes = [0_u8; HEADER_LEN];
        bytes[0..8].copy_from_slice(&MAGIC);
        bytes[8..10].copy_from_slice(&LAYOUT_VERSION.to_le_bytes());
        bytes[10..12].copy_from_slice(&(HEADER_LEN as u16).to_le_bytes());
        bytes[16..24].copy_from_slice(&self.total_capacity.to_le_bytes());
        bytes[24..32].copy_from_slice(&self.request_offset.to_le_bytes());
        bytes[32..36].copy_from_slice(&self.request_len.to_le_bytes());
        bytes[36..40].copy_from_slice(&self.request_capacity.to_le_bytes());
        bytes[40..48].copy_from_slice(&self.response_offset.to_le_bytes());
        bytes[48..52].copy_from_slice(&self.response_capacity.to_le_bytes());
        bytes
    }

    fn decode(bytes: &[u8; HEADER_LEN], file_len: u64) -> io::Result<Self> {
        if bytes[0..8] != MAGIC
            || u16_at(bytes, 8) != LAYOUT_VERSION
            || usize::from(u16_at(bytes, 10)) != HEADER_LEN
        {
            return Err(protocol(
                "handoff magic, version, or header length mismatch",
            ));
        }
        let layout = Self {
            total_capacity: u64_at(bytes, 16),
            request_offset: u64_at(bytes, 24),
            request_len: u32_at(bytes, 32),
            request_capacity: u32_at(bytes, 36),
            response_offset: u64_at(bytes, 40),
            response_capacity: u32_at(bytes, 48),
        };
        let canonical = Self::new(
            layout.request_len as usize,
            layout.request_capacity as usize,
            layout.response_capacity as usize,
        )?;
        if layout.total_capacity != file_len
            || layout.total_capacity != canonical.total_capacity
            || layout.request_offset != canonical.request_offset
            || layout.response_offset != canonical.response_offset
        {
            return Err(protocol("handoff offsets or file capacity are invalid"));
        }
        Ok(layout)
    }
}

pub(crate) struct ParentHandoff {
    file: File,
}

impl ParentHandoff {
    pub(crate) fn create(
        request: &[u8],
        request_capacity: usize,
        response_capacity: usize,
    ) -> io::Result<Self> {
        let layout = Layout::new(request.len(), request_capacity, response_capacity)?;
        let file = tempfile::tempfile()?;
        file.set_len(layout.total_capacity)?;
        write_all_at(&file, &layout.encode(), 0)?;
        write_all_at(&file, request, layout.request_offset)?;
        file.sync_data()?;
        Ok(Self { file })
    }

    pub(crate) fn child_file(&self) -> io::Result<File> {
        self.file.try_clone()
    }

    pub(crate) fn admit(&self) -> io::Result<()> {
        // The request/header durability flush happens in `create`, before spawn. GO is the final
        // admission operation: once any part of this positioned write becomes visible, the
        // already-armed independent deadline watchdog remains able to kill the containment unit.
        write_all_at(&self.file, &GATE_GO.to_le_bytes(), GATE_OFFSET)
    }

    pub(crate) fn read_response(&self) -> io::Result<Vec<u8>> {
        let mut header = [0_u8; HEADER_LEN];
        read_exact_at(&self.file, &mut header, 0)?;
        let layout = Layout::decode(&header, self.file.metadata()?.len())?;
        let response_len = u32_at(&header, 52) as usize;
        let state = u32_at(&header, 56);
        let checksum = u32_at(&header, 60);
        if state != STATE_COMPLETE {
            return Err(protocol("response terminal marker is absent"));
        }
        if response_len > layout.response_capacity as usize {
            return Err(protocol("response length exceeds response capacity"));
        }
        let mut response = vec![0; response_len];
        read_exact_at(&self.file, &mut response, layout.response_offset)?;
        if checksum32(&response) != checksum {
            return Err(protocol("response checksum mismatch"));
        }
        Ok(response)
    }
}

#[cfg(feature = "adversarial-fixtures")]
pub(crate) enum FixtureCorruption {
    MissingCompletion,
    BadChecksum,
    OversizedLength,
    InvalidLayout,
    GrownFile,
    MalformedJson,
    WrongProtocolVersion,
}

pub(crate) struct ChildHandoff {
    file: File,
    layout: Layout,
}

impl ChildHandoff {
    pub(crate) fn open_stdin() -> io::Result<Self> {
        let file = duplicate_stdin()?;
        let deadline = Instant::now() + STARTUP_LIMIT;
        loop {
            let mut gate = [0_u8; 4];
            read_exact_at(&file, &mut gate, GATE_OFFSET)?;
            if u32::from_le_bytes(gate) == GATE_GO {
                break;
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "handoff admission gate timed out",
                ));
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        let mut header = [0_u8; HEADER_LEN];
        read_exact_at(&file, &mut header, 0)?;
        let layout = Layout::decode(&header, file.metadata()?.len())?;
        Ok(Self { file, layout })
    }

    pub(crate) fn read_request(&self) -> io::Result<Vec<u8>> {
        let mut request = vec![0; self.layout.request_len as usize];
        read_exact_at(&self.file, &mut request, self.layout.request_offset)?;
        let unused = self.layout.request_capacity as usize - request.len();
        if unused != 0 {
            let mut trailing = vec![0; unused];
            read_exact_at(
                &self.file,
                &mut trailing,
                self.layout.request_offset + request.len() as u64,
            )?;
            if trailing.iter().any(|byte| *byte != 0) {
                return Err(protocol("request slot has nonzero trailing bytes"));
            }
        }
        Ok(request)
    }

    pub(crate) fn write_response(&self, response: &[u8]) -> io::Result<()> {
        if response.len() > self.layout.response_capacity as usize {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "response exceeds handoff capacity",
            ));
        }
        write_all_at(&self.file, response, self.layout.response_offset)?;
        self.file.sync_data()?;
        write_all_at(&self.file, &(response.len() as u32).to_le_bytes(), 52)?;
        write_all_at(&self.file, &checksum32(response).to_le_bytes(), 60)?;
        write_all_at(&self.file, &STATE_COMPLETE.to_le_bytes(), 56)?;
        self.file.sync_data()
    }

    #[cfg(feature = "adversarial-fixtures")]
    pub(crate) fn corrupt_response(&self, corruption: FixtureCorruption) -> io::Result<()> {
        let valid = br#"{"protocol_version":2,"body":{"status":"ok","value":1,"logs":[]}}"#;
        match corruption {
            FixtureCorruption::MissingCompletion => {
                write_all_at(&self.file, valid, self.layout.response_offset)?;
                write_all_at(&self.file, &(valid.len() as u32).to_le_bytes(), 52)?;
                write_all_at(&self.file, &checksum32(valid).to_le_bytes(), 60)?;
                self.file.sync_data()
            }
            FixtureCorruption::BadChecksum => {
                write_all_at(&self.file, valid, self.layout.response_offset)?;
                write_all_at(&self.file, &(valid.len() as u32).to_le_bytes(), 52)?;
                write_all_at(
                    &self.file,
                    &checksum32(valid).wrapping_add(1).to_le_bytes(),
                    60,
                )?;
                write_all_at(&self.file, &STATE_COMPLETE.to_le_bytes(), 56)?;
                self.file.sync_data()
            }
            FixtureCorruption::OversizedLength => {
                let length = self.layout.response_capacity.saturating_add(1);
                write_all_at(&self.file, &length.to_le_bytes(), 52)?;
                write_all_at(&self.file, &STATE_COMPLETE.to_le_bytes(), 56)?;
                self.file.sync_data()
            }
            FixtureCorruption::InvalidLayout => {
                write_all_at(&self.file, &u64::MAX.to_le_bytes(), 40)?;
                self.file.sync_data()
            }
            FixtureCorruption::GrownFile => {
                self.file
                    .set_len(self.layout.total_capacity.saturating_add(1))?;
                self.file.sync_data()
            }
            FixtureCorruption::MalformedJson => self.write_response(b"not-json"),
            FixtureCorruption::WrongProtocolVersion => self.write_response(
                br#"{"protocol_version":1,"body":{"status":"ok","value":1,"logs":[]}}"#,
            ),
        }
    }
}

pub(crate) fn checksum32(bytes: &[u8]) -> u32 {
    bytes.iter().fold(2_166_136_261_u32, |hash, byte| {
        (hash ^ u32::from(*byte)).wrapping_mul(16_777_619)
    })
}

fn u16_at(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(bytes[offset..offset + 2].try_into().expect("field width"))
}
fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().expect("field width"))
}
fn u64_at(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().expect("field width"))
}
fn invalid(_: std::num::TryFromIntError) -> io::Error {
    overflow()
}
fn overflow() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "handoff layout arithmetic overflow",
    )
}
fn protocol(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(unix)]
fn read_exact_at(file: &File, mut bytes: &mut [u8], mut offset: u64) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    while !bytes.is_empty() {
        let read = file.read_at(bytes, offset)?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "short handoff read",
            ));
        }
        offset = offset.checked_add(read as u64).ok_or_else(overflow)?;
        bytes = &mut bytes[read..];
    }
    Ok(())
}

#[cfg(windows)]
fn read_exact_at(file: &File, mut bytes: &mut [u8], mut offset: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !bytes.is_empty() {
        let read = file.seek_read(bytes, offset)?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "short handoff read",
            ));
        }
        offset = offset.checked_add(read as u64).ok_or_else(overflow)?;
        bytes = &mut bytes[read..];
    }
    Ok(())
}

#[cfg(unix)]
fn write_all_at(file: &File, mut bytes: &[u8], mut offset: u64) -> io::Result<()> {
    use std::os::unix::fs::FileExt;
    while !bytes.is_empty() {
        let written = file.write_at(bytes, offset)?;
        if written == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "short handoff write",
            ));
        }
        offset = offset.checked_add(written as u64).ok_or_else(overflow)?;
        bytes = &bytes[written..];
    }
    Ok(())
}

#[cfg(windows)]
fn write_all_at(file: &File, mut bytes: &[u8], mut offset: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;
    while !bytes.is_empty() {
        let written = file.seek_write(bytes, offset)?;
        if written == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WriteZero,
                "short handoff write",
            ));
        }
        offset = offset.checked_add(written as u64).ok_or_else(overflow)?;
        bytes = &bytes[written..];
    }
    Ok(())
}

#[cfg(unix)]
fn duplicate_stdin() -> io::Result<File> {
    use std::os::fd::FromRawFd;
    let fd = unsafe { libc::dup(libc::STDIN_FILENO) };
    if fd == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

#[cfg(windows)]
fn duplicate_stdin() -> io::Result<File> {
    use std::os::windows::io::FromRawHandle;
    use windows_sys::Win32::Foundation::{DuplicateHandle, DUPLICATE_SAME_ACCESS, HANDLE};
    use windows_sys::Win32::System::Console::{GetStdHandle, STD_INPUT_HANDLE};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let source = unsafe { GetStdHandle(STD_INPUT_HANDLE) };
    let mut duplicate: HANDLE = 0;
    let process = unsafe { GetCurrentProcess() };
    if source == 0
        || unsafe {
            DuplicateHandle(
                process,
                source,
                process,
                &mut duplicate,
                0,
                0,
                DUPLICATE_SAME_ACCESS,
            )
        } == 0
    {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { File::from_raw_handle(duplicate as _) })
    }
}
