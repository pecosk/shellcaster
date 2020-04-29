use std::rc::Rc;
use core::cell::RefCell;

use rusqlite::{Connection, params};
use chrono::{NaiveDateTime, DateTime, Utc};

use crate::types::{Podcast, Episode};

/// Struct holding a sqlite database connection, with methods to interact
/// with this connection.
#[derive(Debug)]
pub struct Database {
    conn: Option<Connection>,
}

impl Database {
    /// Creates a new connection to the database (and creates database if
    /// it does not already exist). Panics if database cannot be accessed.
    pub fn connect() -> Database {
        match Connection::open("data.db") {
            Ok(conn) => {
                let db_conn = Database {
                    conn: Some(conn),
                };
                db_conn.create();
                return db_conn;
            },
            Err(err) => panic!("Could not open database: {}", err),
        };
    }

    /// Creates the necessary database tables, if they do not already
    /// exist. Panics if database cannot be accessed, or if tables cannot
    /// be created.
    pub fn create(&self) {
        let conn = self.conn.as_ref().unwrap();

        // create podcasts table
        match conn.execute(
            "CREATE TABLE IF NOT EXISTS podcasts (
                id INTEGER PRIMARY KEY,
                title TEXT NOT NULL,
                url TEXT NOT NULL UNIQUE,
                description TEXT,
                author TEXT,
                explicit INTEGER,
                last_checked INTEGER
            );",
            params![],
        ) {
            Ok(_) => (),
            Err(err) => panic!("Could not create podcasts database table: {}", err),
        }

        // create episodes table
        match conn.execute(
            "CREATE TABLE IF NOT EXISTS episodes (
                id INTEGER PRIMARY KEY,
                podcast_id INTEGER NOT NULL,
                title TEXT NOT NULL,
                url TEXT NOT NULL UNIQUE,
                description TEXT,
                pubdate INTEGER,
                duration INTEGER,
                played INTEGER,
                hidden INTEGER,
                FOREIGN KEY(podcast_id) REFERENCES podcasts(id)
            );",
            params![],
        ) {
            Ok(_) => (),
            Err(err) => panic!("Could not create episodes database table: {}", err),
        }

        // create files table
        match conn.execute(
            "CREATE TABLE IF NOT EXISTS files (
                id INTEGER PRIMARY KEY,
                episode_id INTEGER NOT NULL,
                path TEXT NOT NULL,
                FOREIGN KEY (episode_id) REFERENCES episodes(id)
            );",
            params![],
        ) {
            Ok(_) => (),
            Err(err) => panic!("Could not create files database table: {}", err),
        }
    }

    /// Inserts a new podcast and list of podcast episodes into the
    /// database.
    pub fn insert_podcast(&self, podcast: Podcast) ->
        Result<usize, Box<dyn std::error::Error>> {

        let conn = self.conn.as_ref().unwrap();
        let _ = conn.execute(
            "INSERT INTO podcasts (title, url, description, author, explicit, last_checked)
                VALUES (?, ?, ?, ?, ?, ?);",
            params![
                podcast.title,
                podcast.url,
                podcast.description,
                podcast.author,
                podcast.explicit,
                podcast.last_checked.timestamp()
            ]
        )?;

        let mut stmt = conn.prepare(
            "SELECT id FROM podcasts WHERE url = ?").unwrap();
        let pod_id = stmt
            .query_row::<i32,_,_>(params![podcast.url], |row| row.get(0))
            .unwrap();
        let num_episodes = podcast.episodes.borrow().len();

        for ep in podcast.episodes.borrow().iter().rev() {
            let _ = &self.insert_episode(&pod_id, &ep)?;
        }

        return Ok(num_episodes);
    }

    /// Inserts a podcast episode into the database.
    pub fn insert_episode(&self, podcast_id: &i32, episode: &Episode) ->
        Result<(), Box<dyn std::error::Error>> {

        let conn = self.conn.as_ref().unwrap();

        let pubdate = match episode.pubdate {
            Some(dt) => Some(dt.timestamp()),
            None => None,
        };

        let _ = conn.execute(
            "INSERT INTO episodes (podcast_id, title, url, description, pubdate, duration, played, hidden)
                VALUES (?, ?, ?, ?, ?, ?, ?, ?);",
            params![
                podcast_id,
                episode.title,
                episode.url,
                episode.description,
                pubdate,
                episode.duration,
                false,
                false,
            ]
        )?;
        return Ok(());
    }

    /// Generates list of all podcasts in database.
    /// TODO: This should probably use a JOIN statement instead.
    pub fn get_podcasts(&self) -> Vec<Podcast> {
        if let Some(conn) = &self.conn {
            let mut stmt = conn.prepare(
                "SELECT * FROM podcasts ORDER BY title;").unwrap();
            let podcast_iter = stmt.query_map(params![], |row| {
                let pod_id = row.get("id")?;
                let episodes = self.get_episodes(pod_id);
                Ok(Podcast {
                    id: Some(pod_id),
                    title: row.get("title")?,
                    url: row.get("url")?,
                    description: row.get("description")?,
                    author: row.get("author")?,
                    explicit: row.get("explicit")?,
                    last_checked: convert_date(row.get("last_checked")).unwrap(),
                    episodes: Rc::new(RefCell::new(episodes)),
                })
            }).unwrap();
            let mut podcasts = Vec::new();
            for pc in podcast_iter {
                podcasts.push(pc.unwrap());
            }
            return podcasts;
        } else {
            return Vec::new();
        }
    }

    /// Generates list of episodes for a given podcast.
    pub fn get_episodes(&self, pod_id: i32) -> Vec<Episode> {
        if let Some(conn) = &self.conn {
            let mut stmt = conn.prepare(
                "SELECT * FROM episodes WHERE podcast_id = ?
                      AND hidden = 0
                      ORDER BY pubdate DESC;").unwrap();
            let episode_iter = stmt.query_map(params![pod_id], |row| {
                Ok(Episode {
                    id: Some(row.get("id")?),
                    title: row.get("title")?,
                    url: row.get("url")?,
                    description: row.get("description")?,
                    pubdate: convert_date(row.get("pubdate")),
                    duration: row.get("duration")?,
                    path: None,  // TODO: Not yet implemented
                    played: row.get("played")?,
                })
            }).unwrap();
            let mut episodes = Vec::new();
            for ep in episode_iter {
                episodes.push(ep.unwrap());
            }
            return episodes;
        } else {
            return Vec::new();
        }
    }
}


/// Helper function converting an (optional) Unix timestamp to a
/// DateTime<Utc> object
fn convert_date(result: Result<i64, rusqlite::Error>) ->
    Option<DateTime<Utc>> {

    return match result {
        Ok(timestamp) => {
            match NaiveDateTime::from_timestamp_opt(timestamp, 0) {
                Some(ndt) => Some(DateTime::from_utc(ndt, Utc)),
                None => None,
            }
        },
        Err(_) => None,
    };
}