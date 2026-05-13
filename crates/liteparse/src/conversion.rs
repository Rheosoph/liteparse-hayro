use std::{
    fmt::{self, Display},
    path::Path,
};

/// Supported file extensions for conversion (non-PDF formats).
const OFFICE_EXTENSIONS: &[&str] = &[
    "doc", "docx", "docm", "dot", "dotm", "dotx", "odt", "ott", "rtf", "pages",
];
const PRESENTATION_EXTENSIONS: &[&str] = &[
    "ppt", "pptx", "pptm", "pot", "potm", "potx", "odp", "otp", "key",
];
const SPREADSHEET_EXTENSIONS: &[&str] = &[
    "xls", "xlsx", "xlsm", "xlsb", "ods", "ots", "csv", "tsv", "numbers",
];
const IMAGE_EXTENSIONS: &[&str] = &[
    "jpg", "jpeg", "png", "gif", "bmp", "tiff", "tif", "webp", "svg",
];

/// Extensions that require Ghostscript for ImageMagick conversion.
const GHOSTSCRIPT_REQUIRED_EXTENSIONS: &[&str] = &["svg", "eps", "ps", "ai"];

/// A resolved external command with its executable path and any required prefix args.
#[derive(Debug, Clone)]
pub struct ResolvedCommand {
    pub command: String,
    pub args: Vec<String>,
    pub resolved_path: String,
}

#[derive(Debug, Clone)]
pub struct ConversionResult {
    pub pdf_path: String,
    pub original_extension: String,
}

enum ConverstionTool {
    LibreOffice,
    ImageMagick,
}

impl Display for ConverstionTool {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::ImageMagick => "ImageMagick",
            Self::LibreOffice => "LibreOffice",
        };
        write!(f, "{}", s)
    }
}

/// Check if a file is a PDF (no conversion needed).
pub fn is_pdf(path: &str) -> bool {
    Path::new(path)
        .extension()
        .map(|ext| ext.eq_ignore_ascii_case("pdf"))
        .unwrap_or(false)
}

/// Check if a file extension is supported (either PDF or convertible).
pub fn is_supported_extension(path: &str) -> bool {
    let ext = match Path::new(path).extension().and_then(|e| e.to_str()) {
        Some(e) => e.to_lowercase(),
        None => return false,
    };

    if ext == "pdf" {
        return true;
    }

    OFFICE_EXTENSIONS.contains(&ext.as_str())
        || PRESENTATION_EXTENSIONS.contains(&ext.as_str())
        || SPREADSHEET_EXTENSIONS.contains(&ext.as_str())
        || IMAGE_EXTENSIONS.contains(&ext.as_str())
}

/// Attempt to convert a non-PDF file to PDF.
///
/// Currently stubbed out — returns an error directing users to install
/// LibreOffice (for office documents) or ImageMagick (for images).
pub async fn convert_to_pdf(
    path: &str,
    password: Option<&str>,
) -> Result<ConversionResult, Box<dyn std::error::Error>> {
    let ext = Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())
        .unwrap_or_default();

    if ext == "pdf" {
        return Ok(ConversionResult {
            pdf_path: path.to_string(),
            original_extension: ext,
        });
    }

    let tool = if OFFICE_EXTENSIONS.contains(&ext.as_str())
        || PRESENTATION_EXTENSIONS.contains(&ext.as_str())
        || SPREADSHEET_EXTENSIONS.contains(&ext.as_str())
    {
        ConverstionTool::LibreOffice
    } else if IMAGE_EXTENSIONS.contains(&ext.as_str()) {
        ConverstionTool::ImageMagick
    } else {
        return Err(format!("unsupported file format: .{}", ext).into());
    };

    let tmp_dir = tempfile::Builder::new().prefix("liteparse-").tempdir()?;
    let pdf_path = match tool {
        ConverstionTool::ImageMagick => {
            convert_image_to_pdf(path, tmp_dir.path().to_str().unwrap()).await?
        }
        ConverstionTool::LibreOffice => {
            convert_office_document(path, tmp_dir.path().to_str().unwrap(), password).await?
        }
    };

    Ok(ConversionResult {
        pdf_path,
        original_extension: ext,
    })
}

/// Execute command with timeout
pub async fn execute_command(
    command: &str,
    args: Vec<&str>,
    timeout_ms: u64,
) -> Result<String, Box<dyn std::error::Error>> {
    let proc = tokio::process::Command::new(command)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    match tokio::time::timeout(
        tokio::time::Duration::from_millis(timeout_ms),
        proc.wait_with_output(),
    )
    .await
    {
        Ok(Ok(output)) => {
            let stdout = String::from_utf8_lossy(&output.stdout).to_string();
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            if output.status.success() {
                Ok(stdout)
            } else {
                Err(anyhow::anyhow!("Command failed: {stderr}").into())
            }
        }
        Ok(Err(e)) => Err(anyhow::anyhow!("Command error: {e}").into()),
        Err(_) => Err(anyhow::anyhow!("Command timed out after {timeout_ms}ms").into()),
    }
}

