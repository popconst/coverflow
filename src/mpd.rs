//! Just enough of the MPD protocol for coverflow: walking the library,
//! an album's songs, the current song, and replacing the queue with an album.

use anyhow::{Context, Result, bail};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;

pub struct Mpd {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
}

/// One entry of an `lsinfo` listing.
pub enum Entry {
    Directory(String),
    /// A song's path and its tags, as (key, value) pairs in MPD's order
    File(String, Vec<(String, String)>),
}

impl Mpd {
    pub fn connect(addr: &str) -> Result<Self> {
        let stream = TcpStream::connect(addr).with_context(|| format!("can't reach MPD at {addr}"))?;
        let mut reader = BufReader::new(stream.try_clone()?);
        let mut greeting = String::new();
        reader.read_line(&mut greeting)?;
        if !greeting.starts_with("OK MPD") {
            bail!("unexpected MPD greeting: {greeting:?}");
        }
        Ok(Self { reader, writer: stream })
    }

    /// Sends a command and returns its response lines as (key, value) pairs.
    fn command(&mut self, command: &str) -> Result<Vec<(String, String)>> {
        self.writer.write_all(command.as_bytes())?;
        self.writer.write_all(b"\n")?;
        let mut pairs = Vec::new();
        let mut line = String::new();
        loop {
            line.clear();
            if self.reader.read_line(&mut line)? == 0 {
                bail!("MPD closed the connection");
            }
            let line = line.trim_end_matches('\n');
            if line == "OK" {
                return Ok(pairs);
            }
            if let Some(err) = line.strip_prefix("ACK ") {
                bail!("MPD: {err}");
            }
            if let Some((key, value)) = line.split_once(": ") {
                pairs.push((key.to_string(), value.to_string()));
            }
        }
    }

    /// The contents of one directory ("" is the root).
    pub fn lsinfo(&mut self, dir: &str) -> Result<Vec<Entry>> {
        let mut entries = Vec::new();
        for (key, value) in self.command(&format!("lsinfo {}", quote(dir)))? {
            match key.as_str() {
                "directory" => entries.push(Entry::Directory(value)),
                "file" => entries.push(Entry::File(value, Vec::new())),
                // a playlist starts its own entry; its tags aren't wanted
                "playlist" => entries.push(Entry::Directory(String::new())),
                _ => {
                    if let Some(Entry::File(_, tags)) = entries.last_mut() {
                        tags.push((key, value));
                    }
                }
            }
        }
        entries.retain(|e| !matches!(e, Entry::Directory(d) if d.is_empty()));
        Ok(entries)
    }

    /// Every song under `dir` (disc folders included) with its tags, in the
    /// order `add` queues them: both walk MPD's database the same way.
    pub fn songs(&mut self, dir: &str) -> Result<Vec<(String, Vec<(String, String)>)>> {
        let mut songs: Vec<(String, Vec<(String, String)>)> = Vec::new();
        let mut in_song = false;
        for (key, value) in self.command(&format!("listallinfo {}", quote(dir)))? {
            match key.as_str() {
                "file" => {
                    songs.push((value, Vec::new()));
                    in_song = true;
                }
                // directories and playlists carry tags of their own
                "directory" | "playlist" => in_song = false,
                _ if in_song => songs.last_mut().expect("in a song").1.push((key, value)),
                _ => {}
            }
        }
        Ok(songs)
    }

    /// The path of the song playing or paused, if any.
    pub fn current_file(&mut self) -> Result<Option<String>> {
        Ok(self.command("currentsong")?.into_iter().find(|(k, _)| k == "file").map(|(_, v)| v))
    }

    /// Replaces the queue with everything under `dir` and starts playing.
    pub fn play_dir(&mut self, dir: &str) -> Result<()> {
        self.command(&format!(
            "command_list_begin\nclear\nadd {}\nplay 0\ncommand_list_end",
            quote(dir)
        ))?;
        Ok(())
    }

    /// Replaces the queue with everything under `dir` and plays from song
    /// number `index` (0 is the first).
    pub fn play_dir_from(&mut self, dir: &str, index: usize) -> Result<()> {
        self.command(&format!(
            "command_list_begin\nclear\nadd {}\nplay {index}\ncommand_list_end",
            quote(dir)
        ))?;
        Ok(())
    }

    /// Adds everything under `dir` to the end of the queue.
    pub fn add_dir(&mut self, dir: &str) -> Result<()> {
        self.command(&format!("add {}", quote(dir)))?;
        Ok(())
    }
}

fn quote(arg: &str) -> String {
    format!("\"{}\"", arg.replace('\\', "\\\\").replace('"', "\\\""))
}
