use anyhow::Result;
use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::mpsc;

use sanitize_filename::{sanitize_with_options, Options};

use crate::config::{Config, DownloadNewEpisodes};
use crate::db::{Database, SyncResult};
use crate::downloads::{self, DownloadMsg, EpData};
use crate::feeds::{self, FeedMsg, PodcastFeed};
use crate::play_file;
use crate::threadpool::Threadpool;
use crate::types::*;
use crate::ui::{Ui, UiMsg};

/// Enum used for communicating with other threads.
#[allow(clippy::enum_variant_names)]
#[derive(Debug)]
pub enum MainMessage {
    UiUpdateMenus,
    UiSpawnNotif(String, bool, u64),
    UiSpawnPersistentNotif(String, bool),
    UiClearPersistentNotif,
    UiSpawnDownloadPopup(Vec<NewEpisode>, bool),
    UiTearDown,
}

/// Main application controller, holding all of the main application
/// state and mechanisms for communicatingg with the rest of the app.
pub struct MainController {
    config: Config,
    db: Database,
    threadpool: Threadpool,
    podcasts: LockVec<Podcast>,
    filters: Filters,
    sync_counter: usize,
    sync_tracker: Vec<SyncResult>,
    download_tracker: HashSet<i64>,
    pub ui_thread: std::thread::JoinHandle<()>,
    pub tx_to_ui: mpsc::Sender<MainMessage>,
    pub tx_to_main: mpsc::Sender<Message>,
    pub rx_to_main: mpsc::Receiver<Message>,
}

impl MainController {
    /// Instantiates the main controller (used during app startup), which
    /// sets up the connection to the database, download manager, and UI
    /// thread, and reads the list of podcasts from the database.
    pub fn new(config: Config, db_path: &Path) -> Result<MainController> {
        // create transmitters and receivers for passing messages between threads
        let (tx_to_ui, rx_from_main) = mpsc::channel();
        let (tx_to_main, rx_to_main) = mpsc::channel();

        // get connection to the database
        let db_inst = Database::connect(db_path)?;

        // set up threadpool
        let threadpool = Threadpool::new(config.simultaneous_downloads);

        // create vector of podcasts, where references are checked at
        // runtime; this is necessary because we want main.rs to hold the
        // "ground truth" list of podcasts, and it must be mutable, but
        // UI needs to check this list and update the screen when
        // necessary
        let podcast_list = LockVec::new(db_inst.get_podcasts()?);

        // set up UI in new thread
        let tx_ui_to_main = mpsc::Sender::clone(&tx_to_main);
        let ui_thread = Ui::spawn(
            config.clone(),
            podcast_list.clone(),
            rx_from_main,
            tx_ui_to_main,
        );
        // TODO: Can we do this without cloning the config?

        return Ok(MainController {
            config: config,
            db: db_inst,
            threadpool: threadpool,
            podcasts: podcast_list,
            filters: Filters::default(),
            ui_thread: ui_thread,
            sync_counter: 0,
            sync_tracker: Vec::new(),
            download_tracker: HashSet::new(),
            tx_to_ui: tx_to_ui,
            tx_to_main: tx_to_main,
            rx_to_main: rx_to_main,
        });
    }

