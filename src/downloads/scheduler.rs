use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use tokio::time::{interval, Duration};

use crate::db;
use crate::downloads::engine::{self, DownloadEvent};
use crate::downloads::queue;
use crate::settings::AppSettings;

pub fn start(settings: Arc<Mutex<AppSettings>>, tx: Sender<DownloadEvent>) {
    tokio::spawn(async move {
        let mut ticker = interval(Duration::from_secs(3));
        loop {
            ticker.tick().await;
            if let Err(e) = tick(&settings, &tx) {
                eprintln!("scheduler tick error: {e}");
            }
        }
    });
}

/// Run one scheduling pass immediately (called right after a user queues a
/// download, so it doesn't wait for the next periodic tick).
pub fn tick_now(settings: Arc<Mutex<AppSettings>>, tx: Sender<DownloadEvent>) {
    tokio::spawn(async move {
        if let Err(e) = tick(&settings, &tx) {
            eprintln!("scheduler tick error: {e}");
        }
    });
}

fn tick(settings: &Arc<Mutex<AppSettings>>, tx: &Sender<DownloadEvent>) -> anyhow::Result<()> {
    let s = settings.lock().unwrap().clone();
    if s.quiet_hours_enabled && in_quiet_hours(&s) {
        return Ok(());
    }

    let conn = db::open()?;
    let ready = queue::get_queued_ready(&conn)?;

    let active_count: usize = queue::get_all(&conn)?
        .iter()
        .filter(|d| d.status == queue::DownloadStatus::Active)
        .count();

    let slots = (s.max_concurrent_downloads as usize).saturating_sub(active_count);
    if slots == 0 {
        return Ok(());
    }

    for item in ready.into_iter().take(slots) {
        queue::update_status(&conn, &item.id, queue::DownloadStatus::Active)?;

        let tx_clone = tx.clone();
        let id = item.id.clone();
        let url = item.url.clone();
        let dest_path = item.dest_path.clone();
        let threads = item.threads;
        tokio::spawn(async move {
            if let Err(e) = engine::download_file(tx_clone.clone(), id.clone(), url, dest_path, threads).await {
                let _ = tx_clone.send(DownloadEvent::Error(engine::ErrorEvent {
                    id,
                    error: e.to_string(),
                }));
            }
        });
    }

    Ok(())
}

fn in_quiet_hours(s: &AppSettings) -> bool {
    let Some(ref start) = s.quiet_hours_start else { return false };
    let Some(ref end) = s.quiet_hours_end else { return false };

    let now = chrono::Local::now();
    let current = format!("{:02}:{:02}", chrono::Timelike::hour(&now), chrono::Timelike::minute(&now));

    if start <= end {
        current >= *start && current < *end
    } else {
        current >= *start || current < *end
    }
}
