//! coverflow: flip through your MPD library by album cover, Cover Flow style,
//! in the terminal. Plays alongside any other MPD client (rmpc, ncmpcpp…).

mod covers;
mod library;
mod kitty;
mod mpd;
mod render;

use anyhow::{Context, Result};
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
    MouseButton, MouseEvent, MouseEventKind,
};
use image::DynamicImage;
use library::Album;
use ratatui::{
    DefaultTerminal, Frame,
    layout::{Alignment, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::Paragraph,
};
use ratatui_image::{
    Image, Resize,
    picker::{Capability, Picker, ProtocolType},
    protocol::Protocol,
};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

const HELP: &str =
    "h/l ←/→ scroll flip · H/L ±10 · g/G ends · / search · c playing · k tracks · ⏎ play · a add · q quit";
const TRACKS_HELP: &str = "j/k ↓/↑ scroll choose · ⏎ click play from here · a add album · esc back";
/// How long the row takes to glide to a new album
const GLIDE: Duration = Duration::from_millis(350);
/// Further than this, a jump (search, g/G) starts its glide this close to
/// the target instead of racing past hundreds of covers
const MAX_GLIDE: f32 = 5.0;
/// Fastest the row flips, by key repeat or scroll wheel or trackpad: one
/// album per this long. Faster events are dropped, not queued, so the row
/// stops the moment the key or the swipe does
const STEP: Duration = Duration::from_millis(70);
/// Widest the row is rendered while it moves, in pixels; the terminal scales
/// it up. Only the folded side covers are drawn this small (the centre one
/// is sharp), and only inside multiplexers like herdr, which slow down with
/// every pixel. Once the row stops it's rendered at full resolution
const MOTION_WIDTH: f32 = 800.0;
/// Time per frame while the row moves, at most
const FRAME: Duration = Duration::from_millis(33);
/// Widest a still frame is rendered
const STILL_WIDTH: f32 = 3200.0;
/// How far around the selection covers are loaded ahead of time
const PREFETCH: usize = render::MAX_SIDE + 2;
/// Covers further than this from the selection are dropped from memory
const KEEP: usize = 40;

struct Options {
    address: String,
    music_dir: PathBuf,
    /// print the album list and exit
    list: bool,
}

enum Mode {
    Browse,
    Search { query: String, from: usize },
    /// The selected album's tracks, next to its cover
    Tracks { tracks: Vec<Track>, cursor: usize, scroll: usize },
}

struct Track {
    /// "3" or, on albums of several discs, "2-03"
    number: String,
    title: String,
    seconds: Option<u32>,
}

/// The row's glide towards the selection: an ease-out from `from` to `to`
struct Glide {
    from: f32,
    to: f32,
    start: Instant,
}

impl Glide {
    fn pos(&self) -> f32 {
        let t = (self.start.elapsed().as_secs_f32() / GLIDE.as_secs_f32()).min(1.0);
        let ease = 1.0 - (1.0 - t).powi(3);
        self.from + (self.to - self.from) * ease
    }

    fn done(&self) -> bool {
        self.start.elapsed() >= GLIDE
    }
}

/// The row, encoded for the terminal
enum Row {
    /// One or more pictures side by side, each over its cells
    Kitty(Vec<(kitty::Picture, Rect)>),
    /// Sixel, iTerm2 or half-blocks, through ratatui-image
    Other(Protocol),
}

struct App {
    options: Options,
    picker: Picker,
    medium: kitty::Medium,
    albums: Option<Vec<Album>>,
    library_rx: Receiver<Result<Vec<Album>>>,
    selected: usize,
    glide: Glide,
    mode: Mode,
    last_query: Option<String>,
    loader: covers::Loader,
    /// Loaded covers; None when an album has none
    images: HashMap<usize, Option<DynamicImage>>,
    requested: HashSet<usize>,
    /// Two sizes of the row, so switching between them doesn't rescale
    /// every cover: small while it moves, full resolution when it stops
    motion: render::Scene,
    still: render::Scene,
    /// The row as last rendered, and what for: position, area, still or not
    frame: Option<(Row, f32, Rect, bool)>,
    /// Something besides the position changed (a cover arrived): re-render
    stale: bool,
    /// Two sets of three image ids (a frame is up to three pictures), used
    /// in turn, so a frame is never shown half-sent
    kitty_ids: [u32; 2],
    flip: usize,
    /// Where each cover was drawn, in canvas pixels, and the canvas scale
    hits: Vec<render::Hit>,
    covers_area: Rect,
    /// Where the track list was drawn, for clicks
    tracks_area: Rect,
    px_per_cell: f32,
    last_step: Instant,
    status: Option<String>,
}

fn main() -> Result<()> {
    let options = parse_args()?;
    if options.list {
        let albums = library::load(&mut mpd::Mpd::connect(&options.address)?)?;
        for a in &albums {
            println!("{}\t{}\t{}\t{}", a.artist, a.year, a.title, a.dir);
        }
        eprintln!("{} albums", albums.len());
        return Ok(());
    }
    let cache_dir = home().join("Library/Caches/coverflow");

    // read the library in the background; it takes a moment on a big one
    let (library_tx, library_rx) = channel();
    let address = options.address.clone();
    std::thread::spawn(move || {
        let result = mpd::Mpd::connect(&address).and_then(|mut mpd| library::load(&mut mpd));
        let _ = library_tx.send(result);
    });

    let mut terminal = ratatui::init();
    // ask the terminal how it draws images (kitty, sixel…) and its cell size
    let picker = Picker::from_query_stdio().unwrap_or_else(|_| Picker::halfblocks());
    // herdr takes images as files too, but then handles every pixel itself
    // and slows to a crawl; inside it, frames go in the escape codes
    let in_herdr = std::env::var("TERM_PROGRAM").is_ok_and(|t| t == "herdr");
    let temp_dir = if picker.protocol_type() == ProtocolType::Kitty && !in_herdr {
        kitty::probe_temp_file()
    } else {
        None
    };
    let medium = match temp_dir {
        Some(dir) => kitty::Medium::TempFile(dir),
        None => kitty::Medium::Direct { compress: picker.capabilities().contains(&Capability::KittyCompression) },
    };
    // kitty image ids are 24 bits plus a high byte; keep them small and ours
    let base = (std::process::id() % 0x7f_ff00) + 0x100;
    let mut app = App {
        options,
        picker,
        medium,
        albums: None,
        library_rx,
        selected: 0,
        glide: Glide { from: 0.0, to: 0.0, start: Instant::now() },
        mode: Mode::Browse,
        last_query: None,
        loader: covers::Loader::spawn(cache_dir),
        images: HashMap::new(),
        requested: HashSet::new(),
        motion: render::Scene::default(),
        still: render::Scene::default(),
        frame: None,
        stale: true,
        kitty_ids: [base, base + 3],
        flip: 0,
        hits: Vec::new(),
        covers_area: Rect::default(),
        tracks_area: Rect::default(),
        px_per_cell: 1.0,
        last_step: Instant::now() - STEP,
        status: None,
    };
    crossterm::execute!(std::io::stdout(), EnableMouseCapture)?;
    let result = app.run(&mut terminal);
    let _ = crossterm::execute!(std::io::stdout(), DisableMouseCapture);
    ratatui::restore();
    result
}

fn parse_args() -> Result<Options> {
    let mut options = Options {
        address: std::env::var("MPD_HOST")
            .map(|h| format!("{h}:{}", std::env::var("MPD_PORT").unwrap_or_else(|_| "6600".into())))
            .unwrap_or_else(|_| "127.0.0.1:6600".into()),
        music_dir: music_dir_from_mpd_conf().unwrap_or_else(|| home().join("Music")),
        list: false,
    };
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--address" | "-a" => options.address = args.next().context("--address needs host:port")?,
            "--music-dir" | "-m" => {
                options.music_dir = PathBuf::from(args.next().context("--music-dir needs a path")?)
            }
            "--list" | "-l" => options.list = true,
            "--help" | "-h" => {
                println!(
                    "coverflow: browse your MPD library by album cover\n\n\
                     Usage: coverflow [--address host:port] [--music-dir path] [--list]\n\n\
                     --address    MPD to talk to (default $MPD_HOST:$MPD_PORT or 127.0.0.1:6600)\n\
                     --music-dir  MPD's music_directory, where covers are read from\n\
                     \x20            (default: from ~/.config/mpd/mpd.conf, else ~/Music)\n\
                     --list       print the albums, in coverflow's order, and exit\n\n\
                     Keys: {HELP}"
                );
                std::process::exit(0);
            }
            other => anyhow::bail!("unknown argument {other:?} (see --help)"),
        }
    }
    Ok(options)
}

