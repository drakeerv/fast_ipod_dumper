use std::path::{Path, PathBuf};
use anyhow::Result;
use sanitize_filename::sanitize;
use id3::TagLike;
use tokio::fs;
use tokio::sync::mpsc;
use tokio::time::{sleep, Duration};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use clap::Parser;
use tracing::{info, warn, error};
use thiserror::Error;

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Cli {
    /// Path to iPod directory (usually the root of the iPod drive)
    ipod_dir: PathBuf,

    /// Output directory for copied files
    output_dir: PathBuf,

    /// Number of worker threads
    #[arg(short, long, default_value_t = 8)]
    threads: usize,

    /// Show detailed progress per thread
    #[arg(short, long)]
    detailed: bool,
}

#[derive(Error, Debug)]
pub enum DumperError {
    #[error("Failed to read metadata: {0}")]
    MetadataError(String),
    #[error("Failed to copy file: {0}")]
    CopyError(String),
    #[error("Invalid path: {0}")]
    PathError(String),
}

#[derive(Debug, Default, Clone)]
struct Statistics {
    total_files: usize,
    copied_files: usize,
    skipped_files: usize,
    failed_files: usize,
    total_bytes: u64,
}

trait MetadataReader {
    fn read_metadata(&self, path: &Path) -> Option<SongMetadata>;
}

struct Mp3Reader;
struct M4aReader;

impl MetadataReader for Mp3Reader {
    fn read_metadata(&self, path: &Path) -> Option<SongMetadata> {
        id3::Tag::read_from_path(path).ok().map(|tag| SongMetadata {
            title: tag.title().unwrap_or("Unknown Title").to_string(),
            artist: tag.artist().unwrap_or("Unknown Artist").to_string(),
            album: tag.album().unwrap_or("Unknown Album").to_string(),
        })
    }
}

impl MetadataReader for M4aReader {
    fn read_metadata(&self, path: &Path) -> Option<SongMetadata> {
        mp4ameta::Tag::read_from_path(path).ok().map(|tag| SongMetadata {
            title: tag.title().unwrap_or("Unknown Title").to_string(),
            artist: tag.artist().unwrap_or("Unknown Artist").to_string(),
            album: tag.album().unwrap_or("Unknown Album").to_string(),
        })
    }
}

#[derive(Debug)]
struct SongMetadata {
    title: String,
    artist: String,
    album: String,
}

async fn read_metadata(path: &Path) -> Option<SongMetadata> {
    let extension = path.extension()?.to_str()?;
    
    let reader: Box<dyn MetadataReader> = match extension.to_lowercase().as_str() {
        "mp3" => Box::new(Mp3Reader),
        "m4a" => Box::new(M4aReader),
        _ => return None,
    };
    
    reader.read_metadata(path)
}

async fn find_files(path: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut dirs = vec![path.to_path_buf()];

    while let Some(dir) = dirs.pop() {
        let mut read_dir = match fs::read_dir(&dir).await {
            Ok(read_dir) => read_dir,
            Err(_) => continue,
        };

        while let Ok(Some(entry)) = read_dir.next_entry().await {
            let path = entry.path();
            if path.is_dir() {
                dirs.push(path);
            } else {
                files.push(path);
            }
        }
    }

    files
}

async fn copy_song(
    source: &Path,
    metadata: &SongMetadata,
    output_dir: &Path,
    stats: &std::sync::Arc<tokio::sync::Mutex<Statistics>>
) -> Result<(), DumperError> {
    let extension = source.extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("unknown");

    let output_dir = output_dir
        .join(sanitize(&metadata.artist))
        .join(sanitize(&metadata.album));

    fs::create_dir_all(&output_dir).await.map_err(|e| DumperError::PathError(e.to_string()))?;

    let output_path = output_dir.join(format!("{}.{}", sanitize(&metadata.title), extension));
    
    if output_path.exists() {
        stats.lock().await.skipped_files += 1;
        return Ok(());
    }

    let mut retries = 3;
    loop {
        match fs::copy(source, &output_path).await {
            Ok(bytes) => {
                let mut stats = stats.lock().await;
                stats.copied_files += 1;
                stats.total_bytes += bytes;
                info!("Copied {} -> {}", source.display(), output_path.display());
                break Ok(());
            },
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied && retries > 0 => {
                retries -= 1;
                warn!("Retry copying {} due to permission error", source.display());
                sleep(Duration::from_millis(100)).await;
                continue;
            },
            Err(e) => {
                stats.lock().await.failed_files += 1;
                error!("Failed to copy {}: {}", source.display(), e);
                break Err(DumperError::CopyError(e.to_string()));
            }
        }
    }
}