/// Execute a command for PowerShel
pub async fn execute_powershell(
    command: &str,
    timeout_ms: u64,
) -> Result<String, Box<dyn std::error::Error>> {
    execute_command(
        "powershell",
        vec!["-NoProfile", "-Command", command],
        timeout_ms,
    )
    .await
}

fn get_resolved_path_from_output(output: &str, use_last_line: bool) -> Option<String> {
    let lines: Vec<String> = output
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    if lines.is_empty() {
        return None;
    }
    let l = if use_last_line {
        lines.last()
    } else {
        lines.first()
    };

    l.cloned()
}

/// Resolve the actual executable path for a command.
pub async fn resolve_command_path(command: &str) -> Option<String> {
    if std::env::consts::FAMILY == "windows" {
        let ps = format!("(Get-Command '{command}' -ErrorAction Stop).Source");
        match execute_powershell(&ps, 5000).await {
            Ok(out) => get_resolved_path_from_output(&out, true),
            Err(_) => None,
        }
    } else {
        match execute_command("which", vec![command], 5000).await {
            Ok(out) => get_resolved_path_from_output(&out, false),
            Err(_) => None,
        }
    }
}

/// Check if a command is available on Unix-like platforms (via `which`).
pub async fn is_command_available(command: &str) -> bool {
    execute_command("which", vec![command], 5000).await.is_ok()
}

/// Check if a command is available on Windows (via PowerShell `Get-Command`).
pub async fn is_command_available_windows(command: &str) -> bool {
    execute_powershell(&format!("Get-Command {command}"), 5000)
        .await
        .is_ok()
}

/// Check if a file path exists and is executable.
pub async fn is_path_executable(file_path: &str) -> bool {
    let p = std::path::PathBuf::from(file_path);
    match tokio::fs::metadata(&p).await {
        Ok(meta) => {
            if !meta.is_file() {
                return false;
            }
            if std::env::consts::FAMILY == "unix" {
                use std::os::unix::fs::PermissionsExt;
                meta.permissions().mode() & 0o111 != 0
            } else {
                true
            }
        }
        Err(_) => false,
    }
}

/// Detect whether a resolved path points at the built-in Windows
/// `System32\convert.exe` (which is unrelated to ImageMagick).
fn is_windows_system_convert(file_path: &str) -> bool {
    let normalized = file_path.replace('/', "\\").to_lowercase();
    let system_root = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".to_string());
    let system32_convert = format!("{system_root}\\System32\\convert.exe")
        .replace('/', "\\")
        .to_lowercase();
    normalized == system32_convert
}

/// Verify an executable identifies itself as ImageMagick via `-version`.
async fn is_image_magick_binary(executable_path: &str, args: &[&str]) -> bool {
    let mut full_args: Vec<&str> = args.to_vec();
    full_args.push("-version");
    match execute_command(executable_path, full_args, 5000).await {
        Ok(out) => out.to_lowercase().contains("imagemagick"),
        Err(_) => false,
    }
}

async fn resolve_image_magick_command(command: &str) -> Option<ResolvedCommand> {
    let resolved_path = resolve_command_path(command).await?;

    if command == "convert"
        && std::env::consts::FAMILY == "windows"
        && is_windows_system_convert(&resolved_path)
    {
        return None;
    }

    if !is_image_magick_binary(&resolved_path, &[]).await {
        return None;
    }

    Some(ResolvedCommand {
        command: resolved_path.clone(),
        args: Vec::new(),
        resolved_path,
    })
}

/// Find LibreOffice command - handles different installation methods.
pub async fn find_libre_office_command() -> Option<String> {
    if is_command_available("libreoffice").await
        || is_command_available_windows("libreoffice").await
    {
        return Some("libreoffice".to_string());
    }

    if is_command_available("soffice").await || is_command_available_windows("soffice").await {
        return Some("soffice".to_string());
    }

    let mac_os_paths = [
        "/Applications/LibreOffice.app/Contents/MacOS/soffice",
        "/Applications/LibreOffice.app/Contents/MacOS/libreoffice",
    ];

    let windows_paths = ["C:\\Program Files\\Libreoffice\\program\\soffice.exe"];

    for lib_path in mac_os_paths.iter() {
        if is_path_executable(lib_path).await {
            return Some(lib_path.to_string());
        }
    }

    for lib_path in windows_paths.iter() {
        if is_path_executable(lib_path).await {
            return Some(lib_path.to_string());
        }
    }

    None
}

