//! coverflow: flip through your MPD library by album cover, Cover Flow style,
//! in the terminal. Plays alongside any other MPD client (rmpc, ncmpcpp…).

mod covers;
mod fold;
mod library;
mod mpd;

use anyhow::{Context, Result};
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers,
    MouseButton, MouseEvent, MouseEventKind,
};
use fold::Facing;
use image::DynamicImage;
use library::Album;
use ratatui::{
    DefaultTerminal, Frame,
    layout::{Alignment, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{Clear, Paragraph},
};
use ratatui_image::{StatefulImage, picker::Picker, protocol::StatefulProtocol};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

const HELP: &str =
    "h/l ←/→ scroll flip · H/L ±10 · g/G ends · / search · c playing · ⏎ play · a add · q quit";
/// Height of the folded side covers compared with the centre one
const SIDE_HEIGHT: f32 = 0.86;
/// How much each folded cover hides of the one behind it
const SIDE_OVERLAP: f32 = 0.45;
/// Most folded covers drawn on each side
const MAX_SIDE: usize = 10;
/// A cover turns through this many steps on its way to or from the centre
const FOLD_STEPS: u8 = 4;
/// How quickly the row glides to the selection (higher is snappier)
const GLIDE: f32 = 14.0;
/// Further than this, a jump starts the glide close to its target
const MAX_GLIDE: f32 = 6.0;
/// Fastest a scroll wheel or trackpad flips: one album per this long
const SCROLL_STEP: Duration = Duration::from_millis(45);
/// How far around the selection covers are loaded ahead of time
const PREFETCH: usize = MAX_SIDE + 2;
/// Covers further than this from the selection are dropped from memory
const KEEP: usize = 40;

struct Options {
    address: String,
    music_dir: PathBuf,
    /// print the album list and exit
    list: bool,
}

/// How a cover is drawn: how far it's turned (0 flat … FOLD_STEPS fully),
/// which way, and how many places beyond the first fold (for shading)
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct View {
    facing: Facing,
    step: u8,
    depth: u8,
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
    /// Where the row is drawn: glides towards `selected`
    pos: f32,
    last_tick: Instant,
    mode: Mode,
    last_query: Option<String>,
    loader: covers::Loader,
    /// Loaded covers; None when an album has none
    images: HashMap<usize, Option<DynamicImage>>,
    /// Encoded images per album and how it's shown; re-encoded only when
    /// their size changes
    protocols: HashMap<(usize, View), StatefulProtocol>,
    /// Where each cover was drawn last, for mouse clicks
    hits: Vec<(Rect, usize)>,
    last_scroll: Instant,
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
        pos: 0.0,
        last_tick: Instant::now(),
        mode: Mode::Browse,
        last_query: None,
        loader: covers::Loader::spawn(cache_dir),
        images: HashMap::new(),
        protocols: HashMap::new(),
        hits: Vec::new(),
        last_scroll: Instant::now(),
        requested: HashSet::new(),
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
                self.pos = self.selected as f32;
            }
            while let Ok(loaded) = self.loader.results.try_recv() {
                self.images.insert(loaded.index, loaded.image);
                self.protocols.retain(|(i, _), _| *i != loaded.index);
            }
            let gliding = self.glide();
            self.prefetch();
            terminal.draw(|frame| self.draw(frame))?;

            // about 60 frames a second while gliding, otherwise just wait
            let wait = if gliding { 16 } else { 80 };
            if event::poll(Duration::from_millis(wait))? {
                match event::read()? {
                    Event::Key(key) if key.kind == KeyEventKind::Press && !self.handle_key(key) => {
                        return Ok(());
                    }
                    Event::Mouse(mouse) => self.handle_mouse(mouse),
                    _ => {}
                }
            }
        }
    }

    /// Moves `pos` a frame's worth towards the selection, easing out;
    /// returns whether it's still moving
    fn glide(&mut self) -> bool {
        let dt = self.last_tick.elapsed().as_secs_f32().min(0.1);
        self.last_tick = Instant::now();
        let target = self.selected as f32;
        // long jumps (search, g/G) start near the target rather than racing
        // past hundreds of covers
        if (target - self.pos).abs() > MAX_GLIDE {
            self.pos = target - MAX_GLIDE.copysign(target - self.pos);
        }
        self.pos += (target - self.pos) * (1.0 - (-GLIDE * dt).exp());
        if (target - self.pos).abs() < 0.01 {
            self.pos = target;
        }
        self.pos != target
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

    /// Scrolling (wheel or trackpad, either direction) flips; clicking a
    /// side cover brings it to the centre
    fn handle_mouse(&mut self, mouse: MouseEvent) {
        let delta = match mouse.kind {
            MouseEventKind::ScrollRight | MouseEventKind::ScrollDown => 1,
            MouseEventKind::ScrollLeft | MouseEventKind::ScrollUp => -1,
            MouseEventKind::Down(MouseButton::Left) => {
                // the cover drawn last is on top
                let hit = self.hits.iter().rev().find(|(r, _)| {
                    (r.x..r.x + r.width).contains(&mouse.column) && (r.y..r.y + r.height).contains(&mouse.row)
                });
                if let Some(&(_, index)) = hit {
                    self.selected = index;
                }
                return;
            }
            _ => return,
        };
        // a trackpad sends dozens of scroll events per swipe
        if self.last_scroll.elapsed() >= SCROLL_STEP {
            self.last_scroll = Instant::now();
            self.status = None;
            self.step(delta);
        }
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
        // every cover by its distance from where the row is (pos glides,
        // so covers slide and turn between places): flat in the centre,
        // turning on the way out, then stacked and overlapping
        let side_rows = (rows as f32) * SIDE_HEIGHT;
        let side_cols = side_rows * aspect * fold::SQUEEZE;
        let first = cols as f32 / 2.0 + 1.0 + side_cols / 2.0;
        let spacing = side_cols * (1.0 - SIDE_OVERLAP);
        let mid_x = covers.x as f32 + covers.width as f32 / 2.0;
        let mid_y = covers.y as f32 + covers.height as f32 / 2.0;

        let lo = (self.pos - MAX_SIDE as f32 - 1.0).floor().max(0.0) as usize;
        let hi = ((self.pos + MAX_SIDE as f32 + 1.0).ceil() as usize).min(albums.len() - 1);
        let mut placed = Vec::new();
        for i in lo..=hi {
            let d = i as f32 - self.pos;
            let far = d.abs();
            let step = (far.min(1.0) * FOLD_STEPS as f32).round() as u8;
            let turn = step as f32 / FOLD_STEPS as f32;
            let h = rows as f32 + (side_rows - rows as f32) * turn;
            let w = h * aspect * (1.0 - (1.0 - fold::SQUEEZE) * turn);
            let offset = if far <= 1.0 { far * first } else { first + (far - 1.0) * spacing };
            let x = mid_x + offset.copysign(d) - w / 2.0;
            let rect = Rect::new(x.round() as u16, (mid_y - h / 2.0).round() as u16, w.round() as u16, h.round() as u16);
            if x < covers.x as f32 || rect.x + rect.width > covers.x + covers.width || rect.width == 0 {
                continue;
            }
            let view = View {
                facing: if d < 0.0 { Facing::Right } else { Facing::Left },
                step,
                // shading levels beyond the first fold; capped, as each is
                // its own image
                depth: (far - 1.0).max(0.0).round().min(4.0) as u8,
            };
            placed.push((far, i, rect, view));
        }
        // far ones first, so nearer covers are drawn over them
        placed.sort_by(|a, b| b.0.total_cmp(&a.0));
        self.hits.clear();
        for (_, i, rect, view) in placed {
            self.draw_cover(frame, i, rect, view);
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

    fn draw_cover(&mut self, frame: &mut Frame, index: usize, area: Rect, view: View) {
        self.hits.push((area, index));
        match self.images.get(&index) {
            Some(Some(image)) => {
                let picker = &self.picker;
                let protocol = self.protocols.entry((index, view)).or_insert_with(|| {
                    let turn = view.step as f32 / FOLD_STEPS as f32;
                    // darker the further out; flat in the centre as it is
                    let light = if view.step == 0 { 1.0 } else { 0.92 - 0.08 * view.depth as f32 };
                    // side covers are small: fold a smaller copy, it's 4× less work
                    let image = if view.step == 0 {
                        image.clone()
                    } else {
                        fold::fold(&image.thumbnail(320, 320), view.facing, turn, light)
                    };
                    picker.new_resize_protocol(image)
                });
                frame.render_stateful_widget(StatefulImage::default(), area, protocol);
            }
            // no cover: just the title, dimmed; still loading: nothing
            Some(None) => {
                let title = self.albums.as_ref().map(|a| a[index].title.clone()).unwrap_or_default();
                let middle = Rect { y: area.y + area.height / 2, height: 1, ..area };
                frame.render_widget(Clear, area);
                frame.render_widget(Paragraph::new(title).alignment(Alignment::Center).dim(), middle);
            }
            None => frame.render_widget(Clear, area),
        }
    }
}
