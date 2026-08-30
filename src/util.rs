//! Formatting helpers shared by every view.

/// `3:45` for track lengths, `1:02:03` past an hour.
pub fn format_duration_ms(ms: u32) -> String {
    let total = ms / 1000;
    let hours = total / 3600;
    let minutes = (total / 60) % 60;
    let seconds = total % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

/// `2 hr 13 min` for playlist totals, `45 min 12 sec` under an hour.
pub fn format_total_ms(ms: u64) -> String {
    let total = ms / 1000;
    let hours = total / 3600;
    let minutes = (total / 60) % 60;
    let seconds = total % 60;
    if hours > 0 {
        format!("{hours} hr {minutes} min")
    } else if minutes > 0 {
        format!("{minutes} min {seconds} sec")
    } else {
        format!("{seconds} sec")
    }
}

/// Episode lengths read as `1 hr 12 min` or `38 min`.
pub fn format_episode_ms(ms: u32) -> String {
    let minutes = ms / 60_000;
    let hours = minutes / 60;
    if hours > 0 {
        format!("{hours} hr {} min", minutes % 60)
    } else {
        format!("{} min", minutes.max(1))
    }
}

pub fn format_count(count: u64) -> String {
    let digits = count.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, character) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(character);
    }
    out
}

/// `Jan 5, 2024` from an ISO-8601 timestamp or a bare date.
pub fn format_date(iso: &str) -> String {
    let date = iso.get(..10).unwrap_or(iso);
    let mut parts = date.split('-');
    let (Some(year), Some(month)) = (parts.next(), parts.next()) else {
        return iso.to_string();
    };
    let day = parts.next();
    let month_name = match month {
        "01" => "Jan",
        "02" => "Feb",
        "03" => "Mar",
        "04" => "Apr",
        "05" => "May",
        "06" => "Jun",
        "07" => "Jul",
        "08" => "Aug",
        "09" => "Sep",
        "10" => "Oct",
        "11" => "Nov",
        "12" => "Dec",
        _ => return iso.to_string(),
    };
    match day.and_then(|day| day.trim_start_matches('0').parse::<u8>().ok()) {
        Some(day) => format!("{month_name} {day}, {year}"),
        None => format!("{month_name} {year}"),
    }
}

/// A bounded, public Spotify URI type accepted by Fastpotify.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpotifyUriKind {
    Track,
    Album,
    Playlist,
    Artist,
    Show,
    Episode,
    Audiobook,
}

impl SpotifyUriKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Track => "track",
            Self::Album => "album",
            Self::Playlist => "playlist",
            Self::Artist => "artist",
            Self::Show => "show",
            Self::Episode => "episode",
            Self::Audiobook => "audiobook",
        }
    }

    /// Types the local control protocol intentionally allows to start.
    pub const fn is_music(self) -> bool {
        matches!(
            self,
            Self::Track | Self::Album | Self::Playlist | Self::Artist
        )
    }

    /// Music contexts that can be reordered in the library sidebar.
    pub const fn is_sidebar_music(self) -> bool {
        matches!(self, Self::Album | Self::Playlist | Self::Artist)
    }
}

/// A parsed Spotify URI. The borrowed text is kept so callers do not need to
/// split or rebuild identifiers independently.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpotifyUri<'a> {
    raw: &'a str,
    kind: SpotifyUriKind,
    id: &'a str,
}

impl<'a> SpotifyUri<'a> {
    pub const fn as_str(self) -> &'a str {
        self.raw
    }

    pub const fn kind(self) -> SpotifyUriKind {
        self.kind
    }

    pub const fn id(self) -> &'a str {
        self.id
    }
}

/// Parses the bounded three-part public URI shape used by API objects and
/// external controls. Local tracks and user collection pseudo-URIs have
/// different shapes and are deliberately not accepted as transferable items.
pub fn spotify_uri(text: &str) -> Option<SpotifyUri<'_>> {
    let mut parts = text.split(':');
    if parts.next()? != "spotify" {
        return None;
    }
    let kind = match parts.next()? {
        "track" => SpotifyUriKind::Track,
        "album" => SpotifyUriKind::Album,
        "playlist" => SpotifyUriKind::Playlist,
        "artist" => SpotifyUriKind::Artist,
        "show" => SpotifyUriKind::Show,
        "episode" => SpotifyUriKind::Episode,
        "audiobook" => SpotifyUriKind::Audiobook,
        _ => return None,
    };
    let id = parts.next()?;
    let valid_id = !id.is_empty()
        && parts.next().is_none()
        && text.len() <= 128
        && id.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.' | '%' | '+')
        });
    valid_id.then_some(SpotifyUri {
        raw: text,
        kind,
        id,
    })
}

/// Tears the id out of a validated `spotify:track:abc`-shaped URI.
pub fn uri_id(uri: &str) -> Option<&str> {
    spotify_uri(uri).map(SpotifyUri::id)
}

pub fn uri_kind(uri: &str) -> Option<&'static str> {
    spotify_uri(uri).map(|uri| uri.kind().as_str())
}

