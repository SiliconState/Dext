use std::io::{Cursor, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard, TryLockError};
use std::time::{Duration, Instant};

use image::imageops::FilterType;
use image::{DynamicImage, ImageDecoder as _, ImageFormat, Rgb, RgbImage};
use serde::{Deserialize, Serialize};

pub(crate) const SOURCE_BYTE_CAP: usize = 20 * 1024 * 1024;
pub(crate) const DECODED_PIXEL_CAP: u64 = 40_000_000;
pub(crate) const LONGEST_SIDE_CAP: u32 = 1_568;
pub(crate) const ENCODED_BYTE_CAP: usize = 1_500 * 1024;
pub(crate) const APPROVED_SOURCE_SHA256_FIELD: &str = "_dext_approved_source_sha256";
const BACKGROUND: [u8; 3] = [240, 240, 240];
pub(crate) const WORKER_ARG: &str = "--dext-image-worker";
const WORKER_MEMORY_CAP: u64 = 768 * 1024 * 1024;
const WORKER_CPU_SECONDS: u64 = 10;
const WORKER_TIMEOUT: Duration = Duration::from_secs(15);
const WORKER_HEADER_BYTES: usize = 8;
#[cfg(not(test))]
static WORKER_SLOT: Mutex<()> = Mutex::new(());

#[cfg(unix)]
fn worker_resource_limits() -> Result<(), String> {
    for (resource, value) in [
        #[cfg(not(target_os = "macos"))]
        (libc::RLIMIT_AS, WORKER_MEMORY_CAP),
        (libc::RLIMIT_CPU, WORKER_CPU_SECONDS),
        (libc::RLIMIT_CORE, 0),
    ] {
        let mut inherited: libc::rlimit = unsafe { std::mem::zeroed() };
        if unsafe { libc::getrlimit(resource, &mut inherited) } != 0 {
            return Err("could not inspect inherited image worker limits".to_string());
        }
        let cap = if inherited.rlim_cur == libc::RLIM_INFINITY {
            value as libc::rlim_t
        } else {
            (value as libc::rlim_t).min(inherited.rlim_cur)
        };
        let cap = if inherited.rlim_max == libc::RLIM_INFINITY {
            cap
        } else {
            cap.min(inherited.rlim_max)
        };
        let limit = libc::rlimit {
            rlim_cur: cap,
            rlim_max: cap,
        };
        if unsafe { libc::setrlimit(resource, &limit) } != 0 {
            return Err(format!(
                "could not limit image worker: {}",
                std::io::Error::last_os_error()
            ));
        }
    }
    Ok(())
}

#[cfg(windows)]
fn worker_resource_limits() -> Result<(), String> {
    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::JobObjects::*;
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
    if job.is_null() {
        return Err("could not create image worker resource job".to_string());
    }
    let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_PROCESS_MEMORY
        | JOB_OBJECT_LIMIT_PROCESS_TIME
        | JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
    limits.BasicLimitInformation.ActiveProcessLimit = 1;
    limits.BasicLimitInformation.PerProcessUserTimeLimit = (WORKER_CPU_SECONDS * 10_000_000) as i64;
    limits.ProcessMemoryLimit = WORKER_MEMORY_CAP as usize;
    let configured = unsafe {
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            std::mem::size_of_val(&limits) as u32,
        ) != 0
            && AssignProcessToJobObject(job, GetCurrentProcess()) != 0
    };
    if !configured {
        unsafe {
            CloseHandle(job);
        }
        return Err("could not apply image worker resource job".to_string());
    }
    // The worker exits through process::exit; retaining the handle keeps the job alive.
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn worker_resource_limits() -> Result<(), String> {
    Err("image worker resource isolation is unsupported on this platform".to_string())
}

