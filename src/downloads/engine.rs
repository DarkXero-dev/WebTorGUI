use anyhow::{anyhow, Context, Result};
use reqwest::Client;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use tokio::fs::File;
use tokio::io::AsyncWriteExt;
use tokio::sync::Semaphore;

#[derive(Clone)]
pub struct ProgressEvent {
    pub id: String,
    pub bytes_done: u64,
    pub total_bytes: Option<u64>,
    pub speed_bps: u64,
}

#[derive(Clone)]
pub struct CompleteEvent {
    pub id: String,
    pub path: String,
    pub bytes_done: u64,
}

#[derive(Clone)]
pub struct ErrorEvent {
    pub id: String,
    pub error: String,
}

pub enum DownloadEvent {
    Progress(ProgressEvent),
    Complete(CompleteEvent),
    Error(ErrorEvent),
}

pub async fn download_file(
    tx: Sender<DownloadEvent>,
    id: String,
    url: String,
    dest_path: String,
    threads: u8,
) -> Result<()> {
    if url.starts_with("file://") {
        let src = url.trim_start_matches("file://");
        return local_copy_download(tx, id, src, &dest_path).await;
    }

    let client = Client::builder()
        .user_agent("Mozilla/5.0 (compatible; WebtorApp/0.1)")
        .redirect(reqwest::redirect::Policy::limited(10))
        .build()?;

    let total = match client.head(&url).send().await {
        Ok(resp) if resp.status().is_success() => resp
            .headers()
            .get("content-length")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok()),
        _ => None,
    };

    if let Some(parent) = Path::new(&dest_path).parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let bytes_done = if total.map(|t| t > 0).unwrap_or(false) && threads > 1 {
        multi_thread_download(&tx, &client, &id, &url, &dest_path, total.unwrap(), threads).await?
    } else {
        single_thread_download(&tx, &client, &id, &url, &dest_path, total).await?
    };

    if bytes_done == 0 {
        tokio::fs::remove_file(&dest_path).await.ok();
        return Err(anyhow!("download returned 0 bytes - URL may be invalid or expired"));
    }

    let _ = tx.send(DownloadEvent::Complete(CompleteEvent {
        id,
        path: dest_path,
        bytes_done,
    }));

    Ok(())
}

async fn local_copy_download(
    tx: Sender<DownloadEvent>,
    id: String,
    src_path: &str,
    dest_path: &str,
) -> Result<()> {
    use tokio::io::AsyncReadExt;

    let total = tokio::fs::metadata(src_path).await.map(|m| m.len()).unwrap_or(0);

    if let Some(parent) = Path::new(dest_path).parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let mut src = tokio::fs::File::open(src_path)
        .await
        .map_err(|e| anyhow!("Cannot open source: {e}"))?;
    let mut dst = tokio::fs::File::create(dest_path).await?;

    let mut bytes_done: u64 = 0;
    let mut last_percent: u64 = u64::MAX;
    let mut buf = vec![0u8; 512 * 1024];

    loop {
        let n = src.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        dst.write_all(&buf[..n]).await?;
        bytes_done += n as u64;

        if total > 0 {
            let pct = bytes_done * 100 / total;
            if pct != last_percent {
                last_percent = pct;
                let _ = tx.send(DownloadEvent::Progress(ProgressEvent {
                    id: id.clone(),
                    bytes_done,
                    total_bytes: Some(total),
                    speed_bps: 0,
                }));
            }
        }
    }
    dst.flush().await?;

    let _ = tx.send(DownloadEvent::Complete(CompleteEvent {
        id,
        path: dest_path.to_string(),
        bytes_done,
    }));
    Ok(())
}

async fn single_thread_download(
    tx: &Sender<DownloadEvent>,
    client: &Client,
    id: &str,
    url: &str,
    dest_path: &str,
    total: Option<u64>,
) -> Result<u64> {
    let mut resp = client.get(url).send().await?.error_for_status()?;

    let mut file = File::create(dest_path).await?;
    let mut bytes_done: u64 = 0;
    let mut last_emit = std::time::Instant::now();
    let mut speed_bytes: u64 = 0;
    let mut last_percent: u64 = u64::MAX;

    while let Some(chunk) = resp.chunk().await? {
        file.write_all(&chunk).await?;
        bytes_done += chunk.len() as u64;
        speed_bytes += chunk.len() as u64;

        let should_emit = match total {
            Some(t) if t > 0 => {
                let pct = bytes_done * 100 / t;
                pct != last_percent
            }
            _ => last_emit.elapsed().as_millis() >= 500,
        };

        if should_emit {
            let elapsed = last_emit.elapsed();
            let speed_bps = if elapsed.as_secs_f64() > 0.0 {
                (speed_bytes as f64 / elapsed.as_secs_f64()) as u64
            } else {
                0
            };
            speed_bytes = 0;
            last_emit = std::time::Instant::now();
            if let Some(t) = total.filter(|&t| t > 0) {
                last_percent = bytes_done * 100 / t;
            }
            let _ = tx.send(DownloadEvent::Progress(ProgressEvent {
                id: id.to_string(),
                bytes_done,
                total_bytes: total,
                speed_bps,
            }));
        }
    }
    file.flush().await?;
    Ok(bytes_done)
}