/// Find ImageMagick command - handles v6 (`convert`) and v7 (`magick`).
pub async fn find_image_magick_command() -> Option<ResolvedCommand> {
    if let Some(cmd) = resolve_image_magick_command("magick").await {
        return Some(cmd);
    }
    resolve_image_magick_command("convert").await
}

/// Convert office documents using LibreOffice.
pub async fn convert_office_document(
    file_path: &str,
    output_dir: &str,
    password: Option<&str>,
) -> Result<String, Box<dyn std::error::Error>> {
    let libre_office_cmd = find_libre_office_command().await.ok_or_else(|| {
        anyhow::anyhow!(
            "LibreOffice is not installed. Please install LibreOffice to convert office documents. \
             On macOS: brew install --cask libreoffice, On Ubuntu: apt-get install libreoffice, \
             On Windows: choco install libreoffice-fresh"
        )
    })?;

    let infilter_arg;
    let mut args: Vec<&str> = vec![
        "--headless",
        "--invisible",
        "--convert-to",
        "pdf",
        "--outdir",
        output_dir,
    ];
    if let Some(pw) = password {
        infilter_arg = format!("--infilter=:{pw}");
        args.push(&infilter_arg);
    }
    args.push(file_path);

    execute_command(&libre_office_cmd, args, 120_000).await?;

    let base_name = Path::new(file_path)
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| anyhow::anyhow!("invalid file path: {file_path}"))?;
    let pdf_path = Path::new(output_dir)
        .join(format!("{base_name}.pdf"))
        .to_string_lossy()
        .to_string();

    if tokio::fs::metadata(&pdf_path).await.is_ok() {
        Ok(pdf_path)
    } else {
        Err(anyhow::anyhow!("LibreOffice conversion succeeded but output PDF not found").into())
    }
}

/// Convert images to PDF using ImageMagick.
pub async fn convert_image_to_pdf(
    file_path: &str,
    output_dir: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let image_magick = find_image_magick_command().await.ok_or_else(|| {
        anyhow::anyhow!(
            "ImageMagick is not installed. Please install ImageMagick to convert images. \
             On macOS: brew install imagemagick, On Ubuntu: apt-get install imagemagick, \
             On Windows: choco install imagemagick.app"
        )
    })?;

    let ext = Path::new(file_path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())
        .unwrap_or_default();
    let needs_ghostscript = GHOSTSCRIPT_REQUIRED_EXTENSIONS.contains(&ext.as_str());

    if needs_ghostscript {
        let has_ghostscript =
            is_command_available("gs").await || is_command_available_windows("gs").await;
        if !has_ghostscript {
            return Err(anyhow::anyhow!(
                "Ghostscript is required to convert {} files but is not installed. \
                 On macOS: brew install ghostscript, On Ubuntu: apt-get install ghostscript, \
                 On Windows: choco install ghostscript",
                ext.to_uppercase()
            )
            .into());
        }
    }

    let base_name = Path::new(file_path)
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or_else(|| anyhow::anyhow!("invalid file path: {file_path}"))?;
    let pdf_path = Path::new(output_dir)
        .join(format!("{base_name}.pdf"))
        .to_string_lossy()
        .to_string();

    let mut args: Vec<&str> = image_magick.args.iter().map(|s| s.as_str()).collect();
    args.push(file_path);
    args.push("-density");
    args.push("150");
    args.push("-units");
    args.push("PixelsPerInch");
    args.push(&pdf_path);

    match execute_command(&image_magick.command, args, 60_000).await {
        Ok(_) => Ok(pdf_path),
        Err(error) => {
            let error_msg = error.to_string();
            if error_msg.contains("gs") && error_msg.contains("command not found") {
                return Err(anyhow::anyhow!(
                    "Ghostscript is required to convert {} files but is not installed. \
                     On macOS: brew install ghostscript, On Ubuntu: apt-get install ghostscript, \
                     On Windows: choco install ghostscript",
                    ext.to_uppercase()
                )
                .into());
            }
            if error_msg.contains("FailedToExecuteCommand") && error_msg.contains("gs") {
                return Err(anyhow::anyhow!(
                    "Ghostscript failed during {} conversion. \
                     Ensure Ghostscript is properly installed: brew install ghostscript",
                    ext.to_uppercase()
                )
                .into());
            }
            Err(error)
        }
    }
}

