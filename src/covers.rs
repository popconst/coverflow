//! Album covers, loaded one at a time in a background thread (gentle on a
//! slow music drive) and kept as small PNG thumbnails in a cache on disk, so
//! each cover is read from the music folder only once.

use image::DynamicImage;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;

const THUMB_SIZE: u32 = 600;
const NAMES: [&str; 6] = ["cover.jpg", "cover.png", "folder.jpg", "Folder.jpg", "front.jpg", "Front.jpg"];

pub struct Loaded {
    pub index: usize,
    pub image: Option<DynamicImage>,
}

pub struct Loader {
    requests: Sender<(usize, PathBuf)>,
    pub results: Receiver<Loaded>,
}

impl Loader {
    pub fn spawn(cache_dir: PathBuf) -> Self {
        let (requests, request_rx) = channel::<(usize, PathBuf)>();
        let (result_tx, results) = channel();
        thread::spawn(move || {
            let _ = std::fs::create_dir_all(&cache_dir);
            for (index, album_dir) in request_rx {
                let image = load(&cache_dir, &album_dir);
                if result_tx.send(Loaded { index, image }).is_err() {
                    break;
                }
            }
        });
        Self { requests, results }
    }

    /// Asks for album `index`'s cover; the answer comes on `results`
    pub fn request(&self, index: usize, album_dir: PathBuf) {
        let _ = self.requests.send((index, album_dir));
    }
}

fn cache_path(cache_dir: &Path, album_dir: &Path) -> PathBuf {
    let mut hasher = DefaultHasher::new();
    album_dir.hash(&mut hasher);
    cache_dir.join(format!("{:016x}.png", hasher.finish()))
}

fn load(cache_dir: &Path, album_dir: &Path) -> Option<DynamicImage> {
    let cached = cache_path(cache_dir, album_dir);
    if let Ok(image) = image::open(&cached) {
        return Some(image);
    }
    let source = NAMES.iter().map(|n| album_dir.join(n)).find(|p| p.is_file())?;
    let image = image::open(source).ok()?.thumbnail(THUMB_SIZE, THUMB_SIZE);
    let _ = image.save(&cached);
    Some(image)
}