    /// Initiates the main loop where the controller waits for messages coming in from the UI and other threads, and processes them.
    pub fn loop_msgs(&mut self) {
        while let Some(message) = self.rx_to_main.iter().next() {
            match message {
                Message::Ui(UiMsg::Quit) => break,

                Message::Ui(UiMsg::AddFeed(url)) => self.add_podcast(url),

                Message::Feed(FeedMsg::NewData(pod)) => self.add_or_sync_data(pod, None),

                Message::Feed(FeedMsg::Error(feed)) => match feed.title {
                    Some(t) => {
                        self.sync_counter -= 1;
                        self.notif_to_ui(format!("Error retrieving RSS feed for {t}."), true)
                    }
                    None => self.notif_to_ui("Error retrieving RSS feed.".to_string(), true),
                },

                Message::Ui(UiMsg::Sync(pod_id)) => self.sync(Some(pod_id)),

                Message::Feed(FeedMsg::SyncData((id, pod))) => self.add_or_sync_data(pod, Some(id)),

                Message::Ui(UiMsg::SyncAll) => self.sync(None),

                Message::Ui(UiMsg::Play(pod_id, ep_id)) => self.play_file(pod_id, ep_id),

                Message::Ui(UiMsg::MarkPlayed(pod_id, ep_id, played)) => {
                    self.mark_played(pod_id, ep_id, played)
                }

                Message::Ui(UiMsg::MarkAllPlayed(pod_id, played)) => {
                    self.mark_all_played(pod_id, played)
                }

                Message::Ui(UiMsg::Download(pod_id, ep_id)) => self.download(pod_id, Some(ep_id)),

                Message::Ui(UiMsg::DownloadMulti(vec)) => {
                    for (pod_id, ep_id) in vec.into_iter() {
                        self.download(pod_id, Some(ep_id));
                    }
                }

                Message::Ui(UiMsg::DownloadAll(pod_id)) => self.download(pod_id, None),

                // downloading can produce any one of these responses
                Message::Dl(DownloadMsg::Complete(ep_data)) => self.download_complete(ep_data),
                Message::Dl(DownloadMsg::ResponseError(_)) => {
                    self.notif_to_ui("Error sending download request.".to_string(), true)
                }
                Message::Dl(DownloadMsg::FileCreateError(_)) => {
                    self.notif_to_ui("Error creating file.".to_string(), true)
                }
                Message::Dl(DownloadMsg::FileWriteError(_)) => {
                    self.notif_to_ui("Error downloading episode.".to_string(), true)
                }

                Message::Ui(UiMsg::Delete(pod_id, ep_id)) => self.delete_file(pod_id, ep_id),

                Message::Ui(UiMsg::DeleteAll(pod_id)) => self.delete_files(pod_id),

                Message::Ui(UiMsg::RemovePodcast(pod_id, delete_files)) => {
                    self.remove_podcast(pod_id, delete_files)
                }

                Message::Ui(UiMsg::RemoveEpisode(pod_id, ep_id, delete_files)) => {
                    self.remove_episode(pod_id, ep_id, delete_files)
                }

                Message::Ui(UiMsg::RemoveAllEpisodes(pod_id, delete_files)) => {
                    self.remove_all_episodes(pod_id, delete_files)
                }

                Message::Ui(UiMsg::FilterChange(filter_type)) => {
                    let new_filter;
                    let message;
                    match filter_type {
                        // we need to handle these separately because the
                        // order that makes the most sense to me is
                        // different:
                        // played goes from all -> neg -> pos;
                        // downloaded goes from all -> pos -> neg;
                        // this is purely based on the idea that people
                        // are most likely to want to specifically find
                        // unplayed episodes, or downloaded episodes
                        FilterType::Played => {
                            match self.filters.played {
                                FilterStatus::All => {
                                    new_filter = FilterStatus::NegativeCases;
                                    message = "Unplayed only";
                                }
                                FilterStatus::NegativeCases => {
                                    new_filter = FilterStatus::PositiveCases;
                                    message = "Played only";
                                }
                                FilterStatus::PositiveCases => {
                                    new_filter = FilterStatus::All;
                                    message = "Played and unplayed";
                                }
                            }
                            self.filters.played = new_filter;
                        }
                        FilterType::Downloaded => {
                            match self.filters.downloaded {
                                FilterStatus::All => {
                                    new_filter = FilterStatus::PositiveCases;
                                    message = "Downloaded only";
                                }
                                FilterStatus::PositiveCases => {
                                    new_filter = FilterStatus::NegativeCases;
                                    message = "Undownloaded only";
                                }
                                FilterStatus::NegativeCases => {
                                    new_filter = FilterStatus::All;
                                    message = "Downloaded and undownloaded";
                                }
                            }
                            self.filters.downloaded = new_filter;
                        }
                    }
                    self.notif_to_ui(format!("Filter: {message}"), false);
                    self.update_filters(self.filters, true);
                }

                Message::Ui(UiMsg::Noop) => (),
            }
        }
    }

    /// Sends the specified notification to the UI, which will display at
    /// the bottom of the screen.
    pub fn notif_to_ui(&self, message: String, error: bool) {
        self.tx_to_ui
            .send(MainMessage::UiSpawnNotif(
                message,
                error,
                crate::config::MESSAGE_TIME,
            ))
            .expect("Thread messaging error");
    }

    /// Sends a persistent notification to the UI, which will display at
    /// the bottom of the screen until cleared.
    pub fn persistent_notif_to_ui(&self, message: String, error: bool) {
        self.tx_to_ui
            .send(MainMessage::UiSpawnPersistentNotif(message, error))
            .expect("Thread messaging error");
    }