async fn multi_thread_download(
    tx: &Sender<DownloadEvent>,
    client: &Client,
    id: &str,
    url: &str,
    dest_path: &str,
    total: u64,
    threads: u8,
) -> Result<u64> {
    let n = threads as u64;
    let chunk_size = total / n;
    let sem = Arc::new(Semaphore::new(threads as usize));
    let bytes_counter = Arc::new(AtomicU64::new(0));

    let tmp_dir = format!("{dest_path}.parts");
    tokio::fs::create_dir_all(&tmp_dir).await?;

    let mut handles = Vec::new();
    for i in 0..n {
        let start = i * chunk_size;
        let end = if i == n - 1 { total - 1 } else { start + chunk_size - 1 };
        let client = client.clone();
        let url = url.to_string();
        let part_path = format!("{tmp_dir}/{i}");
        let permit = sem.clone().acquire_owned().await?;
        let tx_clone = tx.clone();
        let id_clone = id.to_string();
        let bytes_counter = bytes_counter.clone();

        handles.push(tokio::spawn(async move {
            let _permit = permit;
            download_chunk(&tx_clone, &client, &url, &part_path, start, end, &id_clone, total, &bytes_counter).await
        }));
    }

    for handle in handles {
        handle.await.context("chunk task panicked")??;
    }

    merge_parts(&tmp_dir, dest_path, n as usize).await?;
    tokio::fs::remove_dir_all(&tmp_dir).await.ok();
    Ok(total)
}

async fn download_chunk(
    tx: &Sender<DownloadEvent>,
    client: &Client,
    url: &str,
    part_path: &str,
    start: u64,
    end: u64,
    id: &str,
    total: u64,
    bytes_counter: &Arc<AtomicU64>,
) -> Result<()> {
    let range = format!("bytes={start}-{end}");
    let mut resp = client.get(url).header("Range", range).send().await?.error_for_status()?;

    let mut file = File::create(part_path).await?;
    let mut last_emit = std::time::Instant::now();
    let mut since_last: u64 = 0;
    let mut last_percent: u64 = u64::MAX;

    while let Some(chunk) = resp.chunk().await? {
        let n = chunk.len() as u64;
        file.write_all(&chunk).await?;
        bytes_counter.fetch_add(n, Ordering::Relaxed);
        since_last += n;

        let bytes_done = bytes_counter.load(Ordering::Relaxed);
        let pct = bytes_done * 100 / total;
        if pct != last_percent {
            last_percent = pct;
            let elapsed = last_emit.elapsed();
            let speed_bps = if elapsed.as_secs_f64() > 0.0 {
                (since_last as f64 / elapsed.as_secs_f64()) as u64
            } else {
                0
            };
            since_last = 0;
            last_emit = std::time::Instant::now();
            let _ = tx.send(DownloadEvent::Progress(ProgressEvent {
                id: id.to_string(),
                bytes_done,
                total_bytes: Some(total),
                speed_bps,
            }));
        }
    }
    file.flush().await?;

    let bytes_done = bytes_counter.load(Ordering::Relaxed);
    let _ = tx.send(DownloadEvent::Progress(ProgressEvent {
        id: id.to_string(),
        bytes_done,
        total_bytes: Some(total),
        speed_bps: 0,
    }));

    Ok(())
}

async fn merge_parts(tmp_dir: &str, dest_path: &str, n: usize) -> Result<()> {
    let mut dest = File::create(dest_path).await?;
    for i in 0..n {
        let part = format!("{tmp_dir}/{i}");
        let mut src = File::open(&part).await?;
        tokio::io::copy(&mut src, &mut dest).await?;
    }
    dest.flush().await?;
    Ok(())
}
