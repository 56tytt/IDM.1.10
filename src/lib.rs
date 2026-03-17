use anyhow::{Context, Result, bail};
use colored::Colorize;
use futures::StreamExt;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use reqwest::{Client, StatusCode, header};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write, Seek, SeekFrom};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicBool, Ordering};
use std::sync::Mutex;
use tokio::task::JoinSet;
use chrono::Local;
use std::time::{Duration, Instant};
use std::collections::HashMap;
use std::ffi::CStr;
use std::os::raw::c_char;
use tokio::runtime::Runtime;

const NUM_WORKERS: usize = 8;
const CHUNK_SIZE: u64 = 1024 * 1024; // 1MB chunks
const CONNECTION_TIMEOUT: Duration = Duration::from_secs(30);
const READ_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DownloadState {
    url: String,
    output_path: PathBuf,
    total_size: u64,
    downloaded: u64,
    is_paused: bool,
    is_cancelled: bool,
    start_time: u64,
    chunk_status: Vec<bool>,
}

#[derive(Debug, Clone)]
struct DownloadProgress {
    total: u64,
    downloaded: Arc<AtomicU64>,
    paused: Arc<AtomicBool>,
    cancelled: Arc<AtomicBool>,
    start_time: Instant,
}

#[derive(Debug, Clone)]
struct ChunkDownload {
    chunk_id: usize,
    start: u64,
    end: u64,
    url: String,
}

struct DownloadManager {
    client: Client,
    progress: Arc<Mutex<HashMap<String, DownloadProgress>>>,
    active_downloads: Arc<Mutex<Vec<String>>>,
}

impl DownloadManager {
    fn new() -> Result<Self> {
        let client = Client::builder()
        // הנה התחפושת שלנו לכרום 👇
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/122.0.0.0 Safari/537.36")
        .redirect(reqwest::redirect::Policy::limited(10))
        .timeout(READ_TIMEOUT)
        .connect_timeout(CONNECTION_TIMEOUT)
        .build()
        .context("Failed to create HTTP client")?;

        Ok(DownloadManager {
            client,
            progress: Arc::new(Mutex::new(HashMap::new())),
           active_downloads: Arc::new(Mutex::new(Vec::new())),
        })
    }

    async fn get_file_size(&self, url: &str) -> Result<u64> {
        let response = self.client
            .get(url)
            .send()
            .await
            .context("Failed to fetch file info")?;

        if response.status() != StatusCode::OK {
            bail!("Server returned status: {}", response.status());
        }

        let size = response
            .headers()
            .get("content-length")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok())
            .context("Could not determine file size")?;