    /// Clears persistent notifications in the UI.
    pub fn clear_persistent_notif(&self) {
        self.tx_to_ui
            .send(MainMessage::UiClearPersistentNotif)
            .expect("Thread messaging error");
    }

    /// Updates the persistent notification about syncing podcasts and
    /// downloading files.
    pub fn update_tracker_notif(&self) {
        let sync_len = self.sync_counter;
        let dl_len = self.download_tracker.len();
        let sync_plural = if sync_len > 1 { "s" } else { "" };
        let dl_plural = if dl_len > 1 { "s" } else { "" };

        if sync_len > 0 && dl_len > 0 {
            let notif = format!(
                "Syncing {sync_len} podcast{sync_plural}, downloading {dl_len} episode{dl_plural}...");
            self.persistent_notif_to_ui(notif, false);
        } else if sync_len > 0 {
            let notif = format!("Syncing {sync_len} podcast{sync_plural}...");
            self.persistent_notif_to_ui(notif, false);
        } else if dl_len > 0 {
            let notif = format!("Downloading {dl_len} episode{dl_plural}...");
            self.persistent_notif_to_ui(notif, false);
        } else {
            self.clear_persistent_notif();
        }
    }

    /// Add a new podcast by fetching the RSS feed data.
    pub fn add_podcast(&self, url: String) {
        let feed = PodcastFeed::new(None, url, None);
        feeds::check_feed(
            feed,
            self.config.max_retries,
            &self.threadpool,
            self.tx_to_main.clone(),
        );
    }

    /// Synchronize RSS feed data for one or more podcasts.
    pub fn sync(&mut self, pod_id: Option<i64>) {
        // We pull out the data we need here first, so we can
        // stop borrowing the podcast list as quickly as possible.
        // Slightly less efficient (two loops instead of
        // one), but then it won't block other tasks that
        // need to access the list.
        let mut pod_data = Vec::new();
        match pod_id {
            // just grab one podcast
            Some(id) => pod_data.push(
                self.podcasts
                    .map_single(id, |pod| {
                        PodcastFeed::new(Some(pod.id), pod.url.clone(), Some(pod.title.clone()))
                    })
                    .unwrap(),
            ),
            // get all of 'em!
            None => {
                pod_data = self.podcasts.map(
                    |pod| PodcastFeed::new(Some(pod.id), pod.url.clone(), Some(pod.title.clone())),
                    false,
                )
            }
        }
        for feed in pod_data.into_iter() {
            self.sync_counter += 1;
            feeds::check_feed(
                feed,
                self.config.max_retries,
                &self.threadpool,
                self.tx_to_main.clone(),
            )
        }
        self.update_tracker_notif();
    }

    /// Handles the application logic for adding a new podcast, or
    /// synchronizing data from the RSS feed of an existing podcast.
    /// `pod_id` will be None if a new podcast is being added (i.e.,
    /// the database has not given it an id yet).
    pub fn add_or_sync_data(&mut self, pod: PodcastNoId, pod_id: Option<i64>) {
        let title = pod.title.clone();
        let db_result;
        let failure;

        if let Some(id) = pod_id {
            db_result = self.db.update_podcast(id, pod);
            failure = format!("Error synchronizing {title}.");
        } else {
            db_result = self.db.insert_podcast(pod);
            failure = "Error adding podcast to database.".to_string();
        }
        match db_result {
            Ok(result) => {
                {
                    self.podcasts.replace_all(
                        self.db
                            .get_podcasts()
                            .expect("Error retrieving info from database."),
                    );
                }
                self.update_filters(self.filters, true);

                if pod_id.is_some() {
                    self.sync_tracker.push(result);
                    self.sync_counter -= 1;
                    self.update_tracker_notif();

                    if self.sync_counter == 0 {
                        // count up total new episodes and updated
                        // episodes when sync process is finished
                        let mut added = 0;
                        let mut updated = 0;
                        let mut new_eps = Vec::new();
                        for res in self.sync_tracker.iter() {
                            added += res.added.len();
                            updated += res.updated.len();
                            new_eps.extend(res.added.clone());
                        }
                        self.sync_tracker = Vec::new();
                        self.notif_to_ui(
                            format!("Sync complete: Added {added}, updated {updated} episodes."),
                            false,
                        );

                        // deal with new episodes once syncing is
                        // complete, based on user preferences
                        if !new_eps.is_empty() {
                            match self.config.download_new_episodes {
                                DownloadNewEpisodes::Always => {
                                    for ep in new_eps.into_iter() {
                                        self.download(ep.pod_id, Some(ep.id));
                                    }
                                }
                                DownloadNewEpisodes::AskSelected => {
                                    self.tx_to_ui
                                        .send(MainMessage::UiSpawnDownloadPopup(new_eps, true))
                                        .expect("Thread messaging error");
                                }
                                DownloadNewEpisodes::AskUnselected => {
                                    self.tx_to_ui
                                        .send(MainMessage::UiSpawnDownloadPopup(new_eps, false))
                                        .expect("Thread messaging error");
                                }
                                _ => (),
                            }
                        }
                    }
                } else {
                    self.notif_to_ui(
                        format!("Successfully added {} episodes.", result.added.len()),
                        false,
                    );
                }
            }
            Err(_err) => self.notif_to_ui(failure, true),
        }
    }

