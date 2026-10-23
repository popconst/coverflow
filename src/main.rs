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
    "h/l ←/→ scroll flip · H/L ±10 · g/G ends · / search · c playing · ⏎ play · a add · q quit";
/// How long the row takes to glide to a new album
const GLIDE: Duration = Duration::from_millis(350);
/// Further than this, a jump (search, g/G) starts its glide this close to
/// the target instead of racing past hundreds of covers
const MAX_GLIDE: f32 = 5.0;
/// Fastest a scroll wheel or trackpad flips: one album per this long
const SCROLL_STEP: Duration = Duration::from_millis(60);
/// Widest the row is rendered while it moves, in pixels; the terminal scales
/// it up. More than this is more than herdr passes on ~30 times a second.
/// Once it stops it's rendered once more at the pane's full resolution
const MOTION_WIDTH: f32 = 1000.0;
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
    Kitty(kitty::Picture),
    /// Sixel, iTerm2 or half-blocks, through ratatui-image
    Other(Protocol),
}

struct App {
    options: Options,
    picker: Picker,
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
    /// Two image ids, used in turn, so a frame is never shown half-sent
    kitty_ids: [u32; 2],
    flip: usize,
    /// Where each cover was drawn, in canvas pixels, and the canvas scale
    hits: Vec<render::Hit>,
    covers_area: Rect,
    px_per_cell: f32,
    last_scroll: Instant,
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
    // kitty image ids are 24 bits plus a high byte; keep them small and ours
    let base = (std::process::id() % 0x7f_ff00) + 0x100;
    let mut app = App {
        options,
        picker,
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
        kitty_ids: [base, base + 1],
        flip: 0,
        hits: Vec::new(),
        covers_area: Rect::default(),
        px_per_cell: 1.0,
        last_scroll: Instant::now(),
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
            if event::poll(wait)? {
                match event::read()? {
                    Event::Key(key) if key.kind == KeyEventKind::Press && !self.handle_key(key) => {
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
        let covers = Rect { height: area.height.saturating_sub(text_height + 1), ..area };
        let pos = self.glide.pos();
        self.draw_row(frame, covers, pos, albums.len());

        // the album nearest the centre: the title changes as covers arrive
        let shown = &albums[(pos.round() as usize).min(albums.len() - 1)];
        let year = if shown.year.is_empty() { String::new() } else { format!(" · {}", shown.year) };
        let bottom = match (&self.mode, &self.status) {
            (Mode::Search { query, .. }, _) => Line::from(format!("/{query}▏")),
            (_, Some(status)) => Line::from(status.clone()),
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

    /// Renders the row as one image when it moved (or a cover arrived), and
    /// places it; otherwise places the last one again, which costs nothing.
    fn draw_row(&mut self, frame: &mut Frame, area: Rect, pos: f32, count: usize) {
        if area.width == 0 || area.height == 0 {
            return;
        }
        let still = self.glide.done();
        let fresh = matches!(&self.frame, Some((_, p, a, s)) if *p == pos && *a == area && *s == still);
        if !fresh || self.stale {
            let font = self.picker.font_size();
            let full_width = area.width as f32 * font.width as f32;
            let max_width = if still { STILL_WIDTH } else { MOTION_WIDTH };
            let scale = (max_width / full_width).min(1.0);
            let width = (full_width * scale).round() as u32;
            let height = (area.height as f32 * font.height as f32 * scale).round() as u32;
            let scene = if still { &mut self.still } else { &mut self.motion };
            scene.resize(width, height);
            let (canvas, hits) = scene.compose(&self.images, pos, count);
            self.hits = hits;
            self.covers_area = area;
            self.px_per_cell = font.width as f32 * scale;
            if let Some(row) = self.encode(canvas, area) {
                self.frame = Some((row, pos, area, still));
            }
            self.stale = false;
        }
        match &self.frame {
            Some((Row::Kitty(picture), ..)) => frame.render_widget(picture, area),
            Some((Row::Other(protocol), ..)) => frame.render_widget(Image::new(protocol), area),
            None => {}
        }
    }

    /// The row image for the terminal. With kitty graphics it goes to one of
    /// two fixed image ids in turn: re-sending an id replaces its picture
    /// instead of piling up new ones in the terminal.
    fn encode(&mut self, canvas: image::RgbaImage, area: Rect) -> Option<Row> {
        if self.picker.protocol_type() == ProtocolType::Kitty {
            self.flip ^= 1;
            let compress = self.picker.capabilities().contains(&Capability::KittyCompression);
            let id = self.kitty_ids[self.flip];
            Some(Row::Kitty(kitty::Picture::new(&canvas, id, area.width, area.height, compress)))
        } else {
            let image = DynamicImage::ImageRgba8(canvas);
            self.picker.new_protocol(image, area.into(), Resize::Fit(None)).ok().map(Row::Other)
        }
    }
}