        Ok(size)
    }

    async fn download(
        &self,
        url: String,
        output_path: PathBuf,
        use_chunks: bool,
    ) -> Result<()> {
        println!("{}", format!("🚀 Starting download...").cyan().bold());
        println!("{}", format!("📥 URL: {}", url).bright_blue());
        println!("{}", format!("💾 Output: {}", output_path.display()).bright_blue());

        let file_size = self.get_file_size(&url).await?;
        println!("{}", format!("📊 File size: {}", self.format_size(file_size)).green());

        let supports_range = self.check_range_support(&url).await?;
        let use_chunks = use_chunks && supports_range;

        if use_chunks {
            println!("{}", "⚡ Using multi-chunk download (8 workers)".yellow().bold());
            self.download_with_chunks(&url, &output_path, file_size).await?;
        } else {
            println!("{}", "📥 Using single connection download".yellow().bold());
            self.download_single(&url, &output_path, file_size).await?;
        }

        Ok(())
    }

    async fn check_range_support(&self, url: &str) -> Result<bool> {
        let response = self.client
            .get(url)
            .send()
            .await?;

        Ok(response
            .headers()
            .get("accept-ranges")
            .and_then(|v| v.to_str().ok())
            .map(|s| s != "none")
            .unwrap_or(false))
    }

    async fn download_with_chunks(
        &self,
        url: &str,
        output_path: &Path,
        total_size: u64,
    ) -> Result<()> {
        let state_path = PathBuf::from(format!("{}.idm", output_path.display()));
        let mut chunk_status = vec![false; NUM_WORKERS];
        let mut initial_downloaded = 0;

        // 1. נבדוק אם יש קובץ המשך הורדה (.idm) ונטען אותו
        if state_path.exists() && output_path.exists() {
            if let Ok(state_str) = std::fs::read_to_string(&state_path) {
                if let Ok(state) = serde_json::from_str::<DownloadState>(&state_str) {
                    if state.total_size == total_size && state.url == url {
                        chunk_status = state.chunk_status;
                        println!("{}", "🔄 Found incomplete download, resuming...".cyan().bold());
                    }
                }
            }
        }

        // פותחים את הקובץ (ולא דורסים אותו מחדש בטעות!)
        let mut file = OpenOptions::new()
        .create(true)
        .write(true)
        .open(output_path)
        .context("Failed to create output file")?;
        file.set_len(total_size).context("Failed to allocate file space")?;
        drop(file);

        let mut chunks = Vec::new();
        for chunk_id in 0..NUM_WORKERS {
            let start = (chunk_id as u64) * (total_size / NUM_WORKERS as u64);
            let end = if chunk_id == NUM_WORKERS - 1 {
                total_size
            } else {
                (chunk_id as u64 + 1) * (total_size / NUM_WORKERS as u64)
            };

            // אם החלק כבר סומן כהושלם, נדלג עליו ונוסיף לגודל שכבר הורד!
            if chunk_status[chunk_id] {
                initial_downloaded += end - start;
            } else {
                chunks.push(ChunkDownload {
                    chunk_id,
                    start,
                    end,
                    url: url.to_string(),
                });
            }
        }

        let downloaded = Arc::new(AtomicU64::new(initial_downloaded));
        let paused = Arc::new(AtomicBool::new(false));
        let cancelled = Arc::new(AtomicBool::new(false));

        let pb = ProgressBar::new(total_size);
        let style = ProgressStyle::default_bar()
        .template("{spinner:.green} [{elapsed_precise}] [{wide_bar:.cyan/blue}] {bytes}/{total_bytes} ({bytes_per_sec}, ETA: {eta}) {msg}")
        .context("Failed to set progress style")?
        .progress_chars("█▉▊▋▌▍▎▏  ");
        pb.set_style(style);
        pb.set_position(initial_downloaded); // מתחילים את הפס גרפי מהנקודה שבה עצרנו

        pb.println(format!("{}", "⌨️  Controls: Type 'p' (Pause/Resume) or 'c' (Cancel) and press Enter".yellow()));

        let mut join_set = JoinSet::new();
        let output_path_arc = Arc::new(output_path.to_path_buf());
        let client = self.client.clone();

        for chunk in chunks {
            let downloaded = downloaded.clone();
            let paused = paused.clone();
            let cancelled = cancelled.clone();
            let output_path = output_path_arc.clone();
            let client = client.clone();
            let worker_pb = pb.clone();

            join_set.spawn(async move {
                loop {
                    if cancelled.load(Ordering::Relaxed) {
                        return Err(anyhow::anyhow!("Cancelled")); // עוצר בלי לסמן כהושלם
                    }

                    if paused.load(Ordering::Relaxed) {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        continue;
                    }

                    match download_chunk(&client, &chunk, &output_path, &worker_pb).await {
                        Ok(bytes) => {
                            downloaded.fetch_add(bytes, Ordering::Relaxed);
                            return Ok(chunk.chunk_id); // מחזירים איזה חלק סיים בהצלחה!
                        }
                        Err(_e) => {
                            tokio::time::sleep(Duration::from_secs(2)).await;
                        }
                    }
                }
            });
        }

        let command_handler = tokio::spawn({
            let paused = paused.clone();
            let cancelled = cancelled.clone();
            let cmd_pb = pb.clone();
            async move {
                let stdin = io::stdin();
                let mut line = String::new();
                loop {
                    if stdin.read_line(&mut line).is_ok() && !line.trim().is_empty() {
                        match line.trim().to_lowercase().as_str() {
                            "p" => {
                                let new_state = !paused.load(Ordering::Relaxed);
                                paused.store(new_state, Ordering::Relaxed);
                                if new_state {
                                    cmd_pb.set_message("⏸️ PAUSED".yellow().to_string());
                                } else {
                                    cmd_pb.set_message("".to_string());
                                }
                            }
                            "c" => {
                                cancelled.store(true, Ordering::Relaxed);
                                cmd_pb.println(format!("{}", "❌ Stopping workers...".red().bold()));
                                return;
                            }
                            _ => {}
                        }
                        line.clear();
                    } else {
                        break;
                    }
                }
            }
        });

        // 2. ממתינים לסיום החלקים, ועל כל חלק שמסיים שומרים את ההתקדמות לקובץ .idm!
        while let Some(result) = join_set.join_next().await {
            match result {
                Ok(Ok(chunk_id)) => {
                    if !cancelled.load(Ordering::Relaxed) {
                        chunk_status[chunk_id] = true;

                        let state = DownloadState {
                            url: url.to_string(),
                            output_path: output_path.to_path_buf(),
                            total_size,
                            downloaded: downloaded.load(Ordering::Relaxed),
                            is_paused: false,
                            is_cancelled: false,
                            start_time: 0,
                            chunk_status: chunk_status.clone(),
                        };

                        // שמירת קובץ הסטטוס בדיסק
                        if let Ok(json) = serde_json::to_string(&state) {
                            let _ = std::fs::write(&state_path, json);
                        }
                    }
                }
                _ => {} // התעלמות משגיאות או ביטולים
            }
        }

        command_handler.abort();

        if cancelled.load(Ordering::Relaxed) {
            // שים לב: אנחנו **לא מוחקים** את הקובץ פה יותר, כדי שנוכל להמשיך אותו אחר כך!
            pb.finish_with_message("⏸️ Saved for resume");
            bail!("Download cancelled. Run the exact same command to resume later.");
        }

        // 3. הכל עבר בהצלחה? מוחקים את קובץ המצב (כבר אין בו צורך)
        std::fs::remove_file(&state_path).ok();

        pb.finish_with_message("✅ Complete");
        println!("\n{}", "✅ Download completed successfully!".green().bold());
        self.verify_download(output_path).await?;

        Ok(())
    }




    async fn download_single(
        &self,
        url: &str,
        output_path: &Path,
        total_size: u64,
    ) -> Result<()> {
        let downloaded = Arc::new(AtomicU64::new(0));
        let paused = Arc::new(AtomicBool::new(false));
        let cancelled = Arc::new(AtomicBool::new(false));
        let start_time = Instant::now();

        // 1. קודם כל יוצרים את ה-ProgressBar המקורי
        let progress_bar = ProgressBar::new(total_size);
        let style = ProgressStyle::default_bar()
        .template("{spinner:.cyan} [{bar:30.cyan/white}] {percent}% {msg}")
        .context("Failed to set progress style")?
        .tick_strings(&["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"]);
        progress_bar.set_style(style);

        let download_task = {
            let client = self.client.clone();
            let url = url.to_string();
            let output_path = output_path.to_path_buf();
            let downloaded = downloaded.clone();
            let paused = paused.clone();
            let cancelled = cancelled.clone();

            // 2. עכשיו אנחנו משכפלים אותו במיוחד עבור ה-Task
            let pb = progress_bar.clone();

            tokio::spawn(async move {
                let mut file = OpenOptions::new()
                .create(true)
                .write(true)
                .open(&output_path)
                .context("Failed to create output file")?;

                loop {
                    if cancelled.load(Ordering::Relaxed) {
                        return Ok::<(), anyhow::Error>(());
                    }

                    if paused.load(Ordering::Relaxed) {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        continue;
                    }

                    match client.get(&url).send().await {
                        Ok(response) => {
                            let mut stream = response.bytes_stream();
                            while let Some(chunk) = stream.next().await {
                                match chunk {
                                    Ok(data) => {
                                        file.write_all(&data)
                                        .context("Failed to write data")?;
                                        downloaded.fetch_add(data.len() as u64, Ordering::Relaxed);

                                        // 3. כאן אנחנו משתמשים בעותק שיצרנו (pb)
                                        pb.inc(data.len() as u64);
                                    }
                                    Err(e) => {
                                        return Err(e).context("Failed to read stream");
                                    }
                                }
                            }
                            return Ok(());
                        }
                        Err(e) => {
                            eprintln!("Download error: {}, retrying...", e);
                            tokio::time::sleep(Duration::from_secs(2)).await;
                        }
                    }
                }
            })
        };




        let command_handler = tokio::spawn({
            let paused = paused.clone();
            let cancelled = cancelled.clone();
            async move {
                let stdin = io::stdin();
                let mut line = String::new();
                loop {
                    print!("{}", "[P]ause/Resume [C]ancel: ".cyan().bold());
                    let _ = io::stdout().flush();
                    
                    if stdin.read_line(&mut line).is_ok() && !line.trim().is_empty() {
                        match line.trim().to_lowercase().as_str() {
                            "p" => {
                                let new_state = !paused.load(Ordering::Relaxed);
                                paused.store(new_state, Ordering::Relaxed);
                                if new_state {
                                    println!("{}", "⏸️  Download paused".yellow().bold());
                                } else {
                                    println!("{}", "▶️  Download resumed".green().bold());
                                }
                            }
                            "c" => {
                                cancelled.store(true, Ordering::Relaxed);
                                println!("{}", "❌ Download cancelled".red().bold());
                                return;
                            }
                            _ => {}
                        }
                        line.clear();
                    } else {
                        break;
                    }
                }
            }
        });

        let _ = download_task.await?;

        progress_bar.finish_with_message("✅ Complete");
        command_handler.abort();

        if cancelled.load(Ordering::Relaxed) {
            std::fs::remove_file(output_path).ok();
            bail!("Download cancelled by user");
        }

        println!("{}", "✅ Download completed successfully!".green().bold());
        self.verify_download(output_path).await?;

        Ok(())
    }

    async fn verify_download(&self, path: &Path) -> Result<()> {
        println!("{}", "🔍 Verifying file integrity...".cyan());
        
        let mut file = File::open(path).context("Failed to open downloaded file")?;
        let mut hasher = Sha256::new();
        let mut buffer = [0u8; 8192];

        loop {
            let n = file.read(&mut buffer).context("Failed to read file")?;
            if n == 0 {
                break;
            }
            hasher.update(&buffer[..n]);
        }

        let hash = hasher.finalize();
        let hash_hex = format!("{:x}", hash);
        
        println!("{}", format!("✅ SHA256: {}", &hash_hex[..16]).green());
        Ok(())
    }

    fn format_size(&self, bytes: u64) -> String {
        const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
        let mut size = bytes as f64;
        let mut unit_idx = 0;

        while size >= 1024.0 && unit_idx < UNITS.len() - 1 {
            size /= 1024.0;
            unit_idx += 1;
        }

        format!("{:.2} {}", size, UNITS[unit_idx])
    }
}