pub fn guess_extension_from_data(data: &[u8]) -> Option<String> {
    let kind = infer::get(data);
    let ext = kind.map(|k| k.extension());
    if let Some(e) = ext {
        return Some(e.to_string());
    }
    None
}

pub async fn convert_data_to_pdf(
    data: Vec<u8>,
    password: Option<&str>,
) -> Result<ConversionResult, Box<dyn std::error::Error>> {
    let ext = guess_extension_from_data(&data);
    let tmp_dir = tempfile::Builder::new().prefix("liteparse-").tempdir()?;
    let tmp_path = tmp_dir
        .path()
        .join(format!("input.{}", ext.unwrap_or("bin".to_string())));
    tokio::fs::write(&tmp_path, data).await?;
    convert_to_pdf(tmp_path.to_str().unwrap(), password).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_pdf() {
        assert!(is_pdf("foo.pdf"));
        assert!(is_pdf("foo.PDF"));
        assert!(is_pdf("/abs/dir/Bar.Pdf"));
        assert!(!is_pdf("foo.docx"));
        assert!(!is_pdf("foo"));
        assert!(!is_pdf(""));
    }

    #[test]
    fn test_is_supported_extension() {
        assert!(is_supported_extension("a.pdf"));
        assert!(is_supported_extension("A.DOCX"));
        assert!(is_supported_extension("a.pptx"));
        assert!(is_supported_extension("a.xlsx"));
        assert!(is_supported_extension("a.png"));
        assert!(is_supported_extension("a.svg"));
        assert!(!is_supported_extension("a.exe"));
        assert!(!is_supported_extension("noext"));
    }

    #[test]
    fn test_conversion_tool_display() {
        assert_eq!(ConverstionTool::ImageMagick.to_string(), "ImageMagick");
        assert_eq!(ConverstionTool::LibreOffice.to_string(), "LibreOffice");
    }

    #[test]
    fn test_get_resolved_path_from_output_first_and_last() {
        let out = "  /usr/bin/foo\n\n/opt/bin/foo\n";
        assert_eq!(
            get_resolved_path_from_output(out, false).as_deref(),
            Some("/usr/bin/foo")
        );
        assert_eq!(
            get_resolved_path_from_output(out, true).as_deref(),
            Some("/opt/bin/foo")
        );
    }

    #[test]
    fn test_get_resolved_path_from_output_empty() {
        assert!(get_resolved_path_from_output("", false).is_none());
        assert!(get_resolved_path_from_output("   \n  \n", true).is_none());
    }

    #[test]
    fn test_is_windows_system_convert() {
        // SAFETY: tests run single-threaded for env modification scope
        unsafe { std::env::set_var("SystemRoot", "C:\\Windows") };
        assert!(is_windows_system_convert(
            "C:\\Windows\\System32\\convert.exe"
        ));
        assert!(is_windows_system_convert("C:/Windows/System32/convert.exe"));
        assert!(!is_windows_system_convert(
            "C:\\Program Files\\ImageMagick\\convert.exe"
        ));
    }

    #[test]
    fn test_guess_extension_from_data_png() {
        let png_header = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        assert_eq!(
            guess_extension_from_data(&png_header).as_deref(),
            Some("png")
        );
    }

    #[test]
    fn test_guess_extension_from_data_unknown() {
        assert!(guess_extension_from_data(&[0u8, 1, 2, 3]).is_none());
    }

    #[tokio::test]
    async fn test_execute_command_failure() {
        let r = execute_command("ls", vec!["/this/definitely/does/not/exist"], 5000).await;
        assert!(r.is_err());
    }

    #[tokio::test]
    async fn test_execute_command_timeout() {
        let r = execute_command("sleep", vec!["5"], 50).await;
        assert!(r.is_err());
        assert!(r.unwrap_err().to_string().contains("timed out"));
    }

    #[tokio::test]
    async fn test_execute_command_spawn_error() {
        let r = execute_command("definitely_not_a_real_command_xyz123", vec![], 1000).await;
        assert!(r.is_err());
    }

    #[tokio::test]
    async fn test_is_path_executable_nonexistent() {
        assert!(!is_path_executable("/no/such/path/zzz").await);
    }

    #[tokio::test]
    async fn test_convert_to_pdf_passthrough_pdf() {
        let res = convert_to_pdf("/some/file.pdf", None).await.unwrap();
        assert_eq!(res.pdf_path, "/some/file.pdf");
        assert_eq!(res.original_extension, "pdf");
    }

    #[tokio::test]
    async fn test_convert_to_pdf_unsupported() {
        let r = convert_to_pdf("/some/file.xyz", None).await;
        assert!(r.is_err());
        assert!(r.unwrap_err().to_string().contains("unsupported"));
    }
}
