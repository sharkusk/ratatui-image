//! Helper module to build a protocol, and swap protocols at runtime

use std::{
    env,
    io::{self, Read, Write},
    sync::mpsc::Sender,
};

use crate::{
    FontSize, Resize, Result,
    errors::Errors,
    protocol::{
        Protocol, StatefulProtocol, StatefulProtocolType,
        halfblocks::Halfblocks,
        iterm2::Iterm2,
        kitty::{Kitty, StatefulKitty},
        sixel::Sixel,
    },
};
use cap_parser::{Parser, QueryStdioOptions, Response};
use image::{DynamicImage, Rgba};
use rand::random;
use ratatui::layout::Size;
#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

pub mod cap_parser;

#[derive(Debug, PartialEq, Clone)]
pub enum Capability {
    /// Reports supporting kitty graphics protocol.
    Kitty,
    /// Reports supporting sixel graphics protocol.
    Sixel,
    /// Reports supporting rectangular ops.
    RectangularOps,
    /// Reports being able to inflate a zlib-compressed kitty transmission
    /// (`o=z`).
    ///
    /// Only probed for, and so only ever present, when
    /// [`cap_parser::QueryStdioOptions::kitty_compression`] is set: it
    /// optimises for bandwidth at the cost of render latency, so it is off
    /// by default and you probably want it off. See that field's doc for
    /// when it's worth turning on.
    KittyCompression,
    /// Reports being able to read a kitty transmission from a POSIX shared memory
    /// object (`t=s`), which only a terminal on this machine can do.
    ///
    /// Only probed for, and so only ever present, when
    /// [`cap_parser::QueryStdioOptions::kitty_shared_memory_probe`] is set. With that
    /// probe set, [`cap_parser::QueryStdioOptions::kitty_shared_memory_object`] is
    /// honoured only where this capability is present.
    KittySharedMemory,
    /// Reports font size in pixels.
    CellSize(Option<(u16, u16)>),
    /// Reports supporting text sizing protocol.
    TextSizingProtocol,
    /// Reports a background color.
    Background(u8, u8, u8),
}

const STDIN_READ_TIMEOUT_MILLIS: u64 = 2000;

#[derive(Clone, Debug)]
pub struct Picker {
    font_size: FontSize,
    protocol_type: ProtocolType,
    background_color: Option<Rgba<u8>>,
    pub(crate) is_tmux: bool,
    capabilities: Vec<Capability>,
    kitty_shm: Option<u32>,
}

/// Serde-friendly protocol-type enum for [Picker].
#[derive(PartialEq, Eq, Clone, Debug, Copy)]
#[cfg_attr(
    feature = "serde",
    derive(Deserialize, Serialize),
    serde(rename_all = "lowercase")
)]
pub enum ProtocolType {
    Halfblocks,
    Sixel,
    Kitty,
    Iterm2,
}

impl ProtocolType {
    pub fn next(&self) -> ProtocolType {
        match self {
            ProtocolType::Halfblocks => ProtocolType::Sixel,
            ProtocolType::Sixel => ProtocolType::Kitty,
            ProtocolType::Kitty => ProtocolType::Iterm2,
            ProtocolType::Iterm2 => ProtocolType::Halfblocks,
        }
    }
}

/// Helper for building widgets
impl Picker {
    /// Query terminal stdio for graphics capabilities and font-size with some escape sequences.
    ///
    /// This writes and reads from stdio momentarily. WARNING: this method should be called after
    /// entering alternate screen but before reading terminal events.
    ///
    /// # Example
    /// ```rust
    /// use ratatui_image::picker::Picker;
    /// let mut picker = Picker::from_query_stdio();
    /// ```
    ///
    pub fn from_query_stdio() -> Result<Self> {
        Picker::from_query_stdio_with_options(QueryStdioOptions::default())
    }