    /// Attempts to execute the play command on the given podcast
    /// episode.
    pub fn play_file(&self, pod_id: i64, ep_id: i64) {
        self.mark_played(pod_id, ep_id, true);
        let episode = self.podcasts.clone_episode(pod_id, ep_id).unwrap();

        match episode.path {
            // if there is a local file, try to play that
            Some(path) => match path.to_str() {
                Some(p) => {
                    if play_file::execute(&self.config.play_command, p).is_err() {
                        self.notif_to_ui(
                            "Error: Could not play file. Check configuration.".to_string(),
                            true,
                        );
                    }
                }
                None => self.notif_to_ui("Error: Filepath is not valid Unicode.".to_string(), true),
            },
            // otherwise, try to stream the URL
            None => {
                if play_file::execute(&self.config.play_command, &episode.url).is_err() {
                    self.notif_to_ui("Error: Could not stream URL.".to_string(), true);
                }
            }
        }
    }

    /// Given a podcast and episode, it marks the given episode as
    /// played/unplayed, sending this info to the database and updating
    /// in self.podcasts
    pub fn mark_played(&self, pod_id: i64, ep_id: i64, played: bool) {
        let podcast = self.podcasts.clone_podcast(pod_id).unwrap();

        // TODO: Try to find a way to do this without having
        // to clone the episode...
        let mut episode = podcast.episodes.clone_episode(ep_id).unwrap();
        episode.played = played;

        let _ = self.db.set_played_status(episode.id, played);
        podcast.episodes.replace(ep_id, episode);

        self.podcasts.replace(pod_id, podcast);
        self.update_filters(self.filters, true);
    }

    /// Given a podcast, it marks all episodes for that podcast as
    /// played/unplayed, sending this info to the database and updating
    /// in self.podcasts
    pub fn mark_all_played(&self, pod_id: i64, played: bool) {
        let podcast = self.podcasts.clone_podcast(pod_id).unwrap();
        {
            let borrowed_ep_list = podcast.episodes.borrow_order();
            for ep in borrowed_ep_list.iter() {
                let _ = self.db.set_played_status(*ep, played);
            }
        }
        podcast.episodes.replace_all(
            self.db
                .get_episodes(podcast.id, false)
                .expect("Error retrieving info from database."),
        );

        self.podcasts.replace(pod_id, podcast);
        self.update_filters(self.filters, true);
    }

