use std::io::{Cursor, Read as _, Write as _};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use image::{DynamicImage, ImageFormat, Rgb, RgbImage};

fn fixture(format: ImageFormat, width: u32, height: u32) -> Vec<u8> {
    let mut output = Cursor::new(Vec::new());
    DynamicImage::ImageRgb8(RgbImage::from_pixel(width, height, Rgb([12, 34, 56])))
        .write_to(&mut output, format)
        .unwrap();
    output.into_inner()
}

struct TestChild(Child);

impl Drop for TestChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn wait_within(child: &mut Child, started: Instant, timeout: Duration) -> std::process::ExitStatus {
    loop {
        if let Some(status) = child.try_wait().expect("poll child") {
            return status;
        }
        if started.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            panic!("child exceeded {timeout:?} test deadline");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn capture_child(command: &mut Command, bytes: &[u8], timeout: Duration) -> Output {
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    std::thread::scope(|scope| {
        let mut child = TestChild(command.spawn().expect("spawn child"));
        let started = Instant::now();
        let mut stdin = child.0.stdin.take().unwrap();
        let stdout = child.0.stdout.take().unwrap();
        let stderr = child.0.stderr.take().unwrap();
        let writer = scope.spawn(move || stdin.write_all(bytes));
        let reader = scope.spawn(move || {
            let mut output = Vec::new();
            stdout
                .take(4 * 1024 * 1024 + 1)
                .read_to_end(&mut output)
                .unwrap();
            output
        });
        let errors = scope.spawn(move || {
            let mut output = Vec::new();
            stderr.take(64 * 1024 + 1).read_to_end(&mut output).unwrap();
            output
        });
        let status = wait_within(&mut child.0, started, timeout);
        let _ = writer.join().unwrap();
        Output {
            status,
            stdout: reader.join().unwrap(),
            stderr: errors.join().unwrap(),
        }
    })
}

fn worker(bytes: &[u8]) -> (bool, Vec<u8>, String) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_dext"));
    command.arg("--dext-image-worker").env_clear();
    #[cfg(windows)]
    if let Some(root) = std::env::var_os("SystemRoot") {
        command.env("SystemRoot", root);
    }
    // A worker must bypass dotenv, provider auth, crash artifacts and the async runtime.
    command
        .env("DEXT_SANDBOX_PROFILE", "invalid")
        .env("DEXT_HOME", "/nonexistent-image-worker-state");
    let output = capture_child(&mut command, bytes, Duration::from_secs(20));
    (
        output.status.success(),
        output.stdout,
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn real_worker_sanitizes_all_three_formats_without_agent_startup() {
    for format in [ImageFormat::Png, ImageFormat::Jpeg, ImageFormat::WebP] {
        let (success, output, error) = worker(&fixture(format, 8, 4));
        assert!(success, "{format:?}: {error}");
        assert_eq!(u32::from_be_bytes(output[..4].try_into().unwrap()), 8);
        assert_eq!(u32::from_be_bytes(output[4..8].try_into().unwrap()), 4);
        let decoded = image::load_from_memory_with_format(&output[8..], ImageFormat::Jpeg).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (8, 4));
        assert!(output.len() <= 1_500 * 1024 + 8);
        assert!(!output.windows(6).any(|part| part == b"Exif\0\0"));
    }
}

#[cfg(target_os = "linux")]
#[test]
fn real_worker_applies_kernel_limits_before_accepting_pixels() {
    let mut child = TestChild(
        Command::new(env!("CARGO_BIN_EXE_dext"))
            .arg("--dext-image-worker")
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let started = Instant::now();
    let bounded_limit = |limits: &str, name: &str, cap: u64| {
        limits
            .lines()
            .find_map(|line| {
                let values = line
                    .strip_prefix(name)?
                    .split_whitespace()
                    .take(2)
                    .map(str::parse::<u64>)
                    .collect::<Result<Vec<_>, _>>()
                    .ok()?;
                Some(values.len() == 2 && values.iter().all(|value| *value <= cap))
            })
            .unwrap_or(false)
    };
    loop {
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "worker exited before accepting input"
        );
        let limits = std::fs::read_to_string(format!("/proc/{}/limits", child.0.id())).unwrap();
        if bounded_limit(&limits, "Max address space", 768 * 1024 * 1024)
            && bounded_limit(&limits, "Max cpu time", 10)
            && bounded_limit(&limits, "Max core file size", 0)
        {
            break;
        }
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "worker did not install resource limits"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    drop(child.0.stdin.take());
    assert!(!wait_within(&mut child.0, started, Duration::from_secs(20)).success());
}

#[test]
fn real_worker_rejects_hostile_inputs_without_pixel_output() {
    let seed = fixture(ImageFormat::Jpeg, 1, 1);
    let mut dimensions = seed.clone();
    let sof = dimensions
        .windows(2)
        .position(|part| part == [0xff, 0xc0])
        .unwrap();
    dimensions[sof + 5..sof + 7].copy_from_slice(&7_000u16.to_be_bytes());
    dimensions[sof + 7..sof + 9].copy_from_slice(&7_000u16.to_be_bytes());
    for bytes in [
        b"GIF89a".to_vec(),
        b"\x89PNG\r\n\x1a\nbroken".to_vec(),
        dimensions,
        seed[..seed.len() / 2].to_vec(),
        vec![0; 20 * 1024 * 1024 + 1],
    ] {
        let (success, output, error) = worker(&bytes);
        assert!(!success, "hostile input accepted");
        assert!(output.is_empty(), "failed worker emitted pixels");
        assert!(!error.is_empty());
    }
}

#[test]
fn real_agent_sends_only_worker_sanitized_pixels_to_mock_gateway() {
    image_gateway_round_trip(false);
}

#[cfg(target_os = "linux")]
#[test]
fn running_agent_can_launch_workers_after_its_binary_is_unlinked() {
    image_gateway_round_trip(true);
}

fn image_gateway_round_trip(unlink_binary: bool) {
    use base64::Engine as _;
    use serde_json::{Value, json};

    let built_binary = std::path::PathBuf::from(env!("CARGO_BIN_EXE_dext"));
    let fixture_parent = if unlink_binary {
        built_binary.parent().unwrap().to_path_buf()
    } else {
        std::env::temp_dir()
    };
    let root = fixture_parent.join(format!(
        "dext-image-gateway-{}-{unlink_binary}",
        std::process::id()
    ));
    let executable = if unlink_binary {
        let alias = root.join("running-dext");
        std::fs::create_dir_all(&root).unwrap();
        // A same-filesystem hard link gives the child a removable launch path
        // without a fresh executable write racing exec with ETXTBSY.
        std::fs::hard_link(&built_binary, &alias).unwrap();
        alias
    } else {
        built_binary
    };
    let running_binary = executable.clone();
    let state = root.join("state");
    std::fs::create_dir_all(&state).unwrap();
    let source = fixture(ImageFormat::Png, 8, 4);
    std::fs::write(root.join("sample.png"), &source).unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let catalog = json!({
        "version": 3,
        "active_provider": "image-fixture",
        "providers": [{
            "id": "image-fixture", "api_provider": "openai",
            "request_contract": "openai-chat-completions",
            "base_url": format!("http://{address}/v1"),
            "default_model": "fixture-vision", "requires_api_key": false,
            "model_defaults": {"capabilities": {"image_input": true, "reasoning": false}}
        }]
    });
    let catalog_path = state.join("providers.json");
    std::fs::write(&catalog_path, catalog.to_string()).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::set_permissions(&catalog_path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let server = std::thread::spawn(move || {
        let started = Instant::now();
        let mut bodies = Vec::new();
        while bodies.len() < 2 {
            let mut stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            started.elapsed() < Duration::from_secs(20),
                            "missing image request"
                        );
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("accept: {error}"),
                }
            };
            // Winsock accepts inherit the listener's nonblocking setting.
            stream.set_nonblocking(false).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            let mut buffer = [0; 8192];
            let header_end = loop {
                let read = stream.read(&mut buffer).unwrap();
                assert!(read > 0);
                request.extend_from_slice(&buffer[..read]);
                assert!(request.len() < 4 * 1024 * 1024);
                if let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                    break end + 4;
                }
            };
            let length = String::from_utf8_lossy(&request[..header_end])
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .and_then(|value| value.trim().parse::<usize>().ok())
                });
            let Some(length) = length else {
                assert!(
                    request.starts_with(b"GET ") || request.starts_with(b"HEAD "),
                    "unexpected body framing: {}",
                    String::from_utf8_lossy(&request[..header_end])
                );
                write!(
                    stream,
                    "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                )
                .unwrap();
                continue;
            };
            let index = bodies.len();
            assert!(length < 4 * 1024 * 1024);
            while request.len() < header_end + length {
                let read = stream.read(&mut buffer).unwrap();
                assert!(read > 0);
                request.extend_from_slice(&buffer[..read]);
            }
            bodies.push(
                serde_json::from_slice::<Value>(&request[header_end..header_end + length]).unwrap(),
            );
            let delta = if index == 0 {
                if unlink_binary {
                    std::fs::remove_file(&running_binary).unwrap();
                }
                json!({"tool_calls": [{"index": 0, "id": "image-call", "function": {"name": "read_image", "arguments": "{\"path\":\"sample.png\"}"}}]})
            } else {
                json!({"content": "Verified sample.png through read_image. The approved image is 8 by 4 pixels; the gateway received only a sanitized JPEG, not the original PNG."})
            };
            let finish = if index == 0 { "tool_calls" } else { "stop" };
            let body = format!(
                "data: {}\n\ndata: [DONE]\n\n",
                json!({"choices": [{"delta": delta, "finish_reason": finish}]})
            );
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        }
        bodies
    });
    let mut command = Command::new(&executable);
    command
        .args([
            "--no-session",
            "--no-tui",
            "--approval",
            "always",
            "--effort",
            "off",
            "--cd",
        ])
        .arg(&root)
        .arg("Inspect sample.png with read_image.")
        .env_clear()
        .env("HOME", &root)
        .env("DEXT_HOME", &state)
        .env("DEXT_PROVIDER", "image-fixture")
        .env("DEXT_PROVIDER_TOTAL_TIMEOUT_SECS", "5")
        .env("DEXT_LOCAL_PROVIDER_TOTAL_TIMEOUT_SECS", "5");
    #[cfg(windows)]
    if let Some(system_root) = std::env::var_os("SystemRoot") {
        command.env("SystemRoot", system_root);
    }
    let output = capture_child(&mut command, b"", Duration::from_secs(30));
    let bodies = server.join().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!bodies[0].to_string().contains("data:image/"));
    let images: Vec<_> = bodies[1]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["role"] == "user")
        .flat_map(|item| item["content"].as_array().into_iter().flatten())
        .filter(|block| block["type"] == "image_url")
        .collect();
    assert_eq!(images.len(), 1, "{}", bodies[1]);
    let data = images[0]["image_url"]["url"]
        .as_str()
        .unwrap()
        .strip_prefix("data:image/jpeg;base64,")
        .unwrap();
    let jpeg = base64::engine::general_purpose::STANDARD
        .decode(data)
        .unwrap();
    assert_ne!(jpeg, source);
    assert!(jpeg.len() <= 1_500 * 1024);
    assert!(image::load_from_memory_with_format(&jpeg, ImageFormat::Jpeg).is_ok());
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn real_worker_applies_exif_orientation_and_drops_source_metadata() {
    let mut jpeg = fixture(ImageFormat::Jpeg, 8, 4);
    let mut exif = b"Exif\0\0II\x2a\x00\x08\x00\x00\x00\x01\x00\x12\x01\x03\x00\x01\x00\x00\x00\x06\x00\x00\x00\x00\x00\x00\x00".to_vec();
    exif.extend_from_slice(b"source-private-metadata");
    let mut segment = vec![0xff, 0xe1];
    segment.extend_from_slice(&u16::try_from(exif.len() + 2).unwrap().to_be_bytes());
    segment.extend(exif);
    jpeg.splice(2..2, segment);
    let (success, output, error) = worker(&jpeg);
    assert!(success, "{error}");
    assert_eq!(u32::from_be_bytes(output[..4].try_into().unwrap()), 4);
    assert_eq!(u32::from_be_bytes(output[4..8].try_into().unwrap()), 8);
    assert!(!output.windows(6).any(|part| part == b"Exif\0\0"));
    let marker = b"source-private-metadata";
    assert!(!output.windows(marker.len()).any(|part| part == marker));
}