    /// This should ONLY be used if [Capability::TextSizingProtocol] is needed for some external
    /// reason.
    ///
    /// Query for additional capabilities, currently supports querying for [Text Sizing Protocol].
    ///
    /// The result can be checked by searching for [Capability::TextSizingProtocol] in [Picker::capabilities].
    ///
    /// [Text Sizing Protocol] <https://sw.kovidgoyal.net/kitty/text-sizing-protocol//>
    pub fn from_query_stdio_with_options(options: QueryStdioOptions) -> Result<Self> {
        // Detect tmux, and only if positive then take some risky guess for iTerm2 support.
        let (is_tmux, tmux_proto) = detect_tmux_and_outer_protocol_from_env();

        let kitty_shm = options.kitty_shared_memory_object;
        let kitty_shm_probe = options.kitty_shared_memory_probe;
        let mut options_with_blacklist = options;
        let is_wezterm = env::var("WEZTERM_EXECUTABLE").is_ok_and(|s| !s.is_empty());
        let is_konsole = env::var("KONSOLE_VERSION").is_ok_and(|s| !s.is_empty());
        if is_wezterm || is_konsole {
            // WezTerm could use Sixel, but iTerm2 (detected later is better).
            // Konsole's Sixel implementation is buggy: https://github.com/ratatui/ratatui-image?tab=readme-ov-file#compatibility-matrix
            // Neither implement the placeholder part of kitty correctly.
            options_with_blacklist.blacklist_protocols =
                vec![ProtocolType::Kitty, ProtocolType::Sixel];
        }

        // Write and read to stdin to query protocol capabilities and font-size.
        match query_with_timeout(is_tmux, options_with_blacklist) {
            Ok((capability_proto, font_size, caps)) => {
                let iterm2_proto = iterm2_from_env();

                // IO-based detection is authoritative; env-based hints are fallbacks
                // (env vars like KITTY_WINDOW_ID can be stale in tmux sessions).
                let protocol_type = capability_proto
                    .or(tmux_proto)
                    .or(iterm2_proto)
                    .unwrap_or(ProtocolType::Halfblocks);

                let kitty_shm = resolve_kitty_shm(kitty_shm, kitty_shm_probe, &caps);
                if let Some(font_size) = font_size {
                    Ok(Self {
                        font_size,
                        background_color: None,
                        protocol_type,
                        is_tmux,
                        capabilities: caps,
                        kitty_shm,
                    })
                } else {
                    let mut p = DEFAULT_PICKER.clone();
                    p.is_tmux = is_tmux;
                    p.kitty_shm = kitty_shm;
                    Ok(p)
                }
            }
            // The terminal did not answer the query, but it may still be possible to figure out
            // the font-size with an ioctl, and env vars may still hint at iTerm2 support. This
            // happens for example on Windows ConPTY, which does not reliably deliver the
            // responses to the child process.
            Err(Errors::NoCap | Errors::NoStdinResponse | Errors::NoFontSize) => {
                let mut p = fallback_picker(
                    is_tmux,
                    tmux_proto.or_else(iterm2_from_env),
                    font_size_fallback(),
                );
                // Nothing answered at all, so a requested probe answered no.
                p.kitty_shm = resolve_kitty_shm(kitty_shm, kitty_shm_probe, &[]);
                Ok(p)
            }
            Err(err) => Err(err),
        }
    }

    /// Create a picker that is guaranteed to only work with Halfblocks.
    ///
    /// # Example
    /// ```rust
    /// use ratatui_image::picker::Picker;
    ///
    /// let mut picker = Picker::halfblocks();
    /// ```
    pub fn halfblocks() -> Self {
        // Detect tmux, ignore iTerm2 as we don't have font-size.
        let (is_tmux, _tmux_proto) = detect_tmux_and_outer_protocol_from_env();

        Self {
            font_size: FontSize::new(10, 20),
            background_color: None,
            protocol_type: ProtocolType::Halfblocks,
            is_tmux,
            capabilities: Vec::new(),
            kitty_shm: None,
        }
    }

