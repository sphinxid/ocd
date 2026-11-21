//! Terminal graphics capability detection (OpenTUI `terminal.zig`).
//!
//! At startup the terminal is asked for its version (XTVERSION, `CSI > 0 q`), whether it speaks the Kitty graphics
//! protocol (a 1 x 1 query image with id 31337) and, through the primary device attributes (DA1), whether it
//! does Sixel (attribute 4); `CSI 14 t` asks for the window size in pixels. Kitty and Ghostty count as Kitty
//! terminals by name; foot 1.2+ and WezTerm builds from 2020-06-20 as Sixel ones. tmux (by XTVERSION, `TMUX`,
//! `TERM=tmux*` or `TERM_PROGRAM=tmux`) answers for itself, so its replies select nothing and images draw as text
//! unless `OPENTUI_IMAGE_PROTOCOL` forces a protocol (then wrapped in tmux passthrough). Inside GNU screen the
//! graphics queries are not sent. Over SSH or mosh the identity variables (`TMUX`, `TERM_PROGRAM`, ...) describe
//! the remote host, so only the replies count.
//!
//! `OPENTUI_IMAGE_PROTOCOL` = `auto` | `kitty` | `sixel` | `blocks` forces a protocol; `OPENTUI_GRAPHICS=0` (or
//! `false`) turns detection off. `TERM_PROGRAM=vscode` and `VHS_RECORD` turn Kitty graphics off.

/// A requested image protocol (`ImageRenderProtocol`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Requested {
    #[default]
    Auto,
    Kitty,
    Sixel,
    Blocks,
}