    /// Given a podcast index (and not an episode index), this will send
    /// a vector of jobs to the threadpool to download all episodes in
    /// the podcast. If given an episode index as well, it will download
    /// just that episode.
    pub fn download(&mut self, pod_id: i64, ep_id: Option<i64>) {
        let pod_title;
        let mut ep_data = Vec::new();
        {
            let borrowed_map = self.podcasts.borrow_map();
            let podcast = borrowed_map.get(&pod_id).unwrap();
            pod_title = podcast.title.clone();

            // if we are selecting one specific episode, just grab that
            // one; otherwise, loop through them all
            match ep_id {
                Some(ep_id) => {
                    // grab just the relevant data we need
                    let data = podcast
                        .episodes
                        .map_single(ep_id, |ep| {
                            (
                                EpData {
                                    id: ep.id,
                                    pod_id: ep.pod_id,
                                    title: ep.title.clone(),
                                    url: ep.url.clone(),
                                    pubdate: ep.pubdate,
                                    file_path: None,
                                },
                                ep.path.is_none(),
                            )
                        })
                        .unwrap();
                    if data.1 {
                        ep_data.push(data.0);
                    }
                }
                None => {
                    // grab just the relevant data we need
                    ep_data = podcast.episodes.filter_map(|ep| {
                        if ep.path.is_none() {
                            Some(EpData {
                                id: ep.id,
                                pod_id: ep.pod_id,
                                title: ep.title.clone(),
                                url: ep.url.clone(),
                                pubdate: ep.pubdate,
                                file_path: None,
                            })
                        } else {
                            None
                        }
                    });
                }
            }
        }

        // check against episodes currently being downloaded -- so we
        // don't needlessly download them again
        ep_data.retain(|ep| !self.download_tracker.contains(&ep.id));

        if !ep_data.is_empty() {
            // add directory for podcast, create if it does not exist
            let dir_name = sanitize_with_options(&pod_title, Options {
                truncate: true,
                windows: true, // for simplicity, we'll just use Windows-friendly paths for everyone
                replacement: "",
            });
            match self.create_podcast_dir(dir_name) {
                Ok(path) => {
                    for ep in ep_data.iter() {
                        self.download_tracker.insert(ep.id);
                    }
                    downloads::download_list(
                        ep_data,
                        &path,
                        self.config.max_retries,
                        &self.threadpool,
                        self.tx_to_main.clone(),
                    );
                }
                Err(_) => self.notif_to_ui(format!("Could not create dir: {pod_title}"), true),
            }
            self.update_tracker_notif();
        }
    }

    /// Handles logic for what to do when a download successfully completes.
    pub fn download_complete(&mut self, ep_data: EpData) {
        let file_path = ep_data.file_path.unwrap();
        let res = self.db.insert_file(ep_data.id, &file_path);
        if res.is_err() {
            self.notif_to_ui(
                format!(
                    "Could not add episode file to database: {}",
                    file_path.to_string_lossy()
                ),
                true,
            );
            return;
        }
        {
            // TODO: Try to do this without cloning the podcast...
            let podcast = self.podcasts.clone_podcast(ep_data.pod_id).unwrap();
            let mut episode = podcast.episodes.clone_episode(ep_data.id).unwrap();
            episode.path = Some(file_path);
            podcast.episodes.replace(ep_data.id, episode);
        }

        self.download_tracker.remove(&ep_data.id);
        self.update_tracker_notif();
        if self.download_tracker.is_empty() {
            self.notif_to_ui("Downloads complete.".to_string(), false);
        }

        self.update_filters(self.filters, true);
    }

    /// Given a podcast title, creates a download directory for that
    /// podcast if it does not already exist.
    pub fn create_podcast_dir(&self, pod_title: String) -> Result<PathBuf, std::io::Error> {
        let mut download_path = self.config.download_path.clone();
        download_path.push(pod_title);
        return match std::fs::create_dir_all(&download_path) {
            Ok(_) => Ok(download_path),
            Err(err) => Err(err),
        };
    }

    /// Deletes a downloaded file for an episode from the user's local
    /// system.
    pub fn delete_file(&self, pod_id: i64, ep_id: i64) {
        let borrowed_map = self.podcasts.borrow_map();
        let podcast = borrowed_map.get(&pod_id).unwrap();

        let mut episode = podcast.episodes.clone_episode(ep_id).unwrap();
        if episode.path.is_some() {
            let title = episode.title.clone();
            match fs::remove_file(episode.path.unwrap()) {
                Ok(_) => {
                    let res = self.db.remove_file(episode.id);
                    if res.is_err() {
                        self.notif_to_ui(
                            format!("Could not remove file from database: {title}"),
                            true,
                        );
                        return;
                    }
                    episode.path = None;
                    podcast.episodes.replace(ep_id, episode);

                    self.update_filters(self.filters, true);
                    self.notif_to_ui(format!("Deleted \"{title}\""), false);
                }
                Err(_) => self.notif_to_ui(format!("Error deleting \"{title}\""), true),
            }
        }
    }

