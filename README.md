# coverflow

Flip through your [MPD](https://www.musicpd.org/) library by album cover, Cover
Flow style, in the terminal. It's a companion to a full MPD client such as
[rmpc](https://github.com/mierak/rmpc): run it in a split next to it, find an
album by its cover, press Enter, and the other client shows it playing.

```
   ┌──┐ ┌────┐ ┌──────────┐ ┌────┐ ┌──┐
   │  │ │    │ │          │ │    │ │  │
   └──┘ │    │ │  cover   │ │    │ └──┘
        └────┘ │          │ └────┘
               └──────────┘
               Discovery
           Daft Punk · 2001
```

## Keys

| Key | |
|---|---|
| `h` `l` / `←` `→` (also `k` `j`) | previous / next album |
| `H` `L` / PgUp PgDn | 10 back / forward |
| `g` `G` | first / last |
| `/` | search artist, album or year (`Enter` keeps it, `Esc` goes back); `n` `N` next / previous match |
| `c` | the album that's playing |
| `Enter` | play the album (replaces the queue) |
| `a` | add the album to the end of the queue |
| `q` | quit |

## How it works

- **Albums** are the folders in MPD's library that hold tagged songs, with
  disc folders (`CD1/`, `Disc 2/`, …) folded into their album and `Playlists/`
  folders left out; sorted by album artist, year and title.
- **Covers** are each album folder's `cover.jpg` (or `folder.jpg`,
  `front.jpg`). They're read in the background, one at a time, and kept as
  small thumbnails in `~/Library/Caches/coverflow`, so a slow music drive is
  read once per cover and browsing works when it's unplugged.
- **Images** are drawn with whatever the terminal supports (kitty graphics,
  Sixel, iTerm2, or coloured half-blocks), via
  [ratatui-image](https://github.com/ratatui/ratatui-image). Works inside
  multiplexers that pass the kitty protocol through.

## Install

```sh
cargo install --git https://github.com/popconst/coverflow
```

## Options

```
coverflow [--address host:port] [--music-dir path] [--list]
```

- `--address`: MPD to talk to (default `$MPD_HOST:$MPD_PORT`, else
  `127.0.0.1:6600`)
- `--music-dir`: MPD's `music_directory`, where covers are read from
  (default: read from `~/.config/mpd/mpd.conf`, else `~/Music`)
- `--list`: print the album list in coverflow's order and exit

## License

MIT