    /// Create a picker from a given terminal [FontSize].
    #[deprecated(
        since = "9.0.0",
        note = "use `from_query_stdio` or `halfblocks` instead"
    )]
    pub fn from_fontsize(font_size: FontSize) -> Self {
        // Detect tmux, and if positive then take some risky guess for iTerm2 support.
        let (is_tmux, tmux_proto) = detect_tmux_and_outer_protocol_from_env();

        // Disregard protocol-from-capabilities if some env var says that we could try iTerm2.
        let iterm2_proto = iterm2_from_env();

        let protocol_type = tmux_proto
            .or(iterm2_proto)
            .unwrap_or(ProtocolType::Halfblocks);

        Self {
            font_size,
            background_color: None,
            protocol_type,
            is_tmux,
            capabilities: Vec::new(),
            kitty_shm: None,
        }
    }

    /// Returns the current protocol type.
    pub fn protocol_type(&self) -> ProtocolType {
        self.protocol_type
    }

    /// Force a protocol type.
    pub fn set_protocol_type(&mut self, protocol_type: ProtocolType) {
        self.protocol_type = protocol_type;
    }

    /// Returns the [FontSize] detected by [Picker::from_query_stdio].
    pub fn font_size(&self) -> FontSize {
        self.font_size
    }

    /// Set the [FontSize], for when the terminal's cell size changed.
    ///
    /// A cell changes size whenever the user changes the font or its size
    /// mid-session, and nothing else the picker holds changes with it: the
    /// protocol, the detected [Capability] list, the tmux flag and the
    /// background color all still describe the same terminal. Re-querying with
    /// [Picker::from_query_stdio] would redetect them at the cost of writing to
    /// and reading from stdio, which an application that already owns the
    /// keyboard cannot safely do; this updates only the part that moved.
    pub fn set_font_size(&mut self, font_size: FontSize) {
        self.font_size = font_size;
    }

    /// Change the default background color (transparent black).
    pub fn set_background_color<T: Into<Rgba<u8>>>(&mut self, background_color: Option<T>) {
        self.background_color = background_color.map(Into::into);
    }

    /// Returns the capabilities detected by [Picker::from_query_stdio].
    pub fn capabilities(&self) -> &Vec<Capability> {
        &self.capabilities
    }

    /// Returns a new protocol.
    ///
    /// The image must match the given area at the terminal's current font size.
    pub(crate) fn new_protocol_raw(&self, image: DynamicImage, size: Size) -> Result<Protocol> {
        self.new_protocol_raw_with_id(image, size, random())
    }

    /// [`Picker::new_protocol_raw`], under a caller-chosen kitty image id.
    fn new_protocol_raw_with_id(
        &self,
        image: DynamicImage,
        size: Size,
        id: u32,
    ) -> Result<Protocol> {
        match self.protocol_type {
            ProtocolType::Halfblocks => Ok(Protocol::Halfblocks(Halfblocks::new(image, size)?)),
            ProtocolType::Sixel => Ok(Protocol::Sixel(Sixel::new(image, size, self.is_tmux)?)),
            ProtocolType::Kitty => Ok(Protocol::Kitty(Kitty::new(
                image,
                size,
                id,
                self.is_tmux,
                self.capabilities.contains(&Capability::KittyCompression),
                self.kitty_shm,
            )?)),
            ProtocolType::Iterm2 => Ok(Protocol::ITerm2(Iterm2::new(image, size, self.is_tmux)?)),
        }
    }

    /// Returns a new protocol for [`crate::Image`] widgets that fits into the given size.
    pub fn new_protocol(
        &self,
        image: DynamicImage,
        size: Size,
        resize: Resize,
    ) -> Result<Protocol> {
        self.new_protocol_id(image, size, resize, random())
    }

    /// [`Picker::new_protocol`], with the kitty image id chosen by the CALLER
    /// rather than drawn at random.
    ///
    /// Kitty's unicode placeholders put the image id in EVERY cell of a placement
    /// — the low 24 bits as the cell's foreground color, the high byte as a
    /// diacritic — so the id is part of the rendered cells, not merely a handle on
    /// the image. An application that re-encodes changing content into the same
    /// area therefore repaints the whole placement rect whenever the id moves,
    /// which a fresh random id does on every encode. Re-encoding under the id the
    /// previous protocol used replaces the image data behind an unchanged
    /// placement instead, so a changed picture costs the picture.
    ///
    /// Only the caller can know that two encodes are the same thing changing, so
    /// only the caller can choose the id. Reuse one only for content drawn at the
    /// same size in the same place; the transmission replaces whatever the
    /// terminal already holds under it.
    ///
    /// Every other backend ignores `id` — none of them addresses its images by
    /// one — so this is [`Picker::new_protocol`] exactly for those.
    pub fn new_protocol_with_id(
        &self,
        image: DynamicImage,
        size: Size,
        resize: Resize,
        id: u32,
    ) -> Result<Protocol> {
        self.new_protocol_id(image, size, resize, id)
    }

    fn new_protocol_id(
        &self,
        image: DynamicImage,
        size: Size,
        resize: Resize,
        id: u32,
    ) -> Result<Protocol> {
        let desired =
            Resize::round_pixel_size_to_cells(image.width(), image.height(), self.font_size);
        let (image, area) =
            match resize.needs_resize(&image, Some(desired), self.font_size, None, size, false) {
                Some(area) => {
                    let image = resize.resize(&image, self.font_size, area, self.background_color);
                    (image, area)
                }
                None => (image, desired),
            };

        self.new_protocol_raw_with_id(image, area, id)
    }

    /// Returns a new *stateful* protocol for [`crate::StatefulImage`] widgets.
    pub fn new_resize_protocol(&self, image: DynamicImage) -> StatefulProtocol {
        let protocol_type = match self.protocol_type {
            ProtocolType::Halfblocks => StatefulProtocolType::Halfblocks(Halfblocks::default()),
            ProtocolType::Sixel => StatefulProtocolType::Sixel(Sixel {
                is_tmux: self.is_tmux,
                ..Sixel::default()
            }),
            ProtocolType::Kitty => StatefulProtocolType::Kitty(StatefulKitty::new(
                random(),
                self.is_tmux,
                self.capabilities.contains(&Capability::KittyCompression),
                self.kitty_shm,
            )),
            ProtocolType::Iterm2 => StatefulProtocolType::ITerm2(Iterm2 {
                is_tmux: self.is_tmux,
                ..Iterm2::default()
            }),
        };
        StatefulProtocol::new(image, self.font_size, self.background_color, protocol_type)
    }
}

