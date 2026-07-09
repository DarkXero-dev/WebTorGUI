use anyhow::{anyhow, Result};
use librqbit::{AddTorrent, AddTorrentOptions, AddTorrentResponse, ManagedTorrent, Session, SessionOptions, SessionPersistenceConfig};
use std::path::PathBuf;
use std::sync::Arc;

/// A real BitTorrent engine (librqbit) - replaces the old honest-stub magnet
/// handling with actual peer-wire downloads. Also runs librqbit's bundled
/// HTTP server on a loopback port so mpv can stream an in-progress file over
/// a plain HTTP URL (mpv has no BitTorrent support of its own; librqbit's
/// `FileStream` reprioritizes pieces around whatever mpv reads/seeks to).
pub struct TorrentEngine {
    session: Arc<Session>,
    http_port: u16,
    default_output_folder: PathBuf,
}

pub enum AddSource {
    MagnetOrUrl(String),
    TorrentBytes(Vec<u8>),
}

pub struct AddedHandle {
    pub id: usize,
    pub handle: Arc<ManagedTorrent>,
    /// Absolute folder this torrent's files land in - a UUID-named
    /// sub-folder we assign ourselves (rather than relying on librqbit's own
    /// default naming, which isn't exposed anywhere we could read back), so
    /// completed files can be located and routed into their category folder.
    pub output_dir: PathBuf,
}

impl TorrentEngine {
    pub async fn new(default_output_folder: PathBuf) -> Result<Self> {
        // Off by default in librqbit - without it, every torrent (not just
        // our own UI's list of them) is forgotten by the session on
        // restart, regardless of anything we restore on our end.
        let persistence_folder = dirs::data_dir().map(|d| d.join("webtorapp").join("torrent-session"));
        let opts = SessionOptions {
            persistence: Some(SessionPersistenceConfig::Json { folder: persistence_folder }),
            fastresume: true,
            ..Default::default()
        };
        let session = Session::new_with_opts(default_output_folder.clone(), opts)
            .await
            .map_err(|e| anyhow!("could not start torrent session: {e}"))?;

        let api = librqbit::Api::new(session.clone(), None, None);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let http_port = listener.local_addr()?.port();
        let http_api = librqbit::http_api::HttpApi::new(api, None);
        tokio::spawn(async move {
            let _ = http_api.make_http_api_and_run(listener, None).await;
        });

        Ok(Self { session, http_port, default_output_folder })
    }

    /// Adds the torrent *paused* - metadata resolves (for a magnet, that's
    /// the slow part) but no pieces download yet. The caller is expected to
    /// let the user pick which files they want, then call
    /// [`Self::start_download`] - otherwise nothing beyond metadata ever
    /// gets fetched.
    pub async fn add(&self, source: AddSource) -> Result<AddedHandle> {
        let add = match source {
            AddSource::MagnetOrUrl(s) => AddTorrent::from_url(s),
            AddSource::TorrentBytes(bytes) => AddTorrent::from_bytes(bytes),
        };
        let sub_folder = uuid::Uuid::new_v4().to_string();
        let output_dir = self.default_output_folder.join(&sub_folder);
        let opts = AddTorrentOptions { sub_folder: Some(sub_folder), overwrite: true, paused: true, ..Default::default() };
        let resp = self.session.add_torrent(add, Some(opts)).await.map_err(|e| anyhow!("{e}"))?;
        match resp {
            AddTorrentResponse::Added(id, handle) | AddTorrentResponse::AlreadyManaged(id, handle) => Ok(AddedHandle { id, handle, output_dir }),
            AddTorrentResponse::ListOnly(_) => Err(anyhow!("unexpected list-only response")),
        }
    }

    /// Unpauses a torrent added by [`Self::add`], so it actually starts
    /// fetching whichever files `set_only_files` left selected (everything,
    /// if that was never called). Also what resumes a torrent paused later
    /// on via [`Self::pause_download`].
    pub fn start_download(&self, handle: Arc<ManagedTorrent>) {
        let session = self.session.clone();
        tokio::spawn(async move {
            let _ = session.unpause(&handle).await;
        });
    }

    /// Pauses an already-downloading torrent - stops requesting new pieces
    /// without dropping anything already fetched, resumable later via
    /// [`Self::start_download`].
    pub fn pause_download(&self, handle: Arc<ManagedTorrent>) {
        let session = self.session.clone();
        tokio::spawn(async move {
            let _ = session.pause(&handle).await;
        });
    }

    /// An HTTP URL serving `file_id` within `torrent_id`, live, with Range
    /// support - safe to hand to mpv or `open::that` even before the torrent
    /// finishes downloading.
    pub fn stream_url(&self, torrent_id: usize, file_id: usize) -> String {
        format!("http://127.0.0.1:{}/torrents/{torrent_id}/stream/{file_id}", self.http_port)
    }

    /// `delete_files` controls whether the downloaded data on disk is
    /// removed along with the torrent, or just the torrent's entry in the
    /// session (leaving whatever was already downloaded in place).
    pub fn remove(&self, torrent_id: usize, delete_files: bool) {
        let session = self.session.clone();
        tokio::spawn(async move {
            let _ = session.delete(torrent_id.into(), delete_files).await;
        });
    }

    /// Torrents librqbit's own session persistence already resumed before we
    /// got a chance to add anything this run - restores our own display
    /// metadata (title, source, output dir) for each from the `torrents`
    /// table, since librqbit's persistence knows nothing about it. A torrent
    /// with no matching row (added before this feature existed) still shows
    /// up, with its raw torrent name and an output dir under our default
    /// download folder as a best-effort fallback.
    pub fn list_existing(&self, db_conn: &rusqlite::Connection) -> Vec<AddedHandle> {
        self.session.with_torrents(|iter| {
            iter.map(|(id, handle)| {
                let info_hash = handle.info_hash().as_string();
                let output_dir = crate::downloads::torrents::get(db_conn, &info_hash)
                    .map(|s| PathBuf::from(s.output_dir))
                    .unwrap_or_else(|| self.default_output_folder.clone());
                AddedHandle { id, handle: handle.clone(), output_dir }
            })
            .collect()
        })
    }

    /// Restricts which files in the torrent actually download - unselected
    /// files' pieces are simply never fetched, not downloaded then deleted.
    pub fn set_only_files(&self, handle: Arc<ManagedTorrent>, file_ids: std::collections::HashSet<usize>) {
        let session = self.session.clone();
        tokio::spawn(async move {
            let _ = session.update_only_files(&handle, &file_ids).await;
        });
    }
}