/// 3:07, or 1:02:44
fn duration(seconds: u32) -> String {
    let (h, m, s) = (seconds / 3600, seconds / 60 % 60, seconds % 60);
    if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m}:{s:02}") }
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default()
}

fn music_dir_from_mpd_conf() -> Option<PathBuf> {
    let conf = std::fs::read_to_string(home().join(".config/mpd/mpd.conf")).ok()?;
    let line = conf.lines().map(str::trim).find(|l| l.starts_with("music_directory"))?;
    let value = line.split_once(char::is_whitespace)?.1.trim().trim_matches('"');
    Some(match value.strip_prefix("~/") {
        Some(rest) => home().join(rest),
        None => PathBuf::from(value),
    })
}

impl App {
    fn run(&mut self, terminal: &mut DefaultTerminal) -> Result<()> {
        loop {
            if self.albums.is_none()
                && let Ok(result) = self.library_rx.try_recv()
            {
                self.albums = Some(result?);
                self.jump_to_playing();
                let at = self.selected as f32;
                self.glide = Glide { from: at, to: at, start: Instant::now() - GLIDE };
            }
            while let Ok(loaded) = self.loader.results.try_recv() {
                self.images.insert(loaded.index, loaded.image);
                self.motion.forget(loaded.index);
                self.still.forget(loaded.index);
                self.stale = true;
            }
            // a new selection: glide there from wherever the row is now
            let target = self.selected as f32;
            if target != self.glide.to {
                let mut from = self.glide.pos();
                if (target - from).abs() > MAX_GLIDE {
                    from = target - MAX_GLIDE.copysign(target - from);
                }
                self.glide = Glide { from, to: target, start: Instant::now() };
            }
            self.prefetch();
            let frame_start = Instant::now();
            terminal.draw(|frame| self.draw(frame))?;

            // up to ~30 frames a second while gliding, fewer when the
            // terminal takes longer to swallow one (the glide keeps its
            // timing either way); otherwise wait for input
            let wait = if self.glide.done() {
                Duration::from_millis(100)
            } else {
                FRAME.saturating_sub(frame_start.elapsed())
            };
            // then everything that's waiting, before the next frame: a
            // backlog would keep flipping after the key or swipe stopped
            let mut wait = wait;
            while event::poll(wait)? {
                wait = Duration::ZERO;
                match event::read()? {
                    Event::Key(key) if key.kind != KeyEventKind::Release && !self.handle_key(key) => {
                        return Ok(());
                    }
                    Event::Mouse(mouse) => self.handle_mouse(mouse),
                    Event::Resize(..) => self.stale = true,
                    _ => {}
                }
            }
        }
    }