/// The protocol a placement draws with.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Protocol {
    Kitty,
    Sixel,
    /// Quadrant characters.
    Blocks,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mux {
    #[default]
    None,
    Tmux,
    Zellij,
    Screen,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Caps {
    pub kitty_graphics: bool,
    pub sixel: bool,
    pub multiplexer: Mux,
    pub image_protocol: Requested,
    pub name: String,
    pub version: String,
    pub from_xtversion: bool,
    pub graphics_enabled: bool,
    pub skip_graphics_query: bool,
    kitty_queried: bool,
    sixel_queried: bool,
    /// Pixel size of the whole window, from `CSI 4 ; height ; width t`.
    pub resolution: Option<(u32, u32)>,
    /// The terminal size in cells when `resolution` was measured.
    pub measured: (u16, u16),
}

impl Caps {
    /// The state before any reply, from the environment (`checkEnvironmentOverrides`).
    pub fn from_env(env: &dyn Fn(&str) -> Option<String>) -> Caps {
        let mut c = Caps { graphics_enabled: true, ..Default::default() };
        c.apply_env(env);
        c
    }

    fn apply_env(&mut self, env: &dyn Fn(&str) -> Option<String>) {
        let has = |k: &str| env(k).is_some();
        let remote = has("SSH_CONNECTION") || has("SSH_CLIENT") || has("SSH_TTY") || has("MOSH_CONNECTION");
        if remote {
            return;
        }
        if !self.from_xtversion {
            if has("TMUX") {
                self.multiplexer = Mux::Tmux;
            } else if has("ZELLIJ") || has("ZELLIJ_SESSION_NAME") || has("ZELLIJ_PANE_ID") {
                self.multiplexer = Mux::Zellij;
            } else if has("STY") {
                self.multiplexer = Mux::Screen;
                self.skip_graphics_query = true;
            } else if let Some(term) = env("TERM") {
                if term.starts_with("tmux") {
                    self.multiplexer = Mux::Tmux;
                } else if term.starts_with("screen") {
                    self.multiplexer = Mux::Screen;
                    self.skip_graphics_query = true;
                }
            }
        }
        if let Some(v) = env("OPENTUI_GRAPHICS") {
            if v == "false" || v == "0" {
                self.skip_graphics_query = true;
                self.graphics_enabled = false;
                self.kitty_queried = false;
                self.sixel_queried = false;
                self.kitty_graphics = false;
                self.sixel = false;
            } else if v == "true" || v == "1" {
                self.skip_graphics_query = false;
            }
        }
        if let Some(v) = env("OPENTUI_IMAGE_PROTOCOL") {
            match v.to_ascii_lowercase().as_str() {
                "auto" => self.image_protocol = Requested::Auto,
                "kitty" => self.image_protocol = Requested::Kitty,
                "sixel" => self.image_protocol = Requested::Sixel,
                "blocks" => self.image_protocol = Requested::Blocks,
                _ => {}
            }
        }
        if !self.from_xtversion {
            if let Some(prog) = env("TERM_PROGRAM") {
                if self.multiplexer != Mux::Zellij {
                    self.name = prog.clone();
                    if prog == "tmux" {
                        self.multiplexer = Mux::Tmux;
                    }
                    self.version = env("TERM_PROGRAM_VERSION").unwrap_or_default();
                }
                if prog == "vscode" {
                    self.kitty_graphics = false;
                }
            }
            if has("VHS_RECORD") {
                self.kitty_graphics = false;
            }
        }
    }

    /// The queries to send (before the DA1 that ends the probe): XTVERSION, then the Kitty graphics query unless
    /// skipped or inside tmux, then the pixel size.
    pub fn queries(&self) -> String {
        let mut q = String::from("\x1b[>0q");
        if !self.skip_graphics_query && self.multiplexer != Mux::Tmux {
            q.push_str(super::kitty::QUERY);
        }
        q.push_str("\x1b[14t");
        q
    }

    /// Apply a batch of terminal replies (`processCapabilityResponse`).
    pub fn apply_replies(&mut self, data: &[u8]) {
        let text = String::from_utf8_lossy(data);
        if self.graphics_enabled {
            let mut rest: &str = &text;
            while let Some(start) = rest.find("\x1b_G") {
                let Some(end) = rest[start + 3..].find("\x1b\\") else { break };
                let frame = &rest[start + 3..start + 3 + end];
                let control = &frame[..frame.find(';').unwrap_or(frame.len())];
                if control.split(',').any(|f| f == "i=31337") {
                    self.kitty_queried = true;
                    self.kitty_graphics = true;
                    break;
                }
                rest = &rest[start + 3 + end + 2..];
            }
        }
        let graphics = self.graphics_enabled && !self.skip_graphics_query;
        let mut rest: &str = &text;
        while let Some(start) = rest.find("\x1b[?") {
            let body = &rest[start + 3..];
            let end = body.find(|c: char| !(c.is_ascii_digit() || c == ';')).unwrap_or(body.len());
            if end >= body.len() {
                break;
            }
            if body.as_bytes()[end] == b'c' && graphics && body[..end].split(';').any(|p| p == "4") {
                self.sixel_queried = true;
                self.sixel = true;
            }
            rest = &body[end + 1..];
        }
        if let Some(pos) = text.find("\x1bP>|") {
            let after = &text[pos + 4..];
            if let Some(end) = after.find("\x1b\\") {
                self.parse_xtversion(&after[..end]);
            }
        }
        if let Some(r) = parse_resolution(&text) {
            self.resolution = Some(r);
        }
        if self.multiplexer == Mux::Tmux {
            self.kitty_queried = false;
            self.kitty_graphics = false;
            self.sixel_queried = false;
            self.sixel = false;
        }
    }

    fn parse_xtversion(&mut self, s: &str) {
        if s.is_empty() {
            return;
        }
        self.kitty_graphics = self.kitty_queried;
        self.sixel = self.sixel_queried;
        if let Some(p) = s.find('(') {
            self.name = s[..p].to_string();
            self.version = s[p..].find(')').map(|c| s[p + 1..p + c].to_string()).unwrap_or_default();
        } else if let Some(sp) = s.find(' ') {
            self.name = s[..sp].to_string();
            self.version = s[sp + 1..].to_string();
        } else {
            self.name = s.to_string();
            self.version.clear();
        }
        self.from_xtversion = true;
        self.multiplexer = if self.name == "tmux" {
            Mux::Tmux
        } else if self.name.eq_ignore_ascii_case("zellij") {
            Mux::Zellij
        } else {
            Mux::None
        };
        // applyKnownGraphicsIdentity
        if !self.graphics_enabled || self.multiplexer != Mux::None {
            return;
        }
        let name = self.name.to_ascii_lowercase();
        if name == "kitty" || name == "ghostty" {
            self.kitty_graphics = true;
        } else if name == "foot" && semver_at_least(&self.version, 1, 2) {
            self.sixel = true;
        } else if name == "wezterm" && wezterm_build_at_least(&self.version, 20200620) {
            self.sixel = true;
        }
    }

    /// `refusesForcedSixel`: a forced Sixel where the terminal surely can't show one.
    fn refuses_forced_sixel(&self) -> bool {
        if self.multiplexer == Mux::Tmux || self.sixel {
            return false;
        }
        if self.from_xtversion {
            return self.multiplexer != Mux::None || self.name.eq_ignore_ascii_case("kitty");
        }
        self.name.eq_ignore_ascii_case("Apple_Terminal")
    }

    /// `resolveImageProtocol` plus `nextPlacementProtocol`'s size check: what a placement asking for `requested`
    /// draws with. Sixel needs the pixel size.
    pub fn resolve(&self, requested: Requested, pixels: (u32, u32)) -> Protocol {
        let configured = if requested == Requested::Auto { self.image_protocol } else { requested };
        let protocol = match configured {
            Requested::Sixel if self.refuses_forced_sixel() => Protocol::Blocks,
            Requested::Kitty => Protocol::Kitty,
            Requested::Sixel => Protocol::Sixel,
            Requested::Blocks => Protocol::Blocks,
            Requested::Auto => {
                if self.multiplexer == Mux::Tmux {
                    Protocol::Blocks
                } else if self.kitty_graphics {
                    Protocol::Kitty
                } else if self.sixel {
                    Protocol::Sixel
                } else {
                    Protocol::Blocks
                }
            }
        };
        if protocol == Protocol::Sixel && (pixels.0 == 0 || pixels.1 == 0) { Protocol::Blocks } else { protocol }
    }

    pub fn tmux(&self) -> bool {
        self.multiplexer == Mux::Tmux
    }

    /// The terminal is now `cols` x `rows`: OpenTUI forgets the pixel size and asks again (`CSI 14 t`); ocd reads
    /// the window's pixel size from the pty (`TIOCGWINSZ`), or keeps the cell size it measured.
    pub fn resize(&mut self, cols: u16, rows: u16) {
        if (cols, rows) == self.measured || cols == 0 || rows == 0 {
            return;
        }
        let window = crossterm::terminal::window_size().ok().filter(|w| w.width > 0 && w.height > 0 && w.columns == cols && w.rows == rows);
        self.resolution = match (window, self.resolution) {
            (Some(w), _) => Some((w.width as u32, w.height as u32)),
            (None, Some((w, h))) if self.measured.0 > 0 && self.measured.1 > 0 => {
                Some(((w as u64 * cols as u64 / self.measured.0 as u64) as u32, (h as u64 * rows as u64 / self.measured.1 as u64) as u32))
            }
            _ => None,
        };
        self.measured = (cols, rows);
    }
}

/// `parsePixelResolution`: `ESC [ 4 ; height ; width t` as `(width, height)`.
pub fn parse_resolution(text: &str) -> Option<(u32, u32)> {
    let re = regex::Regex::new(r"\x1b\[4;(\d+);(\d+)t").ok()?;
    let c = re.captures(text)?;
    let (h, w) = (c[1].parse::<u32>().ok()?, c[2].parse::<u32>().ok()?);
    (w > 0 && h > 0).then_some((w, h))
}

fn semver_at_least(version: &str, major: u32, minor: u32) -> bool {
    let core = version.split('-').next().unwrap_or("");
    let parts: Vec<&str> = core.split('.').collect();
    if parts.len() != 3 {
        return false;
    }
    let (Ok(a), Ok(b), Ok(_)) = (parts[0].parse::<u32>(), parts[1].parse::<u32>(), parts[2].parse::<u32>()) else { return false };
    (a, b) >= (major, minor)
}

fn wezterm_build_at_least(version: &str, date: u32) -> bool {
    let parts: Vec<&str> = version.split(['-', '.', '_']).collect();
    if parts.len() < 3 || parts[0].len() != 8 || parts[1].len() != 6 || parts[2].len() != 8 {
        return false;
    }
    if !parts[0].bytes().all(|b| b.is_ascii_digit()) || !parts[1].bytes().all(|b| b.is_ascii_digit()) || !parts[2].bytes().all(|b| b.is_ascii_hexdigit()) {
        return false;
    }
    if parts[3..].iter().any(|p| p.is_empty()) {
        return false;
    }
    parts[0].parse::<u32>().is_ok_and(|d| d >= date)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |k| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string())
    }

    #[test]
    fn kitty_reply_selects_kitty() {
        let mut c = Caps::from_env(&env(&[("TERM", "xterm-256color")]));
        assert_eq!(c.resolve(Requested::Auto, (0, 0)), Protocol::Blocks);
        c.apply_replies(b"\x1b_Gi=31337;OK\x1b\\\x1b[4;480;800t\x1b[?62;22c");
        assert!(c.kitty_graphics);
        assert_eq!(c.resolution, Some((800, 480)));
        assert_eq!(c.resolve(Requested::Auto, (0, 0)), Protocol::Kitty);
        // An error reply still names the id: the terminal speaks the protocol.
        let mut c = Caps::from_env(&env(&[]));
        c.apply_replies(b"\x1b_Gi=31337;EINVAL:Zero width/height not allowed\x1b\\\x1b[?62;c");
        assert!(c.kitty_graphics);
    }

    #[test]
    fn da1_attribute_4_selects_sixel_with_a_pixel_size() {
        let mut c = Caps::from_env(&env(&[]));
        c.apply_replies(b"\x1b[?62;4;22c");
        assert!(c.sixel && !c.kitty_graphics);
        assert_eq!(c.resolve(Requested::Auto, (100, 50)), Protocol::Sixel);
        assert_eq!(c.resolve(Requested::Auto, (0, 0)), Protocol::Blocks);
        assert_eq!(c.resolve(Requested::Blocks, (100, 50)), Protocol::Blocks);
    }

    #[test]
    fn known_names_from_xtversion() {
        for (reply, kitty, sixel) in [
            ("\x1bP>|kitty(0.40.1)\x1b\\", true, false),
            ("\x1bP>|ghostty 1.1.3\x1b\\", true, false),
            ("\x1bP>|foot(1.16.2)\x1b\\", false, true),
            ("\x1bP>|foot(1.1.0)\x1b\\", false, false),
            ("\x1bP>|WezTerm 20240203-110809-5046fc22\x1b\\", false, true),
            ("\x1bP>|WezTerm 20200101-110809-5046fc22\x1b\\", false, false),
            ("\x1bP>|XTerm(390)\x1b\\", false, false),
        ] {
            let mut c = Caps::from_env(&env(&[]));
            c.apply_replies(reply.as_bytes());
            assert_eq!((c.kitty_graphics, c.sixel), (kitty, sixel), "{reply:?}");
        }
    }

    #[test]
    fn tmux_draws_text_unless_forced() {
        let mut c = Caps::from_env(&env(&[("TMUX", "/tmp/tmux-1/default,1,0")]));
        assert!(!c.queries().contains("_G"));
        c.apply_replies(b"\x1bP>|tmux 3.5a\x1b\\\x1b[?1;2;4c");
        assert!(!c.sixel);
        assert_eq!(c.resolve(Requested::Auto, (10, 10)), Protocol::Blocks);
        let mut c = Caps::from_env(&env(&[("TMUX", "x"), ("OPENTUI_IMAGE_PROTOCOL", "kitty")]));
        c.apply_replies(b"\x1bP>|tmux 3.5a\x1b\\");
        assert_eq!(c.resolve(Requested::Auto, (10, 10)), Protocol::Kitty);
        assert!(c.tmux());
    }

    #[test]
    fn environment_overrides() {
        let mut c = Caps::from_env(&env(&[("OPENTUI_GRAPHICS", "0")]));
        c.apply_replies(b"\x1b_Gi=31337;OK\x1b\\\x1b[?62;4c");
        assert_eq!((c.kitty_graphics, c.sixel), (false, false));
        let c = Caps::from_env(&env(&[("OPENTUI_IMAGE_PROTOCOL", "SIXEL")]));
        assert_eq!(c.resolve(Requested::Auto, (10, 10)), Protocol::Sixel);
        let c = Caps::from_env(&env(&[("OPENTUI_IMAGE_PROTOCOL", "sixel"), ("TERM_PROGRAM", "Apple_Terminal")]));
        assert_eq!(c.resolve(Requested::Auto, (10, 10)), Protocol::Blocks);
        let c = Caps::from_env(&env(&[("STY", "1.pts")]));
        assert!(!c.queries().contains("_G"));
        // Over SSH the identity variables describe the remote host.
        let c = Caps::from_env(&env(&[("SSH_CONNECTION", "a"), ("TMUX", "x")]));
        assert_eq!(c.multiplexer, Mux::None);
        assert_eq!(Caps::from_env(&env(&[])).queries(), "\x1b[>0q\x1b_Gi=31337,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\\x1b[14t");
    }
}