#[test]
fn real_worker_rejects_animation_without_output() {
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    png.extend_from_slice(&8u32.to_be_bytes());
    png.extend_from_slice(b"acTL");
    png.extend_from_slice(&[0; 12]);
    let mut webp = b"RIFF\x16\0\0\0WEBPVP8X\x0a\0\0\0".to_vec();
    webp.extend_from_slice(&[2, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    for source in [png, webp] {
        let (success, output, error) = worker(&source);
        assert!(!success);
        assert!(output.is_empty());
        assert!(error.contains("animated"), "{error}");
    }
}

#[test]
fn real_worker_sanitizes_extreme_aspect_ratios() {
    for (width, height) in [(2_000_000, 1), (1, 2_000_000)] {
        let source = fixture(ImageFormat::Png, width, height);
        let (success, output, error) = worker(&source);
        assert!(success, "{width}x{height}: {error}");
        let dimensions = (
            u32::from_be_bytes(output[..4].try_into().unwrap()),
            u32::from_be_bytes(output[4..8].try_into().unwrap()),
        );
        assert_eq!(
            dimensions,
            if width > height {
                (1_568, 1)
            } else {
                (1, 1_568)
            }
        );
    }
}

#[test]
fn real_worker_handles_compression_heavy_png_with_bounded_output() {
    let source = fixture(ImageFormat::Png, 4_000, 4_000);
    assert!(source.len() < 1024 * 1024);
    let (success, output, error) = worker(&source);
    assert!(success, "{error}");
    assert_eq!(u32::from_be_bytes(output[..4].try_into().unwrap()), 1_568);
    assert_eq!(u32::from_be_bytes(output[4..8].try_into().unwrap()), 1_568);
    assert!(output.len() <= 1_500 * 1024 + 8);
}
