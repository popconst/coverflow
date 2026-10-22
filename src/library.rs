//! The album list: every folder in MPD's library that holds songs, with disc
//! folders (CD1/, Disc 2/, …) folded into their album, sorted by artist,
//! year and title.

use crate::mpd::{Entry, Mpd};
use anyhow::Result;
use std::collections::BTreeMap;

#[derive(Clone, Debug)]
pub struct Album {
    /// The album's folder, relative to MPD's music directory
    pub dir: String,
    pub title: String,
    pub artist: String,
    pub year: String,
}

impl Album {
    fn sort_key(&self) -> (String, String, String) {
        let artist = self.artist.to_lowercase();
        let artist = artist.strip_prefix("the ").unwrap_or(&artist).to_string();
        (artist, self.year.clone(), self.title.to_lowercase())
    }

    /// Whether a search matches this album (case-insensitive, any word order)
    pub fn matches(&self, query: &str) -> bool {
        let hay = format!("{} {} {}", self.artist, self.title, self.year).to_lowercase();
        query.to_lowercase().split_whitespace().all(|word| hay.contains(word))
    }
}

/// CD1, Disc 2, Disk 3, Диск 1, Часть II, Chapter 1, Vol. 2…
fn is_disc_folder(name: &str) -> bool {
    let name = name.trim_start_matches('(').to_lowercase();
    ["cd", "disc", "disk", "диск", "часть", "chapter", "vol"].iter().any(|p| name.starts_with(p))
}

/// Folders that aren't albums: playlists of loose tracks
fn skipped(dir: &str) -> bool {
    dir.split('/').any(|part| part == "Playlists")
}

fn tag<'a>(tags: &'a [(String, String)], key: &str) -> Option<&'a str> {
    tags.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v.as_str())
}

pub fn load(mpd: &mut Mpd) -> Result<Vec<Album>> {
    let mut albums: BTreeMap<String, Album> = BTreeMap::new();
    let mut pending = vec![String::new()];
    while let Some(dir) = pending.pop() {
        for entry in mpd.lsinfo(&dir)? {
            match entry {
                Entry::Directory(sub) if !skipped(&sub) => pending.push(sub),
                Entry::Directory(_) => {}
                Entry::File(file, tags) => {
                    let mut album_dir = file.rsplit_once('/').map_or("", |(d, _)| d).to_string();
                    if let Some((parent, name)) = album_dir.rsplit_once('/')
                        && is_disc_folder(name)
                    {
                        album_dir = parent.to_string();
                    }
                    // untagged files are extras (enhanced-CD videos, loose
                    // tracks), not an album
                    let Some(title) = tag(&tags, "Album") else { continue };
                    if album_dir.is_empty() || albums.contains_key(&album_dir) {
                        continue;
                    }
                    albums.insert(
                        album_dir.clone(),
                        Album {
                            title: title.to_string(),
                            artist: tag(&tags, "AlbumArtist")
                                .or(tag(&tags, "Artist"))
                                .unwrap_or("")
                                .to_string(),
                            year: tag(&tags, "OriginalDate")
                                .or(tag(&tags, "Date"))
                                .map(|d| d.chars().take(4).collect())
                                .unwrap_or_default(),
                            dir: album_dir,
                        },
                    );
                }
            }
        }
    }
    let mut albums: Vec<Album> = albums.into_values().collect();
    albums.sort_by_key(Album::sort_key);
    Ok(albums)
}

/// The album a song belongs to, by its path
pub fn album_of(albums: &[Album], file: &str) -> Option<usize> {
    albums
        .iter()
        .enumerate()
        .filter(|(_, a)| file.starts_with(&format!("{}/", a.dir)))
        .max_by_key(|(_, a)| a.dir.len())
        .map(|(i, _)| i)
}
