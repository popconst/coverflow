//! coverflow: flip through your MPD library by album cover, Cover Flow style,
//! in the terminal. Plays alongside any other MPD client (rmpc, ncmpcpp…).

mod covers;
mod library;
mod mpd;

use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use image::DynamicImage;
use library::Album;
use ratatui::{
    DefaultTerminal, Frame,
    layout::{Alignment, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, BorderType, Paragraph, Wrap},
};
use ratatui_image::{StatefulImage, picker::Picker, protocol::StatefulProtocol};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;

const HELP: &str = "h/l ←/→ flip · H/L ±10 · g/G ends · / search · c playing · ⏎ play · a add · q quit";
/// Size of each neighbour relative to the centre cover, going outwards
const SIDE_SCALES: [f32; 3] = [0.72, 0.52, 0.38];
/// How far around the selection covers are loaded ahead of time
const PREFETCH: usize = 6;
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
}

struct App {
    options: Options,
    picker: Picker,
    albums: Option<Vec<Album>>,
    library_rx: Receiver<Result<Vec<Album>>>,
    selected: usize,
    mode: Mode,
    last_query: Option<String>,
    loader: covers::Loader,
    /// Loaded covers; None when an album has none
    images: HashMap<usize, Option<DynamicImage>>,
    /// Encoded images per (album, dimmed); re-encoded only when their size changes
    protocols: HashMap<(usize, bool), StatefulProtocol>,
    requested: HashSet<usize>,
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
    let mut app = App {
        options,
        picker,
        albums: None,
        library_rx,
        selected: 0,
        mode: Mode::Browse,
        last_query: None,
        loader: covers::Loader::spawn(cache_dir),
        images: HashMap::new(),
        protocols: HashMap::new(),
        requested: HashSet::new(),
        status: None,
    };
    let result = app.run(&mut terminal);
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
            }
            while let Ok(loaded) = self.loader.results.try_recv() {
                self.images.insert(loaded.index, loaded.image);
                self.protocols.remove(&(loaded.index, false));
                self.protocols.remove(&(loaded.index, true));
            }
            self.prefetch();
            terminal.draw(|frame| self.draw(frame))?;

            if event::poll(Duration::from_millis(80))?
                && let Event::Key(key) = event::read()?
                && key.kind == KeyEventKind::Press
                && !self.handle_key(key)
            {
                return Ok(());
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

    fn step(&mut self, delta: isize) {
        self.select((self.selected as isize + delta).max(0) as usize);
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
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => return false,
            KeyCode::Char('c') if ctrl => return false,
            KeyCode::Char('l' | 'j') | KeyCode::Right | KeyCode::Down => self.step(1),
            KeyCode::Char('h' | 'k') | KeyCode::Left | KeyCode::Up => self.step(-1),
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
        }
        self.protocols.retain(|(i, _), _| i.abs_diff(selected) <= KEEP);
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

        // three text lines under the covers, a blank line between
        let text_height = 3.min(area.height);
        let covers = Rect { height: area.height.saturating_sub(text_height + 1), ..area };
        let font = self.picker.font_size();
        // a square cover is this many cells wide per cell of height
        let aspect = font.height as f32 / font.width.max(1) as f32;

        let mut rows = covers.height;
        let mut cols = (rows as f32 * aspect).round() as u16;
        if cols > covers.width * 45 / 100 {
            cols = covers.width * 45 / 100;
            rows = (cols as f32 / aspect).round() as u16;
        }
        if rows == 0 || cols == 0 {
            return;
        }
        let center_x = covers.x + (covers.width - cols) / 2;
        let top = covers.y + (covers.height - rows) / 2;
        self.draw_cover(frame, self.selected, Rect::new(center_x, top, cols, rows), false);

        // neighbours, shrinking outwards, as many as fit
        let gap = 1;
        let mut left_edge = center_x;
        let mut right_edge = center_x + cols;
        for (step, scale) in SIDE_SCALES.iter().enumerate() {
            let side_rows = ((rows as f32) * scale).round().max(1.0) as u16;
            let side_cols = ((cols as f32) * scale).round().max(1.0) as u16;
            let y = top + (rows - side_rows) / 2;
            if let Some(i) = self.selected.checked_sub(step + 1)
                && left_edge >= covers.x + side_cols + gap
            {
                left_edge -= side_cols + gap;
                self.draw_cover(frame, i, Rect::new(left_edge, y, side_cols, side_rows), true);
            }
            let i = self.selected + step + 1;
            if i < albums.len() && right_edge + gap + side_cols <= covers.x + covers.width {
                self.draw_cover(frame, i, Rect::new(right_edge + gap, y, side_cols, side_rows), true);
                right_edge += gap + side_cols;
            }
        }

        // title, artist · year, then position / search / status
        let album = &albums[self.selected];
        let year = if album.year.is_empty() { String::new() } else { format!(" · {}", album.year) };
        let bottom = match (&self.mode, &self.status) {
            (Mode::Search { query, .. }, _) => Line::from(format!("/{query}▏")),
            (_, Some(status)) => Line::from(status.clone()),
            _ => Line::from(format!("{} / {}   {HELP}", self.selected + 1, albums.len())).dim(),
        };
        let lines = vec![
            Line::from(Span::styled(album.title.clone(), Style::new().add_modifier(Modifier::BOLD))),
            Line::from(vec![Span::styled(album.artist.clone(), Style::new().fg(Color::Blue)), Span::raw(year)]),
            bottom,
        ];
        let text_area = Rect { y: area.y + area.height - text_height, height: text_height, ..area };
        frame.render_widget(Paragraph::new(lines).alignment(Alignment::Center), text_area);
    }

    fn draw_cover(&mut self, frame: &mut Frame, index: usize, area: Rect, dim: bool) {
        if let Some(Some(image)) = self.images.get(&index) {
            let picker = &self.picker;
            let protocol = self.protocols.entry((index, dim)).or_insert_with(|| {
                let image = if dim { image.brighten(-70) } else { image.clone() };
                picker.new_resize_protocol(image)
            });
            frame.render_stateful_widget(StatefulImage::default(), area, protocol);
            return;
        }
        // no cover (or not loaded yet): a frame, with the title once we know there's none
        let title = match self.images.get(&index) {
            Some(None) => self.albums.as_ref().map(|a| a[index].title.clone()).unwrap_or_default(),
            _ => String::new(),
        };
        let style = if dim { Style::new().dim() } else { Style::new() };
        let block = Block::bordered().border_type(BorderType::Rounded).style(style);
        let text = Paragraph::new(title).block(block).alignment(Alignment::Center).wrap(Wrap { trim: true });
        frame.render_widget(text, area);
    }
}