async fn download_chunk(
    client: &reqwest::Client,
    chunk: &ChunkDownload,
    output_path: &Path,
    progress_bar: &ProgressBar,
) -> Result<u64> {
    let range_header = format!("bytes={}-{}", chunk.start, chunk.end - 1);
    
    let response = client
        .get(&chunk.url)
        .header(header::RANGE, range_header)
        .send()
        .await
        .context("Failed to download chunk")?;

    if response.status() != StatusCode::PARTIAL_CONTENT && response.status() != StatusCode::OK {
        bail!("Server returned status: {}", response.status());
    }

    let mut file = OpenOptions::new()
        .write(true)
        .open(output_path)
        .context("Failed to open file")?;
    
    file.seek(SeekFrom::Start(chunk.start))
        .context("Failed to seek in file")?;

    let mut stream = response.bytes_stream();
    let mut total_bytes = 0u64;

    while let Some(chunk_result) = stream.next().await {
        let data = chunk_result.context("Failed to read response stream")?;
        file.write_all(&data)
            .context("Failed to write chunk data")?;
        total_bytes += data.len() as u64;
        progress_bar.inc(data.len() as u64);
    }

    Ok(total_bytes)
}

fn format_speed(bytes_per_sec: f64) -> String {
    const UNITS: &[&str] = &["B/s", "KB/s", "MB/s", "GB/s"];
    let mut speed = bytes_per_sec;
    let mut unit_idx = 0;

    while speed >= 1024.0 && unit_idx < UNITS.len() - 1 {
        speed /= 1024.0;
        unit_idx += 1;
    }

    format!("{:.2} {}", speed, UNITS[unit_idx])
}