static DEFAULT_PICKER: Picker = Picker {
    // This is completely arbitrary. For halfblocks, it doesn't have to be precise
    // since we're not rendering pixels. It should be roughly 1:2 ratio, and some
    // reasonable size.
    font_size: FontSize::new(10, 20),
    background_color: None,
    protocol_type: ProtocolType::Halfblocks,
    is_tmux: false,
    capabilities: Vec::new(),
    kitty_shm: None,
};

/// Build a picker from whatever could be detected without the terminal answering the query.
///
/// A protocol other than halfblocks can only render meaningfully with an actual font-size, so
/// without one the `protocol_type` hint is discarded, just like on the query's success path.
fn fallback_picker(
    is_tmux: bool,
    protocol_type: Option<ProtocolType>,
    font_size: Option<FontSize>,
) -> Picker {
    let mut picker = DEFAULT_PICKER.clone();
    picker.is_tmux = is_tmux;
    if let Some(font_size) = font_size {
        picker.font_size = font_size;
        picker.protocol_type = protocol_type.unwrap_or(ProtocolType::Halfblocks);
    }
    picker
}

fn detect_tmux_and_outer_protocol_from_env() -> (bool, Option<ProtocolType>) {
    // Check if we're inside tmux.
    if !env::var("TERM").is_ok_and(|term| term.starts_with("tmux"))
        && !env::var("TERM_PROGRAM").is_ok_and(|term_program| term_program == "tmux")
    {
        return (false, None);
    }

    let _ = std::process::Command::new("tmux")
        .args(["set", "-p", "allow-passthrough", "on"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .and_then(|mut child| child.wait()); // wait(), for check_device_attrs.

    // Crude guess based on the *existence* of some magic program specific env vars.
    // Note: kitty is detected via io query (which works through tmux passthrough),
    // not env vars, since KITTY_WINDOW_ID is often stale in tmux sessions.
    const OUTER_TERM_HINTS: [(&str, ProtocolType); 2] = [
        ("ITERM_SESSION_ID", ProtocolType::Iterm2),
        ("WEZTERM_EXECUTABLE", ProtocolType::Iterm2),
    ];
    for (hint, proto) in OUTER_TERM_HINTS {
        if env::var(hint).is_ok_and(|s| !s.is_empty()) {
            return (true, Some(proto));
        }
    }
    (true, None)
}

fn iterm2_from_env() -> Option<ProtocolType> {
    if env::var("TERM_PROGRAM").is_ok_and(|term_program| {
        term_program.contains("iTerm")
            || term_program.contains("WezTerm")
            || term_program.contains("mintty")
            || term_program.contains("vscode")
            || term_program.contains("Tabby")
            || term_program.contains("Hyper")
            || term_program.contains("rio")
            || term_program.contains("Bobcat")
            || term_program.contains("WarpTerminal")
    }) {
        return Some(ProtocolType::Iterm2);
    }
    if env::var("LC_TERMINAL").is_ok_and(|lc_term| lc_term.contains("iTerm")) {
        return Some(ProtocolType::Iterm2);
    }
    None
}

#[cfg(not(windows))]
fn enable_raw_mode() -> Result<impl FnOnce() -> Result<()>> {
    use rustix::termios::{self, LocalModes, OptionalActions};

    let stdin = io::stdin();
    let mut termios = termios::tcgetattr(&stdin)?;
    let termios_original = termios.clone();

    // Disable canonical mode to read without waiting for Enter, disable echoing.
    termios.local_modes &= !LocalModes::ICANON;
    termios.local_modes &= !LocalModes::ECHO;
    termios::tcsetattr(&stdin, OptionalActions::Drain, &termios)?;

    Ok(move || {
        Ok(termios::tcsetattr(
            io::stdin(),
            OptionalActions::Now,
            &termios_original,
        )?)
    })
}

#[cfg(windows)]
fn enable_raw_mode() -> Result<impl FnOnce() -> Result<()>> {
    use windows::{
        Win32::{
            Foundation::{GENERIC_READ, GENERIC_WRITE, HANDLE},
            Storage::FileSystem::{
                self, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
            },
            System::Console::{
                self, CONSOLE_MODE, ENABLE_ECHO_INPUT, ENABLE_LINE_INPUT, ENABLE_PROCESSED_INPUT,
            },
        },
        core::PCWSTR,
    };

    let utf16: Vec<u16> = "CONIN$\0".encode_utf16().collect();
    let utf16_ptr: *const u16 = utf16.as_ptr();

    let in_handle = unsafe {
        FileSystem::CreateFileW(
            PCWSTR(utf16_ptr),
            (GENERIC_READ | GENERIC_WRITE).0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES(0),
            HANDLE::default(),
        )
    }?;

    let mut original_in_mode = CONSOLE_MODE::default();
    unsafe { Console::GetConsoleMode(in_handle, &mut original_in_mode) }?;

    let requested_in_modes = !ENABLE_ECHO_INPUT & !ENABLE_LINE_INPUT & !ENABLE_PROCESSED_INPUT;
    let in_mode = original_in_mode & requested_in_modes;
    unsafe { Console::SetConsoleMode(in_handle, in_mode) }?;

    Ok(move || {
        unsafe { Console::SetConsoleMode(in_handle, original_in_mode) }?;
        Ok(())
    })
}

#[cfg(not(windows))]
fn font_size_fallback() -> Option<FontSize> {
    use rustix::termios::{self, Winsize};

    let winsize = termios::tcgetwinsize(io::stdout()).ok()?;
    let Winsize {
        ws_xpixel: x,
        ws_ypixel: y,
        ws_col: cols,
        ws_row: rows,
    } = winsize;
    if x == 0 || y == 0 || cols == 0 || rows == 0 {
        return None;
    }

    Some(FontSize::new(x / cols, y / rows))
}

#[cfg(windows)]
fn font_size_fallback() -> Option<FontSize> {
    None
}

/// Query the terminal, by writing and reading to stdin and stdout.
/// The terminal must be in "raw mode" and should probably be reset to "cooked mode" when this
/// operation has completed.
///
/// The returned [ProtocolType] and [FontSize] may be included in the list of [Capability]s,
/// but the burden of picking out the right one or a font-size fallback is already resolved here.
fn query_stdio_capabilities(
    is_tmux: bool,
    options: QueryStdioOptions,
    tx: &Sender<QueryResult>,
) -> Result<()> {
    // Send several control sequences at once:
    // `_Gi=...`: Kitty graphics support.
    // `[c`: Capabilities including sixels.
    // `[16t`: Cell-size (perhaps we should also do `[14t`).
    // `[1337n`: iTerm2 (some terminals implement the protocol but sadly not this custom CSI)
    // `[5n`: Device Status Report, implemented by all terminals, ensure that there is some
    // response and we don't hang reading forever.
    // The shared memory probe asks the terminal to read a real object, so it has to
    // exist before the query naming it goes out — and be gone again afterwards,
    // which the guard's drop takes care of on every path out of here. If the object
    // cannot be created the probe is dropped from the query rather than asked
    // blindly, since an unanswerable probe would read as a refusal.
    #[cfg(not(windows))]
    let mut options = options;
    #[cfg(not(windows))]
    let _shm_probe = if options.kitty_shared_memory_probe {
        let probe = ShmProbe::create();
        options.kitty_shared_memory_probe = probe.is_some();
        probe
    } else {
        None
    };

    let query = Parser::query(is_tmux, options);
    io::stdout().write_all(query.as_bytes())?;
    io::stdout().flush()?;

    let mut parser = Parser::new();
    let mut responses = vec![];
    'out: loop {
        let mut charbuf: [u8; 50] = [0; 50];

        let read = io::stdin().read(&mut charbuf)?;
        // A read blocks a bit, keep receiver busy now.
        tx.send(QueryResult::Busy)
            .map_err(|_senderr| Errors::NoStdinResponse)?;

        for ch in charbuf.iter().take(read) {
            let mut more_caps = parser.push(char::from(*ch));
            match more_caps[..] {
                [Response::Status] => {
                    break 'out;
                }
                _ => responses.append(&mut more_caps),
            }
        }
    }

    let result = interpret_parser_responses(responses)?;
    tx.send(QueryResult::Done(result))
        .map_err(|_senderr| Errors::NoStdinResponse)?;
    Ok(())
}

/// Shared memory is used only where the caller asked for it and, if a probe was
/// asked for too, only where the terminal answered it.
///
/// Without a probe this is [`QueryStdioOptions::kitty_shared_memory_object`]
/// unchanged — the blind opt-in that field documents.
fn resolve_kitty_shm(object: Option<u32>, probe: bool, caps: &[Capability]) -> Option<u32> {
    if probe && !caps.contains(&Capability::KittySharedMemory) {
        return None;
    }
    object
}

/// The shared memory object the `t=s` probe names, unlinked when this is dropped.
///
/// Kitty unlinks an object it has read, so the drop is for every other outcome: a
/// terminal that ignored the probe, answered an error, or never answered at all
/// would otherwise leave the object behind for the life of the machine.
#[cfg(not(windows))]
struct ShmProbe(String);

#[cfg(not(windows))]
impl ShmProbe {
    /// Create the object and fill it with the one RGBA pixel the probe's
    /// `f=32,s=1,v=1` promises.
    ///
    /// `None` if the platform refused, in which case the probe is simply not sent
    /// and the capability cannot appear.
    fn create() -> Option<Self> {
        // Exactly the one RGBA pixel the probe's `f=32,s=1,v=1` promises: a
        // terminal refuses an object smaller than `s * v * bpp`.
        const PIXEL: [u8; 4] = [0, 0, 0, 0];

        let name = cap_parser::shm_probe_name();
        // Named before it is filled, so that a half-made object is still unlinked.
        let probe = Self(name);
        crate::protocol::kitty::shm_write(&probe.0, &PIXEL).ok()?;
        Some(probe)
    }
}

#[cfg(not(windows))]
impl Drop for ShmProbe {
    fn drop(&mut self) {
        // Gone already is the successful case, not an error.
        let _ = rustix::shm::unlink(self.0.as_str());
    }
}

fn interpret_parser_responses(
    responses: Vec<Response>,
) -> Result<(Option<ProtocolType>, Option<FontSize>, Vec<Capability>)> {
    if responses.is_empty() {
        return Err(Errors::NoCap);
    }

    let mut capabilities = Vec::new();

    let mut proto = None;
    let mut font_size = None;

    let mut cursor_position_reports = vec![];
    for response in &responses {
        if let Some(capability) = match response {
            Response::Kitty => {
                proto = Some(ProtocolType::Kitty);
                Some(Capability::Kitty)
            }
            Response::Sixel => {
                if proto.is_none() {
                    // Only if kitty is not supported.
                    proto = Some(ProtocolType::Sixel);
                }
                Some(Capability::Sixel)
            }
            Response::RectangularOps => Some(Capability::RectangularOps),
            Response::KittyCompression => Some(Capability::KittyCompression),
            Response::KittySharedMemory => Some(Capability::KittySharedMemory),
            Response::CellSize(cell_size) => {
                if let Some((w, h)) = cell_size {
                    font_size = Some((*w, *h).into());
                }
                Some(Capability::CellSize(*cell_size))
            }
            Response::CursorPositionReport(x, y) => {
                cursor_position_reports.push((x, y));
                None
            }
            Response::Background(r, g, b) => Some(Capability::Background(*r, *g, *b)),
            Response::Status => None,
        } {
            capabilities.push(capability);
        }
    }

    // In case some terminal didn't support the cell-size query.
    font_size = font_size.or_else(font_size_fallback);

    if let [(x1, _y1), (x2, _y2), (x3, _y3)] = cursor_position_reports[..] {
        // Test if the cursor advanced exactly two columns (instead of one) on both the width and
        // scaling queries of the protocol.
        // The documentation is a bit ambiguous, as it only says the cursor positions "need to be
        // different from each other".
        // However from my testing on Kitty and other terminals that do not support the feature,
        // the cursor always advances at least one column since it is printing a space, so the CPRs
        // will always be different from each other (unless we would move the cursor to a known
        // position or something like that - and this also begs the question of needing to do this
        // anyway, for the edge case of the cursor being at the very end of a line).
        // My interpretation is that the cursor should advance 2 columns, instead of one, with both
        // queries, and only then can we interpret it as supported.
        // The Foot terminal notably reports a 2 column movement but fortunately only for the `w=2`
        // query.
        //
        // The row part can be ignored.
        if *x2 == x1 + 2 && *x3 == x2 + 2 {
            capabilities.push(Capability::TextSizingProtocol);
        }
    }

    Ok((proto, font_size, capabilities))
}

enum QueryResult {
    Done((Option<ProtocolType>, Option<FontSize>, Vec<Capability>)),
    Err(Errors),
    Busy,
}
fn query_with_timeout(
    is_tmux: bool,
    options: QueryStdioOptions,
) -> Result<(Option<ProtocolType>, Option<FontSize>, Vec<Capability>)> {
    use std::{sync::mpsc, thread};
    let (tx, rx) = mpsc::channel();

    let timeout = options.timeout;
    thread::spawn(move || {
        if let Err(err) = tx
            .send(QueryResult::Busy)
            .map_err(|_senderr| Errors::NoStdinResponse)
            .and_then(|_| enable_raw_mode())
            .and_then(|disable_raw_mode| {
                tx.send(QueryResult::Busy)
                    .map_err(|_senderr| Errors::NoStdinResponse)?;
                let result = query_stdio_capabilities(is_tmux, options, &tx);
                disable_raw_mode()?;
                result
            })
        {
            // Last chance, fire and forget now.
            let _ = tx.send(QueryResult::Err(err));
        }
    });

    loop {
        match rx.recv_timeout(timeout) {
            Ok(qresult) => match qresult {
                QueryResult::Done(result) => return Ok(result),
                QueryResult::Err(err) => return Err(err),
                QueryResult::Busy => continue, // restarts the timeout
            },
            Err(_recverr) => {
                return Err(Errors::NoStdinResponse);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::assert_eq;

    use image::DynamicImage;
    use ratatui::buffer::Buffer;
    use ratatui::layout::{Rect, Size};

    use crate::{
        FontSize, Resize,
        picker::{Capability, Picker, ProtocolType},
    };

    use super::{cap_parser::Response, fallback_picker, interpret_parser_responses};

    /// A probe that cannot create its object is never sent, so an unusable name
    /// or mode would silently mean "this terminal said no" for everybody. The
    /// syscall has to be exercised, not just the string it takes.
    #[test]
    #[cfg(not(windows))]
    fn test_shm_probe_round_trips() {
        use super::{ShmProbe, cap_parser::shm_probe_name};

        let name = shm_probe_name();
        let probe = ShmProbe::create().expect("this platform creates the probe object");
        let fd = rustix::shm::open(
            name.as_str(),
            rustix::shm::OFlags::RDONLY,
            rustix::fs::Mode::empty(),
        )
        .expect("the object exists while the probe is alive");
        // At least the one RGBA pixel `f=32,s=1,v=1` promises, since a terminal
        // rejects an object smaller than `s * v * bpp`. Not exactly: macOS rounds
        // a shared memory object up to a page, so this reads 16384 there and 4 on
        // Linux, and only the floor is a portable claim.
        let size = rustix::fs::fstat(&fd).expect("stat the object").st_size;
        assert!(
            size >= 4,
            "the probe object holds one RGBA pixel, got {size}"
        );
        drop(fd);

        drop(probe);
        assert!(
            rustix::shm::open(
                name.as_str(),
                rustix::shm::OFlags::RDONLY,
                rustix::fs::Mode::empty(),
            )
            .is_err(),
            "the probe leaves nothing behind for a terminal that never read it"
        );
    }

    /// The blind opt-in is unchanged when no probe was asked for, and gated on the
    /// answer when one was — the whole point of adding the probe.
    #[test]
    fn test_resolve_kitty_shm() {
        use super::resolve_kitty_shm;

        assert_eq!(resolve_kitty_shm(Some(7), false, &[]), Some(7));
        assert_eq!(resolve_kitty_shm(Some(7), true, &[]), None);
        assert_eq!(
            resolve_kitty_shm(Some(7), true, &[Capability::Kitty]),
            None,
            "kitty graphics does not imply reaching our filesystem"
        );
        assert_eq!(
            resolve_kitty_shm(Some(7), true, &[Capability::KittySharedMemory]),
            Some(7)
        );
        assert_eq!(
            resolve_kitty_shm(None, true, &[Capability::KittySharedMemory]),
            None,
            "answered, but never asked for"
        );
    }

    #[test]
    fn test_cycle_protocol() {
        let mut proto = ProtocolType::Halfblocks;
        proto = proto.next();
        assert_eq!(proto, ProtocolType::Sixel);
        proto = proto.next();
        assert_eq!(proto, ProtocolType::Kitty);
        proto = proto.next();
        assert_eq!(proto, ProtocolType::Iterm2);
        proto = proto.next();
        assert_eq!(proto, ProtocolType::Halfblocks);
    }

    #[test]
    fn test_set_font_size_keeps_the_rest_of_the_picker() {
        let mut picker = Picker {
            font_size: FontSize::new(10, 20),
            protocol_type: ProtocolType::Kitty,
            background_color: None,
            is_tmux: true,
            capabilities: vec![Capability::Kitty, Capability::KittyCompression],
            kitty_shm: None,
        };

        picker.set_font_size(FontSize::new(7, 15));

        assert_eq!(7, picker.font_size().width);
        assert_eq!(15, picker.font_size().height);
        assert_eq!(ProtocolType::Kitty, picker.protocol_type());
        assert_eq!(
            &vec![Capability::Kitty, Capability::KittyCompression],
            picker.capabilities()
        );
        assert!(picker.is_tmux);
    }

    /// A caller-chosen id reaches the wire, and two encodes under one id are two
    /// transmissions of the same image rather than two images.
    ///
    /// The id is what every placeholder cell of a placement carries, so this is
    /// the difference between a changed picture costing the picture and costing
    /// the whole rect of cells as well.
    #[test]
    fn test_new_protocol_with_id_transmits_under_the_callers_id() {
        let picker = Picker {
            font_size: FontSize::new(10, 20),
            protocol_type: ProtocolType::Kitty,
            background_color: None,
            is_tmux: false,
            capabilities: vec![Capability::Kitty],
            kitty_shm: None,
        };
        let img = |v: u8| {
            DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
                20,
                40,
                image::Rgba([v, 0x40, 0x80, 0xff]),
            ))
        };
        let transmit = |v: u8| {
            let proto = picker
                .new_protocol_with_id(img(v), Size::new(2, 2), Resize::Fit(None), 0x00B0_0007)
                .expect("kitty encodes");
            let mut buf = Buffer::empty(Rect::new(0, 0, 2, 2));
            proto.render(Rect::new(0, 0, 2, 2), &mut buf);
            buf.cell((0, 0))
                .expect("the transmit rides the first cell")
                .symbol()
                .to_string()
        };

        let first = transmit(0x10);
        let second = transmit(0x20);
        for (n, t) in [("first", &first), ("second", &second)] {
            assert!(
                t.contains("i=11534343,"),
                "{n} names the caller's id: {t:?}"
            );
            assert!(t.contains("p=1,"), "{n} names its placement: {t:?}");
        }
        assert_ne!(first, second, "different pixels are still transmitted");
    }

    #[test]
    fn test_from_query_stdio_no_hang() {
        let _ = Picker::from_query_stdio();
    }

    #[test]
    fn test_fallback_picker() {
        // The terminal did not answer, but the font-size is known from the ioctl fallback and
        // some env var hinted at iTerm2 support: use it instead of halfblocks.
        let picker = fallback_picker(
            false,
            Some(ProtocolType::Iterm2),
            Some(FontSize::new(8, 16)),
        );
        assert_eq!(picker.protocol_type(), ProtocolType::Iterm2);
        assert_eq!(
            (picker.font_size().width, picker.font_size().height),
            (8, 16)
        );

        // Without a font-size, no other protocol can be rendered meaningfully.
        let picker = fallback_picker(false, Some(ProtocolType::Iterm2), None);
        assert_eq!(picker.protocol_type(), ProtocolType::Halfblocks);

        // Without a hint, stay on halfblocks.
        let picker = fallback_picker(false, None, Some(FontSize::new(8, 16)));
        assert_eq!(picker.protocol_type(), ProtocolType::Halfblocks);
    }

    #[test]
    fn test_interpret_parser_responses_text_sizing_protocol() {
        let (_, _, caps) = interpret_parser_responses(vec![
            // Example response from Kitty.
            Response::CursorPositionReport(1, 1),
            Response::CursorPositionReport(3, 1),
            Response::CursorPositionReport(5, 1),
        ])
        .unwrap();
        assert!(caps.contains(&Capability::TextSizingProtocol));
    }

    #[test]
    fn test_interpret_parser_responses_text_sizing_protocol_incomplete() {
        let (_, _, caps) = interpret_parser_responses(vec![
            // Example response from Foot, notably moves 2 columns only on `w=2` query, but not
            // `s=2`.
            Response::CursorPositionReport(1, 22),
            Response::CursorPositionReport(3, 22),
            Response::CursorPositionReport(4, 22),
        ])
        .unwrap();
        assert!(!caps.contains(&Capability::TextSizingProtocol));
    }
}