    fn count(&self) -> usize {
        self.albums.as_ref().map_or(0, Vec::len)
    }

    fn select(&mut self, index: usize) {
        if self.count() > 0 {
            self.selected = index.min(self.count() - 1);
        }
    }

    /// Flips by `delta` albums, unless the last flip was too recent
    fn step(&mut self, delta: isize) {
        if self.last_step.elapsed() >= STEP {
            self.last_step = Instant::now();
            self.select((self.selected as isize + delta).max(0) as usize);
        }
    }

    fn mpd(&self) -> Result<mpd::Mpd> {
        mpd::Mpd::connect(&self.options.address)
    }

    fn jump_to_playing(&mut self) {
        let file = self.mpd().and_then(|mut m| m.current_file()).ok().flatten();
        if let (Some(file), Some(albums)) = (file, &self.albums)
            && let Some(index) = library::album_of(albums, &file)
        {
            self.selected = index;
        }
    }

    /// Returns false to quit.
    fn handle_key(&mut self, key: KeyEvent) -> bool {
        self.status = None;
        if let Mode::Search { query, from } = &mut self.mode {
            let from = *from;
            match key.code {
                KeyCode::Esc => {
                    self.mode = Mode::Browse;
                    self.select(from);
                }
                KeyCode::Enter => self.mode = Mode::Browse,
                KeyCode::Backspace | KeyCode::Char(_) => {
                    match key.code {
                        KeyCode::Char(c) => query.push(c),
                        _ => {
                            query.pop();
                        }
                    }
                    let query = query.clone();
                    self.last_query = Some(query.clone());
                    self.find(&query, from, true);
                }
                _ => {}
            }
            return true;
        }

        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if let Mode::Tracks { tracks, cursor, .. } = &mut self.mode {
            let last = tracks.len().saturating_sub(1);
            match key.code {
                KeyCode::Char('c') if ctrl => return false,
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Backspace => self.mode = Mode::Browse,
                KeyCode::Char('j') | KeyCode::Down => *cursor = (*cursor + 1).min(last),
                KeyCode::Char('k') | KeyCode::Up => *cursor = cursor.saturating_sub(1),
                KeyCode::Char('g') | KeyCode::Home => *cursor = 0,
                KeyCode::Char('G') | KeyCode::End => *cursor = last,
                KeyCode::Enter => {
                    let index = *cursor;
                    self.play_from(index);
                }
                KeyCode::Char('a') => self.with_selected("Added to the queue", |mpd, dir| mpd.add_dir(dir)),
                // flipping to another album closes the list
                KeyCode::Char('h') | KeyCode::Left => {
                    self.mode = Mode::Browse;
                    self.step(-1);
                }
                KeyCode::Char('l') | KeyCode::Right => {
                    self.mode = Mode::Browse;
                    self.step(1);
                }
                _ => {}
            }
            return true;
        }
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return false,
            KeyCode::Char('c') if ctrl => return false,
            KeyCode::Char('l') | KeyCode::Right | KeyCode::Down => self.step(1),
            KeyCode::Char('h') | KeyCode::Left | KeyCode::Up => self.step(-1),
            KeyCode::Char('k') => self.open_tracks(),
            KeyCode::Char('L') | KeyCode::PageDown => self.step(10),
            KeyCode::Char('H') | KeyCode::PageUp => self.step(-10),
            KeyCode::Char('g') | KeyCode::Home => self.select(0),
            KeyCode::Char('G') | KeyCode::End => self.select(usize::MAX),
            KeyCode::Char('/') => self.mode = Mode::Search { query: String::new(), from: self.selected },
            KeyCode::Char('n') => self.find_last(true),
            KeyCode::Char('N') => self.find_last(false),
            KeyCode::Char('c') => self.jump_to_playing(),
            KeyCode::Enter => self.with_selected("Playing", |mpd, dir| mpd.play_dir(dir)),
            KeyCode::Char('a') => self.with_selected("Added to the queue", |mpd, dir| mpd.add_dir(dir)),
            _ => {}
        }
        true
    }

    /// Scrolling (wheel or trackpad, either direction) flips; clicking a
    /// side cover brings it to the centre
    fn handle_mouse(&mut self, mouse: MouseEvent) {
        if let Mode::Tracks { tracks, cursor, scroll } = &mut self.mode {
            let list = self.tracks_area;
            let last = tracks.len().saturating_sub(1);
            match mouse.kind {
                MouseEventKind::ScrollDown | MouseEventKind::ScrollRight => *cursor = (*cursor + 1).min(last),
                MouseEventKind::ScrollUp | MouseEventKind::ScrollLeft => *cursor = cursor.saturating_sub(1),
                MouseEventKind::Down(MouseButton::Left) => {
                    let inside = (list.x..list.x + list.width).contains(&mouse.column)
                        && (list.y..list.y + list.height).contains(&mouse.row);
                    let index = *scroll + mouse.row.saturating_sub(list.y) as usize;
                    if inside && index < tracks.len() {
                        *cursor = index;
                        self.play_from(index);
                    } else if !inside {
                        // a click on the covers goes back to them
                        self.mode = Mode::Browse;
                    }
                }
                _ => {}
            }
            return;
        }
        let delta = match mouse.kind {
            MouseEventKind::ScrollRight | MouseEventKind::ScrollDown => 1,
            MouseEventKind::ScrollLeft | MouseEventKind::ScrollUp => -1,
            MouseEventKind::Down(MouseButton::Left) => {
                let a = self.covers_area;
                if !(a.y..a.y + a.height).contains(&mouse.row) {
                    return;
                }
                let x = (mouse.column.saturating_sub(a.x) as f32 + 0.5) * self.px_per_cell;
                // the cover drawn last is on top
                if let Some(hit) = self.hits.iter().rev().find(|h| (h.left..h.right).contains(&x)) {
                    self.selected = hit.index;
                }
                return;
            }
            _ => return,
        };
        // a trackpad sends dozens of scroll events per swipe; step() drops
        // the ones that come too fast
        self.status = None;
        self.step(delta);
    }

    /// k: the selected album's tracks, from MPD's database
    fn open_tracks(&mut self) {
        let Some(album) = self.albums.as_ref().and_then(|a| a.get(self.selected)).cloned() else {
            return;
        };
        let songs = match self.mpd().and_then(|mut m| m.songs(&album.dir)) {
            Ok(songs) => songs,
            Err(e) => {
                self.status = Some(format!("Error: {e:#}"));
                return;
            }
        };
        let tag = |tags: &[(String, String)], key: &str| {
            tags.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v.clone())
        };
        // "3/12" → 3
        let number = |value: Option<String>| value.and_then(|v| v.split('/').next()?.trim().parse::<u32>().ok());
        let discs: HashSet<u32> = songs.iter().filter_map(|(_, t)| number(tag(t, "Disc"))).collect();
        let tracks = songs
            .iter()
            .map(|(file, tags)| {
                let track = number(tag(tags, "Track"));
                let disc = number(tag(tags, "Disc"));
                Track {
                    number: match (track, disc) {
                        (Some(t), Some(d)) if discs.len() > 1 => format!("{d}-{t:02}"),
                        (Some(t), _) => t.to_string(),
                        _ => String::new(),
                    },
                    title: tag(tags, "Title").unwrap_or_else(|| {
                        // no title tag: the file name, without its extension
                        let name = file.rsplit('/').next().unwrap_or(file);
                        name.rsplit_once('.').map_or(name, |(n, _)| n).to_string()
                    }),
                    seconds: tag(tags, "duration")
                        .or_else(|| tag(tags, "Time"))
                        .and_then(|d| d.parse::<f32>().ok())
                        .map(|d| d.round() as u32),
                }
            })
            .collect();
        // start at the song playing, if it's on this album
        let playing = self.mpd().and_then(|mut m| m.current_file()).ok().flatten();
        let cursor = playing.and_then(|p| songs.iter().position(|(f, _)| *f == p)).unwrap_or(0);
        self.mode = Mode::Tracks { tracks, cursor, scroll: 0 };
    }

    /// Plays the selected album from its `index`th track
    fn play_from(&mut self, index: usize) {
        let title = match &self.mode {
            Mode::Tracks { tracks, .. } => tracks.get(index).map(|t| t.title.clone()).unwrap_or_default(),
            _ => String::new(),
        };
        self.with_selected(&format!("Playing {title}"), |mpd, dir| mpd.play_dir_from(dir, index));
    }

    fn with_selected(&mut self, verb: &str, action: impl FnOnce(&mut mpd::Mpd, &str) -> Result<()>) {
        let Some(album) = self.albums.as_ref().and_then(|a| a.get(self.selected)).cloned() else {
            return;
        };
        self.status = Some(match self.mpd().and_then(|mut m| action(&mut m, &album.dir)) {
            Ok(()) => format!("{verb}: {} — {}", album.artist, album.title),
            Err(e) => format!("Error: {e:#}"),
        });
    }

    /// Jumps to the first match from `from` on (wrapping), `from` itself
    /// included or not
    fn find(&mut self, query: &str, from: usize, inclusive: bool) {
        let Some(albums) = &self.albums else { return };
        if query.trim().is_empty() || albums.is_empty() {
            return;
        }
        let n = albums.len();
        let start = usize::from(!inclusive);
        if let Some(offset) = (start..n + start).find(|o| albums[(from + o) % n].matches(query)) {
            self.selected = (from + offset) % n;
        }
    }

    /// n / N: the next or previous match of the last search
    fn find_last(&mut self, forward: bool) {
        let Some(query) = self.last_query.clone() else { return };
        if forward {
            self.find(&query, self.selected, false);
        } else if let Some(albums) = &self.albums {
            let n = albums.len();
            if let Some(offset) = (1..=n).find(|o| albums[(self.selected + n - o) % n].matches(&query)) {
                self.selected = (self.selected + n - offset) % n;
            }
        }
    }

    fn prefetch(&mut self) {
        let Some(albums) = &self.albums else { return };
        if albums.is_empty() {
            return;
        }
        let lo = self.selected.saturating_sub(PREFETCH);
        let hi = (self.selected + PREFETCH).min(albums.len() - 1);
        // the selection first, then outwards
        let mut order: Vec<usize> = (lo..=hi).collect();
        order.sort_by_key(|i| i.abs_diff(self.selected));
        for i in order {
            if self.requested.insert(i) {
                self.loader.request(i, self.options.music_dir.join(&albums[i].dir));
            }
        }
        // forget covers far away, so memory stays flat
        let selected = self.selected;
        let far: Vec<usize> = self.images.keys().copied().filter(|i| i.abs_diff(selected) > KEEP).collect();
        for i in far {
            self.images.remove(&i);
            self.requested.remove(&i);
            self.motion.forget(i);
            self.still.forget(i);
        }
    }

    fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();
        let Some(albums) = self.albums.clone() else {
            let text = Paragraph::new("Reading the library…").alignment(Alignment::Center);
            frame.render_widget(text, Rect { y: area.height / 2, height: 1, ..area });
            return;
        };
        if albums.is_empty() {
            frame.render_widget(Paragraph::new("No albums in MPD's library."), area);
            return;
        }

        // the row of covers, then a blank line and three lines of text
        let text_height = 3.min(area.height);
        let mut covers = Rect { height: area.height.saturating_sub(text_height + 1), ..area };
        if matches!(self.mode, Mode::Tracks { .. }) {
            // the row on the left, the list on the right
            let row_width = covers.width * 9 / 20;
            let gap = 3.min(covers.width - row_width);
            let list = Rect { x: covers.x + row_width + gap, width: covers.width - row_width - gap, ..covers };
            self.draw_tracks(frame, list);
            covers.width = row_width;
        } else {
            self.tracks_area = Rect::default();
        }
        let pos = self.glide.pos();
        self.draw_row(frame, covers, pos, albums.len());

        // the album nearest the centre: the title changes as covers arrive
        let shown = &albums[(pos.round() as usize).min(albums.len() - 1)];
        let year = if shown.year.is_empty() { String::new() } else { format!(" · {}", shown.year) };
        let bottom = match (&self.mode, &self.status) {
            (Mode::Search { query, .. }, _) => Line::from(format!("/{query}▏")),
            (_, Some(status)) => Line::from(status.clone()),
            (Mode::Tracks { tracks, .. }, _) => {
                let total: u32 = tracks.iter().filter_map(|t| t.seconds).sum();
                Line::from(format!("{} tracks · {}   {TRACKS_HELP}", tracks.len(), duration(total))).dim()
            }
            _ => Line::from(format!("{} / {}   {HELP}", self.selected + 1, albums.len())).dim(),
        };
        let lines = vec![
            Line::from(Span::styled(shown.title.clone(), Style::new().add_modifier(Modifier::BOLD))),
            Line::from(vec![Span::styled(shown.artist.clone(), Style::new().fg(Color::Blue)), Span::raw(year)]),
            bottom,
        ];
        let text_area = Rect { y: area.y + area.height - text_height, height: text_height, ..area };
        frame.render_widget(Paragraph::new(lines).alignment(Alignment::Center), text_area);
    }

    /// The track list: number, title and length, the chosen one highlighted
    fn draw_tracks(&mut self, frame: &mut Frame, area: Rect) {
        let Mode::Tracks { tracks, cursor, scroll } = &mut self.mode else { return };
        self.tracks_area = area;
        let rows = area.height as usize;
        if rows == 0 || area.width < 12 {
            return;
        }
        // keep the chosen track in view, with a line of context
        if *cursor < *scroll + 1 {
            *scroll = cursor.saturating_sub(1);
        } else if *cursor + 2 > *scroll + rows {
            *scroll = (*cursor + 2).saturating_sub(rows);
        }
        *scroll = (*scroll).min(tracks.len().saturating_sub(rows));
        let shown = &tracks[*scroll..tracks.len().min(*scroll + rows)];

        let number_width = tracks.iter().map(|t| t.number.chars().count()).max().unwrap_or(0) as u16;
        let numbers = Rect { width: number_width, ..area };
        let lengths = Rect { x: area.x + area.width - 6, width: 6, ..area };
        let titles = Rect {
            x: area.x + number_width + 2,
            width: area.width.saturating_sub(number_width + 2 + 7),
            ..area
        };
        let column = |f: fn(&Track) -> String| shown.iter().map(|t| Line::from(f(t))).collect::<Vec<_>>();
        frame.render_widget(
            Paragraph::new(column(|t| t.number.clone())).alignment(Alignment::Right).fg(Color::Blue),
            numbers,
        );
        frame.render_widget(Paragraph::new(column(|t| t.title.clone())), titles);
        frame.render_widget(
            Paragraph::new(column(|t| t.seconds.map(duration).unwrap_or_default())).alignment(Alignment::Right),
            lengths,
        );
        let row = Rect { y: area.y + (*cursor - *scroll) as u16, height: 1, ..area };
        frame.buffer_mut().set_style(row, Style::new().bg(Color::Blue).fg(Color::Black));
    }

    /// Renders the row when it moved (or a cover arrived) and places it;
    /// otherwise places the last one again, which costs nothing.
    fn draw_row(&mut self, frame: &mut Frame, area: Rect, pos: f32, count: usize) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let still = self.glide.done();
        let fresh = matches!(&self.frame, Some((_, p, a, s)) if *p == pos && *a == area && *s == still);
        if !fresh || self.stale {
            let font = self.picker.font_size();
            let (fw, fh) = (font.width as f32, font.height as f32);
            let full = (area.width as f32 * fw, area.height as f32 * fh);
            let sharp = (STILL_WIDTH / full.0).min(1.0);
            let kitty = self.picker.protocol_type() == ProtocolType::Kitty;
            // frames sent as files cost the terminal nothing to pass on
            let cheap = matches!(self.medium, kitty::Medium::TempFile(_));
            let size = |scale: f32| ((full.0 * scale).round() as u32, (full.1 * scale).round() as u32);

            let (w, h) = size(sharp);
            self.still.resize(w, h);
            let (canvas, hits) = self.still.compose(&self.images, pos, count);
            self.hits = hits;
            self.covers_area = area;
            self.px_per_cell = fw * sharp;

            let row = if still || cheap || !kitty {
                self.encode(canvas, area)
            } else {
                // moving, through a multiplexer that slows down with every
                // pixel: the middle sharp, the folded sides from a smaller
                // render
                let soft = (MOTION_WIDTH / full.0).min(1.0);
                let (sw, sh) = size(soft);
                self.motion.resize(sw, sh);
                let (small, _) = self.motion.compose(&self.images, pos, count);
                // the centre cover, with a cell to spare each side
                let middle = ((h as f32 / (fw * sharp)).ceil() as u16 + 2).min(area.width);
                let left = (area.width - middle) / 2;
                let cut = |image: &image::RgbaImage, scale: f32, from: u16, to: u16| {
                    let x0 = (from as f32 * fw * scale).round() as u32;
                    let x1 = ((to as f32 * fw * scale).round() as u32).min(image.width());
                    image::imageops::crop_imm(image, x0, 0, x1.saturating_sub(x0), image.height()).to_image()
                };
                let parts = [
                    (cut(&small, soft, 0, left), Rect { width: left, ..area }),
                    (cut(&canvas, sharp, left, left + middle), Rect { x: area.x + left, width: middle, ..area }),
                    (
                        cut(&small, soft, left + middle, area.width),
                        Rect { x: area.x + left + middle, width: area.width - left - middle, ..area },
                    ),
                ];
                self.flip ^= 1;
                let ids = self.kitty_ids[self.flip];
                Some(Row::Kitty(
                    parts
                        .into_iter()
                        .enumerate()
                        .filter(|(_, (image, rect))| rect.width > 0 && image.width() > 0)
                        .map(|(k, (image, rect))| {
                            let picture = kitty::Picture::new(&image, ids + k as u32, rect.width, rect.height, &self.medium);
                            (picture, rect)
                        })
                        .collect(),
                ))
            };
            if let Some(row) = row {
                self.frame = Some((row, pos, area, still));
            }
            self.stale = false;
        }
        match &self.frame {
            Some((Row::Kitty(pictures), ..)) => {
                for (picture, rect) in pictures {
                    frame.render_widget(picture, *rect);
                }
            }
            Some((Row::Other(protocol), ..)) => frame.render_widget(Image::new(protocol), area),
            None => {}
        }
    }

    /// The row as one image for the terminal. With kitty graphics it goes to
    /// one of two fixed sets of image ids in turn: re-sending an id replaces
    /// its picture instead of piling up new ones in the terminal.
    fn encode(&mut self, canvas: image::RgbaImage, area: Rect) -> Option<Row> {
        if self.picker.protocol_type() == ProtocolType::Kitty {
            self.flip ^= 1;
            let id = self.kitty_ids[self.flip];
            let picture = kitty::Picture::new(&canvas, id, area.width, area.height, &self.medium);
            Some(Row::Kitty(vec![(picture, area)]))
        } else {
            let image = DynamicImage::ImageRgba8(canvas);
            self.picker.new_protocol(image, area.into(), Resize::Fit(None)).ok().map(Row::Other)
        }
    }
}