    /// Deletes all downloaded files for a given podcast from the user's
    /// local system.
    pub fn delete_files(&self, pod_id: i64) {
        let mut eps_to_remove = Vec::new();
        let mut success = true;
        {
            let borrowed_map = self.podcasts.borrow_map();
            let podcast = borrowed_map.get(&pod_id).unwrap();
            let mut borrowed_ep_map = podcast.episodes.borrow_map();

            for (_, ep) in borrowed_ep_map.iter_mut() {
                if ep.path.is_some() {
                    let mut episode = ep.clone();
                    match fs::remove_file(episode.path.unwrap()) {
                        Ok(_) => {
                            eps_to_remove.push(episode.id);
                            episode.path = None;
                            *ep = episode;
                        }
                        Err(_) => success = false,
                    }
                }
            }
        }

        let res = self.db.remove_files(&eps_to_remove);
        if res.is_err() {
            success = false;
        }
        self.update_filters(self.filters, true);

        if success {
            self.notif_to_ui("Files successfully deleted.".to_string(), false);
        } else {
            self.notif_to_ui("Error while deleting files".to_string(), true);
        }
    }

    /// Removes a podcast from the list, optionally deleting local files
    /// first
    pub fn remove_podcast(&mut self, pod_id: i64, delete_files: bool) {
        if delete_files {
            self.delete_files(pod_id);
        }

        let pod_id = self.podcasts.map_single(pod_id, |pod| pod.id).unwrap();
        let res = self.db.remove_podcast(pod_id);
        if res.is_err() {
            self.notif_to_ui("Could not remove podcast from database".to_string(), true);
            return;
        }
        {
            self.podcasts.replace_all(
                self.db
                    .get_podcasts()
                    .expect("Error retrieving info from database."),
            );
        }
        self.tx_to_ui
            .send(MainMessage::UiUpdateMenus)
            .expect("Thread messaging error");
    }

    /// Removes an episode from the list, optionally deleting local files
    /// first
    pub fn remove_episode(&self, pod_id: i64, ep_id: i64, delete_files: bool) {
        if delete_files {
            self.delete_file(pod_id, ep_id);
        }

        let _ = self.db.hide_episode(ep_id, true);
        {
            let mut borrowed_map = self.podcasts.borrow_map();
            let podcast = borrowed_map.get_mut(&pod_id).unwrap();
            podcast.episodes.replace_all(
                self.db
                    .get_episodes(pod_id, false)
                    .expect("Error retrieving info from database."),
            );
        }
        self.tx_to_ui
            .send(MainMessage::UiUpdateMenus)
            .expect("Thread messaging error");
    }

    /// Removes all episodes for a podcast from the list, optionally
    /// deleting local files first
    pub fn remove_all_episodes(&self, pod_id: i64, delete_files: bool) {
        if delete_files {
            self.delete_files(pod_id);
        }

        let mut podcast = self.podcasts.clone_podcast(pod_id).unwrap();
        podcast.episodes.map(
            |ep| {
                let _ = self.db.hide_episode(ep.id, true);
            },
            false,
        );
        podcast.episodes = LockVec::new(Vec::new());
        self.podcasts.replace(pod_id, podcast);

        self.tx_to_ui
            .send(MainMessage::UiUpdateMenus)
            .expect("Thread messaging error");
    }

    /// Updates the user-selected filters to show only played/unplayed
    /// or downloaded/not downloaded episodes.
    pub fn update_filters(&self, filters: Filters, update_menus: bool) {
        {
            let (pod_map, pod_order, mut pod_filtered_order) = self.podcasts.borrow();
            let mut new_filtered_pods = Vec::new();
            for pod_id in pod_order.iter() {
                let pod = pod_map.get(pod_id).unwrap();
                let new_filter = pod.episodes.filter_map(|ep| {
                    let play_filter = match filters.played {
                        FilterStatus::All => false,
                        FilterStatus::PositiveCases => !ep.is_played(),
                        FilterStatus::NegativeCases => ep.is_played(),
                    };
                    let download_filter = match filters.downloaded {
                        FilterStatus::All => false,
                        FilterStatus::PositiveCases => ep.path.is_none(),
                        FilterStatus::NegativeCases => ep.path.is_some(),
                    };
                    if !(play_filter | download_filter) {
                        return Some(ep.id);
                    } else {
                        return None;
                    }
                });
                if !new_filter.is_empty() {
                    new_filtered_pods.push(pod.id);
                }
                let mut filtered_order = pod.episodes.borrow_filtered_order();
                *filtered_order = new_filter;
            }
            *pod_filtered_order = new_filtered_pods;
        }
        if update_menus {
            self.tx_to_ui
                .send(MainMessage::UiUpdateMenus)
                .expect("Thread messaging error");
        }
    }
}