pub(crate) fn worker_main() -> i32 {
    let result = (|| -> Result<(), String> {
        if std::env::args_os().count() != 2 {
            return Err("image worker accepts no paths or options".to_string());
        }
        worker_resource_limits()?;
        let mut bytes = Vec::new();
        std::io::stdin()
            .take((SOURCE_BYTE_CAP + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|error| format!("could not read image worker input: {error}"))?;
        if bytes.len() > SOURCE_BYTE_CAP {
            return Err("image worker source byte limit exceeded".to_string());
        }
        let (width, height, encoded) = sanitize_bytes(&bytes)?;
        let mut stdout = std::io::stdout().lock();
        stdout
            .write_all(&width.to_be_bytes())
            .and_then(|_| stdout.write_all(&height.to_be_bytes()))
            .and_then(|_| stdout.write_all(&encoded))
            .and_then(|_| stdout.flush())
            .map_err(|error| format!("could not write image worker output: {error}"))
    })();
    match result {
        Ok(()) => 0,
        Err(error) => {
            eprintln!(
                "{}",
                crate::cap_bytes_with_hint(error, 4000, "image worker error truncated")
            );
            1
        }
    }
}

struct WorkerChild(Child, Option<crate::ChildProcessTree>);

impl Drop for WorkerChild {
    fn drop(&mut self) {
        if let Some(tree) = self.1.take() {
            tree.terminate_std_child(&mut self.0);
        } else {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

fn worker_command(executable: &Path) -> Command {
    let mut command = Command::new(executable);
    command.arg(WORKER_ARG).env_clear();
    #[cfg(windows)]
    if let Some(root) = std::env::var_os("SystemRoot") {
        command.env("SystemRoot", root);
    }
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

#[cfg(target_os = "macos")]
fn worker_memory_within_limit(child: &Child) -> Result<(), String> {
    let mut info: libc::proc_taskinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of_val(&info) as libc::c_int;
    let read = unsafe {
        libc::proc_pidinfo(
            child.id() as libc::c_int,
            libc::PROC_PIDTASKINFO,
            0,
            (&mut info as *mut libc::proc_taskinfo).cast(),
            size,
        )
    };
    if read != size {
        return Err("could not inspect image worker memory".to_string());
    }
    if info.pti_resident_size > WORKER_MEMORY_CAP {
        return Err("image worker exceeded its resident-memory limit".to_string());
    }
    Ok(())
}

fn check_worker_interrupt(interrupt: Option<&AtomicBool>) -> Result<(), String> {
    if interrupt.is_some_and(|flag| flag.load(Ordering::Relaxed)) {
        Err("image worker interrupted by user".to_string())
    } else {
        Ok(())
    }
}

fn run_worker(
    command: &mut Command,
    bytes: &[u8],
    timeout: Duration,
    interrupt: Option<&AtomicBool>,
) -> Result<Vec<u8>, String> {
    check_worker_interrupt(interrupt)?;
    if bytes.len() > SOURCE_BYTE_CAP {
        return Err("image worker source byte limit exceeded".to_string());
    }
    let start = Instant::now();
    crate::configure_std_process_group(command);
    std::thread::scope(|scope| {
        // Drop the child before scope joins on an early return or thread-spawn panic.
        let mut child = WorkerChild(
            command
                .spawn()
                .map_err(|error| format!("could not start image worker: {error}"))?,
            None,
        );
        child.1 = Some(
            crate::ChildProcessTree::for_std(&child.0)
                .map_err(|error| format!("could not contain image worker: {error}"))?,
        );
        let stdin = child.0.stdin.take().ok_or("image worker has no stdin")?;
        let stdout = child.0.stdout.take().ok_or("image worker has no stdout")?;
        let stderr = child.0.stderr.take().ok_or("image worker has no stderr")?;
        let writer = scope.spawn(move || {
            let mut stdin = stdin;
            stdin.write_all(bytes)
        });
        let reader = scope.spawn(move || {
            let mut output = Vec::new();
            stdout
                .take((ENCODED_BYTE_CAP + WORKER_HEADER_BYTES + 1) as u64)
                .read_to_end(&mut output)
                .map(|_| output)
        });
        let errors = scope.spawn(move || {
            let mut output = Vec::new();
            stderr.take(4096).read_to_end(&mut output).map(|_| output)
        });
        let status = loop {
            if let Err(error) = check_worker_interrupt(interrupt) {
                break Err(error);
            }
            match child.0.try_wait() {
                Ok(Some(status)) => break Ok(status),
                Ok(None) if start.elapsed() < timeout => {
                    #[cfg(target_os = "macos")]
                    if let Err(error) = worker_memory_within_limit(&child.0) {
                        // A worker can exit between try_wait and proc_pidinfo.
                        if matches!(child.0.try_wait(), Ok(Some(_))) {
                            continue;
                        }
                        break Err(error);
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                Ok(None) => break Err("image worker exceeded its wall-clock limit".to_string()),
                Err(error) => break Err(format!("could not wait for image worker: {error}")),
            }
        };
        // Descendants must close inherited pipes before the bounded readers are joined.
        if let Some(tree) = child.1.take() {
            if status.is_err() {
                tree.terminate_std_child(&mut child.0);
            } else {
                tree.terminate_after_root_exit();
            }
        }
        let written = writer
            .join()
            .map_err(|_| "image worker input thread failed")?;
        let output = reader
            .join()
            .map_err(|_| "image worker output thread failed")?
            .map_err(|error| format!("could not read image worker output: {error}"))?;
        let errors = errors
            .join()
            .map_err(|_| "image worker error thread failed")?
            .map_err(|error| format!("could not read image worker errors: {error}"))?;
        let status = status?;
        if !status.success() {
            let detail = String::from_utf8_lossy(&errors);
            return Err(format!("image worker failed ({status}): {}", detail.trim()));
        }
        written.map_err(|error| format!("could not write image worker input: {error}"))?;
        check_worker_interrupt(interrupt)?;
        Ok(output)
    })
}

fn worker_jpeg_matches_dimensions(bytes: &[u8], width: u32, height: u32) -> bool {
    if !bytes.starts_with(&[0xff, 0xd8]) {
        return false;
    }
    let mut offset = 2usize;
    let mut jfif = false;
    let mut frame = false;
    let mut quantization = false;
    let mut huffman = false;
    while bytes.get(offset) == Some(&0xff) {
        let Some(&marker) = bytes.get(offset + 1) else {
            return false;
        };
        let Some(length_bytes) = bytes.get(offset + 2..offset + 4) else {
            return false;
        };
        let length = usize::from(u16::from_be_bytes([length_bytes[0], length_bytes[1]]));
        if length < 2 {
            return false;
        }
        let Some(end) = offset
            .checked_add(2 + length)
            .filter(|end| *end <= bytes.len())
        else {
            return false;
        };
        let data = &bytes[offset + 4..end];
        match marker {
            0xe0 if !jfif
                && data.len() == 14
                && data.starts_with(b"JFIF\0")
                && data[12..] == [0, 0] =>
            {
                jfif = true
            }
            0xc0 if !frame && data.len() == 15 && data[0] == 8 && data[5] == 3 => {
                if u32::from(u16::from_be_bytes([data[1], data[2]])) != height
                    || u32::from(u16::from_be_bytes([data[3], data[4]])) != width
                {
                    return false;
                }
                frame = true;
            }
            0xdb if !data.is_empty() => quantization = true,
            0xc4 if !data.is_empty() => huffman = true,
            0xda if jfif
                && frame
                && quantization
                && huffman
                && data.len() == 10
                && data[0] == 3
                && data[7..] == [0, 63, 0] =>
            {
                // Validate framing without invoking any image decoder in the parent.
                let mut scan = end;
                while scan < bytes.len() {
                    if bytes[scan] != 0xff {
                        scan += 1;
                        continue;
                    }
                    match bytes.get(scan + 1) {
                        Some(0x00) => scan += 2,
                        Some(0xd9) => return scan > end && scan + 2 == bytes.len(),
                        _ => return false,
                    }
                }
                return false;
            }
            _ => return false,
        }
        offset = end;
    }
    false
}

fn worker_output(output: Vec<u8>) -> Result<(u32, u32, Vec<u8>), String> {
    if output.len() <= WORKER_HEADER_BYTES || output.len() > ENCODED_BYTE_CAP + WORKER_HEADER_BYTES
    {
        return Err("image worker output byte limit or framing is invalid".to_string());
    }
    let width = u32::from_be_bytes(output[..4].try_into().expect("worker width"));
    let height = u32::from_be_bytes(output[4..8].try_into().expect("worker height"));
    if width == 0 || height == 0 || width.max(height) > LONGEST_SIDE_CAP {
        return Err("image worker output dimensions are invalid".to_string());
    }
    let encoded = output[WORKER_HEADER_BYTES..].to_vec();
    if !worker_jpeg_matches_dimensions(&encoded, width, height) {
        return Err("image worker output is not a sanitized JPEG".to_string());
    }
    Ok((width, height, encoded))
}

fn acquire_worker_slot<'a>(
    slot: &'a Mutex<()>,
    interrupt: Option<&AtomicBool>,
    timeout: Duration,
) -> Result<MutexGuard<'a, ()>, String> {
    let started = Instant::now();
    loop {
        check_worker_interrupt(interrupt)?;
        match slot.try_lock() {
            Ok(guard) => return Ok(guard),
            Err(TryLockError::Poisoned(_)) => {
                return Err("image worker slot is unavailable".to_string());
            }
            Err(TryLockError::WouldBlock) if started.elapsed() < timeout => {
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(TryLockError::WouldBlock) => {
                return Err("image worker slot wait exceeded its wall-clock limit".to_string());
            }
        }
    }
}

fn worker_executable() -> Result<PathBuf, String> {
    // current_exe() can name an unlinked inode after cargo install replaces Dext.
    #[cfg(target_os = "linux")]
    {
        Ok(PathBuf::from("/proc/self/exe"))
    }
    #[cfg(not(target_os = "linux"))]
    {
        std::env::current_exe().map_err(|error| format!("could not locate image worker: {error}"))
    }
}

#[cfg(not(test))]
fn isolated_sanitize(
    bytes: &[u8],
    interrupt: Option<&AtomicBool>,
) -> Result<(u32, u32, Vec<u8>), String> {
    let _slot = acquire_worker_slot(&WORKER_SLOT, interrupt, WORKER_TIMEOUT)?;
    let mut command = worker_command(&worker_executable()?);
    worker_output(run_worker(&mut command, bytes, WORKER_TIMEOUT, interrupt)?)
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub(crate) struct ImageReference {
    pub(crate) path: String,
    pub(crate) media_type: String,
    pub(crate) width: u32,
    pub(crate) height: u32,
    pub(crate) source_sha256: String,
}

#[derive(Debug)]
pub(crate) struct PreparedImage {
    pub(crate) reference: ImageReference,
    pub(crate) bytes: Vec<u8>,
}

fn png_is_animated(bytes: &[u8]) -> bool {
    let mut offset = 8usize;
    while let Some(header_end) = offset.checked_add(8).filter(|end| *end <= bytes.len()) {
        let length = u32::from_be_bytes(
            bytes[offset..offset + 4]
                .try_into()
                .expect("PNG chunk length is four bytes"),
        ) as usize;
        let kind = &bytes[offset + 4..header_end];
        if kind == b"acTL" {
            return true;
        }
        let Some(next) = header_end
            .checked_add(length)
            .and_then(|end| end.checked_add(4))
            .filter(|end| *end <= bytes.len())
        else {
            return false;
        };
        offset = next;
        if kind == b"IEND" {
            return false;
        }
    }
    false
}

fn webp_is_animated(bytes: &[u8]) -> bool {
    let mut offset = 12usize;
    while let Some(header_end) = offset.checked_add(8).filter(|end| *end <= bytes.len()) {
        let kind = &bytes[offset..offset + 4];
        let length = u32::from_le_bytes(
            bytes[offset + 4..header_end]
                .try_into()
                .expect("WebP chunk length is four bytes"),
        ) as usize;
        if kind == b"ANIM"
            || kind == b"ANMF"
            || kind == b"VP8X"
                && length >= 1
                && bytes.get(header_end).is_some_and(|flags| flags & 0x02 != 0)
        {
            return true;
        }
        let Some(next) = header_end
            .checked_add(length)
            .and_then(|end| end.checked_add(length & 1))
            .filter(|end| *end <= bytes.len())
        else {
            return false;
        };
        offset = next;
    }
    false
}

fn sniff_format(bytes: &[u8]) -> Result<ImageFormat, String> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        if png_is_animated(bytes) {
            return Err("animated PNG is not supported; convert one intended frame explicitly before read_image".to_string());
        }
        Ok(ImageFormat::Png)
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Ok(ImageFormat::Jpeg)
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        if webp_is_animated(bytes) {
            return Err("animated WebP is not supported; convert one intended frame explicitly before read_image".to_string());
        }
        Ok(ImageFormat::WebP)
    } else {
        Err("unsupported image format; read_image accepts only PNG, JPEG, and WebP. Convert other images explicitly or use OCR".to_string())
    }
}

fn source_bytes(path: &Path, interrupt: Option<&AtomicBool>) -> Result<Vec<u8>, String> {
    crate::session::read_regular_file_bytes_with_limit(
        path,
        SOURCE_BYTE_CAP,
        interrupt,
        "image source",
    )
    .map(|(bytes, _)| bytes)
}

fn decoder_limits() -> image::Limits {
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(u32::try_from(DECODED_PIXEL_CAP).unwrap_or(u32::MAX));
    limits.max_image_height = Some(u32::try_from(DECODED_PIXEL_CAP).unwrap_or(u32::MAX));
    limits.max_alloc = Some(DECODED_PIXEL_CAP.saturating_mul(5));
    limits
}

fn checked_dimensions(bytes: &[u8], format: ImageFormat) -> Result<(u32, u32), String> {
    let mut reader = image::ImageReader::with_format(Cursor::new(bytes), format);
    reader.limits(decoder_limits());
    let (width, height) = reader
        .into_dimensions()
        .map_err(|error| format!("could not read image dimensions: {error}"))?;
    if width == 0 || height == 0 {
        return Err("image dimensions must be non-zero".to_string());
    }
    let pixels = u64::from(width)
        .checked_mul(u64::from(height))
        .ok_or_else(|| "image dimensions overflow the decoded-pixel limit".to_string())?;
    if pixels > DECODED_PIXEL_CAP {
        return Err(format!(
            "image has {pixels} decoded pixels, exceeding the {DECODED_PIXEL_CAP} pixel limit"
        ));
    }
    Ok((width, height))
}

fn composite_neutral(image: DynamicImage) -> RgbImage {
    let rgba = image.into_rgba8();
    let mut rgb = RgbImage::new(rgba.width(), rgba.height());
    for (x, y, pixel) in rgba.enumerate_pixels() {
        let alpha = u16::from(pixel[3]);
        let inverse = 255 - alpha;
        rgb.put_pixel(
            x,
            y,
            Rgb([
                ((u16::from(pixel[0]) * alpha + u16::from(BACKGROUND[0]) * inverse + 127) / 255)
                    as u8,
                ((u16::from(pixel[1]) * alpha + u16::from(BACKGROUND[1]) * inverse + 127) / 255)
                    as u8,
                ((u16::from(pixel[2]) * alpha + u16::from(BACKGROUND[2]) * inverse + 127) / 255)
                    as u8,
            ]),
        );
    }
    rgb
}

fn bounded_jpeg(image: &RgbImage) -> Result<Vec<u8>, String> {
    for quality in [88, 82, 76, 68, 58, 48, 38, 30] {
        let mut encoded = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut encoded, quality)
            .encode(
                image.as_raw(),
                image.width(),
                image.height(),
                image::ExtendedColorType::Rgb8,
            )
            .map_err(|error| format!("could not encode sanitized image: {error}"))?;
        if encoded.len() <= ENCODED_BYTE_CAP {
            return Ok(encoded);
        }
    }
    Err(format!(
        "sanitized image still exceeds the {ENCODED_BYTE_CAP} byte transmission limit"
    ))
}

fn decode_bounded(bytes: &[u8], format: ImageFormat) -> Result<DynamicImage, String> {
    let mut reader = image::ImageReader::with_format(Cursor::new(bytes), format);
    reader.limits(decoder_limits());
    let mut decoder = reader
        .into_decoder()
        .map_err(|error| format!("could not initialize bounded image decoder: {error}"))?;
    let orientation = decoder
        .orientation()
        .map_err(|error| format!("could not read image orientation: {error}"))?;
    let mut decoded = DynamicImage::from_decoder(decoder)
        .map_err(|error| format!("could not decode image: {error}"))?;
    decoded.apply_orientation(orientation);
    Ok(decoded)
}

fn resize_bounded(decoded: DynamicImage) -> DynamicImage {
    if decoded.width().max(decoded.height()) <= LONGEST_SIDE_CAP {
        return decoded;
    }
    // image's separable resize samples vertically first with a full-width f32
    // intermediate. Rotate wide sources so that intermediate uses the short axis.
    if decoded.width() > decoded.height() {
        let rotated = decoded.rotate90();
        drop(decoded);
        rotated
            .resize(LONGEST_SIDE_CAP, LONGEST_SIDE_CAP, FilterType::Lanczos3)
            .rotate270()
    } else {
        decoded.resize(LONGEST_SIDE_CAP, LONGEST_SIDE_CAP, FilterType::Lanczos3)
    }
}

fn sanitize_bytes(bytes: &[u8]) -> Result<(u32, u32, Vec<u8>), String> {
    let format = sniff_format(bytes)?;
    checked_dimensions(bytes, format)?;
    let decoded = decode_bounded(bytes, format)?;
    // Composite before resampling so hidden RGB from transparent pixels cannot
    // bleed into visible neighbors or be disclosed by an opaque output.
    let opaque = DynamicImage::ImageRgb8(composite_neutral(decoded));
    let rgb = resize_bounded(opaque).into_rgb8();
    let encoded = bounded_jpeg(&rgb)?;
    Ok((rgb.width(), rgb.height(), encoded))
}

fn prepare_bytes(
    path: PathBuf,
    bytes: Vec<u8>,
    interrupt: Option<&AtomicBool>,
) -> Result<PreparedImage, String> {
    check_worker_interrupt(interrupt)?;
    let path = path
        .to_str()
        .ok_or_else(|| "read_image requires a UTF-8 workspace path".to_string())?
        .to_string();
    let source_sha256 = crate::sha256_hex_bytes(&bytes);
    #[cfg(not(test))]
    let (width, height, encoded) = isolated_sanitize(&bytes, interrupt)?;
    #[cfg(test)]
    let (width, height, encoded) = sanitize_bytes(&bytes)?;
    check_worker_interrupt(interrupt)?;
    let prepared = PreparedImage {
        reference: ImageReference {
            path,
            media_type: "image/jpeg".to_string(),
            width,
            height,
            source_sha256,
        },
        bytes: encoded,
    };
    validate_reference(&prepared.reference)?;
    Ok(prepared)
}

#[cfg(test)]
pub(crate) fn prepare(path: PathBuf) -> Result<PreparedImage, String> {
    let bytes = source_bytes(&path, None)?;
    prepare_bytes(path, bytes, None)
}

pub(crate) fn validate_reference(reference: &ImageReference) -> Result<(), String> {
    let path = Path::new(&reference.path);
    if reference.path.is_empty()
        || reference.path.len() > 16 * 1024
        || reference.path.contains('\0')
        || !path.is_absolute()
    {
        return Err("image reference path is invalid".to_string());
    }
    if reference.media_type != "image/jpeg" {
        return Err("image reference media type is invalid".to_string());
    }
    if reference.width == 0
        || reference.height == 0
        || reference.width.max(reference.height) > LONGEST_SIDE_CAP
    {
        return Err("image reference dimensions are invalid".to_string());
    }
    if reference.source_sha256.len() != 64
        || !reference
            .source_sha256
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err("image reference digest is invalid".to_string());
    }
    Ok(())
}

pub(crate) fn prepare_if_unchanged(
    reference: &ImageReference,
    interrupt: Option<&AtomicBool>,
) -> Result<PreparedImage, String> {
    check_worker_interrupt(interrupt)?;
    validate_reference(reference)?;
    let path = PathBuf::from(&reference.path);
    let bytes = source_bytes(&path, interrupt)?;
    let source_sha256 = crate::sha256_hex_bytes(&bytes);
    if source_sha256 != reference.source_sha256 {
        return Err("source changed since read_image approval".to_string());
    }
    let prepared = prepare_bytes(path, bytes, interrupt)?;
    if prepared.reference != *reference {
        return Err("sanitized image metadata changed since read_image approval".to_string());
    }
    Ok(prepared)
}

fn workspace_path(root: &Path, input: &serde_json::Value) -> Result<PathBuf, String> {
    let raw = input["path"].as_str().ok_or("missing path")?;
    let path = crate::canonical_read_path(root, raw)?;
    // Compare against the canonical root: symlinked ancestors (for example
    // macOS TMPDIR under /var) must match canonical_read_path output.
    let root = crate::session::canonicalize_or_clone(root);
    if !path.starts_with(&root) {
        return Err(format!(
            "read_image only sends images from the active workspace {}; copy the image into the workspace first",
            root.display()
        ));
    }
    Ok(path)
}

pub(crate) fn validate_workspace_path(
    root: &Path,
    input: &serde_json::Value,
) -> Result<(), String> {
    workspace_path(root, input).map(|_| ())
}

pub(crate) fn approval_digest(
    root: &Path,
    input: &serde_json::Value,
    interrupt: Option<&AtomicBool>,
) -> Result<String, String> {
    check_worker_interrupt(interrupt)?;
    let path = workspace_path(root, input)?;
    source_bytes(&path, interrupt).map(|bytes| crate::sha256_hex_bytes(&bytes))
}

pub(crate) fn read_tool(
    root: &Path,
    input: &serde_json::Value,
    interrupt: Option<&AtomicBool>,
) -> Result<String, String> {
    let expected = input[APPROVED_SOURCE_SHA256_FIELD]
        .as_str()
        .ok_or("missing internal read_image approval digest")?;
    let path = workspace_path(root, input)?;
    let bytes = source_bytes(&path, interrupt)?;
    if crate::sha256_hex_bytes(&bytes) != expected {
        return Err("source changed after read_image approval".to_string());
    }
    let prepared = prepare_bytes(path, bytes, interrupt)?;
    serde_json::to_string(&prepared.reference)
        .map_err(|error| format!("could not serialize image reference: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{ImageBuffer, Rgba};

    fn temp_dir(label: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "dext-{label}-{}-{}",
            std::process::id(),
            crate::unix_timestamp_secs()
        ));
        std::fs::create_dir_all(&path).expect("create image test directory");
        path
    }

    fn write_png(path: &Path, width: u32, height: u32, pixel: Rgba<u8>) {
        ImageBuffer::from_pixel(width, height, pixel)
            .save_with_format(path, ImageFormat::Png)
            .expect("write PNG fixture");
    }

    #[test]
    fn png_animation_detection_uses_chunk_types_not_compressed_payload_text() {
        let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        bytes.extend_from_slice(&4u32.to_be_bytes());
        bytes.extend_from_slice(b"IDAT");
        bytes.extend_from_slice(b"acTL");
        bytes.extend_from_slice(&0u32.to_be_bytes());
        assert!(!png_is_animated(&bytes));

        let mut animated = b"\x89PNG\r\n\x1a\n".to_vec();
        animated.extend_from_slice(&8u32.to_be_bytes());
        animated.extend_from_slice(b"acTL");
        animated.extend_from_slice(&[0; 8]);
        animated.extend_from_slice(&0u32.to_be_bytes());
        assert!(png_is_animated(&animated));
    }

    #[test]
    fn worker_command_never_inherits_credentials_or_opt_in() {
        let command = worker_command(Path::new("dext"));
        let environment: Vec<_> = command.get_envs().collect();
        assert!(environment.iter().all(|(key, _)| *key == "SystemRoot"));
        assert_eq!(command.get_args().collect::<Vec<_>>(), [WORKER_ARG]);
    }

    #[test]
    fn worker_output_rejects_bad_framing_dimensions_and_payloads() {
        assert!(worker_output(Vec::new()).is_err());
        assert!(worker_output(vec![0; ENCODED_BYTE_CAP + WORKER_HEADER_BYTES + 1]).is_err());
        let output = |width: u32, height: u32, bytes: &[u8]| {
            [
                width.to_be_bytes().as_slice(),
                height.to_be_bytes().as_slice(),
                bytes,
            ]
            .concat()
        };
        let jpeg = bounded_jpeg(&RgbImage::from_pixel(1, 1, Rgb([12, 34, 56]))).unwrap();
        assert!(worker_output(output(0, 1, &jpeg)).is_err());
        assert!(worker_output(output(LONGEST_SIDE_CAP + 1, 1, &jpeg)).is_err());
        assert!(worker_output(output(1, 1, b"not pixels")).is_err());
        assert!(worker_output(output(1, 1, &[0xff, 0xd8, 0xff, 0xd9])).is_err());
        assert!(worker_output(output(2, 1, &jpeg)).is_err());
        assert!(worker_output(output(1, 1, &jpeg)).is_ok());
        for length in 0..jpeg.len() {
            assert!(worker_output(output(1, 1, &jpeg[..length])).is_err());
        }
        let mut noisy = RgbImage::new(96, 64);
        let mut state = 1u32;
        for pixel in noisy.pixels_mut() {
            for channel in &mut pixel.0 {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                *channel = (state >> 24) as u8;
            }
        }
        let noisy = bounded_jpeg(&noisy).unwrap();
        assert!(noisy.windows(2).any(|part| part == [0xff, 0x00]));
        assert!(worker_output(output(96, 64, &noisy)).is_ok());
        let mut metadata = jpeg.clone();
        metadata.splice(2..2, [0xff, 0xe1, 0, 8, b'E', b'x', b'i', b'f', 0, 0]);
        assert!(worker_output(output(1, 1, &metadata)).is_err());
        let mut trailing = jpeg.clone();
        trailing.extend_from_slice(&[0xff, 0xd9]);
        assert!(worker_output(output(1, 1, &trailing)).is_err());
    }

    #[test]
    fn worker_supervisor_reaps_a_successful_child() {
        let mut command = Command::new(worker_executable().unwrap());
        command
            .args([
                "--exact",
                "image::tests::validates_persisted_reference_shape",
                "--list",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let output = run_worker(&mut command, b"", WORKER_TIMEOUT, None).unwrap();
        assert!(
            String::from_utf8(output)
                .unwrap()
                .contains("validates_persisted_reference_shape")
        );
    }

    #[test]
    fn worker_slot_wait_is_bounded_and_cancellable() {
        let slot = Mutex::new(());
        let held = slot.lock().unwrap();
        assert!(
            acquire_worker_slot(&slot, None, Duration::from_millis(20))
                .unwrap_err()
                .contains("slot wait")
        );
        let interrupt = AtomicBool::new(true);
        assert!(
            acquire_worker_slot(&slot, Some(&interrupt), WORKER_TIMEOUT)
                .unwrap_err()
                .contains("interrupted")
        );
        drop(held);
        assert!(acquire_worker_slot(&slot, None, WORKER_TIMEOUT).is_ok());
    }

    #[test]
    fn interrupted_worker_does_not_spawn_or_return_pixels() {
        let interrupt = AtomicBool::new(true);
        let mut command = worker_command(Path::new("dext-nonexistent-worker-fixture"));
        assert!(
            run_worker(&mut command, b"", WORKER_TIMEOUT, Some(&interrupt))
                .unwrap_err()
                .contains("interrupted")
        );
        let root = temp_dir("image-interrupt");
        let png = root.join("sample.png");
        write_png(&png, 2, 2, Rgba([0, 0, 0, 255]));
        let mut input = serde_json::json!({"path": png});
        input[APPROVED_SOURCE_SHA256_FIELD] = approval_digest(&root, &input, None).unwrap().into();
        assert!(
            read_tool(&root, &input, Some(&interrupt))
                .unwrap_err()
                .contains("interrupted")
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn rebuilt_reference_metadata_and_approval_reads_fail_closed() {
        let root = temp_dir("image-rebuild-metadata");
        let png = root.join("sample.png");
        write_png(&png, 2, 3, Rgba([12, 34, 56, 255]));
        let reference = prepare(png.clone()).unwrap().reference;
        let interrupt = AtomicBool::new(true);
        assert!(
            approval_digest(&root, &serde_json::json!({"path": png}), Some(&interrupt))
                .unwrap_err()
                .contains("interrupted")
        );
        assert!(
            prepare_if_unchanged(&reference, Some(&interrupt))
                .unwrap_err()
                .contains("interrupted")
        );
        let mut altered = reference.clone();
        altered.width = 3;
        assert!(
            prepare_if_unchanged(&altered, None)
                .unwrap_err()
                .contains("metadata changed")
        );
        assert!(prepare_if_unchanged(&reference, None).is_ok());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn resized_transparent_pixels_cannot_leak_hidden_rgb() {
        let encode = |hidden: [u8; 3]| {
            let mut rgba =
                ImageBuffer::from_pixel(1_600, 2, Rgba([hidden[0], hidden[1], hidden[2], 0]));
            for x in 0..800 {
                for y in 0..2 {
                    rgba.put_pixel(x, y, Rgba([255, 0, 0, 255]));
                }
            }
            let mut output = Cursor::new(Vec::new());
            DynamicImage::ImageRgba8(rgba)
                .write_to(&mut output, ImageFormat::Png)
                .unwrap();
            sanitize_bytes(&output.into_inner()).unwrap().2
        };
        assert_eq!(encode([0, 0, 255]), encode([0, 255, 0]));
    }

    #[test]
    fn bounded_resize_preserves_wide_image_orientation() {
        let mut image = RgbImage::from_pixel(2_000, 4, Rgb([255, 0, 0]));
        for x in 1_000..2_000 {
            for y in 0..4 {
                image.put_pixel(x, y, Rgb([0, 0, 255]));
            }
        }
        let resized = resize_bounded(DynamicImage::ImageRgb8(image)).into_rgb8();
        assert_eq!(resized.dimensions(), (1_568, 3));
        assert_eq!(resized.get_pixel(0, 0).0, [255, 0, 0]);
        assert_eq!(resized.get_pixel(1_567, 0).0, [0, 0, 255]);
    }

    #[cfg(unix)]
    #[test]
    fn worker_interrupt_reaps_a_stalled_child_with_blocked_stdin() {
        let interrupt = AtomicBool::new(false);
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", "while :; do :; done"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let started = Instant::now();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                std::thread::sleep(Duration::from_millis(50));
                interrupt.store(true, Ordering::Relaxed);
            });
            assert!(
                run_worker(
                    &mut command,
                    &vec![0; SOURCE_BYTE_CAP],
                    WORKER_TIMEOUT,
                    Some(&interrupt)
                )
                .unwrap_err()
                .contains("interrupted")
            );
        });
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[cfg(unix)]
    #[test]
    fn worker_supervisor_reaps_stalls_crashes_and_output_floods() {
        for (script, expected) in [
            ("while :; do :; done", "wall-clock limit"),
            ("(while :; do :; done) & exit 1", "image worker failed"),
            ("kill -KILL $$", "image worker failed"),
            (
                "while :; do printf 'xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx'; done",
                "",
            ),
        ] {
            let mut command = Command::new("/bin/sh");
            command
                .args(["-c", script])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let started = Instant::now();
            let result = run_worker(&mut command, b"input", Duration::from_millis(100), None);
            if expected.is_empty() {
                assert!(result.and_then(worker_output).is_err());
            } else {
                assert!(result.unwrap_err().contains(expected));
            }
            assert!(started.elapsed() < WORKER_TIMEOUT);
        }
    }

    #[test]
    fn deterministic_malformed_corpus_does_not_panic() {
        let mut seeds = Vec::new();
        for format in [ImageFormat::Png, ImageFormat::Jpeg, ImageFormat::WebP] {
            let mut cursor = Cursor::new(Vec::new());
            DynamicImage::ImageRgb8(RgbImage::from_pixel(3, 2, Rgb([1, 2, 3])))
                .write_to(&mut cursor, format)
                .unwrap();
            seeds.push(cursor.into_inner());
        }
        for seed in seeds {
            for length in 0..seed.len() {
                let _ = sanitize_bytes(&seed[..length]);
            }
            for index in 0..seed.len() {
                let mut mutated = seed.clone();
                mutated[index] ^= 0xff;
                let _ = sanitize_bytes(&mutated);
            }
        }
    }

    #[test]
    fn rejects_unsupported_and_malformed_inputs() {
        let root = temp_dir("image-invalid");
        let gif = root.join("sample.gif");
        std::fs::write(&gif, b"GIF89a").unwrap();
        assert!(
            prepare(gif)
                .unwrap_err()
                .contains("only PNG, JPEG, and WebP")
        );

        let malformed = root.join("broken.png");
        std::fs::write(&malformed, b"\x89PNG\r\n\x1a\nbroken").unwrap();
        assert!(prepare(malformed).unwrap_err().contains("dimensions"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn enforces_source_byte_and_decoded_pixel_limits_before_decode() {
        let root = temp_dir("image-limits");
        let oversized = root.join("oversized.png");
        let file = std::fs::File::create(&oversized).expect("create oversized fixture");
        file.set_len((SOURCE_BYTE_CAP + 1) as u64)
            .expect("size oversized fixture");
        assert!(
            prepare(oversized)
                .unwrap_err()
                .contains("image source 20971520 byte limit")
        );

        let jpeg = root.join("dimensions.jpg");
        let mut bytes = Vec::new();
        image::codecs::jpeg::JpegEncoder::new(&mut bytes)
            .encode(&[0, 0, 0], 1, 1, image::ExtendedColorType::Rgb8)
            .expect("encode JPEG fixture");
        let sof = bytes
            .windows(2)
            .position(|window| matches!(window, [0xff, 0xc0] | [0xff, 0xc2]))
            .expect("JPEG SOF marker");
        bytes[sof + 5..sof + 7].copy_from_slice(&7_000u16.to_be_bytes());
        bytes[sof + 7..sof + 9].copy_from_slice(&7_000u16.to_be_bytes());
        std::fs::write(&jpeg, bytes).expect("write oversized-dimension JPEG");
        assert!(
            prepare(jpeg)
                .unwrap_err()
                .contains("exceeding the 40000000 pixel limit")
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn bounds_dimensions_strips_metadata_and_composites_alpha() {
        let root = temp_dir("image-sanitize");
        let png = root.join("wide.png");
        write_png(&png, 1_600, 2, Rgba([255, 0, 0, 128]));
        let prepared = prepare(png).expect("prepare image");
        assert_eq!(
            (prepared.reference.width, prepared.reference.height),
            (1_568, 2)
        );
        assert_eq!(prepared.reference.media_type, "image/jpeg");
        assert!(prepared.bytes.len() <= ENCODED_BYTE_CAP);
        assert!(
            !prepared
                .bytes
                .windows(6)
                .any(|window| window == b"Exif\0\0")
        );
        let decoded = image::load_from_memory_with_format(&prepared.bytes, ImageFormat::Jpeg)
            .expect("decode sanitized JPEG")
            .to_rgb8();
        let pixel = decoded.get_pixel(0, 0);
        assert!(
            pixel[0] > 220 && pixel[1] > 90 && pixel[2] > 90,
            "{pixel:?}"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn approval_digest_rejects_source_replacement_before_decode() {
        let root = temp_dir("image-approval-change");
        let png = root.join("approval.png");
        write_png(&png, 2, 2, Rgba([0, 0, 0, 255]));
        let mut input = serde_json::json!({"path": png});
        let digest = approval_digest(&root, &input, None).expect("approval digest");
        write_png(&png, 2, 2, Rgba([255, 255, 255, 255]));
        input[APPROVED_SOURCE_SHA256_FIELD] = serde_json::Value::String(digest);
        assert_eq!(
            read_tool(&root, &input, None).unwrap_err(),
            "source changed after read_image approval"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn validates_persisted_reference_shape() {
        let absolute = if cfg!(windows) {
            r"C:\workspace\image.png"
        } else {
            "/workspace/image.png"
        };
        let valid = ImageReference {
            path: absolute.to_string(),
            media_type: "image/jpeg".to_string(),
            width: 10,
            height: 20,
            source_sha256: "a".repeat(64),
        };
        validate_reference(&valid).expect("valid image reference");

        let mut malformed = valid.clone();
        malformed.path = "relative.png".to_string();
        assert!(validate_reference(&malformed).is_err());
        malformed = valid.clone();
        malformed.media_type = "image/png".to_string();
        assert!(validate_reference(&malformed).is_err());
        malformed = valid.clone();
        malformed.width = LONGEST_SIDE_CAP + 1;
        assert!(validate_reference(&malformed).is_err());
        malformed = valid;
        malformed.source_sha256 = "A".repeat(64);
        assert!(validate_reference(&malformed).is_err());
    }

    #[test]
    fn detects_source_change_before_transmission() {
        let root = temp_dir("image-change");
        let png = root.join("change.png");
        write_png(&png, 2, 2, Rgba([0, 0, 0, 255]));
        let reference = prepare(png.clone()).expect("prepare first image").reference;
        write_png(&png, 2, 2, Rgba([255, 255, 255, 255]));
        assert_eq!(
            prepare_if_unchanged(&reference, None).unwrap_err(),
            "source changed since read_image approval"
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