pub fn open_spotify_url(uri: &str) -> Option<String> {
    let kind = uri_kind(uri)?;
    let id = uri_id(uri)?;
    Some(format!("https://open.spotify.com/{kind}/{id}"))
}

/// The application icon, drawn at runtime: a green disc with a play mark.
/// Shared by the window icon and the Linux and Windows tray pixmaps.
pub fn app_icon_rgba(size: usize) -> Vec<u8> {
    let mut rgba = vec![0u8; size * size * 4];
    let center = size as f32 / 2.0;
    let radius = center - 2.0;
    let scale = size as f32 / 128.0;
    let triangle = [
        (center - 12.0 * scale, center - 22.0 * scale),
        (center - 12.0 * scale, center + 22.0 * scale),
        (center + 26.0 * scale, center),
    ];
    let sign = |a: (f32, f32), b: (f32, f32), c: (f32, f32)| {
        (a.0 - c.0) * (b.1 - c.1) - (b.0 - c.0) * (a.1 - c.1)
    };
    for y in 0..size {
        for x in 0..size {
            let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
            let distance = ((px - center).powi(2) + (py - center).powi(2)).sqrt();
            let coverage = (radius - distance + 0.5).clamp(0.0, 1.0);
            if coverage <= 0.0 {
                continue;
            }
            let d1 = sign((px, py), triangle[0], triangle[1]);
            let d2 = sign((px, py), triangle[1], triangle[2]);
            let d3 = sign((px, py), triangle[2], triangle[0]);
            let negative = d1 < 0.0 || d2 < 0.0 || d3 < 0.0;
            let positive = d1 > 0.0 || d2 > 0.0 || d3 > 0.0;
            let inside = !(negative && positive);
            let (r, g, b) = if inside { (10, 20, 14) } else { (30, 215, 96) };
            let index = (y * size + x) * 4;
            rgba[index] = r;
            rgba[index + 1] = g;
            rgba[index + 2] = b;
            rgba[index + 3] = (coverage * 255.0) as u8;
        }
    }
    rgba
}

pub fn greeting() -> &'static str {
    match local_hour() {
        5..=11 => "Good morning",
        12..=17 => "Good afternoon",
        _ => "Good evening",
    }
}

fn local_hour() -> u8 {
    jiff::Zoned::now().hour() as u8
}

/// Strips the HTML Spotify embeds in playlist descriptions.
pub fn strip_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_tag = false;
    for character in text.chars() {
        match character {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(character),
            _ => {}
        }
    }
    out.replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#x27;", "'")
        .replace("&#39;", "'")
        .replace("&#x2F;", "/")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(format_duration_ms(225_000), "3:45");
        assert_eq!(format_duration_ms(3_723_000), "1:02:03");
        assert_eq!(format_total_ms(7_980_000), "2 hr 13 min");
        assert_eq!(format_total_ms(2_712_000), "45 min 12 sec");
        assert_eq!(format_episode_ms(4_320_000), "1 hr 12 min");
    }

    #[test]
    fn counts_and_dates() {
        assert_eq!(format_count(1_234_567), "1,234,567");
        assert_eq!(format_count(12), "12");
        assert_eq!(format_date("2024-01-05T10:00:00Z"), "Jan 5, 2024");
        assert_eq!(format_date("2024-03"), "Mar 2024");
        assert_eq!(format_date("2024"), "2024");
    }

    #[test]
    fn uris() {
        assert_eq!(uri_id("spotify:track:abc"), Some("abc"));
        assert_eq!(uri_kind("spotify:playlist:x"), Some("playlist"));
        assert_eq!(
            open_spotify_url("spotify:album:z").as_deref(),
            Some("https://open.spotify.com/album/z")
        );
    }

    #[test]
    fn spotify_uris_are_typed_and_strictly_bounded() {
        let track = spotify_uri("spotify:track:aZ09-_.%+").expect("valid track");
        assert_eq!(track.as_str(), "spotify:track:aZ09-_.%+");
        assert_eq!(track.kind(), SpotifyUriKind::Track);
        assert_eq!(track.id(), "aZ09-_.%+");
        assert!(track.kind().is_music());
        assert!(!SpotifyUriKind::Episode.is_music());
        assert!(SpotifyUriKind::Album.is_sidebar_music());
        assert!(!SpotifyUriKind::Show.is_sidebar_music());

        for invalid in [
            "",
            "track:x",
            "https:track:x",
            "spotify:track:",
            "spotify:unknown:x",
            "spotify:track:x:extra",
            "spotify:user:x:collection",
            "spotify:track:has space",
            "spotify:track:bad\u{9b}",
            "spotify:track:café",
        ] {
            assert_eq!(spotify_uri(invalid), None, "accepted {invalid:?}");
        }
        let oversized = format!("spotify:track:{}", "x".repeat(120));
        assert!(oversized.len() > 128);
        assert_eq!(spotify_uri(&oversized), None);
    }

    #[test]
    fn html_is_stripped() {
        assert_eq!(
            strip_html("Hi <a href=\"x\">there</a> &amp; you"),
            "Hi there & you"
        );
        assert_eq!(strip_html("ONE&#x2F;TWO&#x2F;THREE"), "ONE/TWO/THREE");
    }
}
