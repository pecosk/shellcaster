use std::io::{self, Write};
use std::rc::Rc;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use crossterm::{
    self, cursor,
    event::{self, Event, MouseButton, MouseEvent, MouseEventKind, EnableMouseCapture, DisableMouseCapture},
    execute, terminal,
};
use lazy_static::lazy_static;
use regex::Regex;

#[cfg_attr(not(test), path = "panel.rs")]
#[cfg_attr(test, path = "mock_panel.rs")]
mod panel;

pub mod colors;
mod details_panel;
mod menu;
mod notification;
mod popup;

use self::colors::AppColors;
use self::details_panel::{Details, DetailsPanel};
use self::menu::Menu;
use self::notification::NotifWin;
use self::panel::Panel;
use self::popup::PopupWin;

use super::MainMessage;
use crate::config::Config;
use crate::keymap::{Keybindings, UserAction};
use crate::types::*;

/// Amount of time between ticks in the event loop
const TICK_RATE: u64 = 20;

lazy_static! {
    /// Regex for finding <br/> tags -- also captures any surrounding
    /// line breaks
    static ref RE_BR_TAGS: Regex = Regex::new(r"((\r\n)|\r|\n)*<br */?>((\r\n)|\r|\n)*").expect("Regex error");

    /// Regex for finding HTML tags
    static ref RE_HTML_TAGS: Regex = Regex::new(r"<[^<>]*>").expect("Regex error");

    /// Regex for finding more than two line breaks
    static ref RE_MULT_LINE_BREAKS: Regex = Regex::new(r"((\r\n)|\r|\n){3,}").expect("Regex error");
}


/// Enum used for communicating back to the main controller after user
/// input has been captured by the UI. usize values always represent the
/// selected podcast, and (if applicable), the selected episode, in that
/// order.
#[derive(Debug)]
pub enum UiMsg {
    AddFeed(String),
    Play(i64, i64),
    MarkPlayed(i64, i64, bool),
    MarkAllPlayed(i64, bool),
    Sync(i64),
    SyncAll,
    Download(i64, i64),
    DownloadMulti(Vec<(i64, i64)>),
    DownloadAll(i64),
    Delete(i64, i64),
    DeleteAll(i64),
    RemovePodcast(i64, bool),
    RemoveEpisode(i64, i64, bool),
    RemoveAllEpisodes(i64, bool),
    FilterChange(FilterType),
    Quit,
    Noop,
}

/// Holds a value for how much to scroll the menu up or down, without
/// having to deal with positive/negative values.
pub enum Scroll {
    Up(u16),
    Down(u16),
}

/// Simple enum to identify which menu is currently active.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ActivePanel {
    PodcastMenu,
    EpisodeMenu,
    DetailsPanel,
}

/// Mouse interaction state for tracking mousedown/drag/mouseup events
#[derive(Debug, Clone)]
struct MouseState {
    is_mouse_down: bool,
    mouse_down_panel: Option<ActivePanel>,
    mouse_down_row: Option<u16>,
    highlighted_during_drag: Option<u16>,
}

impl Default for MouseState {
    fn default() -> Self {
        Self {
            is_mouse_down: false,
            mouse_down_panel: None,
            mouse_down_row: None,
            highlighted_during_drag: None,
        }
    }
}

/// Struct containing all interface elements of the TUI. Functionally,
/// it encapsulates the terminal menus and panels, and holds data about
/// the size of the screen.
#[derive(Debug)]
pub struct Ui<'a> {
    n_row: u16,
    n_col: u16,
    keymap: &'a Keybindings,
    colors: Rc<AppColors>,
    podcast_menu: Menu<Podcast>,
    episode_menu: Menu<Episode>,
    details_panel: Option<DetailsPanel>,
    active_panel: ActivePanel,
    notif_win: NotifWin,
    popup_win: PopupWin<'a>,
    mouse_state: MouseState,
}

impl<'a> Ui<'a> {
    /// Spawns a UI object in a new thread, with message channels to send
    /// and receive messages
    pub fn spawn(
        config: Config,
        items: LockVec<Podcast>,
        rx_from_main: mpsc::Receiver<MainMessage>,
        tx_to_main: mpsc::Sender<Message>,
    ) -> thread::JoinHandle<()> {
        return thread::spawn(move || {
            let mut ui = Ui::new(&config, items);
            ui.init();
            let mut message_iter = rx_from_main.try_iter();
            // this is the main event loop: on each loop, we update
            // any messages at the bottom, check for user input, and
            // then process any messages from the main thread
            loop {
                ui.notif_win.check_notifs();

                match ui.getch() {
                    UiMsg::Noop => (),
                    input => tx_to_main
                        .send(Message::Ui(input))
                        .expect("Thread messaging error"),
                }

                if let Some(message) = message_iter.next() {
                    match message {
                        MainMessage::UiUpdateMenus => ui.update_menus(),
                        MainMessage::UiSpawnNotif(msg, duration, error) => {
                            ui.timed_notif(msg, error, duration)
                        }
                        MainMessage::UiSpawnPersistentNotif(msg, error) => {
                            ui.persistent_notif(msg, error)
                        }
                        MainMessage::UiClearPersistentNotif => ui.clear_persistent_notif(),
                        MainMessage::UiTearDown => {
                            ui.tear_down();
                            break;
                        }
                        MainMessage::UiSpawnDownloadPopup(episodes, selected) => {
                            ui.popup_win.spawn_download_win(episodes, selected);
                        }
                    }
                }

                io::stdout().flush().unwrap();

                // slight delay to avoid excessive CPU usage
                thread::sleep(Duration::from_millis(TICK_RATE));
            }
        });
    }