fn format_duration(duration: Duration) -> String {
    let total_secs = duration.as_secs();
    let hours = total_secs / 3600;
    let minutes = (total_secs % 3600) / 60;
    let seconds = total_secs % 60;

    if hours > 0 {
        format!("{:02}:{:02}:{:02}", hours, minutes, seconds)
    } else {
        format!("{:02}:{:02}", minutes, seconds)
    }
}

//#[tokio::main]
//async fn main() -> Result<()> {
    println!("{}", "╔════════════════════════════════════╗".cyan().bold());
    println!("{}", "║   🚀 IDM - Advanced Download Mgr   ║".cyan().bold());
    println!("{}", "║      Rust Async | 8 Workers       ║".cyan().bold());
    println!("{}", "╚════════════════════════════════════╝".cyan().bold());
    println!();

    //let args: Vec<String> = std::env::args().collect();




    //let state_path = PathBuf::from(format!("{}.idm", output_file.display()));

    //if output_file.exists() && !state_path.exists() {
      //  print!("{} ", format!("⚠️  File '{}' already exists. Overwrite? [y/N]:", output_file.display()).yellow().bold());
        io::stdout().flush().context("Failed to flush stdout")?;

        let mut input = String::new();
        io::stdin().read_line(&mut input).context("Failed to read user input")?;

        if !input.trim().eq_ignore_ascii_case("y") {
            println!("{}", "❌ Download cancelled by user.".red().bold());
            return Ok(());
        }
    //}
    // ------------------------------------------------

    let manager = DownloadManager::new().context("Failed to initialize download manager")?;
    manager.download(url.to_string(), output_file, true).await?;

    println!();
    println!("{}", "📋 Download Session Summary:".green().bold());
    println!("{}", format!("   ✅ Status: Completed").green());
    println!("{}", format!("   📅 Time: {}", Local::now().format("%Y-%m-%d %H:%M:%S")).cyan());
    println!();

    Ok(())