struct CopyJob {
    source: PathBuf,
    metadata: SongMetadata,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();
    
    // Validate paths
    if !cli.ipod_dir.exists() {
        return Err(DumperError::PathError("iPod directory not found".into()).into());
    }

    let music_path = cli.ipod_dir.join("iPod_Control").join("Music");
    if !music_path.exists() {
        return Err(DumperError::PathError("iPod Music directory not found".into()).into());
    }

    info!("Starting iPod dump from {} to {}", cli.ipod_dir.display(), cli.output_dir.display());
    
    let files = find_files(&music_path).await;
    let stats = std::sync::Arc::new(tokio::sync::Mutex::new(Statistics::default()));

    let multi_progress = MultiProgress::new();
    let main_progress = multi_progress.add(ProgressBar::new(files.len() as u64));
    main_progress.set_style(
        ProgressStyle::default_bar()
            .template("{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} ({eta})")
            .unwrap()
            .progress_chars("=>-")
    );

    let main_progress = std::sync::Arc::new(main_progress);
    let (tx, rx) = mpsc::channel::<CopyJob>(32);
    let rx = std::sync::Arc::new(tokio::sync::Mutex::new(rx));
    let output_dir = std::sync::Arc::new(cli.output_dir);
    
    // Create a channel for progress bar cleanup
    let (progress_tx, mut progress_rx) = mpsc::channel::<ProgressBar>(cli.threads);
    let progress_tx = std::sync::Arc::new(progress_tx);
    
    let mut handles = vec![];
    
    for thread_id in 0..cli.threads {
        let rx = rx.clone();
        let main_progress = main_progress.clone();
        let output_dir = output_dir.clone();
        let stats = stats.clone();
        let progress_tx = progress_tx.clone();
        
        let thread_progress = if cli.detailed {
            let pb = multi_progress.add(ProgressBar::new_spinner());
            pb.set_style(ProgressStyle::default_spinner()
                .template("{spinner:.green} Thread {prefix}: {msg}")
                .unwrap());
            pb.set_prefix(thread_id.to_string());
            Some(pb)
        } else {
            None
        };
        
        let handle = tokio::spawn(async move {
            loop {
                let job = {
                    let mut rx = rx.lock().await;
                    rx.recv().await
                };

                match job {
                    Some(job) => {
                        if let Some(pb) = &thread_progress {
                            pb.set_message(format!("Copying {}", job.source.file_name().unwrap_or_default().to_string_lossy()));
                        }
                        
                        match copy_song(&job.source, &job.metadata, &output_dir, &stats).await {
                            Ok(()) => main_progress.inc(1),
                            Err(e) => {
                                main_progress.println(format!("Error copying {}: {}", job.source.display(), e));
                                main_progress.inc(1);
                            }
                        }
                    }
                    None => break,
                }
            }
            if let Some(pb) = thread_progress {
                pb.finish_with_message("Done");
                let _ = progress_tx.send(pb).await;
            }
        });
        handles.push(handle);
    }

    // Send jobs to workers
    for file in files {
        if let Some(metadata) = read_metadata(&file).await {
            let job = CopyJob {
                source: file,
                metadata,
            };
            tx.send(job).await.unwrap();
        } else {
            main_progress.inc(1);
        }
    }
    
    // Clean up
    drop(tx);
    for handle in handles {
        handle.await.unwrap();
    }

    // Clean up progress bars
    drop(progress_tx);
    while let Some(pb) = progress_rx.recv().await {
        pb.finish_and_clear();
    }
    main_progress.finish_and_clear();

    // Print statistics at the end
    let stats = stats.lock().await;
    println!("Dump completed:");
    println!("Total files processed: {}", stats.total_files);
    println!("Files copied: {}", stats.copied_files);
    println!("Files skipped: {}", stats.skipped_files);
    println!("Files failed: {}", stats.failed_files);
    println!("Total bytes copied: {} MB", stats.total_bytes / 1024 / 1024);
    
    Ok(())
}