    /// Initializes the UI with a list of podcasts and podcast episodes,
    /// creates the menus and panels, and returns a UI object for future
    /// manipulation.
    pub fn new(config: &'a Config, items: LockVec<Podcast>) -> Ui<'a> {
        terminal::enable_raw_mode().expect("Terminal can't run in raw mode.");
        execute!(
            io::stdout(),
            terminal::EnterAlternateScreen,
            terminal::Clear(terminal::ClearType::All),
            cursor::Hide,
            EnableMouseCapture
        )
        .expect("Can't draw to screen.");

        let colors = Rc::new(config.colors.clone());

        let (n_col, n_row) = terminal::size().expect("Can't get terminal size");
        let (pod_col, ep_col, det_col) = Self::calculate_sizes(n_col);

        let first_pod = match items.borrow_filtered_order().get(0) {
            Some(first_id) => match items.borrow_map().get(first_id) {
                Some(pod) => pod.episodes.clone(),
                None => LockVec::new(Vec::new()),
            },
            None => LockVec::new(Vec::new()),
        };

        let podcast_panel = Panel::new(
            "Podcasts".to_string(),
            0,
            colors.clone(),
            n_row - 1,
            pod_col,
            0,
            (0, 0, 0, 0),
        );
        let podcast_menu = Menu::new(podcast_panel, None, items);

        let episode_panel = Panel::new(
            "Episodes".to_string(),
            1,
            colors.clone(),
            n_row - 1,
            ep_col,
            pod_col - 1,
            (0, 0, 0, 0),
        );

        let episode_menu = Menu::new(episode_panel, None, first_pod);

        let details_panel = if n_col > crate::config::DETAILS_PANEL_LENGTH {
            Some(DetailsPanel::new(
                "Details".to_string(),
                2,
                colors.clone(),
                n_row - 1,
                det_col,
                pod_col + ep_col - 2,
                (0, 1, 0, 1),
            ))
        } else {
            None
        };

        let notif_win = NotifWin::new(colors.clone(), n_row - 1, n_row, n_col);
        let popup_win = PopupWin::new(&config.keybindings, colors.clone(), n_row, n_col);

        return Ui {
            n_row: n_row,
            n_col: n_col,
            keymap: &config.keybindings,
            colors: colors,
            podcast_menu: podcast_menu,
            episode_menu: episode_menu,
            details_panel: details_panel,
            active_panel: ActivePanel::PodcastMenu,
            notif_win: notif_win,
            popup_win: popup_win,
            mouse_state: MouseState::default(),
        };
    }

    /// This should be called immediately after creating the UI, in order
    /// to draw everything to the screen.
    pub fn init(&mut self) {
        self.podcast_menu.redraw();
        self.episode_menu.redraw();
        self.podcast_menu.activate();
        self.update_details_panel();

        self.notif_win.redraw();

        // welcome screen if user does not have any podcasts yet
        if self.podcast_menu.items.is_empty() {
            self.popup_win.spawn_welcome_win();
        }
        io::stdout().flush().unwrap();
    }

    /// Waits for user input and, where necessary, provides UiMsgs
    /// back to the main controller.
    ///
    /// Anything UI-related (e.g., scrolling up and down menus) is
    /// handled internally, producing an empty UiMsg. This allows for
    /// some greater degree of abstraction; for example, input to add a
    /// new podcast feed spawns a UI window to capture the feed URL, and
    /// only then passes this data back to the main controller.
    pub fn getch(&mut self) -> UiMsg {
        if event::poll(Duration::from_secs(0)).expect("Can't poll for inputs") {
            match event::read().expect("Can't read inputs") {
                Event::Resize(n_col, n_row) => self.resize(n_col, n_row),
                Event::Mouse(mouse_event) => {
                    return self.handle_mouse_event(mouse_event);
                }
                Event::Key(input) => {
                    let (curr_pod_id, curr_ep_id) = self.get_current_ids();

                    // get rid of the "welcome" window once the podcast
                    // list is no longer empty
                    if self.popup_win.welcome_win && !self.podcast_menu.items.is_empty() {
                        self.popup_win.turn_off_welcome_win();
                    }

                    // if there is a popup window active (apart from the
                    // welcome window which takes no input), then
                    // redirect user input there
                    if self.popup_win.is_non_welcome_popup_active() {
                        let popup_msg = self.popup_win.handle_input(input);

                        // need to check if popup window is still active,
                        // as handling character input above may involve
                        // closing the popup window
                        if !self.popup_win.is_popup_active() {
                            self.update_menus();
                            if self.details_panel.is_some() {
                                self.update_details_panel();
                            }
                            io::stdout().flush().unwrap();
                        }
                        return popup_msg;
                    } else {
                        match self.keymap.get_from_input(input) {
                            Some(a @ UserAction::Down)
                            | Some(a @ UserAction::Up)
                            | Some(a @ UserAction::Left)
                            | Some(a @ UserAction::Right)
                            | Some(a @ UserAction::PageUp)
                            | Some(a @ UserAction::PageDown)
                            | Some(a @ UserAction::BigUp)
                            | Some(a @ UserAction::BigDown)
                            | Some(a @ UserAction::GoTop)
                            | Some(a @ UserAction::GoBot) => {
                                self.move_cursor(a, curr_pod_id, curr_ep_id)
                            }

                            Some(UserAction::AddFeed) => {
                                let url = &self.spawn_input_notif("Feed URL: ");
                                if !url.is_empty() {
                                    return UiMsg::AddFeed(url.to_string());
                                }
                            }

                            Some(UserAction::Sync) => {
                                if let Some(pod_id) = curr_pod_id {
                                    return UiMsg::Sync(pod_id);
                                }
                            }
                            Some(UserAction::SyncAll) => {
                                if curr_pod_id.is_some() {
                                    return UiMsg::SyncAll;
                                }
                            }

                            Some(UserAction::Play) => {
                                if let Some(pod_id) = curr_pod_id {
                                    if let Some(ep_id) = curr_ep_id {
                                        return UiMsg::Play(pod_id, ep_id);
                                    }
                                }
                            }
                            Some(UserAction::MarkPlayed) => {
                                if let ActivePanel::EpisodeMenu = self.active_panel {
                                    if let Some(ui_msg) = self.mark_played(curr_pod_id, curr_ep_id)
                                    {
                                        return ui_msg;
                                    }
                                }
                            }
                            Some(UserAction::MarkAllPlayed) => {
                                if let Some(ui_msg) = self.mark_all_played(curr_pod_id) {
                                    return ui_msg;
                                }
                            }

                            Some(UserAction::Download) => {
                                if let Some(pod_id) = curr_pod_id {
                                    if let Some(ep_id) = curr_ep_id {
                                        return UiMsg::Download(pod_id, ep_id);
                                    }
                                }
                            }
                            Some(UserAction::DownloadAll) => {
                                if let Some(pod_id) = curr_pod_id {
                                    return UiMsg::DownloadAll(pod_id);
                                }
                            }

                            Some(UserAction::Delete) => {
                                if let ActivePanel::EpisodeMenu = self.active_panel {
                                    if let Some(pod_id) = curr_pod_id {
                                        if let Some(ep_id) = curr_ep_id {
                                            return UiMsg::Delete(pod_id, ep_id);
                                        }
                                    }
                                }
                            }
                            Some(UserAction::DeleteAll) => {
                                if let Some(pod_id) = curr_pod_id {
                                    return UiMsg::DeleteAll(pod_id);
                                }
                            }

                            Some(UserAction::Remove) => match self.active_panel {
                                ActivePanel::PodcastMenu => {
                                    if let Some(ui_msg) = self.remove_podcast(curr_pod_id) {
                                        return ui_msg;
                                    }
                                }
                                ActivePanel::EpisodeMenu => {
                                    if let Some(ui_msg) =
                                        self.remove_episode(curr_pod_id, curr_ep_id)
                                    {
                                        return ui_msg;
                                    }
                                }
                                _ => (),
                            },
                            Some(UserAction::RemoveAll) => {
                                let ui_msg = match self.active_panel {
                                    ActivePanel::PodcastMenu => self.remove_podcast(curr_pod_id),
                                    ActivePanel::EpisodeMenu => {
                                        self.remove_all_episodes(curr_pod_id)
                                    }
                                    _ => None,
                                };
                                if let Some(ui_msg) = ui_msg {
                                    return ui_msg;
                                }
                            }

                            Some(UserAction::FilterPlayed) => {
                                return UiMsg::FilterChange(FilterType::Played);
                            }
                            Some(UserAction::FilterDownloaded) => {
                                return UiMsg::FilterChange(FilterType::Downloaded);
                            }

                            Some(UserAction::Help) => self.popup_win.spawn_help_win(),

                            Some(UserAction::Quit) => {
                                return UiMsg::Quit;
                            }
                            None => (),
                        } // end of input match
                    }
                }
            }
        } // end of poll()
        return UiMsg::Noop;
    }

    /// Handles mouse events for user interaction
    fn handle_mouse_event(&mut self, mouse_event: MouseEvent) -> UiMsg {
        match mouse_event.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.handle_mouse_down(mouse_event.column, mouse_event.row)
            }
            MouseEventKind::Up(MouseButton::Left) => {
                self.handle_mouse_up(mouse_event.column, mouse_event.row)
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                self.handle_mouse_drag(mouse_event.column, mouse_event.row)
            }
            _ => UiMsg::Noop,
        }
    }

    /// Handles mouse down events - highlights item for visual feedback (selection happens on mouseup)
    fn handle_mouse_down(&mut self, col: u16, row: u16) -> UiMsg {
        // Determine which panel and item was clicked
        if let Some((panel, item_row)) = self.get_panel_and_row_from_coordinates(col, row) {
            // Store mouse down state
            self.mouse_state.is_mouse_down = true;
            self.mouse_state.mouse_down_panel = Some(panel.clone());
            self.mouse_state.mouse_down_row = Some(item_row);

            // Highlight the item for visual feedback
            match panel {
                ActivePanel::PodcastMenu => {
                    // Bounds checking is already done in get_panel_and_row_from_coordinates
                    // Unhighlight current selection
                    self.podcast_menu.unhighlight_item(self.podcast_menu.selected);
                    // Highlight the clicked item
                    self.podcast_menu.highlight_item(item_row, false); // false = not active highlighting
                    self.mouse_state.highlighted_during_drag = Some(item_row);
                }
                ActivePanel::EpisodeMenu => {
                    // Bounds checking is already done in get_panel_and_row_from_coordinates
                    // Unhighlight current selection
                    self.episode_menu.unhighlight_item(self.episode_menu.selected);
                    // Highlight the clicked item
                    self.episode_menu.highlight_item(item_row, false);
                    self.mouse_state.highlighted_during_drag = Some(item_row);
                }
                _ => {} // Details panel doesn't have selectable items
            }
        }
        UiMsg::Noop
    }

    /// Handles mouse up events - selects the item on which the mouseup occurs (after any drag sequence)
    fn handle_mouse_up(&mut self, col: u16, row: u16) -> UiMsg {
        if !self.mouse_state.is_mouse_down {
            return UiMsg::Noop;
        }

        let result = if let Some((panel, item_row)) = self.get_panel_and_row_from_coordinates(col, row) {
            // Select the item where mouse up occurs, regardless of where mouse down happened
            match panel {
                ActivePanel::PodcastMenu => {
                    // Bounds checking is already done in get_panel_and_row_from_coordinates
                    let old_panel = self.active_panel.clone();
                    
                    // Change active panel and update selection
                    self.active_panel = ActivePanel::PodcastMenu;
                    
                    // Update selection to the item where mouseup occurred
                    self.podcast_menu.selected = item_row;
                    
                    // Update menus and highlighting
                    self.podcast_menu.activate();
                    self.episode_menu.deactivate(true);
                    
                    // Update episode menu with episodes from selected podcast
                    self.episode_menu.items = self.podcast_menu.get_episodes();
                    self.episode_menu.top_row = 0;
                    self.episode_menu.selected = self.episode_menu.start_row;
                    
                    // If panel changed and we're in adaptive mode, trigger resize
                    if old_panel != self.active_panel && self.n_col <= crate::config::DETAILS_PANEL_LENGTH {
                        self.resize(self.n_col, self.n_row);
                    } else {
                        self.update_menus();
                        if self.details_panel.is_some() {
                            self.update_details_panel();
                        }
                        io::stdout().flush().unwrap();
                    }
                    UiMsg::Noop
                }
                ActivePanel::EpisodeMenu => {
                    // Bounds checking is already done in get_panel_and_row_from_coordinates
                    let old_panel = self.active_panel.clone();
                    
                    // Change active panel and update selection
                    self.active_panel = ActivePanel::EpisodeMenu;
                    
                    // Update selection to the item where mouseup occurred
                    self.episode_menu.selected = item_row;
                    
                    // Update highlighting - only highlight podcast menu if it's visible
                    let (pod_col, _ep_col, _det_col) = Self::calculate_adaptive_sizes(self.n_col, &self.active_panel);
                    if pod_col > 0 {
                        self.podcast_menu.highlight_selected();
                    }
                    self.episode_menu.activate();
                    
                    // If panel changed and we're in adaptive mode, trigger resize
                    if old_panel != self.active_panel && self.n_col <= crate::config::DETAILS_PANEL_LENGTH {
                        self.resize(self.n_col, self.n_row);
                    } else {
                        if self.details_panel.is_some() {
                            self.update_details_panel();
                        }
                        io::stdout().flush().unwrap();
                    }
                    UiMsg::Noop
                }
                _ => UiMsg::Noop,
            }
        } else {
            UiMsg::Noop
        };

        // Clean up mouse state and restore normal highlighting
        self.cleanup_mouse_state();
        result
    }

    /// Handles mouse drag events - updates highlighting as user drags
    fn handle_mouse_drag(&mut self, col: u16, row: u16) -> UiMsg {
        if !self.mouse_state.is_mouse_down {
            return UiMsg::Noop;
        }

        if let Some((panel, item_row)) = self.get_panel_and_row_from_coordinates(col, row) {
            // Only handle drag within the same panel as the original mouse down
            if Some(panel.clone()) == self.mouse_state.mouse_down_panel {
                // Unhighlight previously highlighted item during drag
                if let Some(prev_row) = self.mouse_state.highlighted_during_drag {
                    match panel {
                        ActivePanel::PodcastMenu => {
                            self.podcast_menu.unhighlight_item(prev_row);
                        }
                        ActivePanel::EpisodeMenu => {
                            self.episode_menu.unhighlight_item(prev_row);
                        }
                        _ => {}
                    }
                }

                // Highlight new item
                match panel {
                    ActivePanel::PodcastMenu => {
                        // Bounds checking is already done in get_panel_and_row_from_coordinates
                        self.podcast_menu.highlight_item(item_row, false);
                        self.mouse_state.highlighted_during_drag = Some(item_row);
                    }
                    ActivePanel::EpisodeMenu => {
                        // Bounds checking is already done in get_panel_and_row_from_coordinates
                        self.episode_menu.highlight_item(item_row, false);
                        self.mouse_state.highlighted_during_drag = Some(item_row);
                    }
                    _ => {}
                }
            }
        }
        UiMsg::Noop
    }

    /// Maps mouse coordinates to panel and row within that panel
    fn get_panel_and_row_from_coordinates(&self, col: u16, row: u16) -> Option<(ActivePanel, u16)> {
        let (pod_col, ep_col, det_col) = Self::calculate_adaptive_sizes(self.n_col, &self.active_panel);

        // Account for panel borders: panels draw content at screen_row + 1 due to top border
        // So we need to convert screen coordinates back to panel coordinates
        
        // In adaptive mode, some panels may be hidden (width = 0)
        // We need to check which panels are actually visible and map coordinates accordingly
        
        // Check if click is in podcast menu (only if visible)
        if pod_col > 0 && col < pod_col && row < self.n_row - 1 {
            // Convert screen row to panel-relative row by subtracting 1 for the top border
            if row > 0 { // Make sure we don't underflow
                let panel_row = row - 1;
                // Check if this panel row is within the menu area
                if panel_row >= self.podcast_menu.start_row && panel_row < self.n_row - 2 {
                    // Check if this row actually has a menu item
                    let menu_idx = self.podcast_menu.get_menu_idx(panel_row);
                    if menu_idx < self.podcast_menu.items.len(true) {
                        return Some((ActivePanel::PodcastMenu, panel_row));
                    }
                }
            }
        }
        
        // Calculate episode menu start position based on whether podcast menu is visible
        let ep_start_x = if pod_col > 0 { pod_col - 1 } else { 0 };
        
        // Check if click is in episode menu (only if visible)
        if ep_col > 0 && col >= ep_start_x && col < ep_start_x + ep_col && row < self.n_row - 1 {
            // Convert screen row to panel-relative row by subtracting 1 for the top border
            if row > 0 { // Make sure we don't underflow
                let panel_row = row - 1;
                // Check if this panel row is within the menu area
                if panel_row >= self.episode_menu.start_row && panel_row < self.n_row - 2 {
                    // Check if this row actually has a menu item
                    let menu_idx = self.episode_menu.get_menu_idx(panel_row);
                    if menu_idx < self.episode_menu.items.len(true) {
                        return Some((ActivePanel::EpisodeMenu, panel_row));
                    }
                }
            }
        }
        
        // Calculate details panel start position
        let det_start_x = if pod_col > 0 && ep_col > 0 {
            pod_col + ep_col - 2
        } else if ep_col > 0 {
            ep_col - 1
        } else {
            0
        };
        
        // Check if click is in details panel (only if visible)
        if det_col > 0 && col >= det_start_x && self.details_panel.is_some() {
            return Some((ActivePanel::DetailsPanel, row));
        }

        None
    }

    /// Cleans up mouse state and restores normal menu highlighting
    fn cleanup_mouse_state(&mut self) {
        // Restore normal highlighting for currently selected items
        if let Some(highlighted_row) = self.mouse_state.highlighted_during_drag {
            match self.mouse_state.mouse_down_panel.as_ref() {
                Some(ActivePanel::PodcastMenu) => {
                    self.podcast_menu.unhighlight_item(highlighted_row);
                    self.podcast_menu.highlight_selected();
                }
                Some(ActivePanel::EpisodeMenu) => {
                    self.episode_menu.unhighlight_item(highlighted_row);
                    self.episode_menu.highlight_selected();
                }
                _ => {}
            }
        }

        // Reset mouse state
        self.mouse_state = MouseState::default();
    }

    /// Resize all the windows on the screen and redraw them.
    pub fn resize(&mut self, n_col: u16, n_row: u16) {
        self.n_row = n_row;
        self.n_col = n_col;

        let (pod_col, ep_col, det_col) = Self::calculate_adaptive_sizes(n_col, &self.active_panel);

        // Clear the entire screen to avoid overdraw issues when panels switch visibility
        use crossterm::{execute, terminal};
        execute!(
            io::stdout(),
            terminal::Clear(terminal::ClearType::All)
        ).unwrap();

        // Resize podcast menu
        if pod_col > 0 {
            self.podcast_menu.resize(n_row - 1, pod_col, 0);
            self.podcast_menu.redraw();
        }

        // Resize episode menu 
        if ep_col > 0 {
            let ep_start_x = if pod_col > 0 { pod_col - 1 } else { 0 };
            self.episode_menu.resize(n_row - 1, ep_col, ep_start_x);
            self.episode_menu.redraw();
        }

        self.highlight_items();

        // Handle details panel
        if self.details_panel.is_some() {
            if det_col > 0 {
                let det = self.details_panel.as_mut().unwrap();
                let det_start_x = if pod_col > 0 { pod_col + ep_col - 2 } else { ep_col - 1 };
                det.resize(n_row - 1, det_col, det_start_x);
                // resizing the menus may change which item is selected
                self.update_details_panel();
            } else {
                self.details_panel = None;
                // if the details panel is currently active, but the
                // terminal is resized so the panel disappears, switch
                // the active focus to the episode menu automatically
                if let ActivePanel::DetailsPanel = self.active_panel {
                    self.active_panel = ActivePanel::EpisodeMenu;
                    self.episode_menu.activate();
                }
            }
        } else if det_col > 0 {
            let det_start_x = if pod_col > 0 { pod_col + ep_col - 2 } else { ep_col - 1 };
            self.details_panel = Some(DetailsPanel::new(
                "Details".to_string(),
                2,
                self.colors.clone(),
                n_row - 1,
                det_col,
                det_start_x,
                (0, 1, 0, 1),
            ));
            self.update_details_panel();
        }

        self.popup_win.resize(n_row, n_col);
        self.notif_win.resize(n_row, n_col);
    }

    /// Move the menu cursor around and redraw menus when necessary.
    pub fn move_cursor(
        &mut self,
        action: &UserAction,
        curr_pod_id: Option<i64>,
        curr_ep_id: Option<i64>,
    ) {
        match action {
            UserAction::Down => {
                self.scroll_current_window(curr_pod_id, Scroll::Down(1));
            }

            UserAction::Up => {
                self.scroll_current_window(curr_pod_id, Scroll::Up(1));
            }

            UserAction::Left => {
                if curr_pod_id.is_some() {
                    let old_panel = self.active_panel.clone();
                    match self.active_panel {
                        ActivePanel::PodcastMenu => (),
                        ActivePanel::EpisodeMenu => {
                            self.active_panel = ActivePanel::PodcastMenu;
                            self.podcast_menu.activate();
                            self.episode_menu.deactivate(false);
                        }
                        ActivePanel::DetailsPanel => {
                            self.active_panel = ActivePanel::EpisodeMenu;
                            self.episode_menu.activate();
                        }
                    }
                    
                    // If panel changed and we're in adaptive mode, trigger resize
                    if old_panel != self.active_panel && self.n_col <= crate::config::DETAILS_PANEL_LENGTH {
                        self.resize(self.n_col, self.n_row);
                    }
                }
            }

            UserAction::Right => {
                if curr_pod_id.is_some() && curr_ep_id.is_some() {
                    let old_panel = self.active_panel.clone();
                    match self.active_panel {
                        ActivePanel::PodcastMenu => {
                            self.active_panel = ActivePanel::EpisodeMenu;
                            self.podcast_menu.deactivate();
                            self.episode_menu.activate();
                        }
                        ActivePanel::EpisodeMenu => {
                            if self.details_panel.is_some() || self.n_col <= crate::config::DETAILS_PANEL_LENGTH {
                                self.active_panel = ActivePanel::DetailsPanel;
                                self.episode_menu.deactivate(true);
                            }
                        }
                        ActivePanel::DetailsPanel => (),
                    }
                    
                    // If panel changed and we're in adaptive mode, trigger resize
                    if old_panel != self.active_panel && self.n_col <= crate::config::DETAILS_PANEL_LENGTH {
                        self.resize(self.n_col, self.n_row);
                    }
                }
            }

            UserAction::PageUp => {
                self.scroll_current_window(curr_pod_id, Scroll::Up(self.n_row - 3));
            }

            UserAction::PageDown => {
                self.scroll_current_window(curr_pod_id, Scroll::Down(self.n_row - 3));
            }

            UserAction::BigUp => {
                self.scroll_current_window(
                    curr_pod_id,
                    Scroll::Up(self.n_row / crate::config::BIG_SCROLL_AMOUNT),
                );
            }

            UserAction::BigDown => {
                self.scroll_current_window(
                    curr_pod_id,
                    Scroll::Down(self.n_row / crate::config::BIG_SCROLL_AMOUNT),
                );
            }

            UserAction::GoTop => {
                self.scroll_current_window(curr_pod_id, Scroll::Up(u16::MAX));
            }

            UserAction::GoBot => {
                self.scroll_current_window(curr_pod_id, Scroll::Down(u16::MAX));
            }

            // this shouldn't occur because we only trigger this
            // function when the UserAction is Up, Down, Left, Right,
            // BigUp, BigDown, PageUp, PageDown, GoBot and GoTop
            _ => (),
        }
    }

    /// Scrolls the current active menu by the specified amount and
    /// refreshes the window.
    pub fn scroll_current_window(&mut self, pod_id: Option<i64>, scroll: Scroll) {
        match self.active_panel {
            ActivePanel::PodcastMenu => {
                if pod_id.is_some() {
                    self.podcast_menu.scroll(scroll);

                    self.episode_menu.top_row = 0;
                    self.episode_menu.selected = 0;

                    // update episodes menu with new list
                    self.episode_menu.items = self.podcast_menu.get_episodes();
                    self.episode_menu.redraw();
                    self.update_details_panel();
                }
            }
            ActivePanel::EpisodeMenu => {
                if pod_id.is_some() {
                    self.episode_menu.scroll(scroll);
                    self.update_details_panel();
                }
            }
            ActivePanel::DetailsPanel => {
                if let Some(ref mut det) = self.details_panel {
                    det.scroll(scroll);
                }
            }
        }
    }

    /// Mark an episode as played or unplayed (opposite of its current
    /// status).
    pub fn mark_played(
        &mut self,
        curr_pod_id: Option<i64>,
        curr_ep_id: Option<i64>,
    ) -> Option<UiMsg> {
        if let Some(pod_id) = curr_pod_id {
            if let Some(ep_id) = curr_ep_id {
                if let Some(played) = self
                    .episode_menu
                    .items
                    .map_single(ep_id, |ep| ep.is_played())
                {
                    return Some(UiMsg::MarkPlayed(pod_id, ep_id, !played));
                }
            }
        }
        return None;
    }

    /// Mark all episodes for a given podcast as played or unplayed. If
    /// there are any unplayed episodes, this will convert all episodes
    /// to played; if all are played already, only then will it convert
    /// all to unplayed.
    pub fn mark_all_played(&mut self, curr_pod_id: Option<i64>) -> Option<UiMsg> {
        if let Some(pod_id) = curr_pod_id {
            if let Some(played) = self
                .podcast_menu
                .items
                .map_single(pod_id, |pod| pod.is_played())
            {
                return Some(UiMsg::MarkAllPlayed(pod_id, !played));
            }
        }
        return None;
    }

    /// Remove a podcast from the list.
    pub fn remove_podcast(&mut self, curr_pod_id: Option<i64>) -> Option<UiMsg> {
        let confirm = self.ask_for_confirmation("Are you sure you want to remove the podcast?");
        // If we don't get a confirmation to delete, then don't remove
        if !confirm {
            return None;
        }
        let mut delete = false;

        if let Some(pod_id) = curr_pod_id {
            // check if we have local files first and if so, ask whether
            // to delete those too
            if self.check_for_local_files(pod_id) {
                let ask_delete = self.spawn_yes_no_notif("Delete local files too?");
                delete = ask_delete.unwrap_or(false); // default not to delete
            }

            return Some(UiMsg::RemovePodcast(pod_id, delete));
        }
        return None;
    }

    /// Remove an episode from the list for the current podcast.
    fn remove_episode(
        &mut self,
        curr_pod_id: Option<i64>,
        curr_ep_id: Option<i64>,
    ) -> Option<UiMsg> {
        let confirm = self.ask_for_confirmation("Are you sure you want to remove the episode?");
        // If we don't get a confirmation to delete, then don't remove
        if !confirm {
            return None;
        }
        let mut delete = false;
        if let Some(pod_id) = curr_pod_id {
            if let Some(ep_id) = curr_ep_id {
                // check if we have local files first
                let is_downloaded = self
                    .episode_menu
                    .items
                    .map_single(ep_id, |ep| ep.path.is_some())
                    .unwrap_or(false);
                if is_downloaded {
                    let ask_delete = self.spawn_yes_no_notif("Delete local file too?");
                    delete = ask_delete.unwrap_or(false); // default not to delete
                }

                return Some(UiMsg::RemoveEpisode(pod_id, ep_id, delete));
            }
        }
        return None;
    }

    /// Remove all episodes from the list for the current podcast.
    fn remove_all_episodes(&mut self, curr_pod_id: Option<i64>) -> Option<UiMsg> {
        if let Some(pod_id) = curr_pod_id {
            let mut delete = false;

            // check if we have local files first and if so, ask whether
            // to delete those too
            if self.check_for_local_files(pod_id) {
                let ask_delete = self.spawn_yes_no_notif("Delete local files too?");
                delete = ask_delete.unwrap_or(false); // default not to delete
            }
            return Some(UiMsg::RemoveAllEpisodes(pod_id, delete));
        }
        return None;
    }


    /// Based on the current selected value of the podcast and episode
    /// menus, returns the IDs of the current podcast and episode (if
    /// they exist).
    pub fn get_current_ids(&self) -> (Option<i64>, Option<i64>) {
        let current_pod_index = (self.podcast_menu.selected + self.podcast_menu.top_row) as usize;
        let current_ep_index = (self.episode_menu.selected + self.episode_menu.top_row) as usize;

        let current_pod_id = self
            .podcast_menu
            .items
            .borrow_filtered_order()
            .get(current_pod_index)
            .copied();
        let current_ep_id = self
            .episode_menu
            .items
            .borrow_filtered_order()
            .get(current_ep_index)
            .copied();
        return (current_pod_id, current_ep_id);
    }

    /// Calculates the number of columns to allocate for each of the
    /// main panels: podcast menu, episodes menu, and details panel; if
    /// the screen is too small to display all panels, this returns the
    /// active panel plus one to the right
    pub fn calculate_sizes(n_col: u16) -> (u16, u16, u16) {
        let pod_col;
        let ep_col;
        let det_col;
        if n_col > crate::config::DETAILS_PANEL_LENGTH {
            // Full 3-pane layout
            pod_col = (n_col + 2) / 3;
            ep_col = (n_col + 2) / 3;
            det_col = n_col + 2 - pod_col - ep_col;
        } else {
            // 2-pane layout: show only podcast and episode menus
            pod_col = (n_col + 1) / 2;
            ep_col = n_col + 1 - pod_col;
            det_col = 0;
        }
        return (pod_col, ep_col, det_col);
    }
    
    /// Calculates the adaptive layout based on active panel and available width.
    /// Shows the active panel plus one to the right when space is limited.
    pub fn calculate_adaptive_sizes(n_col: u16, active_panel: &ActivePanel) -> (u16, u16, u16) {
        if n_col > crate::config::DETAILS_PANEL_LENGTH {
            // Enough space for all 3 panels
            return Self::calculate_sizes(n_col);
        }
        
        // Limited space: show active panel + one to the right
        match active_panel {
            ActivePanel::PodcastMenu => {
                // Show: Podcast + Episodes
                let pod_col = (n_col + 1) / 2;
                let ep_col = n_col + 1 - pod_col;
                (pod_col, ep_col, 0)
            }
            ActivePanel::EpisodeMenu => {
                // Show: Episodes + Details
                let ep_col = (n_col + 1) / 2;
                let det_col = n_col + 1 - ep_col;
                (0, ep_col, det_col)
            }
            ActivePanel::DetailsPanel => {
                // Show: Episodes + Details (can't go further right)
                let ep_col = (n_col + 1) / 2;
                let det_col = n_col + 1 - ep_col;
                (0, ep_col, det_col)
            }
        }
    }

    /// Checks whether the user has downloaded any episodes for the
    /// given podcast to their local system.
    pub fn check_for_local_files(&self, pod_id: i64) -> bool {
        let mut any_downloaded = false;
        let borrowed_map = self.podcast_menu.items.borrow_map();
        let borrowed_pod = borrowed_map
            .get(&pod_id)
            .expect("Could not retrieve podcast info.");

        let borrowed_ep_list = borrowed_pod.episodes.borrow_map();

        for (_ep_id, ep) in borrowed_ep_list.iter() {
            if ep.path.is_some() {
                any_downloaded = true;
                break;
            }
        }
        return any_downloaded;
    }

    /// Spawns a "(y/n)" notification with the specified input
    /// `message` using `spawn_input_notif`. If the the user types
    /// 'y', then the function returns `true`, and 'n' returns
    /// `false`. Cancelling the action returns `false` as well.
    pub fn ask_for_confirmation(&self, message: &str) -> bool {
        self.spawn_yes_no_notif(message).unwrap_or(false)
    }

    /// Adds a notification to the bottom of the screen that solicits
    /// user text input. A prefix can be specified as a prompt for the
    /// user at the beginning of the input line. This returns the user's
    /// input; if the user cancels their input, the String will be empty.
    pub fn spawn_input_notif(&self, prefix: &str) -> String {
        return self.notif_win.input_notif(prefix);
    }

    /// Adds a notification to the bottom of the screen that solicits
    /// user for a yes/no input. A prefix can be specified as a prompt
    /// for the user at the beginning of the input line. "(y/n)" will
    /// automatically be appended to the end of the prefix. If the user
    /// types 'y' or 'n', the boolean will represent this value. If the
    /// user cancels the input or types anything else, the function will
    /// return None.
    pub fn spawn_yes_no_notif(&self, prefix: &str) -> Option<bool> {
        let mut out_val = None;
        let input = self.notif_win.input_notif(&format!("{prefix} (y/n) "));
        if let Some(c) = input.trim().chars().next() {
            if c == 'Y' || c == 'y' {
                out_val = Some(true);
            } else if c == 'N' || c == 'n' {
                out_val = Some(false);
            }
        }
        return out_val;
    }

    /// Adds a notification to the bottom of the screen for `duration`
    /// time (in milliseconds). Useful for presenting error messages,
    /// among other things.
    pub fn timed_notif(&mut self, message: String, duration: u64, error: bool) {
        self.notif_win.timed_notif(message, duration, error);
    }

    /// Adds a notification to the bottom of the screen that will stay on
    /// screen indefinitely. Must use `clear_persistent_msg()` to erase.
    pub fn persistent_notif(&mut self, message: String, error: bool) {
        self.notif_win.persistent_notif(message, error);
    }

    /// Clears any persistent notification that is being displayed at the
    /// bottom of the screen. Does not affect timed notifications, user
    /// input notifications, etc.
    pub fn clear_persistent_notif(&mut self) {
        self.notif_win.clear_persistent_notif();
    }

    /// Forces the menus to check the list of podcasts/episodes again and
    /// update.
    pub fn update_menus(&mut self) {
        // In adaptive mode, only redraw visible panels
        let (pod_col, ep_col, _det_col) = Self::calculate_adaptive_sizes(self.n_col, &self.active_panel);
        
        if pod_col > 0 {
            self.podcast_menu.redraw();
        }

        self.episode_menu.items = if !self.podcast_menu.items.is_empty() {
            self.podcast_menu.get_episodes()
        } else {
            LockVec::new(Vec::new())
        };
        
        if ep_col > 0 {
            self.episode_menu.redraw();
        }
        self.highlight_items();
    }

    /// Forces the menus to redraw the highlighted item.
    pub fn highlight_items(&mut self) {
        // Check which panels are visible based on current layout
        let (pod_col, ep_col, _det_col) = Self::calculate_adaptive_sizes(self.n_col, &self.active_panel);
        
        match self.active_panel {
            ActivePanel::PodcastMenu => {
                if pod_col > 0 {
                    self.podcast_menu.highlight_selected();
                }
            }
            ActivePanel::EpisodeMenu => {
                // Only highlight podcast menu if it's visible (has width > 0)
                if pod_col > 0 {
                    self.podcast_menu.highlight_selected();
                }
                if ep_col > 0 {
                    self.episode_menu.highlight_selected();
                }
            }
            _ => (),
        }
    }

    /// When the program is ending, this performs tear-down functions so
    /// that the terminal is properly restored to its prior settings.
    pub fn tear_down(&self) {
        terminal::disable_raw_mode().unwrap();
        execute!(
            io::stdout(),
            terminal::Clear(terminal::ClearType::All),
            terminal::LeaveAlternateScreen,
            cursor::Show,
            DisableMouseCapture
        )
        .unwrap();
    }

    /// Updates the details panel with information about the current
    /// podcast and episode, and redraws to the screen.
    pub fn update_details_panel(&mut self) {
        if self.details_panel.is_some() {
            let (curr_pod_id, curr_ep_id) = self.get_current_ids();
            let det = self.details_panel.as_mut().unwrap();
            if let Some(pod_id) = curr_pod_id {
                if let Some(ep_id) = curr_ep_id {
                    // get a couple details from the current podcast
                    let mut pod_title = None;
                    let mut pod_explicit = None;
                    if let Some(pod) = self.podcast_menu.items.borrow_map().get(&pod_id) {
                        pod_title = if pod.title.is_empty() {
                            None
                        } else {
                            Some(pod.title.clone())
                        };
                        pod_explicit = pod.explicit;
                    };

                    // the rest of the details come from the current episode
                    if let Some(ep) = self.episode_menu.items.borrow_map().get(&ep_id) {
                        let ep_title = if ep.title.is_empty() {
                            None
                        } else {
                            Some(ep.title.clone())
                        };

                        let desc = if ep.description.is_empty() {
                            None
                        } else {
                            // convert <br/> tags to a single line break
                            let br_to_lb = RE_BR_TAGS.replace_all(&ep.description, "\n");

                            // strip all HTML tags
                            let stripped_tags = RE_HTML_TAGS.replace_all(&br_to_lb, "");

                            // convert HTML entities (e.g., &amp;)
                            let decoded = match escaper::decode_html(&stripped_tags) {
                                Err(_) => stripped_tags.to_string(),
                                Ok(s) => s,
                            };

                            // remove anything more than two line breaks (i.e., one blank line)
                            let no_line_breaks = RE_MULT_LINE_BREAKS.replace_all(&decoded, "\n\n");

                            Some(no_line_breaks.to_string())
                        };

                        let details = Details {
                            pod_title: pod_title,
                            ep_title: ep_title,
                            pubdate: ep.pubdate,
                            duration: Some(ep.format_duration()),
                            explicit: pod_explicit,
                            description: desc,
                        };
                        det.change_details(details);
                    };
                }
            }
        }
    }
}