use std::ffi::CStr;
use std::os::raw::c_char;
use tokio::runtime::Runtime;

// המילה no_mangle אומרת ל-Rust לא לשנות את שם הפונקציה כדי ששפות אחרות ימצאו אותה
// extern "C" מגדיר שהפונקציה תשתמש בסטנדרט התקשורת של שפת C (הסטנדרט העולמי לספריות)
#[no_mangle]
pub extern "C" fn download_file(url_ptr: *const c_char, path_ptr: *const c_char) -> bool {
    // 1. המרת המחרוזות שמגיעות משפות אחרות למחרוזות ש-Rust מבין
    let url = unsafe {
        assert!(!url_ptr.is_null());
        CStr::from_ptr(url_ptr).to_string_lossy().into_owned()
    };

    let path = unsafe {
        assert!(!path_ptr.is_null());
        CStr::from_ptr(path_ptr).to_string_lossy().into_owned()
    };

    // 2. יצירת סביבת העבודה האסינכרונית של Tokio בתוך הפונקציה
    let rt = match Runtime::new() {
        Ok(rt) => rt,
        Err(_) => return false,
    };

    // 3. הרצת המנוע שלנו והחזרת התשובה
    rt.block_on(async {
        match DownloadManager::new() {
            Ok(manager) => {
                let output_path = PathBuf::from(path);
                // קריאה לפונקציה המקורית שלך
                match manager.download(url, output_path, true).await {
                    Ok(_) => true, // הצלחה
                Err(e) => {
                    eprintln!("Download failed: {}", e);
                    false // כישלון
                }
                }
            }
            Err(_) => false,
        }
    })
}

